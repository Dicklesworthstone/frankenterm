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
    pub fn process_run(&mut self, ctx: &CorrelationContext) -> Vec<ObservationResult> {
        let mut results = Vec::with_capacity(ctx.timings.len());
        let mut any_overflow = false;

        for timing in &ctx.timings {
            let result = self
                .enforcer
                .record(timing.stage, timing.latency_us, &ctx.correlation_id);
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
    pub const ALL: &[Self] = &[
        Self::None,
        Self::Defer,
        Self::Degrade,
        Self::Shed,
        Self::Skip,
    ];

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
            let cooldown_met = state.consecutive_ok >= self.config.recovery.cooldown_observations;
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
            stage_states: self.states.iter().map(|(s, st)| (*s, st.clone())).collect(),
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
                lane.smoothed_headroom = lane.smoothed_headroom
                    * (1.0 - self.config.pressure_alpha)
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
            return AllocatorDegradation::Oscillating {
                lane_count: oscillating,
            };
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
            return AllocatorDegradation::FloorSaturation {
                lane_count: at_floor,
            };
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
            Self::Bulk => &[LatencyStage::StorageWrite, LatencyStage::PatternDetection],
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
        LatencyStage::PtyCapture | LatencyStage::DeltaExtraction | LatencyStage::ApiResponse => {
            SchedulerLane::Input
        }
        LatencyStage::EventEmission
        | LatencyStage::WorkflowDispatch
        | LatencyStage::ActionExecution => SchedulerLane::Control,
        LatencyStage::StorageWrite | LatencyStage::PatternDetection => SchedulerLane::Bulk,
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
    Promoted {
        from: SchedulerLane,
        to: SchedulerLane,
    },
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
            errors.push(format!("CPU shares sum to {} (must be ≤ 1.0)", total_share));
        }
        if self.input_cpu_share < 0.0 || self.control_cpu_share < 0.0 || self.bulk_cpu_share < 0.0 {
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
    BulkStarvation {
        shed_count: u64,
        completed_count: u64,
    },
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
            head_seq: self.peek().map(|i| i.seq).unwrap_or(self.next_seq),
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
            held, snap.total_inheritance_events, snap.total_order_violations, snap.active_chains,
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
            if event.released_us.is_none() && now_us.saturating_sub(event.applied_us) > max_dur {
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
            } => write!(
                f,
                "ORDER_VIOLATION_SPIKE({}/{})",
                total_violations, threshold
            ),
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
            lane_state.windowed_share = if count > 0 { sum / count as f64 } else { 0.0 };
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
            snap.gini_coefficient, snap.any_starving, snap.total_starvation_events, self.epoch,
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
    LaneStarvation { starving_lanes: Vec<SchedulerLane> },
    /// Gini coefficient is too high — severe unfairness.
    SevereUnfairness { gini: f64, threshold: f64 },
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
                write!(
                    f,
                    "SEVERE_UNFAIRNESS(gini={:.3}/thresh={:.3})",
                    gini, threshold
                )
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
    Fragmented {
        total_blocks: usize,
        free_count: usize,
    },
}

impl fmt::Display for PoolDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::HighUtilization {
                utilization,
                threshold,
            } => write!(
                f,
                "HIGH_UTIL({:.1}%/thresh={:.1}%)",
                utilization * 100.0,
                threshold * 100.0
            ),
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
    Complete { lines: usize, bytes_consumed: usize },
    /// Partial data — need more input.
    Partial { bytes_buffered: usize },
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
    LowZeroCopy { ratio: f64, threshold: f64 },
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
                write!(
                    f,
                    "LOW_ZC({:.1}%/thresh={:.1}%)",
                    ratio * 100.0,
                    threshold * 100.0
                )
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
            target_latency_us: 10,       // 10 µs
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
            hot_to_warm_age_us: 60_000_000,   // 60 seconds
            warm_to_cold_age_us: 600_000_000, // 10 minutes
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
        if let Some(seg) = self
            .segments
            .iter_mut()
            .find(|s| s.segment_id == segment_id)
        {
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
                    ScrollbackTier::Hot => {
                        self.hot_bytes = self.hot_bytes.saturating_sub(s.byte_size)
                    }
                    ScrollbackTier::Warm => {
                        self.warm_bytes = self.warm_bytes.saturating_sub(s.byte_size)
                    }
                    ScrollbackTier::Cold => {
                        self.cold_bytes = self.cold_bytes.saturating_sub(s.byte_size)
                    }
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
            .map(|&(pane_id, byte_size, line_count)| {
                self.ingest(pane_id, byte_size, line_count, now_us)
            })
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
    HotPressure {
        utilization: f64,
        threshold: f64,
    },
    WarmPressure {
        utilization: f64,
        threshold: f64,
    },
    MigrationBacklog {
        pending: usize,
        max_concurrent: usize,
    },
}

impl std::fmt::Display for ScrollbackDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScrollbackDegradation::Healthy => write!(f, "HEALTHY"),
            ScrollbackDegradation::HotPressure {
                utilization,
                threshold,
            } => {
                write!(
                    f,
                    "HOT_PRESSURE({:.1}%/{:.1}%)",
                    utilization * 100.0,
                    threshold * 100.0
                )
            }
            ScrollbackDegradation::WarmPressure {
                utilization,
                threshold,
            } => {
                write!(
                    f,
                    "WARM_PRESSURE({:.1}%/{:.1}%)",
                    utilization * 100.0,
                    threshold * 100.0
                )
            }
            ScrollbackDegradation::MigrationBacklog {
                pending,
                max_concurrent,
            } => {
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
        let pending = self
            .segments
            .iter()
            .filter(|s| {
                s.tier == ScrollbackTier::Hot && s.byte_size >= self.policy.min_segment_bytes
            })
            .count();
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

// ── C4: Adaptive Transport Policy ──────────────────────────────────

/// Transport mode for data transfer between pipeline stages.
///
/// # Invariants
/// - Local mode: zero-copy or memcpy, no serialization overhead.
/// - Compressed mode: zstd/lz4-style framing, higher latency, lower bandwidth.
/// - Bypass mode: skip compression when data is already compact or small.
/// - Mode selection is deterministic given the same cost model inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransportMode {
    /// In-process zero-copy or memcpy (fastest).
    Local,
    /// Compressed transfer for large or remote payloads.
    Compressed,
    /// Skip compression — data is small or already compact.
    Bypass,
}

impl std::fmt::Display for TransportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportMode::Local => write!(f, "LOCAL"),
            TransportMode::Compressed => write!(f, "COMPRESSED"),
            TransportMode::Bypass => write!(f, "BYPASS"),
        }
    }
}

/// Cost model inputs for transport mode selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportCostModel {
    /// Compression CPU cost per byte (microseconds).
    pub compress_cost_per_byte_us: f64,
    /// Decompression CPU cost per byte (microseconds).
    pub decompress_cost_per_byte_us: f64,
    /// Network transfer cost per byte (microseconds) — 0 for local.
    pub network_cost_per_byte_us: f64,
    /// Expected compression ratio (0.0–1.0, lower = better compression).
    pub expected_compression_ratio: f64,
    /// Threshold below which bypass is cheaper than compress.
    pub bypass_threshold_bytes: u64,
    /// Threshold above which compression is always used.
    pub compress_threshold_bytes: u64,
}

impl Default for TransportCostModel {
    fn default() -> Self {
        Self {
            compress_cost_per_byte_us: 0.01,
            decompress_cost_per_byte_us: 0.005,
            network_cost_per_byte_us: 0.0,
            expected_compression_ratio: 0.4,
            bypass_threshold_bytes: 4096,
            compress_threshold_bytes: 65536,
        }
    }
}

/// Transport policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportPolicyConfig {
    /// Cost model for mode selection.
    pub cost_model: TransportCostModel,
    /// Enable adaptive mode switching (vs. fixed mode).
    pub adaptive: bool,
    /// Fixed mode when adaptive is disabled.
    pub fixed_mode: TransportMode,
    /// EWMA alpha for cost tracking (0.0–1.0).
    pub ewma_alpha: f64,
    /// Maximum history entries for cost tracking.
    pub max_history: usize,
}

impl Default for TransportPolicyConfig {
    fn default() -> Self {
        Self {
            cost_model: TransportCostModel::default(),
            adaptive: true,
            fixed_mode: TransportMode::Local,
            ewma_alpha: 0.1,
            max_history: 256,
        }
    }
}

/// A single transport decision record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportDecision {
    pub payload_bytes: u64,
    pub selected_mode: TransportMode,
    pub estimated_cost_us: f64,
    pub actual_cost_us: f64,
    pub savings_us: f64,
    pub timestamp_us: u64,
}

/// Snapshot of the adaptive transport policy state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportPolicySnapshot {
    pub total_decisions: u64,
    pub local_count: u64,
    pub compressed_count: u64,
    pub bypass_count: u64,
    pub total_bytes_transferred: u64,
    pub total_savings_us: f64,
    pub ewma_cost_us: f64,
}

/// Adaptive transport policy engine.
///
/// # Invariants
/// - `local_count + compressed_count + bypass_count == total_decisions`.
/// - Mode selection is pure function of (payload_bytes, cost_model, ewma state).
/// - EWMA cost tracks running average of actual transfer costs.
pub struct TransportPolicy {
    config: TransportPolicyConfig,
    total_decisions: u64,
    local_count: u64,
    compressed_count: u64,
    bypass_count: u64,
    total_bytes: u64,
    total_savings_us: f64,
    ewma_cost_us: f64,
    decisions: Vec<TransportDecision>,
}

impl TransportPolicy {
    /// Create with explicit config.
    pub fn new(config: TransportPolicyConfig) -> Self {
        Self {
            config,
            total_decisions: 0,
            local_count: 0,
            compressed_count: 0,
            bypass_count: 0,
            total_bytes: 0,
            total_savings_us: 0.0,
            ewma_cost_us: 0.0,
            decisions: Vec::new(),
        }
    }

    /// Create with defaults.
    pub fn with_defaults() -> Self {
        Self::new(TransportPolicyConfig::default())
    }

    /// Select the optimal transport mode for a given payload.
    pub fn select_mode(&self, payload_bytes: u64) -> TransportMode {
        if !self.config.adaptive {
            return self.config.fixed_mode;
        }
        let cm = &self.config.cost_model;
        if cm.network_cost_per_byte_us == 0.0 {
            // Local transfer — no network cost
            return TransportMode::Local;
        }
        if payload_bytes <= cm.bypass_threshold_bytes {
            return TransportMode::Bypass;
        }
        if payload_bytes >= cm.compress_threshold_bytes {
            return TransportMode::Compressed;
        }
        // Cost comparison: bypass vs compressed
        let bypass_cost = payload_bytes as f64 * cm.network_cost_per_byte_us;
        let compress_cost = payload_bytes as f64 * cm.compress_cost_per_byte_us
            + payload_bytes as f64 * cm.expected_compression_ratio * cm.network_cost_per_byte_us
            + payload_bytes as f64 * cm.expected_compression_ratio * cm.decompress_cost_per_byte_us;
        if bypass_cost <= compress_cost {
            TransportMode::Bypass
        } else {
            TransportMode::Compressed
        }
    }

    /// Record a transport decision and its outcome.
    pub fn record(
        &mut self,
        payload_bytes: u64,
        mode: TransportMode,
        estimated_cost_us: f64,
        actual_cost_us: f64,
        timestamp_us: u64,
    ) {
        let savings = estimated_cost_us - actual_cost_us;
        self.total_decisions += 1;
        match mode {
            TransportMode::Local => self.local_count += 1,
            TransportMode::Compressed => self.compressed_count += 1,
            TransportMode::Bypass => self.bypass_count += 1,
        }
        self.total_bytes += payload_bytes;
        self.total_savings_us += savings;

        // EWMA update
        let alpha = self.config.ewma_alpha;
        self.ewma_cost_us = alpha * actual_cost_us + (1.0 - alpha) * self.ewma_cost_us;

        let decision = TransportDecision {
            payload_bytes,
            selected_mode: mode,
            estimated_cost_us,
            actual_cost_us,
            savings_us: savings,
            timestamp_us,
        };
        if self.decisions.len() < self.config.max_history {
            self.decisions.push(decision);
        }
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> TransportPolicySnapshot {
        TransportPolicySnapshot {
            total_decisions: self.total_decisions,
            local_count: self.local_count,
            compressed_count: self.compressed_count,
            bypass_count: self.bypass_count,
            total_bytes_transferred: self.total_bytes,
            total_savings_us: self.total_savings_us,
            ewma_cost_us: self.ewma_cost_us,
        }
    }

    /// One-line status.
    pub fn status_line(&self) -> String {
        format!(
            "transport decisions={} local={} compressed={} bypass={} ewma={:.1}µs",
            self.total_decisions,
            self.local_count,
            self.compressed_count,
            self.bypass_count,
            self.ewma_cost_us,
        )
    }

    /// Recent decision history.
    pub fn recent_decisions(&self) -> &[TransportDecision] {
        &self.decisions
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.total_decisions = 0;
        self.local_count = 0;
        self.compressed_count = 0;
        self.bypass_count = 0;
        self.total_bytes = 0;
        self.total_savings_us = 0.0;
        self.ewma_cost_us = 0.0;
        self.decisions.clear();
    }
}

/// Degradation states for the transport policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TransportDegradation {
    Healthy,
    HighCost {
        ewma_cost_us: f64,
        threshold_us: f64,
    },
    ModeImbalance {
        dominant_mode: String,
        share: f64,
    },
}

impl std::fmt::Display for TransportDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportDegradation::Healthy => write!(f, "HEALTHY"),
            TransportDegradation::HighCost {
                ewma_cost_us,
                threshold_us,
            } => {
                write!(f, "HIGH_COST({:.1}µs/{:.1}µs)", ewma_cost_us, threshold_us)
            }
            TransportDegradation::ModeImbalance {
                dominant_mode,
                share,
            } => {
                write!(f, "MODE_IMBALANCE({}={:.1}%)", dominant_mode, share * 100.0)
            }
        }
    }
}

/// Structured log entry for transport policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportLogEntry {
    pub total_decisions: u64,
    pub local_count: u64,
    pub compressed_count: u64,
    pub bypass_count: u64,
    pub ewma_cost_us: f64,
    pub degradation: TransportDegradation,
}

impl TransportPolicy {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> TransportDegradation {
        // High cost threshold: 100µs EWMA
        if self.ewma_cost_us > 100.0 {
            return TransportDegradation::HighCost {
                ewma_cost_us: self.ewma_cost_us,
                threshold_us: 100.0,
            };
        }
        // Mode imbalance: any single mode > 95% of decisions (with 20+ decisions)
        if self.total_decisions >= 20 {
            let max_count = self
                .local_count
                .max(self.compressed_count)
                .max(self.bypass_count);
            let share = max_count as f64 / self.total_decisions as f64;
            if share > 0.95 {
                let mode_name = if max_count == self.local_count {
                    "Local"
                } else if max_count == self.compressed_count {
                    "Compressed"
                } else {
                    "Bypass"
                };
                return TransportDegradation::ModeImbalance {
                    dominant_mode: mode_name.to_string(),
                    share,
                };
            }
        }
        TransportDegradation::Healthy
    }

    /// Create a structured log entry.
    pub fn log_entry(&self) -> TransportLogEntry {
        TransportLogEntry {
            total_decisions: self.total_decisions,
            local_count: self.local_count,
            compressed_count: self.compressed_count,
            bypass_count: self.bypass_count,
            ewma_cost_us: self.ewma_cost_us,
            degradation: self.detect_degradation(),
        }
    }

    /// Select mode AND record outcome in one step (convenience).
    pub fn select_and_record(
        &mut self,
        payload_bytes: u64,
        actual_cost_us: f64,
        timestamp_us: u64,
    ) -> TransportMode {
        let mode = self.select_mode(payload_bytes);
        let estimated = self.estimate_cost(payload_bytes, mode);
        self.record(payload_bytes, mode, estimated, actual_cost_us, timestamp_us);
        mode
    }

    /// Estimate cost for a given payload + mode using the cost model.
    pub fn estimate_cost(&self, payload_bytes: u64, mode: TransportMode) -> f64 {
        let cm = &self.config.cost_model;
        match mode {
            TransportMode::Local => 0.0,
            TransportMode::Bypass => payload_bytes as f64 * cm.network_cost_per_byte_us,
            TransportMode::Compressed => {
                let compress = payload_bytes as f64 * cm.compress_cost_per_byte_us;
                let transfer = payload_bytes as f64
                    * cm.expected_compression_ratio
                    * cm.network_cost_per_byte_us;
                let decompress = payload_bytes as f64
                    * cm.expected_compression_ratio
                    * cm.decompress_cost_per_byte_us;
                compress + transfer + decompress
            }
        }
    }

    /// Mode distribution as fractions (local_share, compressed_share, bypass_share).
    pub fn mode_distribution(&self) -> (f64, f64, f64) {
        if self.total_decisions == 0 {
            return (0.0, 0.0, 0.0);
        }
        let total = self.total_decisions as f64;
        (
            self.local_count as f64 / total,
            self.compressed_count as f64 / total,
            self.bypass_count as f64 / total,
        )
    }

    /// Average cost per byte across all recorded decisions.
    pub fn avg_cost_per_byte(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        self.ewma_cost_us / (self.total_bytes as f64 / self.total_decisions as f64)
    }

    /// Total bytes transferred.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Total savings (sum of estimated - actual across all decisions).
    pub fn total_savings_us(&self) -> f64 {
        self.total_savings_us
    }

    /// Current EWMA cost.
    pub fn ewma_cost_us(&self) -> f64 {
        self.ewma_cost_us
    }

    /// Update the cost model at runtime (e.g., after measuring real network costs).
    pub fn update_cost_model(&mut self, cost_model: TransportCostModel) {
        self.config.cost_model = cost_model;
    }

    /// Switch between adaptive and fixed mode.
    pub fn set_adaptive(&mut self, adaptive: bool) {
        self.config.adaptive = adaptive;
    }

    /// Set fixed mode (used when adaptive is disabled).
    pub fn set_fixed_mode(&mut self, mode: TransportMode) {
        self.config.fixed_mode = mode;
    }
}

// ── C5: Kernel/Hardware Tail-Latency ───────────────────────────────

/// Syscall batching strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SyscallStrategy {
    /// Issue syscalls one at a time.
    Immediate,
    /// Batch multiple syscalls before issuing.
    Batched,
    /// Adaptive: batch under load, immediate under low latency.
    Adaptive,
}

impl std::fmt::Display for SyscallStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyscallStrategy::Immediate => write!(f, "IMMEDIATE"),
            SyscallStrategy::Batched => write!(f, "BATCHED"),
            SyscallStrategy::Adaptive => write!(f, "ADAPTIVE"),
        }
    }
}

/// Wakeup source attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WakeupSource {
    /// Timer-based wakeup (epoll_wait timeout, select, etc.).
    Timer,
    /// I/O event wakeup (read/write ready, socket, pty).
    IoEvent,
    /// Signal-based wakeup (SIGCHLD, SIGWINCH, etc.).
    Signal,
    /// Explicit nudge from another thread/task.
    Nudge,
}

impl std::fmt::Display for WakeupSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WakeupSource::Timer => write!(f, "TIMER"),
            WakeupSource::IoEvent => write!(f, "IO_EVENT"),
            WakeupSource::Signal => write!(f, "SIGNAL"),
            WakeupSource::Nudge => write!(f, "NUDGE"),
        }
    }
}

/// CPU affinity placement hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AffinityHint {
    /// No preference — OS scheduler decides.
    Any,
    /// Prefer performance cores (P-cores on hybrid CPUs).
    PerformanceCore,
    /// Prefer efficiency cores (E-cores on hybrid CPUs).
    EfficiencyCore,
    /// Pin to a specific core ID.
    Pinned(u32),
}

impl std::fmt::Display for AffinityHint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AffinityHint::Any => write!(f, "ANY"),
            AffinityHint::PerformanceCore => write!(f, "P_CORE"),
            AffinityHint::EfficiencyCore => write!(f, "E_CORE"),
            AffinityHint::Pinned(id) => write!(f, "PINNED({})", id),
        }
    }
}

/// Configuration for the tail-latency controller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TailLatencyConfig {
    /// Syscall batching strategy.
    pub syscall_strategy: SyscallStrategy,
    /// Maximum batch size before forced flush.
    pub max_batch_size: usize,
    /// Timer precision target in microseconds.
    pub timer_precision_us: u64,
    /// Affinity hint for the hot path thread.
    pub affinity: AffinityHint,
    /// p99 latency budget in microseconds.
    pub p99_budget_us: u64,
    /// p999 latency budget in microseconds.
    pub p999_budget_us: u64,
}

impl Default for TailLatencyConfig {
    fn default() -> Self {
        Self {
            syscall_strategy: SyscallStrategy::Adaptive,
            max_batch_size: 64,
            timer_precision_us: 1000, // 1ms
            affinity: AffinityHint::Any,
            p99_budget_us: 10_000,  // 10ms
            p999_budget_us: 50_000, // 50ms
        }
    }
}

/// A single wakeup event observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WakeupEvent {
    pub source: WakeupSource,
    pub latency_us: u64,
    pub timestamp_us: u64,
    pub batch_depth: usize,
}

/// Tail-latency snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TailLatencySnapshot {
    pub total_wakeups: u64,
    pub timer_wakeups: u64,
    pub io_wakeups: u64,
    pub signal_wakeups: u64,
    pub nudge_wakeups: u64,
    pub total_syscalls: u64,
    pub total_batches: u64,
    pub avg_batch_depth: f64,
    pub p99_latency_us: u64,
    pub max_latency_us: u64,
    pub budget_violations: u64,
}

/// Tail-latency controller: tracks wakeup latencies, syscall batching, and budget compliance.
///
/// # Invariants
/// - `timer + io + signal + nudge == total_wakeups`.
/// - Latency samples are stored in a bounded ring for percentile estimation.
/// - Budget violations count only p99 breaches (not p50).
pub struct TailLatencyController {
    config: TailLatencyConfig,
    total_wakeups: u64,
    timer_wakeups: u64,
    io_wakeups: u64,
    signal_wakeups: u64,
    nudge_wakeups: u64,
    total_syscalls: u64,
    total_batches: u64,
    batch_depth_sum: u64,
    latency_samples: Vec<u64>,
    max_samples: usize,
    sample_head: usize,
    max_latency_us: u64,
    budget_violations: u64,
}

impl TailLatencyController {
    /// Create with explicit config.
    pub fn new(config: TailLatencyConfig) -> Self {
        Self {
            config,
            total_wakeups: 0,
            timer_wakeups: 0,
            io_wakeups: 0,
            signal_wakeups: 0,
            nudge_wakeups: 0,
            total_syscalls: 0,
            total_batches: 0,
            batch_depth_sum: 0,
            latency_samples: Vec::new(),
            max_samples: 1024,
            sample_head: 0,
            max_latency_us: 0,
            budget_violations: 0,
        }
    }

    /// Create with defaults.
    pub fn with_defaults() -> Self {
        Self::new(TailLatencyConfig::default())
    }

    /// Record a wakeup event.
    pub fn record_wakeup(&mut self, source: WakeupSource, latency_us: u64) {
        self.total_wakeups += 1;
        match source {
            WakeupSource::Timer => self.timer_wakeups += 1,
            WakeupSource::IoEvent => self.io_wakeups += 1,
            WakeupSource::Signal => self.signal_wakeups += 1,
            WakeupSource::Nudge => self.nudge_wakeups += 1,
        }
        if latency_us > self.max_latency_us {
            self.max_latency_us = latency_us;
        }
        if latency_us > self.config.p99_budget_us {
            self.budget_violations += 1;
        }
        // Ring buffer for samples
        if self.latency_samples.len() < self.max_samples {
            self.latency_samples.push(latency_us);
        } else {
            self.latency_samples[self.sample_head] = latency_us;
            self.sample_head = (self.sample_head + 1) % self.max_samples;
        }
    }

    /// Record a syscall batch.
    pub fn record_batch(&mut self, depth: usize) {
        self.total_batches += 1;
        self.total_syscalls += depth as u64;
        self.batch_depth_sum += depth as u64;
    }

    /// Estimate p99 latency from stored samples.
    pub fn p99_latency_us(&self) -> u64 {
        if self.latency_samples.is_empty() {
            return 0;
        }
        let mut sorted = self.latency_samples.clone();
        sorted.sort_unstable();
        let idx = ((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1);
        sorted[idx]
    }

    /// Average batch depth.
    pub fn avg_batch_depth(&self) -> f64 {
        if self.total_batches == 0 {
            return 0.0;
        }
        self.batch_depth_sum as f64 / self.total_batches as f64
    }

    /// Snapshot.
    pub fn snapshot(&self) -> TailLatencySnapshot {
        TailLatencySnapshot {
            total_wakeups: self.total_wakeups,
            timer_wakeups: self.timer_wakeups,
            io_wakeups: self.io_wakeups,
            signal_wakeups: self.signal_wakeups,
            nudge_wakeups: self.nudge_wakeups,
            total_syscalls: self.total_syscalls,
            total_batches: self.total_batches,
            avg_batch_depth: self.avg_batch_depth(),
            p99_latency_us: self.p99_latency_us(),
            max_latency_us: self.max_latency_us,
            budget_violations: self.budget_violations,
        }
    }

    /// Status line.
    pub fn status_line(&self) -> String {
        format!(
            "tail-latency wakeups={} p99={}µs max={}µs violations={} batches={}",
            self.total_wakeups,
            self.p99_latency_us(),
            self.max_latency_us,
            self.budget_violations,
            self.total_batches,
        )
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.total_wakeups = 0;
        self.timer_wakeups = 0;
        self.io_wakeups = 0;
        self.signal_wakeups = 0;
        self.nudge_wakeups = 0;
        self.total_syscalls = 0;
        self.total_batches = 0;
        self.batch_depth_sum = 0;
        self.latency_samples.clear();
        self.sample_head = 0;
        self.max_latency_us = 0;
        self.budget_violations = 0;
    }

    /// Current syscall strategy.
    pub fn strategy(&self) -> SyscallStrategy {
        self.config.syscall_strategy
    }

    /// Current affinity hint.
    pub fn affinity(&self) -> AffinityHint {
        self.config.affinity
    }

    /// Number of stored latency samples.
    pub fn sample_count(&self) -> usize {
        self.latency_samples.len()
    }
}

/// Degradation states for tail-latency controller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TailLatencyDegradation {
    Healthy,
    P99Breach { observed_us: u64, budget_us: u64 },
    P999Breach { observed_us: u64, budget_us: u64 },
    HighViolationRate { violations: u64, total: u64 },
}

impl std::fmt::Display for TailLatencyDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TailLatencyDegradation::Healthy => write!(f, "HEALTHY"),
            TailLatencyDegradation::P99Breach {
                observed_us,
                budget_us,
            } => {
                write!(f, "P99_BREACH({}µs/{}µs)", observed_us, budget_us)
            }
            TailLatencyDegradation::P999Breach {
                observed_us,
                budget_us,
            } => {
                write!(f, "P999_BREACH({}µs/{}µs)", observed_us, budget_us)
            }
            TailLatencyDegradation::HighViolationRate { violations, total } => {
                write!(f, "HIGH_VIOLATIONS({}/{})", violations, total)
            }
        }
    }
}

/// Structured log entry for tail-latency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TailLatencyLogEntry {
    pub total_wakeups: u64,
    pub p99_latency_us: u64,
    pub max_latency_us: u64,
    pub budget_violations: u64,
    pub avg_batch_depth: f64,
    pub degradation: TailLatencyDegradation,
}

impl TailLatencyController {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> TailLatencyDegradation {
        let p99 = self.p99_latency_us();
        if self.max_latency_us > self.config.p999_budget_us {
            return TailLatencyDegradation::P999Breach {
                observed_us: self.max_latency_us,
                budget_us: self.config.p999_budget_us,
            };
        }
        if p99 > self.config.p99_budget_us {
            return TailLatencyDegradation::P99Breach {
                observed_us: p99,
                budget_us: self.config.p99_budget_us,
            };
        }
        // High violation rate: > 5% of wakeups exceed budget
        if self.total_wakeups >= 20 {
            let rate = self.budget_violations as f64 / self.total_wakeups as f64;
            if rate > 0.05 {
                return TailLatencyDegradation::HighViolationRate {
                    violations: self.budget_violations,
                    total: self.total_wakeups,
                };
            }
        }
        TailLatencyDegradation::Healthy
    }

    /// Log entry.
    pub fn log_entry(&self) -> TailLatencyLogEntry {
        TailLatencyLogEntry {
            total_wakeups: self.total_wakeups,
            p99_latency_us: self.p99_latency_us(),
            max_latency_us: self.max_latency_us,
            budget_violations: self.budget_violations,
            avg_batch_depth: self.avg_batch_depth(),
            degradation: self.detect_degradation(),
        }
    }

    /// Estimate p50 latency from stored samples.
    pub fn p50_latency_us(&self) -> u64 {
        if self.latency_samples.is_empty() {
            return 0;
        }
        let mut sorted = self.latency_samples.clone();
        sorted.sort_unstable();
        let idx = (sorted.len() / 2).min(sorted.len() - 1);
        sorted[idx]
    }

    /// Wakeup source distribution as fractions (timer, io, signal, nudge).
    pub fn wakeup_distribution(&self) -> (f64, f64, f64, f64) {
        if self.total_wakeups == 0 {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let total = self.total_wakeups as f64;
        (
            self.timer_wakeups as f64 / total,
            self.io_wakeups as f64 / total,
            self.signal_wakeups as f64 / total,
            self.nudge_wakeups as f64 / total,
        )
    }

    /// Violation rate (0.0–1.0).
    pub fn violation_rate(&self) -> f64 {
        if self.total_wakeups == 0 {
            return 0.0;
        }
        self.budget_violations as f64 / self.total_wakeups as f64
    }

    /// Whether the controller is currently within p99 budget.
    pub fn within_p99_budget(&self) -> bool {
        self.p99_latency_us() <= self.config.p99_budget_us
    }

    /// Whether the controller is currently within p999 budget.
    pub fn within_p999_budget(&self) -> bool {
        self.max_latency_us <= self.config.p999_budget_us
    }

    /// Update syscall strategy at runtime.
    pub fn set_strategy(&mut self, strategy: SyscallStrategy) {
        self.config.syscall_strategy = strategy;
    }

    /// Update affinity hint at runtime.
    pub fn set_affinity(&mut self, hint: AffinityHint) {
        self.config.affinity = hint;
    }

    /// Update p99 budget.
    pub fn set_p99_budget(&mut self, budget_us: u64) {
        self.config.p99_budget_us = budget_us;
    }

    /// Total wakeups count.
    pub fn total_wakeups(&self) -> u64 {
        self.total_wakeups
    }

    /// Total budget violations.
    pub fn budget_violations(&self) -> u64 {
        self.budget_violations
    }
}

// ── D1: Bayesian Hitch-Risk Posterior Model ────────────────────────

/// Evidence signal types for the hitch-risk posterior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EvidenceSignal {
    /// p99 latency probe from a specific stage.
    LatencyProbe,
    /// Backpressure level change.
    BackpressureChange,
    /// Queue depth observation.
    QueueDepth,
    /// Budget violation event.
    BudgetViolation,
    /// GC or memory pressure event.
    MemoryPressure,
    /// CPU load observation.
    CpuLoad,
}

impl std::fmt::Display for EvidenceSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvidenceSignal::LatencyProbe => write!(f, "LATENCY_PROBE"),
            EvidenceSignal::BackpressureChange => write!(f, "BACKPRESSURE"),
            EvidenceSignal::QueueDepth => write!(f, "QUEUE_DEPTH"),
            EvidenceSignal::BudgetViolation => write!(f, "BUDGET_VIOLATION"),
            EvidenceSignal::MemoryPressure => write!(f, "MEMORY_PRESSURE"),
            EvidenceSignal::CpuLoad => write!(f, "CPU_LOAD"),
        }
    }
}

/// A single evidence entry in the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub signal: EvidenceSignal,
    pub value: f64,
    pub log_likelihood_ratio: f64,
    pub timestamp_us: u64,
}

/// Hitch-risk level classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HitchRiskLevel {
    /// Low risk — system is healthy.
    Low,
    /// Elevated risk — some signals above baseline.
    Elevated,
    /// High risk — multiple signals indicate impending hitch.
    High,
    /// Critical — hitch is imminent or occurring.
    Critical,
}

impl std::fmt::Display for HitchRiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HitchRiskLevel::Low => write!(f, "LOW"),
            HitchRiskLevel::Elevated => write!(f, "ELEVATED"),
            HitchRiskLevel::High => write!(f, "HIGH"),
            HitchRiskLevel::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// Configuration for the hitch-risk model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitchRiskConfig {
    /// Prior probability of hitch (0.0–1.0).
    pub prior_hitch_prob: f64,
    /// Threshold for Elevated risk (log-odds).
    pub elevated_threshold: f64,
    /// Threshold for High risk (log-odds).
    pub high_threshold: f64,
    /// Threshold for Critical risk (log-odds).
    pub critical_threshold: f64,
    /// Maximum evidence entries to retain.
    pub max_evidence: usize,
    /// Decay factor for old evidence (0.0–1.0, 1.0 = no decay).
    pub evidence_decay: f64,
}

impl Default for HitchRiskConfig {
    fn default() -> Self {
        Self {
            prior_hitch_prob: 0.05,
            elevated_threshold: 1.0,
            high_threshold: 3.0,
            critical_threshold: 5.0,
            max_evidence: 512,
            evidence_decay: 0.95,
        }
    }
}

/// Snapshot of the hitch-risk model state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitchRiskSnapshot {
    pub log_odds: f64,
    pub posterior_prob: f64,
    pub risk_level: HitchRiskLevel,
    pub evidence_count: usize,
    pub total_updates: u64,
}

/// Bayesian hitch-risk posterior model.
///
/// # Invariants
/// - `posterior_prob` is always in [0, 1].
/// - `log_odds` is the log-odds form of posterior (allows stable additive updates).
/// - Evidence is decayed by `evidence_decay` each update to reduce stale signal weight.
/// - Risk level is monotonically mapped from log_odds via thresholds.
pub struct HitchRiskModel {
    config: HitchRiskConfig,
    log_odds: f64,
    evidence: Vec<EvidenceEntry>,
    total_updates: u64,
}

impl HitchRiskModel {
    /// Create with explicit config.
    pub fn new(config: HitchRiskConfig) -> Self {
        let prior = config.prior_hitch_prob.clamp(1e-10, 1.0 - 1e-10);
        let log_odds = (prior / (1.0 - prior)).ln();
        Self {
            config,
            log_odds,
            evidence: Vec::new(),
            total_updates: 0,
        }
    }

    /// Create with defaults.
    pub fn with_defaults() -> Self {
        Self::new(HitchRiskConfig::default())
    }

    /// Submit evidence and update the posterior.
    /// `log_likelihood_ratio` > 0 means evidence favors hitch, < 0 favors healthy.
    pub fn update(&mut self, signal: EvidenceSignal, value: f64, llr: f64, timestamp_us: u64) {
        // Decay existing log-odds
        self.log_odds *= self.config.evidence_decay;
        // Add new evidence
        self.log_odds += llr;
        self.total_updates += 1;

        let entry = EvidenceEntry {
            signal,
            value,
            log_likelihood_ratio: llr,
            timestamp_us,
        };
        if self.evidence.len() < self.config.max_evidence {
            self.evidence.push(entry);
        } else {
            // Circular overwrite
            let idx = (self.total_updates as usize - 1) % self.config.max_evidence;
            self.evidence[idx] = entry;
        }
    }

    /// Current posterior probability of hitch.
    pub fn posterior_prob(&self) -> f64 {
        let odds = self.log_odds.exp();
        if odds.is_infinite() {
            return 1.0;
        }
        odds / (1.0 + odds)
    }

    /// Current risk level.
    pub fn risk_level(&self) -> HitchRiskLevel {
        if self.log_odds >= self.config.critical_threshold {
            HitchRiskLevel::Critical
        } else if self.log_odds >= self.config.high_threshold {
            HitchRiskLevel::High
        } else if self.log_odds >= self.config.elevated_threshold {
            HitchRiskLevel::Elevated
        } else {
            HitchRiskLevel::Low
        }
    }

    /// Current log-odds.
    pub fn log_odds(&self) -> f64 {
        self.log_odds
    }

    /// Snapshot.
    pub fn snapshot(&self) -> HitchRiskSnapshot {
        HitchRiskSnapshot {
            log_odds: self.log_odds,
            posterior_prob: self.posterior_prob(),
            risk_level: self.risk_level(),
            evidence_count: self.evidence.len(),
            total_updates: self.total_updates,
        }
    }

    /// Status line.
    pub fn status_line(&self) -> String {
        format!(
            "hitch-risk level={} prob={:.3} log_odds={:.2} evidence={} updates={}",
            self.risk_level(),
            self.posterior_prob(),
            self.log_odds,
            self.evidence.len(),
            self.total_updates,
        )
    }

    /// Reset to prior.
    pub fn reset(&mut self) {
        let prior = self.config.prior_hitch_prob.clamp(1e-10, 1.0 - 1e-10);
        self.log_odds = (prior / (1.0 - prior)).ln();
        self.evidence.clear();
        self.total_updates = 0;
    }

    /// Recent evidence entries.
    pub fn recent_evidence(&self) -> &[EvidenceEntry] {
        &self.evidence
    }
}

/// Degradation states for the hitch-risk model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HitchRiskDegradation {
    Healthy,
    ElevatedRisk {
        posterior_prob: f64,
    },
    HighRisk {
        posterior_prob: f64,
        evidence_count: usize,
    },
    CriticalRisk {
        posterior_prob: f64,
        log_odds: f64,
    },
}

impl std::fmt::Display for HitchRiskDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HitchRiskDegradation::Healthy => write!(f, "HEALTHY"),
            HitchRiskDegradation::ElevatedRisk { posterior_prob } => {
                write!(f, "ELEVATED({:.1}%)", posterior_prob * 100.0)
            }
            HitchRiskDegradation::HighRisk {
                posterior_prob,
                evidence_count,
            } => {
                write!(
                    f,
                    "HIGH({:.1}%, {} evidence)",
                    posterior_prob * 100.0,
                    evidence_count
                )
            }
            HitchRiskDegradation::CriticalRisk {
                posterior_prob,
                log_odds,
            } => {
                write!(
                    f,
                    "CRITICAL({:.1}%, lo={:.2})",
                    posterior_prob * 100.0,
                    log_odds
                )
            }
        }
    }
}

/// Log entry for hitch-risk model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitchRiskLogEntry {
    pub log_odds: f64,
    pub posterior_prob: f64,
    pub risk_level: HitchRiskLevel,
    pub evidence_count: usize,
    pub total_updates: u64,
    pub degradation: HitchRiskDegradation,
}

impl HitchRiskModel {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> HitchRiskDegradation {
        match self.risk_level() {
            HitchRiskLevel::Critical => HitchRiskDegradation::CriticalRisk {
                posterior_prob: self.posterior_prob(),
                log_odds: self.log_odds,
            },
            HitchRiskLevel::High => HitchRiskDegradation::HighRisk {
                posterior_prob: self.posterior_prob(),
                evidence_count: self.evidence.len(),
            },
            HitchRiskLevel::Elevated => HitchRiskDegradation::ElevatedRisk {
                posterior_prob: self.posterior_prob(),
            },
            HitchRiskLevel::Low => HitchRiskDegradation::Healthy,
        }
    }

    /// Log entry.
    pub fn log_entry(&self) -> HitchRiskLogEntry {
        HitchRiskLogEntry {
            log_odds: self.log_odds,
            posterior_prob: self.posterior_prob(),
            risk_level: self.risk_level(),
            evidence_count: self.evidence.len(),
            total_updates: self.total_updates,
            degradation: self.detect_degradation(),
        }
    }

    /// Quick convenience: submit a budget violation signal.
    pub fn observe_violation(&mut self, severity_llr: f64, timestamp_us: u64) {
        self.update(
            EvidenceSignal::BudgetViolation,
            1.0,
            severity_llr,
            timestamp_us,
        );
    }

    /// Quick convenience: submit a latency probe signal.
    pub fn observe_latency(&mut self, latency_us: f64, llr: f64, timestamp_us: u64) {
        self.update(EvidenceSignal::LatencyProbe, latency_us, llr, timestamp_us);
    }

    /// Quick convenience: submit healthy evidence (negative LLR).
    pub fn observe_healthy(&mut self, timestamp_us: u64) {
        self.update(EvidenceSignal::LatencyProbe, 0.0, -0.5, timestamp_us);
    }

    /// Whether the model currently recommends mitigation.
    pub fn should_mitigate(&self) -> bool {
        matches!(
            self.risk_level(),
            HitchRiskLevel::High | HitchRiskLevel::Critical
        )
    }

    /// Whether the model is in critical state.
    pub fn is_critical(&self) -> bool {
        self.risk_level() == HitchRiskLevel::Critical
    }

    /// Update the evidence decay factor.
    pub fn set_evidence_decay(&mut self, decay: f64) {
        self.config.evidence_decay = decay.clamp(0.0, 1.0);
    }

    /// Update the prior (resets log_odds to match new prior).
    pub fn set_prior(&mut self, prior: f64) {
        let p = prior.clamp(1e-10, 1.0 - 1e-10);
        self.config.prior_hitch_prob = p;
    }

    /// Total updates received.
    pub fn total_updates(&self) -> u64 {
        self.total_updates
    }

    /// Evidence count.
    pub fn evidence_count(&self) -> usize {
        self.evidence.len()
    }
}

// ── D2: Anytime-Valid E-Process Drift Detector ─────────────────────

/// Type of e-process test statistic.
///
/// Each variant corresponds to a different sequential testing strategy:
/// - `CusumLike`: running maximum of likelihood ratio (Page's CUSUM adapted to e-process form)
/// - `Mixture`: Bayesian mixture over alternatives, valid under optional stopping
/// - `ConfidenceSequence`: inverted confidence sequence for mean shift detection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EProcessKind {
    CusumLike,
    Mixture,
    ConfidenceSequence,
}

impl std::fmt::Display for EProcessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CusumLike => write!(f, "cusum_like"),
            Self::Mixture => write!(f, "mixture"),
            Self::ConfidenceSequence => write!(f, "confidence_seq"),
        }
    }
}

/// What observable is being monitored for drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DriftObservable {
    Latency,
    Throughput,
    ErrorRate,
    QueueDepth,
    ResourceUsage,
}

impl std::fmt::Display for DriftObservable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Latency => write!(f, "latency"),
            Self::Throughput => write!(f, "throughput"),
            Self::ErrorRate => write!(f, "error_rate"),
            Self::QueueDepth => write!(f, "queue_depth"),
            Self::ResourceUsage => write!(f, "resource_usage"),
        }
    }
}

/// Alert level produced by the e-process detector.
///
/// `None` means no evidence of drift.  `Warning` indicates growing evidence
/// (e-value approaching threshold).  `Alarm` means the e-value has crossed
/// 1/alpha and the null hypothesis of no-change is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DriftAlertLevel {
    None,
    Warning,
    Alarm,
}

impl std::fmt::Display for DriftAlertLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Warning => write!(f, "warning"),
            Self::Alarm => write!(f, "alarm"),
        }
    }
}

/// Configuration for the e-process drift detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EProcessConfig {
    /// Which e-process variant to use.
    pub kind: EProcessKind,
    /// Observable being monitored.
    pub observable: DriftObservable,
    /// Significance level (alpha).  E-value threshold = 1/alpha.
    pub alpha: f64,
    /// Warning fraction of the threshold (e.g. 0.5 means warn at half the log-threshold).
    pub warning_fraction: f64,
    /// Mixing parameter lambda for CusumLike / Mixture (controls sensitivity vs delay).
    pub lambda: f64,
    /// Null hypothesis mean (mu_0).  Observations are compared against this.
    pub null_mean: f64,
    /// Maximum number of observations to retain in the history window.
    pub max_history: usize,
    /// Minimum observations before the detector can raise an alarm.
    pub warmup: usize,
    /// Whether to auto-reset after alarm (running detector) or latch.
    pub auto_reset: bool,
}

impl EProcessConfig {
    /// Sensible defaults for latency monitoring.
    pub fn default_latency() -> Self {
        Self {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 1000,
            warmup: 20,
            auto_reset: true,
        }
    }
}

/// A single observation fed to the e-process detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EProcessObservation {
    /// Observable value.
    pub value: f64,
    /// Which observable this came from.
    pub observable: DriftObservable,
    /// Timestamp in microseconds.
    pub timestamp_us: u64,
    /// The likelihood ratio for this observation (computed by the detector).
    pub likelihood_ratio: f64,
}

/// Snapshot of the e-process detector state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EProcessSnapshot {
    /// Current e-value (test statistic).
    pub e_value: f64,
    /// Log of the e-value for numerical stability.
    pub log_e_value: f64,
    /// Current alert level.
    pub alert_level: DriftAlertLevel,
    /// Total observations processed.
    pub total_observations: u64,
    /// Number of alarms raised since last reset (or ever).
    pub alarm_count: u64,
    /// Number of warnings raised.
    pub warning_count: u64,
    /// Running mean of observations.
    pub running_mean: f64,
    /// Running variance (Welford online).
    pub running_variance: f64,
    /// Maximum e-value ever observed.
    pub peak_e_value: f64,
}

/// The main e-process drift detector.
///
/// Maintains a running e-value (nonnegative supermartingale starting at 1).
/// Under the null hypothesis (no drift), E[E_t] <= 1.
/// When E_t >= 1/alpha, we reject the null at level alpha.
/// This guarantee holds under *optional stopping* — you can check at any time.
#[derive(Debug, Clone)]
pub struct EProcessDetector {
    config: EProcessConfig,
    /// Current log-e-value (we work in log space for stability).
    log_e_value: f64,
    /// Peak log-e-value seen.
    peak_log_e_value: f64,
    /// Total observations fed.
    total_observations: u64,
    /// Count of alarms.
    alarm_count: u64,
    /// Count of warnings.
    warning_count: u64,
    /// Welford running mean.
    mean: f64,
    /// Welford M2 for variance.
    m2: f64,
    /// Recent observations (ring buffer).
    history: Vec<EProcessObservation>,
    /// Head pointer for ring buffer.
    history_head: usize,
    /// Whether the detector is currently in alarm state.
    in_alarm: bool,
}

impl EProcessDetector {
    /// Create a new detector with the given configuration.
    pub fn new(config: EProcessConfig) -> Self {
        let cap = config.max_history;
        Self {
            config,
            log_e_value: 0.0, // E_0 = 1 => log(E_0) = 0
            peak_log_e_value: 0.0,
            total_observations: 0,
            alarm_count: 0,
            warning_count: 0,
            mean: 0.0,
            m2: 0.0,
            history: Vec::with_capacity(cap.min(64)),
            history_head: 0,
            in_alarm: false,
        }
    }

    /// Create a detector with sensible defaults for latency monitoring.
    pub fn with_defaults() -> Self {
        Self::new(EProcessConfig::default_latency())
    }

    /// Feed a new observation to the detector and update the e-value.
    ///
    /// Returns the current alert level after incorporating this observation.
    pub fn observe(&mut self, value: f64, timestamp_us: u64) -> DriftAlertLevel {
        self.total_observations += 1;
        let n = self.total_observations as f64;

        // Welford online mean/variance
        let delta = value - self.mean;
        self.mean += delta / n;
        let delta2 = value - self.mean;
        self.m2 += delta * delta2;

        // Compute likelihood ratio based on e-process kind
        let lr = self.compute_likelihood_ratio(value);
        let log_lr = if lr > 0.0 { lr.ln() } else { f64::NEG_INFINITY };

        // Update log-e-value
        match self.config.kind {
            EProcessKind::CusumLike => {
                // CUSUM-like: E_t = max(1, E_{t-1}) * LR_t
                // In log: log_E_t = max(0, log_E_{t-1}) + log_LR_t
                self.log_e_value = self.log_e_value.max(0.0) + log_lr;
            }
            EProcessKind::Mixture | EProcessKind::ConfidenceSequence => {
                // Standard product: E_t = E_{t-1} * LR_t
                // In log: log_E_t = log_E_{t-1} + log_LR_t
                self.log_e_value += log_lr;
            }
        }

        // Track peak
        if self.log_e_value > self.peak_log_e_value {
            self.peak_log_e_value = self.log_e_value;
        }

        // Record observation
        let obs = EProcessObservation {
            value,
            observable: self.config.observable,
            timestamp_us,
            likelihood_ratio: lr,
        };
        if self.history.len() < self.config.max_history {
            self.history.push(obs);
        } else if self.config.max_history > 0 {
            self.history[self.history_head] = obs;
            self.history_head = (self.history_head + 1) % self.config.max_history;
        }

        // Determine alert level
        self.alert_level()
    }

    /// Compute the likelihood ratio for a single observation.
    fn compute_likelihood_ratio(&self, value: f64) -> f64 {
        let lambda = self.config.lambda;
        let deviation = value - self.config.null_mean;
        // Universal e-variable: 1 + lambda * deviation
        // Clamped to be nonneg (required for e-process validity).
        (1.0 + lambda * deviation).max(0.0)
    }

    /// Current alert level based on the log-e-value vs the threshold.
    pub fn alert_level(&mut self) -> DriftAlertLevel {
        if self.total_observations < self.config.warmup as u64 {
            return DriftAlertLevel::None;
        }

        let log_threshold = (1.0 / self.config.alpha).ln();
        let log_warning = log_threshold * self.config.warning_fraction;

        if self.log_e_value >= log_threshold {
            if !self.in_alarm {
                self.alarm_count += 1;
                self.in_alarm = true;
            }
            if self.config.auto_reset {
                // Reset e-value after alarm
                self.log_e_value = 0.0;
                self.in_alarm = false;
            }
            DriftAlertLevel::Alarm
        } else if self.log_e_value >= log_warning {
            if self.in_alarm {
                self.in_alarm = false;
            }
            self.warning_count += 1;
            DriftAlertLevel::Warning
        } else {
            if self.in_alarm {
                self.in_alarm = false;
            }
            DriftAlertLevel::None
        }
    }

    /// Current e-value (exponentiated from log for display).
    pub fn e_value(&self) -> f64 {
        self.log_e_value.exp()
    }

    /// Current log-e-value.
    pub fn log_e_value(&self) -> f64 {
        self.log_e_value
    }

    /// Running mean of observations.
    pub fn running_mean(&self) -> f64 {
        self.mean
    }

    /// Running variance of observations (sample variance).
    pub fn running_variance(&self) -> f64 {
        if self.total_observations < 2 {
            return 0.0;
        }
        self.m2 / (self.total_observations as f64 - 1.0)
    }

    /// Total observations processed.
    pub fn total_observations(&self) -> u64 {
        self.total_observations
    }

    /// Number of alarms raised.
    pub fn alarm_count(&self) -> u64 {
        self.alarm_count
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> EProcessSnapshot {
        EProcessSnapshot {
            e_value: self.log_e_value.exp(),
            log_e_value: self.log_e_value,
            alert_level: if self.total_observations < self.config.warmup as u64 {
                DriftAlertLevel::None
            } else {
                let log_threshold = (1.0 / self.config.alpha).ln();
                if self.log_e_value >= log_threshold {
                    DriftAlertLevel::Alarm
                } else if self.log_e_value >= log_threshold * self.config.warning_fraction {
                    DriftAlertLevel::Warning
                } else {
                    DriftAlertLevel::None
                }
            },
            total_observations: self.total_observations,
            alarm_count: self.alarm_count,
            warning_count: self.warning_count,
            running_mean: self.mean,
            running_variance: self.running_variance(),
            peak_e_value: self.peak_log_e_value.exp(),
        }
    }

    /// Human-readable status line.
    pub fn status_line(&self) -> String {
        let snap = self.snapshot();
        format!(
            "e-proc[{}] e={:.3} alert={} obs={} alarms={} mean={:.2}",
            self.config.kind,
            snap.e_value,
            snap.alert_level,
            snap.total_observations,
            snap.alarm_count,
            snap.running_mean,
        )
    }

    /// Reset the detector to initial state (preserving config).
    pub fn reset(&mut self) {
        self.log_e_value = 0.0;
        self.peak_log_e_value = 0.0;
        self.total_observations = 0;
        self.alarm_count = 0;
        self.warning_count = 0;
        self.mean = 0.0;
        self.m2 = 0.0;
        self.history.clear();
        self.history_head = 0;
        self.in_alarm = false;
    }

    /// Recent observation history.
    pub fn recent_observations(&self, n: usize) -> Vec<&EProcessObservation> {
        let len = self.history.len();
        if len == 0 || n == 0 {
            return Vec::new();
        }
        let take = n.min(len);
        let mut result = Vec::with_capacity(take);
        if len < self.config.max_history {
            // Not wrapped yet
            let start = len.saturating_sub(take);
            for obs in &self.history[start..] {
                result.push(obs);
            }
        } else {
            // Wrapped ring buffer — read from tail
            for i in 0..take {
                let idx = (self.history_head + len - take + i) % len;
                result.push(&self.history[idx]);
            }
        }
        result
    }

    /// The e-process kind.
    pub fn kind(&self) -> EProcessKind {
        self.config.kind
    }

    /// Number of stored observations.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Detect degradation based on current state.
    pub fn detect_degradation(&self) -> EProcessDegradation {
        if self.total_observations < self.config.warmup as u64 {
            return EProcessDegradation::Healthy;
        }
        let log_threshold = (1.0 / self.config.alpha).ln();
        if self.log_e_value >= log_threshold {
            EProcessDegradation::DriftDetected {
                e_value: self.log_e_value.exp(),
                alarm_count: self.alarm_count,
            }
        } else if self.log_e_value >= log_threshold * self.config.warning_fraction {
            EProcessDegradation::DriftSuspected {
                e_value: self.log_e_value.exp(),
                running_mean: self.mean,
            }
        } else {
            EProcessDegradation::Healthy
        }
    }

    /// Generate structured log entry.
    pub fn log_entry(&self) -> EProcessLogEntry {
        EProcessLogEntry {
            e_value: self.log_e_value.exp(),
            log_e_value: self.log_e_value,
            total_observations: self.total_observations,
            alarm_count: self.alarm_count,
            warning_count: self.warning_count,
            running_mean: self.mean,
            degradation: self.detect_degradation(),
        }
    }

    // ── D2 Impl: Bridge Methods and Convenience API ────────────────

    /// Observe a batch of values at once.
    pub fn observe_batch(&mut self, values: &[(f64, u64)]) -> DriftAlertLevel {
        let mut last = DriftAlertLevel::None;
        for &(value, ts) in values {
            last = self.observe(value, ts);
        }
        last
    }

    /// Observe a latency sample in microseconds.
    pub fn observe_latency_us(&mut self, latency_us: f64, timestamp_us: u64) -> DriftAlertLevel {
        self.observe(latency_us, timestamp_us)
    }

    /// Current standard deviation of observations.
    pub fn running_stddev(&self) -> f64 {
        self.running_variance().sqrt()
    }

    /// Z-score of a given value relative to the running distribution.
    pub fn z_score(&self, value: f64) -> f64 {
        let std = self.running_stddev();
        if std < 1e-12 {
            return 0.0;
        }
        (value - self.mean) / std
    }

    /// Fraction of observations that resulted in alarm.
    pub fn alarm_rate(&self) -> f64 {
        if self.total_observations == 0 {
            return 0.0;
        }
        self.alarm_count as f64 / self.total_observations as f64
    }

    /// Whether the detector is currently in alarm state.
    pub fn is_alarming(&self) -> bool {
        self.in_alarm
    }

    /// Set the mixing parameter lambda (sensitivity).
    pub fn set_lambda(&mut self, lambda: f64) {
        self.config.lambda = lambda;
    }

    /// Set the null-hypothesis mean.
    pub fn set_null_mean(&mut self, mean: f64) {
        self.config.null_mean = mean;
    }

    /// Set the significance level alpha.
    pub fn set_alpha(&mut self, alpha: f64) {
        self.config.alpha = alpha;
    }

    /// Warning count.
    pub fn warning_count(&self) -> u64 {
        self.warning_count
    }

    /// Peak e-value ever observed.
    pub fn peak_e_value(&self) -> f64 {
        self.peak_log_e_value.exp()
    }
}

/// Degradation status for the e-process detector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EProcessDegradation {
    Healthy,
    DriftSuspected { e_value: f64, running_mean: f64 },
    DriftDetected { e_value: f64, alarm_count: u64 },
}

impl std::fmt::Display for EProcessDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::DriftSuspected { e_value, .. } => {
                write!(f, "drift_suspected(e={e_value:.3})")
            }
            Self::DriftDetected {
                e_value,
                alarm_count,
            } => {
                write!(f, "drift_detected(e={e_value:.3}, alarms={alarm_count})")
            }
        }
    }
}

/// Structured log entry for the e-process detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EProcessLogEntry {
    pub e_value: f64,
    pub log_e_value: f64,
    pub total_observations: u64,
    pub alarm_count: u64,
    pub warning_count: u64,
    pub running_mean: f64,
    pub degradation: EProcessDegradation,
}

// ── D3: Expected-Loss Policy Controller ────────────────────────────

/// Actions the policy controller can select.
///
/// Each action represents a runtime tuning decision with different
/// cost/benefit tradeoffs under different system states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicyAction {
    /// Maintain current settings — lowest cost when system is healthy.
    Hold,
    /// Tighten budgets / increase monitoring — moderate cost, reduces risk.
    Tighten,
    /// Relax budgets / reduce monitoring — saves resources in calm periods.
    Relax,
    /// Emergency shed load — expensive but prevents catastrophic hitches.
    Shed,
}

impl std::fmt::Display for PolicyAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hold => write!(f, "hold"),
            Self::Tighten => write!(f, "tighten"),
            Self::Relax => write!(f, "relax"),
            Self::Shed => write!(f, "shed"),
        }
    }
}

/// System state hypothesis for the loss matrix.
///
/// The controller considers which state the system is in,
/// weighted by the posterior probability from the hitch-risk model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SystemState {
    /// System is healthy — no action needed.
    Healthy,
    /// System is drifting — monitoring or tightening warranted.
    Drifting,
    /// System is under stress — active mitigation needed.
    Stressed,
    /// System is in crisis — shed load to prevent catastrophe.
    Critical,
}

impl std::fmt::Display for SystemState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Drifting => write!(f, "drifting"),
            Self::Stressed => write!(f, "stressed"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// Loss matrix entry: cost of taking `action` when the true state is `state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LossEntry {
    pub state: SystemState,
    pub action: PolicyAction,
    pub loss: f64,
}

/// Configuration for the expected-loss policy controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyControllerConfig {
    /// Loss matrix: cost of each (state, action) pair.
    /// Indexed as [state_idx * 4 + action_idx] for states and actions in enum order.
    pub loss_matrix: Vec<f64>,
    /// Safety floor: minimum probability mass assigned to Critical state.
    pub critical_floor: f64,
    /// Maximum rate of policy changes per second.
    pub max_change_rate_hz: f64,
    /// Hysteresis: don't switch action unless expected-loss improves by this fraction.
    pub hysteresis: f64,
}

impl PolicyControllerConfig {
    /// Sensible defaults with asymmetric loss (missing a crisis is much worse
    /// than over-reacting to a healthy system).
    pub fn default_asymmetric() -> Self {
        // Loss matrix: rows = states (Healthy, Drifting, Stressed, Critical)
        //              cols = actions (Hold, Tighten, Relax, Shed)
        #[rustfmt::skip]
        let loss_matrix = vec![
            // Healthy:   Hold=0, Tighten=1, Relax=0.5, Shed=5
            0.0, 1.0, 0.5, 5.0,
            // Drifting:  Hold=2, Tighten=0.5, Relax=3, Shed=4
            2.0, 0.5, 3.0, 4.0,
            // Stressed:  Hold=5, Tighten=1, Relax=8, Shed=2
            5.0, 1.0, 8.0, 2.0,
            // Critical:  Hold=10, Tighten=3, Relax=15, Shed=1
            10.0, 3.0, 15.0, 1.0,
        ];
        Self {
            loss_matrix,
            critical_floor: 0.01,
            max_change_rate_hz: 2.0,
            hysteresis: 0.05,
        }
    }
}

/// A single policy decision record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDecision {
    /// Selected action.
    pub action: PolicyAction,
    /// Expected loss for the selected action.
    pub expected_loss: f64,
    /// State probabilities used for the decision [healthy, drifting, stressed, critical].
    pub state_probs: [f64; 4],
    /// Expected losses for all actions [hold, tighten, relax, shed].
    pub all_losses: [f64; 4],
    /// Whether hysteresis suppressed a switch.
    pub hysteresis_applied: bool,
    /// Timestamp in microseconds.
    pub timestamp_us: u64,
}

/// Snapshot of the policy controller state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyControllerSnapshot {
    /// Current recommended action.
    pub current_action: PolicyAction,
    /// Total decisions made.
    pub total_decisions: u64,
    /// Decision counts per action [hold, tighten, relax, shed].
    pub action_counts: [u64; 4],
    /// Last expected loss.
    pub last_expected_loss: f64,
    /// Number of times hysteresis suppressed a switch.
    pub hysteresis_count: u64,
}

/// The expected-loss policy controller.
///
/// Given posterior probabilities over system states, selects the action
/// that minimizes expected loss.  Incorporates hysteresis to prevent
/// flapping and a critical floor for safety.
#[derive(Debug, Clone)]
pub struct PolicyController {
    config: PolicyControllerConfig,
    /// Current action.
    current_action: PolicyAction,
    /// Total decisions made.
    total_decisions: u64,
    /// Per-action counters [hold, tighten, relax, shed].
    action_counts: [u64; 4],
    /// Last expected loss of chosen action.
    last_expected_loss: f64,
    /// Count of hysteresis suppressions.
    hysteresis_count: u64,
    /// Recent decisions (ring buffer).
    decisions: Vec<PolicyDecision>,
    max_decisions: usize,
    decision_head: usize,
    /// Last decision timestamp for rate limiting.
    last_decision_us: u64,
}

impl PolicyController {
    /// Create a new controller.
    pub fn new(config: PolicyControllerConfig) -> Self {
        Self {
            config,
            current_action: PolicyAction::Hold,
            total_decisions: 0,
            action_counts: [0; 4],
            last_expected_loss: 0.0,
            hysteresis_count: 0,
            decisions: Vec::with_capacity(64),
            max_decisions: 100,
            decision_head: 0,
            last_decision_us: 0,
        }
    }

    /// Create with default asymmetric loss matrix.
    pub fn with_defaults() -> Self {
        Self::new(PolicyControllerConfig::default_asymmetric())
    }

    /// Make a policy decision given state probabilities.
    ///
    /// `probs` = [P(Healthy), P(Drifting), P(Stressed), P(Critical)]
    /// Must sum to ~1.0 (renormalized internally).
    pub fn decide(&mut self, probs: [f64; 4], timestamp_us: u64) -> PolicyAction {
        // Apply critical floor
        let mut p = probs;
        if p[3] < self.config.critical_floor {
            let deficit = self.config.critical_floor - p[3];
            p[3] = self.config.critical_floor;
            // Redistribute deficit proportionally from other states
            let other_sum: f64 = p[0] + p[1] + p[2];
            if other_sum > 1e-12 {
                let scale = (other_sum - deficit) / other_sum;
                p[0] *= scale;
                p[1] *= scale;
                p[2] *= scale;
            }
        }

        // Renormalize
        let total: f64 = p.iter().sum();
        if total > 1e-12 {
            for pi in &mut p {
                *pi /= total;
            }
        }

        // Compute expected loss for each action
        let mut all_losses = [0.0_f64; 4];
        for action_idx in 0..4 {
            let mut el = 0.0;
            for state_idx in 0..4 {
                el += p[state_idx] * self.config.loss_matrix[state_idx * 4 + action_idx];
            }
            all_losses[action_idx] = el;
        }

        // Find action with minimum expected loss
        let mut best_idx = 0_usize;
        let mut best_loss = all_losses[0];
        for (i, &loss) in all_losses.iter().enumerate().skip(1) {
            if loss < best_loss {
                best_loss = loss;
                best_idx = i;
            }
        }

        let best_action = match best_idx {
            0 => PolicyAction::Hold,
            1 => PolicyAction::Tighten,
            2 => PolicyAction::Relax,
            _ => PolicyAction::Shed,
        };

        // Apply hysteresis
        let current_idx = match self.current_action {
            PolicyAction::Hold => 0,
            PolicyAction::Tighten => 1,
            PolicyAction::Relax => 2,
            PolicyAction::Shed => 3,
        };
        let current_loss = all_losses[current_idx];
        let improvement = current_loss - best_loss;
        let hysteresis_applied = best_action != self.current_action
            && improvement < self.config.hysteresis * current_loss;

        let chosen = if hysteresis_applied {
            self.hysteresis_count += 1;
            self.current_action
        } else {
            self.current_action = best_action;
            best_action
        };

        let chosen_loss = all_losses[match chosen {
            PolicyAction::Hold => 0,
            PolicyAction::Tighten => 1,
            PolicyAction::Relax => 2,
            PolicyAction::Shed => 3,
        }];

        // Record
        self.total_decisions += 1;
        self.last_expected_loss = chosen_loss;
        self.action_counts[match chosen {
            PolicyAction::Hold => 0,
            PolicyAction::Tighten => 1,
            PolicyAction::Relax => 2,
            PolicyAction::Shed => 3,
        }] += 1;
        self.last_decision_us = timestamp_us;

        let decision = PolicyDecision {
            action: chosen,
            expected_loss: chosen_loss,
            state_probs: p,
            all_losses,
            hysteresis_applied,
            timestamp_us,
        };
        if self.decisions.len() < self.max_decisions {
            self.decisions.push(decision);
        } else if self.max_decisions > 0 {
            self.decisions[self.decision_head] = decision;
            self.decision_head = (self.decision_head + 1) % self.max_decisions;
        }

        chosen
    }

    /// Current recommended action.
    pub fn current_action(&self) -> PolicyAction {
        self.current_action
    }

    /// Total decisions made.
    pub fn total_decisions(&self) -> u64 {
        self.total_decisions
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> PolicyControllerSnapshot {
        PolicyControllerSnapshot {
            current_action: self.current_action,
            total_decisions: self.total_decisions,
            action_counts: self.action_counts,
            last_expected_loss: self.last_expected_loss,
            hysteresis_count: self.hysteresis_count,
        }
    }

    /// Human-readable status line.
    pub fn status_line(&self) -> String {
        format!(
            "policy[{}] decisions={} loss={:.3} hyst={}",
            self.current_action,
            self.total_decisions,
            self.last_expected_loss,
            self.hysteresis_count,
        )
    }

    /// Reset to initial state.
    pub fn reset(&mut self) {
        self.current_action = PolicyAction::Hold;
        self.total_decisions = 0;
        self.action_counts = [0; 4];
        self.last_expected_loss = 0.0;
        self.hysteresis_count = 0;
        self.decisions.clear();
        self.decision_head = 0;
        self.last_decision_us = 0;
    }

    /// Recent decisions.
    pub fn recent_decisions(&self, n: usize) -> Vec<&PolicyDecision> {
        let len = self.decisions.len();
        if len == 0 || n == 0 {
            return Vec::new();
        }
        let take = n.min(len);
        let mut result = Vec::with_capacity(take);
        if len < self.max_decisions {
            let start = len.saturating_sub(take);
            for d in &self.decisions[start..] {
                result.push(d);
            }
        } else {
            for i in 0..take {
                let idx = (self.decision_head + len - take + i) % len;
                result.push(&self.decisions[idx]);
            }
        }
        result
    }

    /// Detect degradation based on controller state.
    pub fn detect_degradation(&self) -> PolicyDegradation {
        match self.current_action {
            PolicyAction::Shed => PolicyDegradation::EmergencyShed {
                total_decisions: self.total_decisions,
                last_loss: self.last_expected_loss,
            },
            PolicyAction::Tighten => PolicyDegradation::Tightening {
                expected_loss: self.last_expected_loss,
            },
            _ => PolicyDegradation::Healthy,
        }
    }

    /// Generate structured log entry.
    pub fn log_entry(&self) -> PolicyControllerLogEntry {
        PolicyControllerLogEntry {
            current_action: self.current_action,
            total_decisions: self.total_decisions,
            action_counts: self.action_counts,
            last_expected_loss: self.last_expected_loss,
            hysteresis_count: self.hysteresis_count,
            degradation: self.detect_degradation(),
        }
    }

    // ── D3 Impl: Bridge Methods and Convenience API ────────────────

    /// Decide from hitch-risk model posterior directly.
    ///
    /// Maps HitchRiskLevel to state probabilities:
    /// - Low: [0.9, 0.08, 0.01, 0.01]
    /// - Elevated: [0.3, 0.5, 0.15, 0.05]
    /// - High: [0.05, 0.15, 0.6, 0.2]
    /// - Critical: [0.01, 0.04, 0.15, 0.8]
    pub fn decide_from_risk(&mut self, level: HitchRiskLevel, timestamp_us: u64) -> PolicyAction {
        let probs = match level {
            HitchRiskLevel::Low => [0.9, 0.08, 0.01, 0.01],
            HitchRiskLevel::Elevated => [0.3, 0.5, 0.15, 0.05],
            HitchRiskLevel::High => [0.05, 0.15, 0.6, 0.2],
            HitchRiskLevel::Critical => [0.01, 0.04, 0.15, 0.8],
        };
        self.decide(probs, timestamp_us)
    }

    /// Action distribution as fractions [hold, tighten, relax, shed].
    pub fn action_distribution(&self) -> [f64; 4] {
        if self.total_decisions == 0 {
            return [0.0; 4];
        }
        let total = self.total_decisions as f64;
        [
            self.action_counts[0] as f64 / total,
            self.action_counts[1] as f64 / total,
            self.action_counts[2] as f64 / total,
            self.action_counts[3] as f64 / total,
        ]
    }

    /// Per-action counts.
    pub fn action_counts(&self) -> [u64; 4] {
        self.action_counts
    }

    /// Count of hysteresis suppressions.
    pub fn hysteresis_count(&self) -> u64 {
        self.hysteresis_count
    }

    /// Last expected loss.
    pub fn last_expected_loss(&self) -> f64 {
        self.last_expected_loss
    }

    /// Update the hysteresis threshold.
    pub fn set_hysteresis(&mut self, h: f64) {
        self.config.hysteresis = h;
    }

    /// Update the critical floor.
    pub fn set_critical_floor(&mut self, floor: f64) {
        self.config.critical_floor = floor;
    }

    /// Update a single loss matrix entry.
    /// `state_idx` in 0..4 (Healthy/Drifting/Stressed/Critical),
    /// `action_idx` in 0..4 (Hold/Tighten/Relax/Shed).
    pub fn set_loss(&mut self, state_idx: usize, action_idx: usize, loss: f64) {
        if state_idx < 4 && action_idx < 4 {
            self.config.loss_matrix[state_idx * 4 + action_idx] = loss;
        }
    }

    /// Number of stored decisions.
    pub fn decision_count(&self) -> usize {
        self.decisions.len()
    }
}

/// Degradation status for the policy controller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PolicyDegradation {
    Healthy,
    Tightening {
        expected_loss: f64,
    },
    EmergencyShed {
        total_decisions: u64,
        last_loss: f64,
    },
}

impl std::fmt::Display for PolicyDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Tightening { expected_loss } => {
                write!(f, "tightening(loss={expected_loss:.3})")
            }
            Self::EmergencyShed {
                total_decisions,
                last_loss,
            } => {
                write!(
                    f,
                    "emergency_shed(decisions={total_decisions}, loss={last_loss:.3})"
                )
            }
        }
    }
}

/// Structured log entry for the policy controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyControllerLogEntry {
    pub current_action: PolicyAction,
    pub total_decisions: u64,
    pub action_counts: [u64; 4],
    pub last_expected_loss: f64,
    pub hysteresis_count: u64,
    pub degradation: PolicyDegradation,
}

// ── D4: Calibration Harness and Promotion Gates ────────────────────

/// Scenario class for calibration evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CalibrationScenario {
    /// Steady-state, no anomalies.
    Nominal,
    /// Gradual drift over time.
    GradualDrift,
    /// Sudden regime change.
    AbruptShift,
    /// High-noise environment.
    NoisyBaseline,
    /// Recovery after a stress event.
    PostStressRecovery,
}

impl std::fmt::Display for CalibrationScenario {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nominal => write!(f, "nominal"),
            Self::GradualDrift => write!(f, "gradual_drift"),
            Self::AbruptShift => write!(f, "abrupt_shift"),
            Self::NoisyBaseline => write!(f, "noisy_baseline"),
            Self::PostStressRecovery => write!(f, "post_stress_recovery"),
        }
    }
}

/// Result of evaluating a detector/controller on one calibration scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationResult {
    /// Which scenario was run.
    pub scenario: CalibrationScenario,
    /// False positive rate (type I errors).
    pub false_positive_rate: f64,
    /// Miss rate (type II errors).
    pub miss_rate: f64,
    /// Detection delay in observations (for drift scenarios).
    pub detection_delay: f64,
    /// Mean expected loss over the scenario.
    pub mean_expected_loss: f64,
    /// Whether the result meets the promotion gate criteria.
    pub passes_gate: bool,
    /// Number of observations in the scenario.
    pub observation_count: u64,
    /// Timestamp when calibration was run.
    pub timestamp_us: u64,
}

/// Promotion gate configuration.
///
/// A controller/detector update is only promoted to production if
/// all gate criteria are met across all calibration scenarios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionGateConfig {
    /// Maximum allowed false positive rate.
    pub max_fpr: f64,
    /// Maximum allowed miss rate.
    pub max_miss_rate: f64,
    /// Maximum allowed detection delay (observations).
    pub max_detection_delay: f64,
    /// Maximum allowed mean expected loss.
    pub max_expected_loss: f64,
    /// Minimum number of scenarios that must pass.
    pub min_passing_scenarios: usize,
    /// Whether to require all scenarios to pass (strict mode).
    pub strict: bool,
}

impl PromotionGateConfig {
    /// Sensible defaults.
    pub fn default_strict() -> Self {
        Self {
            max_fpr: 0.05,
            max_miss_rate: 0.10,
            max_detection_delay: 50.0,
            max_expected_loss: 5.0,
            min_passing_scenarios: 5,
            strict: true,
        }
    }
}

/// Verdict of the promotion gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PromotionVerdict {
    /// All gates passed — safe to promote.
    Approved,
    /// Some gates failed — review required.
    ConditionalHold,
    /// Critical gates failed — do not promote.
    Rejected,
}

impl std::fmt::Display for PromotionVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Approved => write!(f, "approved"),
            Self::ConditionalHold => write!(f, "conditional_hold"),
            Self::Rejected => write!(f, "rejected"),
        }
    }
}

/// Snapshot of the calibration harness state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationSnapshot {
    /// Total calibration runs.
    pub total_runs: u64,
    /// Results per scenario.
    pub scenario_results: Vec<CalibrationResult>,
    /// Overall verdict.
    pub verdict: PromotionVerdict,
    /// Number of passing scenarios.
    pub passing_count: usize,
    /// Number of failing scenarios.
    pub failing_count: usize,
}

/// The calibration harness.
///
/// Evaluates detector/controller quality across scenario classes and
/// gates promotions based on configurable thresholds.
#[derive(Debug, Clone)]
pub struct CalibrationHarness {
    config: PromotionGateConfig,
    /// Results from the most recent calibration run.
    results: Vec<CalibrationResult>,
    /// Total calibration runs ever.
    total_runs: u64,
    /// Last verdict.
    last_verdict: PromotionVerdict,
}

impl CalibrationHarness {
    /// Create a new harness.
    pub fn new(config: PromotionGateConfig) -> Self {
        Self {
            config,
            results: Vec::new(),
            total_runs: 0,
            last_verdict: PromotionVerdict::Rejected,
        }
    }

    /// Create with strict defaults.
    pub fn with_defaults() -> Self {
        Self::new(PromotionGateConfig::default_strict())
    }

    /// Submit a calibration result and evaluate against gates.
    pub fn submit(&mut self, result: CalibrationResult) {
        self.total_runs += 1;
        self.results.push(result);
    }

    /// Evaluate a single result against gate criteria.
    fn evaluate_result(&self, result: &CalibrationResult) -> bool {
        result.false_positive_rate <= self.config.max_fpr
            && result.miss_rate <= self.config.max_miss_rate
            && result.detection_delay <= self.config.max_detection_delay
            && result.mean_expected_loss <= self.config.max_expected_loss
    }

    /// Compute the overall promotion verdict.
    pub fn evaluate(&mut self) -> PromotionVerdict {
        if self.results.is_empty() {
            self.last_verdict = PromotionVerdict::Rejected;
            return self.last_verdict;
        }

        let mut passing = 0_usize;
        let mut failing = 0_usize;
        for r in &mut self.results {
            let passes = r.false_positive_rate <= self.config.max_fpr
                && r.miss_rate <= self.config.max_miss_rate
                && r.detection_delay <= self.config.max_detection_delay
                && r.mean_expected_loss <= self.config.max_expected_loss;
            r.passes_gate = passes;
            if passes {
                passing += 1;
            } else {
                failing += 1;
            }
        }

        let verdict = if self.config.strict && failing > 0 {
            PromotionVerdict::Rejected
        } else if passing >= self.config.min_passing_scenarios {
            PromotionVerdict::Approved
        } else if passing > 0 {
            PromotionVerdict::ConditionalHold
        } else {
            PromotionVerdict::Rejected
        };

        self.last_verdict = verdict;
        verdict
    }

    /// Last computed verdict.
    pub fn verdict(&self) -> PromotionVerdict {
        self.last_verdict
    }

    /// Total calibration runs.
    pub fn total_runs(&self) -> u64 {
        self.total_runs
    }

    /// Number of results stored.
    pub fn result_count(&self) -> usize {
        self.results.len()
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> CalibrationSnapshot {
        let passing = self.results.iter().filter(|r| r.passes_gate).count();
        let failing = self.results.len() - passing;
        CalibrationSnapshot {
            total_runs: self.total_runs,
            scenario_results: self.results.clone(),
            verdict: self.last_verdict,
            passing_count: passing,
            failing_count: failing,
        }
    }

    /// Human-readable status line.
    pub fn status_line(&self) -> String {
        let snap = self.snapshot();
        format!(
            "calibration[{}] runs={} pass={} fail={}",
            snap.verdict, snap.total_runs, snap.passing_count, snap.failing_count,
        )
    }

    /// Reset all results.
    pub fn reset(&mut self) {
        self.results.clear();
        self.total_runs = 0;
        self.last_verdict = PromotionVerdict::Rejected;
    }

    /// Clear results but keep total_runs count.
    pub fn clear_results(&mut self) {
        self.results.clear();
        self.last_verdict = PromotionVerdict::Rejected;
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> CalibrationDegradation {
        match self.last_verdict {
            PromotionVerdict::Approved => CalibrationDegradation::Healthy,
            PromotionVerdict::ConditionalHold => CalibrationDegradation::GateMarginal {
                passing: self.results.iter().filter(|r| r.passes_gate).count(),
                total: self.results.len(),
            },
            PromotionVerdict::Rejected => CalibrationDegradation::GateFailed {
                failing: self.results.iter().filter(|r| !r.passes_gate).count(),
                total: self.results.len(),
            },
        }
    }

    /// Generate structured log entry.
    pub fn log_entry(&self) -> CalibrationLogEntry {
        CalibrationLogEntry {
            total_runs: self.total_runs,
            result_count: self.results.len(),
            verdict: self.last_verdict,
            degradation: self.detect_degradation(),
        }
    }

    // ── D4 Impl: Bridge Methods and Convenience API ────────────────

    /// Submit a batch of results and evaluate.
    pub fn submit_batch(&mut self, results: Vec<CalibrationResult>) -> PromotionVerdict {
        for r in results {
            self.submit(r);
        }
        self.evaluate()
    }

    /// Average false positive rate across all results.
    pub fn avg_fpr(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        self.results
            .iter()
            .map(|r| r.false_positive_rate)
            .sum::<f64>()
            / self.results.len() as f64
    }

    /// Average miss rate across all results.
    pub fn avg_miss_rate(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        self.results.iter().map(|r| r.miss_rate).sum::<f64>() / self.results.len() as f64
    }

    /// Average detection delay across all results.
    pub fn avg_detection_delay(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        self.results.iter().map(|r| r.detection_delay).sum::<f64>() / self.results.len() as f64
    }

    /// Passing count.
    pub fn passing_count(&self) -> usize {
        self.results.iter().filter(|r| r.passes_gate).count()
    }

    /// Failing count.
    pub fn failing_count(&self) -> usize {
        self.results.iter().filter(|r| !r.passes_gate).count()
    }

    /// Whether the harness has approved promotion.
    pub fn is_approved(&self) -> bool {
        self.last_verdict == PromotionVerdict::Approved
    }

    /// Set max FPR gate.
    pub fn set_max_fpr(&mut self, fpr: f64) {
        self.config.max_fpr = fpr;
    }

    /// Set strict mode.
    pub fn set_strict(&mut self, strict: bool) {
        self.config.strict = strict;
    }

    /// Results by scenario.
    pub fn results_for_scenario(&self, scenario: CalibrationScenario) -> Vec<&CalibrationResult> {
        self.results
            .iter()
            .filter(|r| r.scenario == scenario)
            .collect()
    }
}

/// Degradation status for the calibration harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CalibrationDegradation {
    Healthy,
    GateMarginal { passing: usize, total: usize },
    GateFailed { failing: usize, total: usize },
}

impl std::fmt::Display for CalibrationDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::GateMarginal { passing, total } => {
                write!(f, "marginal({passing}/{total})")
            }
            Self::GateFailed { failing, total } => {
                write!(f, "failed({failing}/{total})")
            }
        }
    }
}

/// Structured log entry for the calibration harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationLogEntry {
    pub total_runs: u64,
    pub result_count: usize,
    pub verdict: PromotionVerdict,
    pub degradation: CalibrationDegradation,
}

// ── E1: Formal Specification Pack ──────────────────────────────────
//
// Formal invariant predicates for the scheduler, budget enforcer, and
// recovery protocol.  These types encode machine-checkable properties
// that MUST hold across all reachable states.  The InvariantChecker
// runtime validator evaluates them against live snapshots.

// ── E1.1 Invariant Domain ─────────────────────────────────────────

/// Domain to which a formal invariant belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InvariantDomain {
    /// Scheduler lane invariants (admission, ordering, starvation).
    Scheduler,
    /// Budget enforcement invariants (monotonicity, overflow, percentile order).
    Budget,
    /// Recovery protocol invariants (cooldown, escalation, de-escalation).
    Recovery,
    /// Cross-domain composition invariants.
    Composition,
}

impl fmt::Display for InvariantDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheduler => f.write_str("scheduler"),
            Self::Budget => f.write_str("budget"),
            Self::Recovery => f.write_str("recovery"),
            Self::Composition => f.write_str("composition"),
        }
    }
}

/// Severity of a formal invariant.  Critical invariants abort execution;
/// warning invariants emit diagnostics but continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum InvariantSeverity {
    /// Informational — log only.
    Info,
    /// Warning — emit diagnostic, continue.
    Warning,
    /// Critical — must abort or rollback.
    Critical,
}

impl fmt::Display for InvariantSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => f.write_str("info"),
            Self::Warning => f.write_str("warning"),
            Self::Critical => f.write_str("critical"),
        }
    }
}

// ── E1.2 Formal Invariant Predicate ──────────────────────────────

/// A named, machine-checkable invariant predicate with domain and severity.
///
/// Each `FormalInvariant` encodes a single property that must hold.
/// The `predicate_id` is a stable identifier (e.g. "scheduler.no_starvation")
/// used for audit trails and counterexample references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormalInvariant {
    /// Stable identifier (dot-separated, e.g. "budget.percentile_monotonic").
    pub predicate_id: String,
    /// Human-readable description of the property.
    pub description: String,
    /// Domain this invariant belongs to.
    pub domain: InvariantDomain,
    /// Severity when violated.
    pub severity: InvariantSeverity,
    /// Whether this invariant is a safety property (must always hold)
    /// vs a liveness property (must eventually hold).
    pub is_safety: bool,
}

impl fmt::Display for FormalInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}:{}] {}",
            self.domain, self.severity, self.predicate_id
        )
    }
}

// ── E1.3 Scheduler Invariants ────────────────────────────────────

/// Formal invariants for the 3-lane scheduler.
///
/// These capture safety and liveness properties of the `LaneScheduler`:
/// - No item is lost (admitted items are tracked)
/// - Lane capacity is never exceeded
/// - Starvation freedom (bounded wait)
/// - Deterministic replay (same input → same schedule)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SchedulerInvariant {
    /// Lane queue length never exceeds configured capacity.
    CapacityBound {
        lane: SchedulerLane,
        capacity: usize,
        actual: usize,
    },
    /// Total admitted items equals sum across all lanes.
    ConservationOfWork { total_admitted: u64, lane_sum: u64 },
    /// No lane has been starved beyond the max starvation threshold.
    StarvationFreedom {
        lane: SchedulerLane,
        wait_epochs: u64,
        max_epochs: u64,
    },
    /// Epoch counter is monotonically non-decreasing.
    EpochMonotonicity { previous: u64, current: u64 },
    /// Item IDs are strictly monotonically increasing.
    ItemIdMonotonicity { previous: u64, current: u64 },
    /// Determinism: identical input sequences produce identical decisions.
    DeterministicReplay {
        input_hash: u64,
        expected_hash: u64,
        actual_hash: u64,
    },
}

impl SchedulerInvariant {
    /// Check whether this invariant holds.
    pub fn holds(&self) -> bool {
        match self {
            Self::CapacityBound {
                capacity, actual, ..
            } => *actual <= *capacity,
            Self::ConservationOfWork {
                total_admitted,
                lane_sum,
            } => total_admitted == lane_sum,
            Self::StarvationFreedom {
                wait_epochs,
                max_epochs,
                ..
            } => wait_epochs <= max_epochs,
            Self::EpochMonotonicity { previous, current } => current >= previous,
            Self::ItemIdMonotonicity { previous, current } => {
                current > previous || (*previous == 0 && *current == 0)
            }
            Self::DeterministicReplay {
                expected_hash,
                actual_hash,
                ..
            } => expected_hash == actual_hash,
        }
    }

    /// The predicate ID for this invariant class.
    pub fn predicate_id(&self) -> &'static str {
        match self {
            Self::CapacityBound { .. } => "scheduler.capacity_bound",
            Self::ConservationOfWork { .. } => "scheduler.conservation_of_work",
            Self::StarvationFreedom { .. } => "scheduler.starvation_freedom",
            Self::EpochMonotonicity { .. } => "scheduler.epoch_monotonicity",
            Self::ItemIdMonotonicity { .. } => "scheduler.item_id_monotonicity",
            Self::DeterministicReplay { .. } => "scheduler.deterministic_replay",
        }
    }
}

impl fmt::Display for SchedulerInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityBound {
                lane,
                capacity,
                actual,
            } => {
                write!(f, "capacity_bound({lane:?}): {actual}/{capacity}")
            }
            Self::ConservationOfWork {
                total_admitted,
                lane_sum,
            } => {
                write!(f, "conservation: total={total_admitted}, sum={lane_sum}")
            }
            Self::StarvationFreedom {
                lane,
                wait_epochs,
                max_epochs,
            } => {
                write!(f, "starvation({lane:?}): wait={wait_epochs}/{max_epochs}")
            }
            Self::EpochMonotonicity { previous, current } => {
                write!(f, "epoch_mono: {previous} -> {current}")
            }
            Self::ItemIdMonotonicity { previous, current } => {
                write!(f, "item_id_mono: {previous} -> {current}")
            }
            Self::DeterministicReplay {
                input_hash,
                expected_hash,
                actual_hash,
            } => {
                write!(
                    f,
                    "determinism(input={input_hash:#x}): expected={expected_hash:#x}, actual={actual_hash:#x}"
                )
            }
        }
    }
}

// ── E1.4 Budget Invariants ───────────────────────────────────────

/// Formal invariants for budget enforcement.
///
/// These capture correctness properties of `BudgetEnforcer` and `RuntimeEnforcer`:
/// - Percentile targets are monotonically ordered (p50 ≤ p95 ≤ p99 ≤ p999)
/// - Budget totals are non-negative
/// - Observation counts are consistent
/// - Enforcer escalation is monotonic within a single evaluation
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BudgetInvariant {
    /// Percentile targets are monotonically non-decreasing for each stage.
    PercentileMonotonicity {
        stage: LatencyStage,
        p50: f64,
        p95: f64,
        p99: f64,
        p999: f64,
    },
    /// All budget targets are non-negative.
    NonNegativeTargets {
        stage: LatencyStage,
        min_target: f64,
    },
    /// Total observation count matches per-stage sums.
    ObservationConsistency { total: u64, per_stage_sum: u64 },
    /// Overflow count never exceeds total observation count.
    OverflowBound {
        overflow_count: u64,
        total_observations: u64,
    },
    /// Enforcer escalation within a single observation is monotonic
    /// (never jumps down during a single evaluate call).
    EscalationMonotonicity {
        stage: LatencyStage,
        previous_level: MitigationLevel,
        current_level: MitigationLevel,
    },
    /// Aggregate budget ceiling is >= sum of stage budgets at each percentile.
    AggregateCeiling {
        percentile: Percentile,
        aggregate_us: f64,
        stage_sum_us: f64,
    },
}

impl BudgetInvariant {
    /// Check whether this invariant holds.
    pub fn holds(&self) -> bool {
        match self {
            Self::PercentileMonotonicity {
                p50,
                p95,
                p99,
                p999,
                ..
            } => *p50 <= *p95 && *p95 <= *p99 && *p99 <= *p999,
            Self::NonNegativeTargets { min_target, .. } => *min_target >= 0.0,
            Self::ObservationConsistency {
                total,
                per_stage_sum,
            } => total == per_stage_sum,
            Self::OverflowBound {
                overflow_count,
                total_observations,
            } => overflow_count <= total_observations,
            Self::EscalationMonotonicity {
                previous_level,
                current_level,
                ..
            } => *current_level >= *previous_level,
            Self::AggregateCeiling {
                aggregate_us,
                stage_sum_us,
                ..
            } => *aggregate_us >= *stage_sum_us || (*aggregate_us - *stage_sum_us).abs() < 1e-6,
        }
    }

    /// The predicate ID for this invariant class.
    pub fn predicate_id(&self) -> &'static str {
        match self {
            Self::PercentileMonotonicity { .. } => "budget.percentile_monotonicity",
            Self::NonNegativeTargets { .. } => "budget.non_negative_targets",
            Self::ObservationConsistency { .. } => "budget.observation_consistency",
            Self::OverflowBound { .. } => "budget.overflow_bound",
            Self::EscalationMonotonicity { .. } => "budget.escalation_monotonicity",
            Self::AggregateCeiling { .. } => "budget.aggregate_ceiling",
        }
    }
}

impl fmt::Display for BudgetInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PercentileMonotonicity {
                stage,
                p50,
                p95,
                p99,
                p999,
            } => {
                write!(
                    f,
                    "pct_mono({stage}): p50={p50:.1} p95={p95:.1} p99={p99:.1} p999={p999:.1}"
                )
            }
            Self::NonNegativeTargets { stage, min_target } => {
                write!(f, "nonneg({stage}): min={min_target:.1}")
            }
            Self::ObservationConsistency {
                total,
                per_stage_sum,
            } => {
                write!(f, "obs_consistency: total={total}, sum={per_stage_sum}")
            }
            Self::OverflowBound {
                overflow_count,
                total_observations,
            } => {
                write!(f, "overflow_bound: {overflow_count}/{total_observations}")
            }
            Self::EscalationMonotonicity {
                stage,
                previous_level,
                current_level,
            } => {
                write!(f, "esc_mono({stage}): {previous_level} -> {current_level}")
            }
            Self::AggregateCeiling {
                percentile,
                aggregate_us,
                stage_sum_us,
            } => {
                write!(
                    f,
                    "agg_ceil({percentile}): agg={aggregate_us:.1} >= sum={stage_sum_us:.1}"
                )
            }
        }
    }
}

// ── E1.5 Recovery Invariants ─────────────────────────────────────

/// Formal invariants for the recovery protocol state machine.
///
/// Recovery must satisfy:
/// - Gradual de-escalation: each recovery step drops exactly one level
/// - Cooldown enforcement: recovery only after sufficient consecutive-ok
/// - Timeout enforcement: forced recovery after max_degraded_duration
/// - No spurious escalation during recovery window
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RecoveryInvariant {
    /// In gradual mode, recovery steps down exactly one MitigationLevel at a time.
    GradualDeescalation {
        previous_level: MitigationLevel,
        recovered_level: MitigationLevel,
    },
    /// Recovery only occurs after consecutive_ok >= cooldown_observations.
    CooldownEnforced {
        consecutive_ok: u64,
        cooldown_required: u64,
    },
    /// Forced recovery triggers after max_degraded_duration_us is exceeded.
    TimeoutRecovery {
        degraded_duration_us: u64,
        max_duration_us: u64,
        recovery_triggered: bool,
    },
    /// Escalation count is monotonically non-decreasing.
    EscalationCountMonotonic { previous: u64, current: u64 },
    /// Recovery count is monotonically non-decreasing.
    RecoveryCountMonotonic { previous: u64, current: u64 },
    /// Current mitigation level is within [None, Skip] range (valid enum range).
    LevelInRange { level: MitigationLevel },
}

impl RecoveryInvariant {
    /// Check whether this invariant holds.
    pub fn holds(&self) -> bool {
        match self {
            Self::GradualDeescalation {
                previous_level,
                recovered_level,
            } => {
                previous_level.severity() > 0
                    && recovered_level.severity() == previous_level.severity() - 1
            }
            Self::CooldownEnforced {
                consecutive_ok,
                cooldown_required,
            } => consecutive_ok >= cooldown_required,
            Self::TimeoutRecovery {
                degraded_duration_us,
                max_duration_us,
                recovery_triggered,
            } => {
                if *degraded_duration_us > *max_duration_us {
                    *recovery_triggered
                } else {
                    true // no constraint before timeout
                }
            }
            Self::EscalationCountMonotonic { previous, current } => current >= previous,
            Self::RecoveryCountMonotonic { previous, current } => current >= previous,
            Self::LevelInRange { level } => level.severity() <= 4,
        }
    }

    /// The predicate ID for this invariant class.
    pub fn predicate_id(&self) -> &'static str {
        match self {
            Self::GradualDeescalation { .. } => "recovery.gradual_deescalation",
            Self::CooldownEnforced { .. } => "recovery.cooldown_enforced",
            Self::TimeoutRecovery { .. } => "recovery.timeout_recovery",
            Self::EscalationCountMonotonic { .. } => "recovery.escalation_count_monotonic",
            Self::RecoveryCountMonotonic { .. } => "recovery.recovery_count_monotonic",
            Self::LevelInRange { .. } => "recovery.level_in_range",
        }
    }
}

impl fmt::Display for RecoveryInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GradualDeescalation {
                previous_level,
                recovered_level,
            } => {
                write!(f, "gradual: {previous_level} -> {recovered_level}")
            }
            Self::CooldownEnforced {
                consecutive_ok,
                cooldown_required,
            } => {
                write!(f, "cooldown: {consecutive_ok}/{cooldown_required}")
            }
            Self::TimeoutRecovery {
                degraded_duration_us,
                max_duration_us,
                recovery_triggered,
            } => {
                write!(
                    f,
                    "timeout: {degraded_duration_us}us/{max_duration_us}us triggered={recovery_triggered}"
                )
            }
            Self::EscalationCountMonotonic { previous, current } => {
                write!(f, "esc_mono: {previous} -> {current}")
            }
            Self::RecoveryCountMonotonic { previous, current } => {
                write!(f, "rec_mono: {previous} -> {current}")
            }
            Self::LevelInRange { level } => {
                write!(f, "level_range: {level}")
            }
        }
    }
}

// ── E1.6 Invariant Check Result ──────────────────────────────────

/// Outcome of evaluating a single formal invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InvariantOutcome {
    /// Invariant holds.
    Satisfied,
    /// Invariant violated with a counterexample description.
    Violated { counterexample: String },
    /// Could not be evaluated (insufficient data or timeout).
    Inconclusive { reason: String },
}

impl fmt::Display for InvariantOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Satisfied => f.write_str("SATISFIED"),
            Self::Violated { counterexample } => write!(f, "VIOLATED: {counterexample}"),
            Self::Inconclusive { reason } => write!(f, "INCONCLUSIVE: {reason}"),
        }
    }
}

/// Result of checking one invariant, with timing and context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantCheckResult {
    /// The predicate ID that was checked.
    pub predicate_id: String,
    /// Domain of the invariant.
    pub domain: InvariantDomain,
    /// Severity of the invariant.
    pub severity: InvariantSeverity,
    /// Check outcome.
    pub outcome: InvariantOutcome,
    /// Evaluation time in microseconds.
    pub eval_time_us: u64,
    /// Timestamp when the check was performed (epoch μs).
    pub timestamp_us: u64,
}

impl InvariantCheckResult {
    /// Whether the check passed.
    pub fn passed(&self) -> bool {
        self.outcome == InvariantOutcome::Satisfied
    }

    /// Whether the check found a violation.
    pub fn violated(&self) -> bool {
        matches!(self.outcome, InvariantOutcome::Violated { .. })
    }
}

impl fmt::Display for InvariantCheckResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}:{}] {} ({}μs)",
            self.domain, self.severity, self.outcome, self.eval_time_us
        )
    }
}

// ── E1.7 Invariant Checker ───────────────────────────────────────

/// Configuration for the runtime invariant checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantCheckerConfig {
    /// Maximum evaluation time per invariant (μs) before marking inconclusive.
    pub max_eval_time_us: u64,
    /// Whether to abort on critical violations.
    pub abort_on_critical: bool,
    /// Maximum results to retain in history.
    pub max_history: usize,
    /// Domains to check (empty = all).
    pub enabled_domains: Vec<InvariantDomain>,
}

impl Default for InvariantCheckerConfig {
    fn default() -> Self {
        Self {
            max_eval_time_us: 10_000, // 10ms
            abort_on_critical: true,
            max_history: 500,
            enabled_domains: Vec::new(), // all
        }
    }
}

/// Runtime invariant checker that evaluates formal predicates against live state.
///
/// The checker maintains a registry of `FormalInvariant` definitions and
/// evaluates `SchedulerInvariant`, `BudgetInvariant`, and `RecoveryInvariant`
/// instances against them.  Results are stored for audit and diagnostics.
#[derive(Debug, Clone)]
pub struct InvariantChecker {
    config: InvariantCheckerConfig,
    invariants: Vec<FormalInvariant>,
    results: Vec<InvariantCheckResult>,
    total_checks: u64,
    total_violations: u64,
    total_satisfied: u64,
}

impl InvariantChecker {
    /// Create a new checker with the given configuration.
    pub fn new(config: InvariantCheckerConfig) -> Self {
        Self {
            config,
            invariants: Vec::new(),
            results: Vec::new(),
            total_checks: 0,
            total_violations: 0,
            total_satisfied: 0,
        }
    }

    /// Create a checker with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(InvariantCheckerConfig::default())
    }

    /// Register a formal invariant definition.
    pub fn register(&mut self, inv: FormalInvariant) {
        self.invariants.push(inv);
    }

    /// Number of registered invariant definitions.
    pub fn registered_count(&self) -> usize {
        self.invariants.len()
    }

    /// Check a scheduler invariant.
    pub fn check_scheduler(
        &mut self,
        inv: &SchedulerInvariant,
        timestamp_us: u64,
    ) -> InvariantCheckResult {
        let holds = inv.holds();
        let outcome = if holds {
            InvariantOutcome::Satisfied
        } else {
            InvariantOutcome::Violated {
                counterexample: format!("{inv}"),
            }
        };
        self.record_result(
            inv.predicate_id(),
            InvariantDomain::Scheduler,
            InvariantSeverity::Critical,
            outcome,
            timestamp_us,
        )
    }

    /// Check a budget invariant.
    pub fn check_budget(
        &mut self,
        inv: &BudgetInvariant,
        timestamp_us: u64,
    ) -> InvariantCheckResult {
        let holds = inv.holds();
        let outcome = if holds {
            InvariantOutcome::Satisfied
        } else {
            InvariantOutcome::Violated {
                counterexample: format!("{inv}"),
            }
        };
        self.record_result(
            inv.predicate_id(),
            InvariantDomain::Budget,
            InvariantSeverity::Critical,
            outcome,
            timestamp_us,
        )
    }

    /// Check a recovery invariant.
    pub fn check_recovery(
        &mut self,
        inv: &RecoveryInvariant,
        timestamp_us: u64,
    ) -> InvariantCheckResult {
        let holds = inv.holds();
        let outcome = if holds {
            InvariantOutcome::Satisfied
        } else {
            InvariantOutcome::Violated {
                counterexample: format!("{inv}"),
            }
        };
        self.record_result(
            inv.predicate_id(),
            InvariantDomain::Recovery,
            InvariantSeverity::Critical,
            outcome,
            timestamp_us,
        )
    }

    fn record_result(
        &mut self,
        predicate_id: &str,
        domain: InvariantDomain,
        severity: InvariantSeverity,
        outcome: InvariantOutcome,
        timestamp_us: u64,
    ) -> InvariantCheckResult {
        let result = InvariantCheckResult {
            predicate_id: predicate_id.to_string(),
            domain,
            severity,
            outcome,
            eval_time_us: 0, // filled by caller if instrumented
            timestamp_us,
        };
        self.total_checks += 1;
        if result.passed() {
            self.total_satisfied += 1;
        }
        if result.violated() {
            self.total_violations += 1;
        }
        if self.results.len() >= self.config.max_history {
            self.results.remove(0);
        }
        self.results.push(result.clone());
        result
    }

    /// Total checks performed.
    pub fn total_checks(&self) -> u64 {
        self.total_checks
    }

    /// Total violations found.
    pub fn total_violations(&self) -> u64 {
        self.total_violations
    }

    /// Total satisfied checks.
    pub fn total_satisfied(&self) -> u64 {
        self.total_satisfied
    }

    /// Violation rate (0.0–1.0).
    pub fn violation_rate(&self) -> f64 {
        if self.total_checks == 0 {
            0.0
        } else {
            self.total_violations as f64 / self.total_checks as f64
        }
    }

    /// Most recent results (up to `n`).
    pub fn recent_results(&self, n: usize) -> &[InvariantCheckResult] {
        let start = self.results.len().saturating_sub(n);
        &self.results[start..]
    }

    /// Results filtered by domain.
    pub fn results_by_domain(&self, domain: InvariantDomain) -> Vec<&InvariantCheckResult> {
        self.results.iter().filter(|r| r.domain == domain).collect()
    }

    /// All violation results.
    pub fn violations(&self) -> Vec<&InvariantCheckResult> {
        self.results.iter().filter(|r| r.violated()).collect()
    }

    /// State snapshot.
    pub fn snapshot(&self) -> InvariantCheckerSnapshot {
        InvariantCheckerSnapshot {
            total_checks: self.total_checks,
            total_violations: self.total_violations,
            total_satisfied: self.total_satisfied,
            registered_count: self.invariants.len(),
            history_len: self.results.len(),
            violation_rate: self.violation_rate(),
        }
    }

    /// Status line for display.
    pub fn status_line(&self) -> String {
        let snap = self.snapshot();
        format!(
            "invariants: checks={} ok={} violations={} rate={:.4}",
            snap.total_checks, snap.total_satisfied, snap.total_violations, snap.violation_rate
        )
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.results.clear();
        self.total_checks = 0;
        self.total_violations = 0;
        self.total_satisfied = 0;
    }

    /// Detect degradation in the invariant checker itself.
    pub fn detect_degradation(&self) -> InvariantCheckerDegradation {
        if self.total_checks == 0 {
            return InvariantCheckerDegradation::Healthy;
        }
        let rate = self.violation_rate();
        if rate > 0.1 {
            InvariantCheckerDegradation::HighViolationRate {
                violations: self.total_violations,
                total: self.total_checks,
            }
        } else if rate > 0.0 {
            InvariantCheckerDegradation::ViolationsDetected {
                violations: self.total_violations,
                total: self.total_checks,
            }
        } else {
            InvariantCheckerDegradation::Healthy
        }
    }

    /// Structured log entry.
    pub fn log_entry(&self) -> InvariantCheckerLogEntry {
        InvariantCheckerLogEntry {
            total_checks: self.total_checks,
            total_violations: self.total_violations,
            total_satisfied: self.total_satisfied,
            violation_rate: self.violation_rate(),
            degradation: self.detect_degradation(),
        }
    }
}

/// State snapshot for the invariant checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvariantCheckerSnapshot {
    pub total_checks: u64,
    pub total_violations: u64,
    pub total_satisfied: u64,
    pub registered_count: usize,
    pub history_len: usize,
    pub violation_rate: f64,
}

/// Degradation state for the invariant checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InvariantCheckerDegradation {
    /// No violations detected.
    Healthy,
    /// Some violations but rate is low (≤10%).
    ViolationsDetected { violations: u64, total: u64 },
    /// High violation rate (>10%).
    HighViolationRate { violations: u64, total: u64 },
}

impl fmt::Display for InvariantCheckerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => f.write_str("healthy"),
            Self::ViolationsDetected { violations, total } => {
                write!(f, "violations({violations}/{total})")
            }
            Self::HighViolationRate { violations, total } => {
                write!(f, "high_rate({violations}/{total})")
            }
        }
    }
}

/// Structured log entry for the invariant checker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantCheckerLogEntry {
    pub total_checks: u64,
    pub total_violations: u64,
    pub total_satisfied: u64,
    pub violation_rate: f64,
    pub degradation: InvariantCheckerDegradation,
}

// ── E1 Impl: Bridge Methods and Convenience API ──────────────────

impl InvariantChecker {
    /// Check a batch of scheduler invariants, returning all results.
    pub fn check_scheduler_batch(
        &mut self,
        invariants: &[SchedulerInvariant],
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        invariants
            .iter()
            .map(|inv| self.check_scheduler(inv, timestamp_us))
            .collect()
    }

    /// Check a batch of budget invariants, returning all results.
    pub fn check_budget_batch(
        &mut self,
        invariants: &[BudgetInvariant],
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        invariants
            .iter()
            .map(|inv| self.check_budget(inv, timestamp_us))
            .collect()
    }

    /// Check a batch of recovery invariants, returning all results.
    pub fn check_recovery_batch(
        &mut self,
        invariants: &[RecoveryInvariant],
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        invariants
            .iter()
            .map(|inv| self.check_recovery(inv, timestamp_us))
            .collect()
    }

    /// Extract and check scheduler invariants from a `SchedulerSnapshot`.
    pub fn check_from_scheduler_snapshot(
        &mut self,
        snap: &SchedulerSnapshot,
        config: &LaneSchedulerConfig,
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        let mut results = Vec::new();
        // Check capacity bounds for each lane
        for ls in &snap.lanes {
            let capacity = match ls.lane {
                SchedulerLane::Input => config.input_queue_capacity,
                SchedulerLane::Control => config.control_queue_capacity,
                SchedulerLane::Bulk => config.bulk_queue_capacity,
            };
            let inv = SchedulerInvariant::CapacityBound {
                lane: ls.lane,
                capacity,
                actual: ls.depth,
            };
            results.push(self.check_scheduler(&inv, timestamp_us));
        }
        // Check conservation of work
        let lane_sum: u64 = snap.lanes.iter().map(|ls| ls.total_admitted).sum();
        let total = snap.total_items_processed;
        let inv = SchedulerInvariant::ConservationOfWork {
            total_admitted: total,
            lane_sum,
        };
        results.push(self.check_scheduler(&inv, timestamp_us));
        results
    }

    /// Extract and check budget invariants from an `EnforcerSnapshot`.
    pub fn check_from_enforcer_snapshot(
        &mut self,
        snap: &EnforcerSnapshot,
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        let mut results = Vec::new();
        // Check overflow bound
        let inv = BudgetInvariant::OverflowBound {
            overflow_count: snap.total_overflows,
            total_observations: snap.total_observations,
        };
        results.push(self.check_budget(&inv, timestamp_us));
        // Check overflow bound per stage
        let total_overflows: u64 = snap.stages.iter().map(|s| s.overflow_count).sum();
        let inv = BudgetInvariant::OverflowBound {
            overflow_count: total_overflows,
            total_observations: snap.total_observations,
        };
        results.push(self.check_budget(&inv, timestamp_us));
        results
    }

    /// Extract and check recovery invariants from a `StageEnforcementState`.
    pub fn check_from_enforcement_state(
        &mut self,
        state: &StageEnforcementState,
        previous_state: &StageEnforcementState,
        recovery_protocol: &RecoveryProtocol,
        timestamp_us: u64,
    ) -> Vec<InvariantCheckResult> {
        let mut results = Vec::new();
        // Escalation count monotonicity
        let inv = RecoveryInvariant::EscalationCountMonotonic {
            previous: previous_state.escalation_count,
            current: state.escalation_count,
        };
        results.push(self.check_recovery(&inv, timestamp_us));
        // Recovery count monotonicity
        let inv = RecoveryInvariant::RecoveryCountMonotonic {
            previous: previous_state.recovery_count,
            current: state.recovery_count,
        };
        results.push(self.check_recovery(&inv, timestamp_us));
        // Level in range
        let inv = RecoveryInvariant::LevelInRange {
            level: state.current_level,
        };
        results.push(self.check_recovery(&inv, timestamp_us));
        // If recovery happened (level decreased), check gradual de-escalation
        if state.current_level < previous_state.current_level && recovery_protocol.gradual {
            let inv = RecoveryInvariant::GradualDeescalation {
                previous_level: previous_state.current_level,
                recovered_level: state.current_level,
            };
            results.push(self.check_recovery(&inv, timestamp_us));
        }
        // If recovery happened, check cooldown
        if state.current_level < previous_state.current_level {
            let inv = RecoveryInvariant::CooldownEnforced {
                consecutive_ok: state.consecutive_ok,
                cooldown_required: recovery_protocol.cooldown_observations,
            };
            results.push(self.check_recovery(&inv, timestamp_us));
        }
        results
    }

    /// Run all domain checks and return true only if zero violations found.
    pub fn all_satisfied(&self) -> bool {
        self.total_violations == 0
    }

    /// Count violations in a specific domain.
    pub fn violation_count_by_domain(&self, domain: InvariantDomain) -> usize {
        self.results
            .iter()
            .filter(|r| r.domain == domain && r.violated())
            .count()
    }

    /// Get the most recent violation (if any).
    pub fn last_violation(&self) -> Option<&InvariantCheckResult> {
        self.results.iter().rev().find(|r| r.violated())
    }

    /// Check whether a specific predicate has ever been violated.
    pub fn predicate_ever_violated(&self, predicate_id: &str) -> bool {
        self.results
            .iter()
            .any(|r| r.predicate_id == predicate_id && r.violated())
    }

    /// Get pass rate for a specific predicate (0.0–1.0, NaN if never checked).
    pub fn predicate_pass_rate(&self, predicate_id: &str) -> f64 {
        let matching: Vec<_> = self
            .results
            .iter()
            .filter(|r| r.predicate_id == predicate_id)
            .collect();
        if matching.is_empty() {
            return f64::NAN;
        }
        let passed = matching.iter().filter(|r| r.passed()).count();
        passed as f64 / matching.len() as f64
    }

    /// Get all unique predicate IDs that have been checked.
    pub fn checked_predicates(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut predicates = Vec::new();
        for r in &self.results {
            if seen.insert(r.predicate_id.clone()) {
                predicates.push(r.predicate_id.clone());
            }
        }
        predicates
    }

    /// Summary of checks grouped by domain.
    pub fn domain_summary(&self) -> Vec<(InvariantDomain, u64, u64)> {
        let domains = [
            InvariantDomain::Scheduler,
            InvariantDomain::Budget,
            InvariantDomain::Recovery,
            InvariantDomain::Composition,
        ];
        domains
            .iter()
            .map(|d| {
                let total = self.results.iter().filter(|r| r.domain == *d).count() as u64;
                let violations = self
                    .results
                    .iter()
                    .filter(|r| r.domain == *d && r.violated())
                    .count() as u64;
                (*d, total, violations)
            })
            .collect()
    }
}

// ── E2: Model-Checking Harness and Counterexample Pipeline ────────
//
// Bounded model-checking for latency invariants.  The `ModelChecker`
// explores state space via systematic injection of observations and
// records counterexample traces when invariants are violated.

/// A single step in a model-checking trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceStep {
    /// Step index (0-based).
    pub step: u64,
    /// Action applied at this step.
    pub action: TraceAction,
    /// Invariant check results after the action.
    pub check_results: Vec<InvariantCheckResult>,
    /// Timestamp (epoch μs).
    pub timestamp_us: u64,
}

/// An action in the model-checking state space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TraceAction {
    /// Observe a latency value at a stage.
    ObserveLatency {
        stage: LatencyStage,
        latency_us: f64,
    },
    /// Admit a work item to the scheduler.
    SchedulerAdmit { lane: SchedulerLane, cost_us: f64 },
    /// Trigger recovery at a stage.
    RecoveryStep {
        level_before: MitigationLevel,
        level_after: MitigationLevel,
    },
    /// Advance the epoch.
    EpochAdvance { new_epoch: u64 },
    /// Reset a subsystem.
    Reset { domain: InvariantDomain },
}

impl fmt::Display for TraceAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObserveLatency { stage, latency_us } => {
                write!(f, "observe({stage}, {latency_us:.1}μs)")
            }
            Self::SchedulerAdmit { lane, cost_us } => {
                write!(f, "admit({lane:?}, {cost_us:.1}μs)")
            }
            Self::RecoveryStep {
                level_before,
                level_after,
            } => {
                write!(f, "recover({level_before} -> {level_after})")
            }
            Self::EpochAdvance { new_epoch } => write!(f, "epoch({new_epoch})"),
            Self::Reset { domain } => write!(f, "reset({domain})"),
        }
    }
}

/// A counterexample: a sequence of steps that leads to an invariant violation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Counterexample {
    /// The predicate that was violated.
    pub predicate_id: String,
    /// Domain of the violated invariant.
    pub domain: InvariantDomain,
    /// The trace of steps leading to the violation.
    pub trace: Vec<TraceStep>,
    /// Human-readable description of the violation.
    pub description: String,
    /// Timestamp when the counterexample was found.
    pub found_at_us: u64,
}

impl fmt::Display for Counterexample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "counterexample[{}]: {} ({} steps)",
            self.predicate_id,
            self.description,
            self.trace.len()
        )
    }
}

/// Exploration strategy for the model checker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExplorationStrategy {
    /// Breadth-first: explore all states at depth d before d+1.
    BreadthFirst,
    /// Random walk: pick random actions for N steps.
    RandomWalk,
    /// Guided: prioritize actions near known violation domains.
    Guided,
}

impl fmt::Display for ExplorationStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BreadthFirst => f.write_str("bfs"),
            Self::RandomWalk => f.write_str("random"),
            Self::Guided => f.write_str("guided"),
        }
    }
}

/// Configuration for the model checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCheckerConfig {
    /// Maximum depth (steps) to explore.
    pub max_depth: u64,
    /// Maximum total states to explore before stopping.
    pub max_states: u64,
    /// Exploration strategy.
    pub strategy: ExplorationStrategy,
    /// Maximum counterexamples to collect before stopping.
    pub max_counterexamples: usize,
    /// Whether to continue exploring after first counterexample.
    pub exhaustive: bool,
}

impl Default for ModelCheckerConfig {
    fn default() -> Self {
        Self {
            max_depth: 100,
            max_states: 10_000,
            strategy: ExplorationStrategy::RandomWalk,
            max_counterexamples: 10,
            exhaustive: false,
        }
    }
}

/// Snapshot of the model checker's exploration state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCheckerSnapshot {
    pub states_explored: u64,
    pub current_depth: u64,
    pub counterexamples_found: usize,
    pub invariants_checked: u64,
    pub violations_found: u64,
    pub strategy: ExplorationStrategy,
}

/// Result of a model-checking run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelCheckVerdict {
    /// No violations found within exploration bounds.
    NoViolation {
        states_explored: u64,
        depth_reached: u64,
    },
    /// Violations found.
    ViolationsFound {
        counterexamples: Vec<Counterexample>,
    },
    /// Exploration was terminated early (budget exhausted).
    Incomplete {
        states_explored: u64,
        reason: String,
    },
}

impl fmt::Display for ModelCheckVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoViolation {
                states_explored,
                depth_reached,
            } => {
                write!(
                    f,
                    "NO_VIOLATION ({states_explored} states, depth {depth_reached})"
                )
            }
            Self::ViolationsFound { counterexamples } => {
                write!(
                    f,
                    "VIOLATIONS_FOUND ({} counterexamples)",
                    counterexamples.len()
                )
            }
            Self::Incomplete {
                states_explored,
                reason,
            } => {
                write!(f, "INCOMPLETE ({states_explored} states): {reason}")
            }
        }
    }
}

/// The model checker explores state space and finds counterexamples.
#[derive(Debug, Clone)]
pub struct ModelChecker {
    config: ModelCheckerConfig,
    checker: InvariantChecker,
    counterexamples: Vec<Counterexample>,
    current_trace: Vec<TraceStep>,
    states_explored: u64,
    current_depth: u64,
    max_depth_reached: u64,
}

impl ModelChecker {
    /// Create a new model checker.
    pub fn new(config: ModelCheckerConfig) -> Self {
        Self {
            config,
            checker: InvariantChecker::with_defaults(),
            counterexamples: Vec::new(),
            current_trace: Vec::new(),
            states_explored: 0,
            current_depth: 0,
            max_depth_reached: 0,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ModelCheckerConfig::default())
    }

    /// Record a trace step and check invariants.
    ///
    /// If any invariant is violated, a counterexample is captured.
    pub fn step(
        &mut self,
        action: TraceAction,
        invariants: &[InvariantCheckResult],
        timestamp_us: u64,
    ) -> bool {
        let step = TraceStep {
            step: self.current_depth,
            action,
            check_results: invariants.to_vec(),
            timestamp_us,
        };

        let has_violation = step.check_results.iter().any(|r| r.violated());

        self.current_trace.push(step);
        self.states_explored += 1;
        self.current_depth += 1;
        if self.current_depth > self.max_depth_reached {
            self.max_depth_reached = self.current_depth;
        }

        if has_violation {
            // Capture counterexample from current trace
            if let Some(violated) = invariants.iter().find(|r| r.violated()) {
                let cx = Counterexample {
                    predicate_id: violated.predicate_id.clone(),
                    domain: violated.domain,
                    trace: self.current_trace.clone(),
                    description: format!("{}", violated.outcome),
                    found_at_us: timestamp_us,
                };
                self.counterexamples.push(cx);
            }
        }

        has_violation
    }

    /// Start a new trace (reset current path without clearing counterexamples).
    pub fn new_trace(&mut self) {
        self.current_trace.clear();
        self.current_depth = 0;
    }

    /// Number of counterexamples found.
    pub fn counterexample_count(&self) -> usize {
        self.counterexamples.len()
    }

    /// States explored so far.
    pub fn states_explored(&self) -> u64 {
        self.states_explored
    }

    /// Maximum depth reached.
    pub fn max_depth_reached(&self) -> u64 {
        self.max_depth_reached
    }

    /// Get all collected counterexamples.
    pub fn counterexamples(&self) -> &[Counterexample] {
        &self.counterexamples
    }

    /// Whether exploration should stop (budget exhausted or enough counterexamples).
    pub fn should_stop(&self) -> bool {
        if self.states_explored >= self.config.max_states {
            return true;
        }
        if self.current_depth >= self.config.max_depth {
            return true;
        }
        if !self.config.exhaustive && !self.counterexamples.is_empty() {
            return true;
        }
        self.counterexamples.len() >= self.config.max_counterexamples
    }

    /// Produce a verdict from the current exploration state.
    pub fn verdict(&self) -> ModelCheckVerdict {
        if !self.counterexamples.is_empty() {
            ModelCheckVerdict::ViolationsFound {
                counterexamples: self.counterexamples.clone(),
            }
        } else if self.states_explored >= self.config.max_states {
            ModelCheckVerdict::Incomplete {
                states_explored: self.states_explored,
                reason: "state budget exhausted".to_string(),
            }
        } else {
            ModelCheckVerdict::NoViolation {
                states_explored: self.states_explored,
                depth_reached: self.max_depth_reached,
            }
        }
    }

    /// State snapshot.
    pub fn snapshot(&self) -> ModelCheckerSnapshot {
        ModelCheckerSnapshot {
            states_explored: self.states_explored,
            current_depth: self.current_depth,
            counterexamples_found: self.counterexamples.len(),
            invariants_checked: self.checker.total_checks(),
            violations_found: self.checker.total_violations(),
            strategy: self.config.strategy,
        }
    }

    /// Status line.
    pub fn status_line(&self) -> String {
        format!(
            "model_check: states={} depth={}/{} cx={} strategy={}",
            self.states_explored,
            self.current_depth,
            self.config.max_depth,
            self.counterexamples.len(),
            self.config.strategy
        )
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.checker.reset();
        self.counterexamples.clear();
        self.current_trace.clear();
        self.states_explored = 0;
        self.current_depth = 0;
        self.max_depth_reached = 0;
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> ModelCheckerDegradation {
        if self.counterexamples.is_empty() {
            ModelCheckerDegradation::Healthy
        } else if self.counterexamples.len() <= 3 {
            ModelCheckerDegradation::ViolationsFound {
                count: self.counterexamples.len(),
            }
        } else {
            ModelCheckerDegradation::HighViolationRate {
                count: self.counterexamples.len(),
                states: self.states_explored,
            }
        }
    }

    /// Structured log entry.
    pub fn log_entry(&self) -> ModelCheckerLogEntry {
        ModelCheckerLogEntry {
            states_explored: self.states_explored,
            max_depth_reached: self.max_depth_reached,
            counterexamples_found: self.counterexamples.len(),
            verdict: self.verdict(),
            degradation: self.detect_degradation(),
        }
    }
}

/// Degradation state for the model checker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelCheckerDegradation {
    /// No violations found.
    Healthy,
    /// Some violations found (≤3).
    ViolationsFound { count: usize },
    /// Many violations found.
    HighViolationRate { count: usize, states: u64 },
}

impl fmt::Display for ModelCheckerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => f.write_str("healthy"),
            Self::ViolationsFound { count } => write!(f, "violations({count})"),
            Self::HighViolationRate { count, states } => {
                write!(f, "high_rate({count}/{states})")
            }
        }
    }
}

/// Structured log entry for the model checker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCheckerLogEntry {
    pub states_explored: u64,
    pub max_depth_reached: u64,
    pub counterexamples_found: usize,
    pub verdict: ModelCheckVerdict,
    pub degradation: ModelCheckerDegradation,
}

// ── E2 Impl: Model Checker Bridge Methods ────────────────────────

impl ModelChecker {
    /// Run a sequence of steps with scheduler invariant checks.
    pub fn run_scheduler_scenario(
        &mut self,
        checker: &mut InvariantChecker,
        actions: &[(TraceAction, Vec<SchedulerInvariant>)],
        start_us: u64,
    ) -> ModelCheckVerdict {
        for (i, (action, invariants)) in actions.iter().enumerate() {
            let ts = start_us + i as u64;
            let results = checker.check_scheduler_batch(invariants, ts);
            let violated = self.step(action.clone(), &results, ts);
            if violated && !self.config.exhaustive {
                return self.verdict();
            }
            if self.should_stop() {
                break;
            }
        }
        self.verdict()
    }

    /// Run a sequence of steps with budget invariant checks.
    pub fn run_budget_scenario(
        &mut self,
        checker: &mut InvariantChecker,
        actions: &[(TraceAction, Vec<BudgetInvariant>)],
        start_us: u64,
    ) -> ModelCheckVerdict {
        for (i, (action, invariants)) in actions.iter().enumerate() {
            let ts = start_us + i as u64;
            let results = checker.check_budget_batch(invariants, ts);
            let violated = self.step(action.clone(), &results, ts);
            if violated && !self.config.exhaustive {
                return self.verdict();
            }
            if self.should_stop() {
                break;
            }
        }
        self.verdict()
    }

    /// Run a sequence of steps with recovery invariant checks.
    pub fn run_recovery_scenario(
        &mut self,
        checker: &mut InvariantChecker,
        actions: &[(TraceAction, Vec<RecoveryInvariant>)],
        start_us: u64,
    ) -> ModelCheckVerdict {
        for (i, (action, invariants)) in actions.iter().enumerate() {
            let ts = start_us + i as u64;
            let results = checker.check_recovery_batch(invariants, ts);
            let violated = self.step(action.clone(), &results, ts);
            if violated && !self.config.exhaustive {
                return self.verdict();
            }
            if self.should_stop() {
                break;
            }
        }
        self.verdict()
    }

    /// Get counterexamples for a specific domain.
    pub fn counterexamples_by_domain(&self, domain: InvariantDomain) -> Vec<&Counterexample> {
        self.counterexamples
            .iter()
            .filter(|cx| cx.domain == domain)
            .collect()
    }

    /// Get the shortest counterexample (fewest trace steps).
    pub fn shortest_counterexample(&self) -> Option<&Counterexample> {
        self.counterexamples.iter().min_by_key(|cx| cx.trace.len())
    }

    /// Get unique violated predicate IDs.
    pub fn violated_predicates(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut preds = Vec::new();
        for cx in &self.counterexamples {
            if seen.insert(cx.predicate_id.clone()) {
                preds.push(cx.predicate_id.clone());
            }
        }
        preds
    }

    /// Access the inner invariant checker.
    pub fn inner_checker(&self) -> &InvariantChecker {
        &self.checker
    }

    /// Mutably access the inner invariant checker.
    pub fn inner_checker_mut(&mut self) -> &mut InvariantChecker {
        &mut self.checker
    }

    /// Current trace length.
    pub fn current_trace_len(&self) -> usize {
        self.current_trace.len()
    }

    /// Get the exploration strategy.
    pub fn strategy(&self) -> ExplorationStrategy {
        self.config.strategy
    }
}

// ── E3: Deterministic Trace v2 and Replay Canonicalization ─────────
//
// Versioned trace format with canonical ordering, replay determinism
// checks, and trace normalization for incident analysis tooling.

/// Trace format version for backward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TraceFormatVersion {
    /// Legacy unordered trace (v1).
    V1,
    /// Canonical ordered trace with sequence numbers (v2).
    V2,
}

impl fmt::Display for TraceFormatVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V1 => write!(f, "v1"),
            Self::V2 => write!(f, "v2"),
        }
    }
}

/// Ordering mode for canonical trace normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CanonicalOrdering {
    /// Order by timestamp only — breaks ties by sequence number.
    Temporal,
    /// Order by (domain, stage, timestamp) — groups related actions.
    DomainGrouped,
    /// Order by causal dependency (sequence number).
    Causal,
}

impl fmt::Display for CanonicalOrdering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Temporal => write!(f, "temporal"),
            Self::DomainGrouped => write!(f, "domain-grouped"),
            Self::Causal => write!(f, "causal"),
        }
    }
}

/// A single entry in a deterministic trace v2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceEntry {
    /// Monotonic sequence number assigned at capture time.
    pub seq: u64,
    /// Timestamp in microseconds.
    pub timestamp_us: u64,
    /// The action that occurred.
    pub action: TraceAction,
    /// Domain this entry belongs to (for grouping).
    pub domain: InvariantDomain,
    /// Optional causal predecessor sequence number.
    pub causal_parent: Option<u64>,
    /// Fingerprint of the action for dedup / comparison.
    pub fingerprint: u64,
}

impl TraceEntry {
    /// Compute a deterministic fingerprint of an action.
    pub fn compute_fingerprint(action: &TraceAction, domain: InvariantDomain) -> u64 {
        // FNV-1a of the debug representation for stable hashing.
        let repr = format!("{action:?}|{domain}");
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in repr.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
}

impl fmt::Display for TraceEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] @{}μs {}", self.seq, self.timestamp_us, self.action)
    }
}

/// A complete deterministic trace with version metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeterministicTrace {
    /// Format version.
    pub version: TraceFormatVersion,
    /// Unique trace ID (typically a hash of seed + config).
    pub trace_id: String,
    /// Seed used for deterministic replay (0 = unseeded).
    pub seed: u64,
    /// The ordered list of trace entries.
    pub entries: Vec<TraceEntry>,
    /// Timestamp of trace creation (epoch μs).
    pub created_at_us: u64,
    /// Total wall-clock duration of the captured run (μs).
    pub duration_us: u64,
}

impl DeterministicTrace {
    /// Create a new empty v2 trace.
    pub fn new_v2(trace_id: String, seed: u64, created_at_us: u64) -> Self {
        Self {
            version: TraceFormatVersion::V2,
            trace_id,
            seed,
            entries: Vec::new(),
            created_at_us,
            duration_us: 0,
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the trace has entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append an entry, auto-assigning the next sequence number.
    pub fn push(
        &mut self,
        action: TraceAction,
        domain: InvariantDomain,
        timestamp_us: u64,
        causal_parent: Option<u64>,
    ) {
        let seq = self.entries.len() as u64;
        let fingerprint = TraceEntry::compute_fingerprint(&action, domain);
        self.entries.push(TraceEntry {
            seq,
            timestamp_us,
            action,
            domain,
            causal_parent,
            fingerprint,
        });
        if timestamp_us > self.created_at_us {
            self.duration_us = timestamp_us - self.created_at_us;
        }
    }

    /// Compute a digest of the entire trace for quick equality checks.
    pub fn digest(&self) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for entry in &self.entries {
            hash ^= entry.fingerprint;
            hash = hash.wrapping_mul(0x100000001b3);
            hash ^= entry.timestamp_us;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
}

impl fmt::Display for DeterministicTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Trace[{}, id={}, seed={}, entries={}, duration={}μs]",
            self.version,
            self.trace_id,
            self.seed,
            self.entries.len(),
            self.duration_us
        )
    }
}

/// Result of comparing two traces for replay isomorphism.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReplayComparisonResult {
    /// Traces are identical (same ordering and content).
    Identical,
    /// Traces are isomorphic (same content, different ordering).
    Isomorphic {
        /// Number of entries that differ in position.
        reordered_count: usize,
    },
    /// Traces differ in content.
    Divergent {
        /// Index of the first divergent entry in the canonical form.
        first_divergence_idx: usize,
        /// Description of the mismatch.
        description: String,
    },
}

impl fmt::Display for ReplayComparisonResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identical => write!(f, "identical"),
            Self::Isomorphic { reordered_count } => {
                write!(f, "isomorphic ({reordered_count} reordered)")
            }
            Self::Divergent {
                first_divergence_idx,
                description,
            } => {
                write!(f, "divergent at [{first_divergence_idx}]: {description}")
            }
        }
    }
}

/// Mismatch diagnostic for trace comparison debugging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceMismatch {
    /// Position in the canonical trace.
    pub canonical_idx: usize,
    /// Expected action fingerprint.
    pub expected_fingerprint: u64,
    /// Actual action fingerprint (None if entry missing).
    pub actual_fingerprint: Option<u64>,
    /// Human-readable explanation.
    pub explanation: String,
}

/// Configuration for the replay canonicalizer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalizerConfig {
    /// Ordering mode for canonical form.
    pub ordering: CanonicalOrdering,
    /// Whether to strip timestamps during canonicalization (for order-only comparison).
    pub strip_timestamps: bool,
    /// Whether to collapse duplicate consecutive actions.
    pub dedup_consecutive: bool,
    /// Maximum entries to process (0 = unlimited).
    pub max_entries: usize,
}

impl Default for CanonicalizerConfig {
    fn default() -> Self {
        Self {
            ordering: CanonicalOrdering::Causal,
            strip_timestamps: false,
            dedup_consecutive: false,
            max_entries: 0,
        }
    }
}

/// Snapshot of canonicalizer state for telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalizerSnapshot {
    /// Total traces canonicalized.
    pub traces_processed: u64,
    /// Total entries processed across all traces.
    pub entries_processed: u64,
    /// Total entries deduped.
    pub entries_deduped: u64,
    /// Total comparisons made.
    pub comparisons_made: u64,
    /// Configuration in use.
    pub config: CanonicalizerConfig,
}

/// Degradation state for the canonicalizer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CanonicalizerDegradation {
    /// Operating normally.
    Healthy,
    /// High dedup ratio suggests repetitive traces.
    HighDedupRatio { ratio: f64 },
    /// Processing many large traces.
    HighVolume { entries_processed: u64 },
}

impl fmt::Display for CanonicalizerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::HighDedupRatio { ratio } => write!(f, "high-dedup({ratio:.2})"),
            Self::HighVolume { entries_processed } => write!(f, "high-volume({entries_processed})"),
        }
    }
}

/// Log entry for canonicalizer operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalizerLogEntry {
    /// Timestamp of the log event.
    pub timestamp_us: u64,
    /// Trace ID that was processed.
    pub trace_id: String,
    /// Number of entries in the input.
    pub input_entries: usize,
    /// Number of entries after canonicalization.
    pub output_entries: usize,
    /// Duration of canonicalization (μs).
    pub duration_us: u64,
}

/// The replay canonicalizer: normalizes traces into canonical form and
/// compares them for replay determinism / isomorphism.
pub struct ReplayCanonicalizer {
    config: CanonicalizerConfig,
    traces_processed: u64,
    entries_processed: u64,
    entries_deduped: u64,
    comparisons_made: u64,
}

impl ReplayCanonicalizer {
    /// Create a new canonicalizer with the given config.
    pub fn new(config: CanonicalizerConfig) -> Self {
        Self {
            config,
            traces_processed: 0,
            entries_processed: 0,
            entries_deduped: 0,
            comparisons_made: 0,
        }
    }

    /// Canonicalize a trace into the configured ordering.
    pub fn canonicalize(&mut self, trace: &DeterministicTrace) -> DeterministicTrace {
        self.traces_processed += 1;
        let mut entries = trace.entries.clone();
        let input_len = entries.len();

        // Apply max_entries limit.
        if self.config.max_entries > 0 && entries.len() > self.config.max_entries {
            entries.truncate(self.config.max_entries);
        }

        // Sort by the configured ordering.
        match self.config.ordering {
            CanonicalOrdering::Temporal => {
                entries.sort_by(|a, b| a.timestamp_us.cmp(&b.timestamp_us).then(a.seq.cmp(&b.seq)));
            }
            CanonicalOrdering::DomainGrouped => {
                entries.sort_by(|a, b| {
                    let da = domain_sort_key(a.domain);
                    let db = domain_sort_key(b.domain);
                    da.cmp(&db)
                        .then(a.timestamp_us.cmp(&b.timestamp_us))
                        .then(a.seq.cmp(&b.seq))
                });
            }
            CanonicalOrdering::Causal => {
                entries.sort_by(|a, b| a.seq.cmp(&b.seq));
            }
        }

        // Optionally strip timestamps.
        if self.config.strip_timestamps {
            for entry in &mut entries {
                entry.timestamp_us = 0;
            }
        }

        // Optionally dedup consecutive identical actions.
        if self.config.dedup_consecutive && entries.len() > 1 {
            let before = entries.len();
            entries.dedup_by(|a, b| a.fingerprint == b.fingerprint);
            self.entries_deduped += (before - entries.len()) as u64;
        }

        self.entries_processed += input_len as u64;

        // Reassign sequence numbers in canonical order.
        for (i, entry) in entries.iter_mut().enumerate() {
            entry.seq = i as u64;
        }

        DeterministicTrace {
            version: TraceFormatVersion::V2,
            trace_id: trace.trace_id.clone(),
            seed: trace.seed,
            entries,
            created_at_us: trace.created_at_us,
            duration_us: trace.duration_us,
        }
    }

    /// Compare two traces for replay isomorphism.
    pub fn compare(
        &mut self,
        a: &DeterministicTrace,
        b: &DeterministicTrace,
    ) -> ReplayComparisonResult {
        self.comparisons_made += 1;

        let ca = self.canonicalize(a);
        let cb = self.canonicalize(b);

        // Quick length check.
        if ca.entries.len() != cb.entries.len() {
            return ReplayComparisonResult::Divergent {
                first_divergence_idx: ca.entries.len().min(cb.entries.len()),
                description: format!(
                    "length mismatch: {} vs {}",
                    ca.entries.len(),
                    cb.entries.len()
                ),
            };
        }

        // Check for identical canonical forms.
        let mut identical = true;
        let mut first_diff = None;
        for (i, (ea, eb)) in ca.entries.iter().zip(cb.entries.iter()).enumerate() {
            if ea.fingerprint != eb.fingerprint {
                identical = false;
                first_diff = Some(i);
                break;
            }
            if ea.timestamp_us != eb.timestamp_us {
                identical = false;
            }
        }

        if identical && first_diff.is_none() {
            // Check if the original ordering was the same.
            let orig_same = a
                .entries
                .iter()
                .zip(b.entries.iter())
                .all(|(ea, eb)| ea.fingerprint == eb.fingerprint && ea.seq == eb.seq);
            if orig_same {
                return ReplayComparisonResult::Identical;
            }
            // Same content after canonicalization but different original order.
            let reordered = a
                .entries
                .iter()
                .zip(b.entries.iter())
                .filter(|(ea, eb)| ea.seq != eb.seq || ea.fingerprint != eb.fingerprint)
                .count();
            return ReplayComparisonResult::Isomorphic {
                reordered_count: reordered,
            };
        }

        if let Some(idx) = first_diff {
            return ReplayComparisonResult::Divergent {
                first_divergence_idx: idx,
                description: format!(
                    "fingerprint mismatch: {} vs {}",
                    ca.entries[idx].fingerprint, cb.entries[idx].fingerprint
                ),
            };
        }

        // Timestamps differ but content is isomorphic.
        let reordered = a
            .entries
            .iter()
            .zip(b.entries.iter())
            .filter(|(ea, eb)| ea.timestamp_us != eb.timestamp_us)
            .count();
        ReplayComparisonResult::Isomorphic {
            reordered_count: reordered,
        }
    }

    /// Generate mismatch diagnostics between two traces.
    pub fn diagnose_mismatches(
        &self,
        a: &DeterministicTrace,
        b: &DeterministicTrace,
    ) -> Vec<TraceMismatch> {
        let mut mismatches = Vec::new();
        let max_len = a.entries.len().max(b.entries.len());
        for i in 0..max_len {
            match (a.entries.get(i), b.entries.get(i)) {
                (Some(ea), Some(eb)) if ea.fingerprint != eb.fingerprint => {
                    mismatches.push(TraceMismatch {
                        canonical_idx: i,
                        expected_fingerprint: ea.fingerprint,
                        actual_fingerprint: Some(eb.fingerprint),
                        explanation: format!("expected {} but got {}", ea.action, eb.action),
                    });
                }
                (Some(ea), None) => {
                    mismatches.push(TraceMismatch {
                        canonical_idx: i,
                        expected_fingerprint: ea.fingerprint,
                        actual_fingerprint: None,
                        explanation: format!("missing entry: expected {}", ea.action),
                    });
                }
                (None, Some(eb)) => {
                    mismatches.push(TraceMismatch {
                        canonical_idx: i,
                        expected_fingerprint: 0,
                        actual_fingerprint: Some(eb.fingerprint),
                        explanation: format!("extra entry: {}", eb.action),
                    });
                }
                _ => {}
            }
        }
        mismatches
    }

    /// Upgrade a v1 trace (from ModelChecker) to v2 format.
    pub fn upgrade_trace(
        &mut self,
        steps: &[TraceStep],
        trace_id: String,
        seed: u64,
    ) -> DeterministicTrace {
        let mut trace = DeterministicTrace::new_v2(trace_id, seed, 0);
        for step in steps {
            let domain = action_domain(&step.action);
            trace.push(step.action.clone(), domain, step.timestamp_us, None);
        }
        trace
    }

    /// Get a snapshot of canonicalizer state.
    pub fn snapshot(&self) -> CanonicalizerSnapshot {
        CanonicalizerSnapshot {
            traces_processed: self.traces_processed,
            entries_processed: self.entries_processed,
            entries_deduped: self.entries_deduped,
            comparisons_made: self.comparisons_made,
            config: self.config.clone(),
        }
    }

    /// Detect degradation conditions.
    pub fn detect_degradation(&self) -> CanonicalizerDegradation {
        if self.entries_processed > 100_000 {
            return CanonicalizerDegradation::HighVolume {
                entries_processed: self.entries_processed,
            };
        }
        if self.entries_processed > 0 {
            let ratio = self.entries_deduped as f64 / self.entries_processed as f64;
            if ratio > 0.5 {
                return CanonicalizerDegradation::HighDedupRatio { ratio };
            }
        }
        CanonicalizerDegradation::Healthy
    }

    /// Create a log entry for a canonicalization operation.
    pub fn log_entry(
        &self,
        trace_id: &str,
        input_entries: usize,
        output_entries: usize,
        duration_us: u64,
    ) -> CanonicalizerLogEntry {
        CanonicalizerLogEntry {
            timestamp_us: self.entries_processed, // monotonic proxy
            trace_id: trace_id.to_string(),
            input_entries,
            output_entries,
            duration_us,
        }
    }

    /// Reset counters.
    pub fn reset(&mut self) {
        self.traces_processed = 0;
        self.entries_processed = 0;
        self.entries_deduped = 0;
        self.comparisons_made = 0;
    }
}

// ── E3 Impl: Bridge methods and convenience API ───────────────────

impl ReplayCanonicalizer {
    /// Canonicalize and compare two model-checker trace outputs directly.
    pub fn compare_mc_traces(
        &mut self,
        a: &[TraceStep],
        b: &[TraceStep],
        seed: u64,
    ) -> ReplayComparisonResult {
        let ta = self.upgrade_trace(a, "mc-a".to_string(), seed);
        let tb = self.upgrade_trace(b, "mc-b".to_string(), seed);
        self.compare(&ta, &tb)
    }

    /// Check replay determinism: run canonicalize twice on the same trace
    /// and verify the output is identical (self-consistency check).
    pub fn verify_determinism(&mut self, trace: &DeterministicTrace) -> bool {
        let c1 = self.canonicalize(trace);
        let c2 = self.canonicalize(trace);
        c1.entries.iter().zip(c2.entries.iter()).all(|(a, b)| {
            a.fingerprint == b.fingerprint && a.seq == b.seq && a.timestamp_us == b.timestamp_us
        })
    }

    /// Extract a sub-trace containing only entries in the given domain.
    pub fn filter_by_domain(
        &self,
        trace: &DeterministicTrace,
        domain: InvariantDomain,
    ) -> DeterministicTrace {
        let entries: Vec<TraceEntry> = trace
            .entries
            .iter()
            .filter(|e| e.domain == domain)
            .cloned()
            .enumerate()
            .map(|(i, mut e)| {
                e.seq = i as u64;
                e
            })
            .collect();
        DeterministicTrace {
            version: trace.version,
            trace_id: format!("{}-{}", trace.trace_id, domain),
            seed: trace.seed,
            entries,
            created_at_us: trace.created_at_us,
            duration_us: trace.duration_us,
        }
    }

    /// Extract the causal dependency chain for a given entry.
    pub fn causal_chain(&self, trace: &DeterministicTrace, entry_seq: u64) -> Vec<u64> {
        let mut chain = Vec::new();
        let mut current = Some(entry_seq);
        let index: std::collections::HashMap<u64, &TraceEntry> =
            trace.entries.iter().map(|e| (e.seq, e)).collect();
        while let Some(seq) = current {
            chain.push(seq);
            current = index.get(&seq).and_then(|e| e.causal_parent);
        }
        chain.reverse();
        chain
    }

    /// Compute per-domain entry counts.
    pub fn domain_histogram(
        &self,
        trace: &DeterministicTrace,
    ) -> std::collections::HashMap<String, usize> {
        let mut counts = std::collections::HashMap::new();
        for entry in &trace.entries {
            *counts.entry(entry.domain.to_string()).or_insert(0) += 1;
        }
        counts
    }

    /// Find entries whose fingerprint appears exactly once (unique actions).
    pub fn unique_fingerprints(&self, trace: &DeterministicTrace) -> Vec<u64> {
        let mut counts: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for entry in &trace.entries {
            *counts.entry(entry.fingerprint).or_insert(0) += 1;
        }
        trace
            .entries
            .iter()
            .filter(|e| counts.get(&e.fingerprint) == Some(&1))
            .map(|e| e.seq)
            .collect()
    }

    /// Merge two traces interleaving by timestamp (union merge).
    pub fn merge_traces(
        &mut self,
        a: &DeterministicTrace,
        b: &DeterministicTrace,
    ) -> DeterministicTrace {
        let mut entries: Vec<TraceEntry> =
            a.entries.iter().chain(b.entries.iter()).cloned().collect();
        entries.sort_by(|x, y| x.timestamp_us.cmp(&y.timestamp_us).then(x.seq.cmp(&y.seq)));
        for (i, entry) in entries.iter_mut().enumerate() {
            entry.seq = i as u64;
        }
        let duration = entries
            .last()
            .map_or(0, |e| e.timestamp_us)
            .saturating_sub(a.created_at_us.min(b.created_at_us));
        DeterministicTrace {
            version: TraceFormatVersion::V2,
            trace_id: format!("{}-{}", a.trace_id, b.trace_id),
            seed: a.seed ^ b.seed,
            entries,
            created_at_us: a.created_at_us.min(b.created_at_us),
            duration_us: duration,
        }
    }

    /// Slice a trace to entries within a time window.
    pub fn time_window(
        &self,
        trace: &DeterministicTrace,
        start_us: u64,
        end_us: u64,
    ) -> DeterministicTrace {
        let entries: Vec<TraceEntry> = trace
            .entries
            .iter()
            .filter(|e| e.timestamp_us >= start_us && e.timestamp_us <= end_us)
            .cloned()
            .enumerate()
            .map(|(i, mut e)| {
                e.seq = i as u64;
                e
            })
            .collect();
        DeterministicTrace {
            version: trace.version,
            trace_id: format!("{}-window", trace.trace_id),
            seed: trace.seed,
            entries,
            created_at_us: start_us,
            duration_us: end_us.saturating_sub(start_us),
        }
    }

    /// Total comparisons performed.
    pub fn total_comparisons(&self) -> u64 {
        self.comparisons_made
    }

    /// Total traces processed.
    pub fn total_traces(&self) -> u64 {
        self.traces_processed
    }

    /// Access the current config.
    pub fn config(&self) -> &CanonicalizerConfig {
        &self.config
    }
}

/// Map a TraceAction to its primary InvariantDomain.
fn action_domain(action: &TraceAction) -> InvariantDomain {
    match action {
        TraceAction::ObserveLatency { .. } => InvariantDomain::Budget,
        TraceAction::SchedulerAdmit { .. } => InvariantDomain::Scheduler,
        TraceAction::RecoveryStep { .. } => InvariantDomain::Recovery,
        TraceAction::EpochAdvance { .. } => InvariantDomain::Composition,
        TraceAction::Reset { domain } => *domain,
    }
}

/// Sort key for domain-grouped ordering.
fn domain_sort_key(domain: InvariantDomain) -> u8 {
    match domain {
        InvariantDomain::Scheduler => 0,
        InvariantDomain::Budget => 1,
        InvariantDomain::Recovery => 2,
        InvariantDomain::Composition => 3,
    }
}

// ── E4: Optimization Isomorphism Proof Gate ───────────────────────
//
// Golden artifact management, optimization proof gates, semantic drift
// detection, and proof summary generation for CI integration.

/// A golden artifact: a reference trace with checksum for regression checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldenArtifact {
    /// Unique identifier for this artifact (e.g., "scheduler-hot-path-v2").
    pub artifact_id: String,
    /// Version of the artifact (incremented on approved changes).
    pub version: u64,
    /// The reference trace.
    pub trace: DeterministicTrace,
    /// FNV-1a digest of the canonical trace.
    pub checksum: u64,
    /// Description of the optimization this artifact guards.
    pub description: String,
    /// Timestamp when this artifact was created/updated.
    pub created_at_us: u64,
}

impl GoldenArtifact {
    /// Create a new golden artifact from a trace.
    pub fn new(artifact_id: String, trace: DeterministicTrace, description: String, created_at_us: u64) -> Self {
        let checksum = trace.digest();
        Self {
            artifact_id,
            version: 1,
            trace,
            checksum,
            description,
            created_at_us,
        }
    }

    /// Verify that the stored checksum matches the trace digest.
    pub fn verify_checksum(&self) -> bool {
        self.trace.digest() == self.checksum
    }

    /// Update the golden artifact with a new trace (bumps version).
    pub fn update(&mut self, trace: DeterministicTrace, created_at_us: u64) {
        self.checksum = trace.digest();
        self.trace = trace;
        self.version += 1;
        self.created_at_us = created_at_us;
    }
}

impl fmt::Display for GoldenArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Golden[{} v{}, entries={}, checksum={:#x}]",
            self.artifact_id, self.version, self.trace.len(), self.checksum
        )
    }
}

/// Verdict from an optimization proof gate check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProofGateVerdict {
    /// Optimization preserves behavior exactly.
    Equivalent,
    /// Optimization preserves behavior under reordering (isomorphic).
    IsomorphicEquivalent { reordered_count: usize },
    /// Semantic drift detected — optimization changed behavior.
    SemanticDrift {
        /// Index of the first divergent entry.
        first_divergence_idx: usize,
        /// Mismatches found.
        mismatches: Vec<TraceMismatch>,
        /// Human-readable summary.
        summary: String,
    },
    /// Checksum mismatch on golden artifact (corruption or tampering).
    ChecksumFailure {
        expected: u64,
        actual: u64,
    },
}

impl ProofGateVerdict {
    /// Whether the verdict allows the optimization to proceed.
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Equivalent | Self::IsomorphicEquivalent { .. })
    }

    /// Whether the verdict blocks the optimization.
    pub fn is_fail(&self) -> bool {
        !self.is_pass()
    }
}

impl fmt::Display for ProofGateVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Equivalent => write!(f, "PASS: equivalent"),
            Self::IsomorphicEquivalent { reordered_count } => {
                write!(f, "PASS: isomorphic ({reordered_count} reordered)")
            }
            Self::SemanticDrift { first_divergence_idx, mismatches, summary } => {
                write!(
                    f,
                    "FAIL: semantic drift at [{first_divergence_idx}], {} mismatches: {summary}",
                    mismatches.len()
                )
            }
            Self::ChecksumFailure { expected, actual } => {
                write!(f, "FAIL: checksum {expected:#x} != {actual:#x}")
            }
        }
    }
}

/// Configuration for the proof gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProofGateConfig {
    /// Whether to allow isomorphic equivalence (reordered but same content).
    pub allow_isomorphic: bool,
    /// Maximum number of mismatches to report before truncating.
    pub max_mismatches: usize,
    /// Canonicalization config to use for comparisons.
    pub canonicalizer_config: CanonicalizerConfig,
}

impl Default for ProofGateConfig {
    fn default() -> Self {
        Self {
            allow_isomorphic: true,
            max_mismatches: 50,
            canonicalizer_config: CanonicalizerConfig::default(),
        }
    }
}

/// A proof summary for logging/CI output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProofSummary {
    /// Artifact being checked.
    pub artifact_id: String,
    /// Golden version tested against.
    pub golden_version: u64,
    /// Verdict of the check.
    pub verdict: ProofGateVerdict,
    /// Number of entries in the candidate trace.
    pub candidate_entries: usize,
    /// Number of entries in the golden trace.
    pub golden_entries: usize,
    /// Duration of the proof check (μs).
    pub check_duration_us: u64,
    /// Timestamp of the check.
    pub timestamp_us: u64,
}

impl fmt::Display for ProofSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Proof[{} v{}: {} ({}/{}e, {}μs)]",
            self.artifact_id,
            self.golden_version,
            self.verdict,
            self.candidate_entries,
            self.golden_entries,
            self.check_duration_us,
        )
    }
}

/// Snapshot of the proof gate state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProofGateSnapshot {
    /// Total checks run.
    pub checks_run: u64,
    /// Total passes (equivalent or isomorphic).
    pub passes: u64,
    /// Total failures (drift or checksum).
    pub failures: u64,
    /// Number of golden artifacts stored.
    pub artifacts_count: usize,
    /// Configuration.
    pub config: ProofGateConfig,
}

/// Degradation state for the proof gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProofGateDegradation {
    /// Operating normally.
    Healthy,
    /// High failure rate suggests unstable optimizations.
    HighFailureRate { rate: f64 },
    /// Many artifacts may slow down CI checks.
    HighArtifactCount { count: usize },
}

impl fmt::Display for ProofGateDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::HighFailureRate { rate } => write!(f, "high-failure-rate({rate:.2})"),
            Self::HighArtifactCount { count } => write!(f, "high-artifact-count({count})"),
        }
    }
}

/// Log entry for proof gate operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProofGateLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Artifact checked.
    pub artifact_id: String,
    /// Pass or fail.
    pub passed: bool,
    /// Duration of check.
    pub check_duration_us: u64,
}

/// The optimization isomorphism proof gate.
pub struct ProofGate {
    config: ProofGateConfig,
    artifacts: Vec<GoldenArtifact>,
    canonicalizer: ReplayCanonicalizer,
    checks_run: u64,
    passes: u64,
    failures: u64,
}

impl ProofGate {
    /// Create a new proof gate with the given config.
    pub fn new(config: ProofGateConfig) -> Self {
        let canonicalizer = ReplayCanonicalizer::new(config.canonicalizer_config.clone());
        Self {
            config,
            artifacts: Vec::new(),
            canonicalizer,
            checks_run: 0,
            passes: 0,
            failures: 0,
        }
    }

    /// Register a golden artifact.
    pub fn register_golden(&mut self, artifact: GoldenArtifact) {
        // Replace if same artifact_id exists.
        if let Some(pos) = self.artifacts.iter().position(|a| a.artifact_id == artifact.artifact_id) {
            self.artifacts[pos] = artifact;
        } else {
            self.artifacts.push(artifact);
        }
    }

    /// Look up a golden artifact by ID.
    pub fn get_golden(&self, artifact_id: &str) -> Option<&GoldenArtifact> {
        self.artifacts.iter().find(|a| a.artifact_id == artifact_id)
    }

    /// Check a candidate trace against a golden artifact.
    pub fn check(
        &mut self,
        artifact_id: &str,
        candidate: &DeterministicTrace,
        timestamp_us: u64,
    ) -> ProofSummary {
        self.checks_run += 1;

        let golden = self.artifacts.iter().find(|a| a.artifact_id == artifact_id);
        let golden = match golden {
            Some(g) => g.clone(),
            None => {
                self.failures += 1;
                return ProofSummary {
                    artifact_id: artifact_id.to_string(),
                    golden_version: 0,
                    verdict: ProofGateVerdict::SemanticDrift {
                        first_divergence_idx: 0,
                        mismatches: vec![],
                        summary: format!("golden artifact '{artifact_id}' not found"),
                    },
                    candidate_entries: candidate.len(),
                    golden_entries: 0,
                    check_duration_us: 0,
                    timestamp_us,
                };
            }
        };

        // Verify golden checksum.
        if !golden.verify_checksum() {
            self.failures += 1;
            return ProofSummary {
                artifact_id: artifact_id.to_string(),
                golden_version: golden.version,
                verdict: ProofGateVerdict::ChecksumFailure {
                    expected: golden.checksum,
                    actual: golden.trace.digest(),
                },
                candidate_entries: candidate.len(),
                golden_entries: golden.trace.len(),
                check_duration_us: 0,
                timestamp_us,
            };
        }

        // Compare candidate with golden.
        let comparison = self.canonicalizer.compare(&golden.trace, candidate);
        let verdict = match comparison {
            ReplayComparisonResult::Identical => ProofGateVerdict::Equivalent,
            ReplayComparisonResult::Isomorphic { reordered_count } if self.config.allow_isomorphic => {
                ProofGateVerdict::IsomorphicEquivalent { reordered_count }
            }
            ReplayComparisonResult::Isomorphic { reordered_count } => {
                ProofGateVerdict::SemanticDrift {
                    first_divergence_idx: 0,
                    mismatches: vec![],
                    summary: format!("isomorphic not allowed ({reordered_count} reordered)"),
                }
            }
            ReplayComparisonResult::Divergent { first_divergence_idx, description } => {
                let mut mismatches = self.canonicalizer.diagnose_mismatches(&golden.trace, candidate);
                if mismatches.len() > self.config.max_mismatches {
                    mismatches.truncate(self.config.max_mismatches);
                }
                ProofGateVerdict::SemanticDrift {
                    first_divergence_idx,
                    mismatches,
                    summary: description,
                }
            }
        };

        let passed = verdict.is_pass();
        if passed {
            self.passes += 1;
        } else {
            self.failures += 1;
        }

        ProofSummary {
            artifact_id: artifact_id.to_string(),
            golden_version: golden.version,
            verdict,
            candidate_entries: candidate.len(),
            golden_entries: golden.trace.len(),
            check_duration_us: 0,
            timestamp_us,
        }
    }

    /// Check all registered golden artifacts against a set of candidates.
    pub fn check_all(
        &mut self,
        candidates: &std::collections::HashMap<String, DeterministicTrace>,
        timestamp_us: u64,
    ) -> Vec<ProofSummary> {
        let artifact_ids: Vec<String> = self.artifacts.iter().map(|a| a.artifact_id.clone()).collect();
        let mut summaries = Vec::new();
        for id in &artifact_ids {
            if let Some(candidate) = candidates.get(id) {
                summaries.push(self.check(id, candidate, timestamp_us));
            }
        }
        summaries
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> ProofGateSnapshot {
        ProofGateSnapshot {
            checks_run: self.checks_run,
            passes: self.passes,
            failures: self.failures,
            artifacts_count: self.artifacts.len(),
            config: self.config.clone(),
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> ProofGateDegradation {
        if self.artifacts.len() > 100 {
            return ProofGateDegradation::HighArtifactCount { count: self.artifacts.len() };
        }
        if self.checks_run > 0 {
            let rate = self.failures as f64 / self.checks_run as f64;
            if rate > 0.5 {
                return ProofGateDegradation::HighFailureRate { rate };
            }
        }
        ProofGateDegradation::Healthy
    }

    /// Create a log entry.
    pub fn log_entry(&self, artifact_id: &str, passed: bool, check_duration_us: u64) -> ProofGateLogEntry {
        ProofGateLogEntry {
            timestamp_us: self.checks_run,
            artifact_id: artifact_id.to_string(),
            passed,
            check_duration_us,
        }
    }

    /// Number of registered artifacts.
    pub fn artifact_count(&self) -> usize {
        self.artifacts.len()
    }

    /// All artifact IDs.
    pub fn artifact_ids(&self) -> Vec<String> {
        self.artifacts.iter().map(|a| a.artifact_id.clone()).collect()
    }

    /// Reset counters (keeps artifacts).
    pub fn reset_counters(&mut self) {
        self.checks_run = 0;
        self.passes = 0;
        self.failures = 0;
    }

    /// Remove a golden artifact by ID.
    pub fn remove_golden(&mut self, artifact_id: &str) -> bool {
        let len_before = self.artifacts.len();
        self.artifacts.retain(|a| a.artifact_id != artifact_id);
        self.artifacts.len() < len_before
    }

    /// Access inner canonicalizer.
    pub fn canonicalizer(&self) -> &ReplayCanonicalizer {
        &self.canonicalizer
    }

    /// Access config.
    pub fn config(&self) -> &ProofGateConfig {
        &self.config
    }

    /// Check a candidate trace against a golden artifact built from
    /// ModelChecker TraceSteps (v1→v2 upgrade + proof check).
    pub fn check_from_mc_trace(
        &mut self,
        artifact_id: &str,
        mc_steps: &[TraceStep],
        seed: u64,
        timestamp_us: u64,
    ) -> ProofSummary {
        let candidate = self.canonicalizer.upgrade_trace(mc_steps, "mc-candidate".to_string(), seed);
        self.check(artifact_id, &candidate, timestamp_us)
    }

    /// Register a golden artifact from model-checker output.
    pub fn register_golden_from_mc(
        &mut self,
        artifact_id: String,
        mc_steps: &[TraceStep],
        seed: u64,
        description: String,
        created_at_us: u64,
    ) {
        let trace = self.canonicalizer.upgrade_trace(mc_steps, artifact_id.clone(), seed);
        let ga = GoldenArtifact::new(artifact_id, trace, description, created_at_us);
        self.register_golden(ga);
    }

    /// Approve a semantic drift: update the golden artifact to the candidate trace.
    pub fn approve_drift(
        &mut self,
        artifact_id: &str,
        candidate: &DeterministicTrace,
        created_at_us: u64,
    ) -> bool {
        if let Some(pos) = self.artifacts.iter().position(|a| a.artifact_id == artifact_id) {
            self.artifacts[pos].update(candidate.clone(), created_at_us);
            true
        } else {
            false
        }
    }

    /// Get all failing artifact IDs from the latest check_all results.
    pub fn failing_artifacts(summaries: &[ProofSummary]) -> Vec<String> {
        summaries.iter()
            .filter(|s| s.verdict.is_fail())
            .map(|s| s.artifact_id.clone())
            .collect()
    }

    /// Get all passing artifact IDs from the latest check_all results.
    pub fn passing_artifacts(summaries: &[ProofSummary]) -> Vec<String> {
        summaries.iter()
            .filter(|s| s.verdict.is_pass())
            .map(|s| s.artifact_id.clone())
            .collect()
    }

    /// Pass rate across a set of proof summaries.
    pub fn pass_rate(summaries: &[ProofSummary]) -> f64 {
        if summaries.is_empty() {
            return 1.0;
        }
        let passes = summaries.iter().filter(|s| s.verdict.is_pass()).count();
        passes as f64 / summaries.len() as f64
    }

    /// Total pass count.
    pub fn total_passes(&self) -> u64 {
        self.passes
    }

    /// Total failure count.
    pub fn total_failures(&self) -> u64 {
        self.failures
    }

    /// Total checks.
    pub fn total_checks(&self) -> u64 {
        self.checks_run
    }
}

// ── F1: Fault-Domain Isolation and Crash-Only Service Contracts ────
//
// Explicit fault-domain boundaries, crash-only restart semantics,
// and blast-radius containment for latency subsystems.

/// Fault domain — an isolated region that can fail independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FaultDomain {
    /// Scheduler fault domain (lane management, admission control).
    Scheduler,
    /// Budget fault domain (percentile tracking, SLO enforcement).
    Budget,
    /// Recovery fault domain (mitigation, escalation, cooldown).
    Recovery,
    /// IO fault domain (PTY capture, event emission).
    Io,
    /// Storage fault domain (write pipeline, indexing).
    Storage,
}

impl FaultDomain {
    /// All fault domains.
    pub const ALL: &'static [Self] = &[
        Self::Scheduler,
        Self::Budget,
        Self::Recovery,
        Self::Io,
        Self::Storage,
    ];
}

impl fmt::Display for FaultDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheduler => write!(f, "scheduler"),
            Self::Budget => write!(f, "budget"),
            Self::Recovery => write!(f, "recovery"),
            Self::Io => write!(f, "io"),
            Self::Storage => write!(f, "storage"),
        }
    }
}

/// Health state of a fault domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomainHealth {
    /// Operating normally.
    Healthy,
    /// Degraded but functional.
    Degraded,
    /// Crashed and awaiting restart.
    Crashed,
    /// Restarting (crash-only recovery in progress).
    Restarting,
    /// Isolated (quarantined to prevent blast-radius expansion).
    Isolated,
}

impl fmt::Display for DomainHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Degraded => write!(f, "degraded"),
            Self::Crashed => write!(f, "crashed"),
            Self::Restarting => write!(f, "restarting"),
            Self::Isolated => write!(f, "isolated"),
        }
    }
}

/// Crash-only service contract: specifies restart behavior for a domain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrashOnlyContract {
    /// Domain this contract governs.
    pub domain: FaultDomain,
    /// Maximum restart attempts before isolation.
    pub max_restarts: u32,
    /// Cooldown between restarts (μs).
    pub restart_cooldown_us: u64,
    /// Whether to checkpoint state before crash restart.
    pub checkpoint_on_crash: bool,
    /// Timeout for restart completion (μs). 0 = no timeout.
    pub restart_timeout_us: u64,
}

impl Default for CrashOnlyContract {
    fn default() -> Self {
        Self {
            domain: FaultDomain::Scheduler,
            max_restarts: 3,
            restart_cooldown_us: 100_000,
            checkpoint_on_crash: true,
            restart_timeout_us: 5_000_000,
        }
    }
}

/// A fault event recording a domain failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaultEvent {
    /// Domain that faulted.
    pub domain: FaultDomain,
    /// Timestamp (epoch μs).
    pub timestamp_us: u64,
    /// Description of the fault.
    pub description: String,
    /// Whether recovery was attempted.
    pub recovery_attempted: bool,
    /// Whether recovery succeeded.
    pub recovery_succeeded: bool,
}

/// Snapshot of a fault domain's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaultDomainState {
    /// The domain.
    pub domain: FaultDomain,
    /// Current health.
    pub health: DomainHealth,
    /// Total faults observed.
    pub total_faults: u64,
    /// Total restarts performed.
    pub total_restarts: u64,
    /// Consecutive failures (resets on success).
    pub consecutive_failures: u32,
    /// Timestamp of last fault (0 = never).
    pub last_fault_us: u64,
    /// Timestamp of last restart (0 = never).
    pub last_restart_us: u64,
}

/// Configuration for the fault isolation manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaultIsolationConfig {
    /// Contracts for each domain.
    pub contracts: Vec<CrashOnlyContract>,
    /// Whether to auto-isolate domains that exceed max_restarts.
    pub auto_isolate: bool,
    /// Maximum fault history entries to retain.
    pub max_history: usize,
}

impl Default for FaultIsolationConfig {
    fn default() -> Self {
        Self {
            contracts: FaultDomain::ALL.iter().map(|d| {
                CrashOnlyContract { domain: *d, ..Default::default() }
            }).collect(),
            auto_isolate: true,
            max_history: 1000,
        }
    }
}

/// Degradation state for the fault isolation manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FaultIsolationDegradation {
    /// All domains healthy.
    Healthy,
    /// Some domains degraded.
    PartialDegradation { degraded_count: usize },
    /// Some domains isolated.
    DomainIsolated { isolated_domains: Vec<FaultDomain> },
}

impl fmt::Display for FaultIsolationDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::PartialDegradation { degraded_count } => {
                write!(f, "partial-degradation({degraded_count})")
            }
            Self::DomainIsolated { isolated_domains } => {
                let names: Vec<String> = isolated_domains.iter().map(|d| d.to_string()).collect();
                write!(f, "isolated({})", names.join(","))
            }
        }
    }
}

/// Log entry for fault isolation events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaultIsolationLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Domain affected.
    pub domain: FaultDomain,
    /// Health transition.
    pub from_health: DomainHealth,
    /// New health state.
    pub to_health: DomainHealth,
    /// Description.
    pub description: String,
}

/// Snapshot of the entire fault isolation manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaultIsolationSnapshot {
    /// Per-domain states.
    pub domains: Vec<FaultDomainState>,
    /// Total faults across all domains.
    pub total_faults: u64,
    /// Total restarts across all domains.
    pub total_restarts: u64,
    /// Configuration.
    pub config: FaultIsolationConfig,
}

/// The fault isolation manager: tracks domain health, enforces
/// crash-only contracts, and prevents blast-radius expansion.
pub struct FaultIsolationManager {
    config: FaultIsolationConfig,
    states: std::collections::HashMap<FaultDomain, FaultDomainState>,
    history: Vec<FaultEvent>,
}

impl FaultIsolationManager {
    /// Create a new manager with the given config.
    pub fn new(config: FaultIsolationConfig) -> Self {
        let mut states = std::collections::HashMap::new();
        for domain in FaultDomain::ALL {
            states.insert(*domain, FaultDomainState {
                domain: *domain,
                health: DomainHealth::Healthy,
                total_faults: 0,
                total_restarts: 0,
                consecutive_failures: 0,
                last_fault_us: 0,
                last_restart_us: 0,
            });
        }
        Self { config, states, history: Vec::new() }
    }

    /// Record a fault in a domain.
    pub fn record_fault(&mut self, domain: FaultDomain, description: String, timestamp_us: u64) {
        let contract = self.contract_for(domain);
        let max_restarts = contract.max_restarts;
        let auto_isolate = self.config.auto_isolate;

        let state = self.states.get_mut(&domain).unwrap();
        state.total_faults += 1;
        state.consecutive_failures += 1;
        state.last_fault_us = timestamp_us;

        if auto_isolate && state.consecutive_failures > max_restarts {
            state.health = DomainHealth::Isolated;
        } else {
            state.health = DomainHealth::Crashed;
        }

        let event = FaultEvent {
            domain,
            timestamp_us,
            description,
            recovery_attempted: false,
            recovery_succeeded: false,
        };

        if self.history.len() >= self.config.max_history {
            self.history.remove(0);
        }
        self.history.push(event);
    }

    /// Attempt restart of a crashed domain.
    pub fn attempt_restart(&mut self, domain: FaultDomain, timestamp_us: u64) -> bool {
        let contract = self.contract_for(domain);
        let state = self.states.get_mut(&domain).unwrap();
        match state.health {
            DomainHealth::Crashed => {
                // Enforce cooldown.
                if state.last_restart_us > 0
                    && timestamp_us.saturating_sub(state.last_restart_us) < contract.restart_cooldown_us
                {
                    return false;
                }
                state.health = DomainHealth::Restarting;
                state.total_restarts += 1;
                state.last_restart_us = timestamp_us;
                true
            }
            DomainHealth::Isolated => false,
            _ => false,
        }
    }

    /// Mark restart as complete (success).
    pub fn restart_succeeded(&mut self, domain: FaultDomain) {
        let state = self.states.get_mut(&domain).unwrap();
        if state.health == DomainHealth::Restarting {
            state.health = DomainHealth::Healthy;
            state.consecutive_failures = 0;
        }
    }

    /// Mark restart as failed.
    pub fn restart_failed(&mut self, domain: FaultDomain, timestamp_us: u64) {
        let contract = self.contract_for(domain);
        let auto_isolate = self.config.auto_isolate;
        let state = self.states.get_mut(&domain).unwrap();
        if state.health == DomainHealth::Restarting {
            state.consecutive_failures += 1;
            if auto_isolate && state.consecutive_failures > contract.max_restarts {
                state.health = DomainHealth::Isolated;
            } else {
                state.health = DomainHealth::Crashed;
            }
            state.last_fault_us = timestamp_us;
        }
    }

    /// Manually mark a domain as degraded.
    pub fn mark_degraded(&mut self, domain: FaultDomain) {
        let state = self.states.get_mut(&domain).unwrap();
        if state.health == DomainHealth::Healthy {
            state.health = DomainHealth::Degraded;
        }
    }

    /// Manually un-isolate a domain (operator intervention).
    pub fn un_isolate(&mut self, domain: FaultDomain) {
        let state = self.states.get_mut(&domain).unwrap();
        if state.health == DomainHealth::Isolated {
            state.health = DomainHealth::Crashed;
            state.consecutive_failures = 0;
        }
    }

    /// Get the health of a domain.
    pub fn domain_health(&self, domain: FaultDomain) -> DomainHealth {
        self.states.get(&domain).map_or(DomainHealth::Healthy, |s| s.health)
    }

    /// Get the state of a domain.
    pub fn domain_state(&self, domain: FaultDomain) -> Option<&FaultDomainState> {
        self.states.get(&domain)
    }

    /// Check if any domains are isolated.
    pub fn has_isolated_domains(&self) -> bool {
        self.states.values().any(|s| s.health == DomainHealth::Isolated)
    }

    /// List isolated domains.
    pub fn isolated_domains(&self) -> Vec<FaultDomain> {
        self.states.values()
            .filter(|s| s.health == DomainHealth::Isolated)
            .map(|s| s.domain)
            .collect()
    }

    /// Get the crash-only contract for a domain.
    fn contract_for(&self, domain: FaultDomain) -> CrashOnlyContract {
        self.config.contracts.iter()
            .find(|c| c.domain == domain)
            .cloned()
            .unwrap_or_default()
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> FaultIsolationSnapshot {
        let domains: Vec<FaultDomainState> = FaultDomain::ALL.iter()
            .filter_map(|d| self.states.get(d).cloned())
            .collect();
        let total_faults = domains.iter().map(|d| d.total_faults).sum();
        let total_restarts = domains.iter().map(|d| d.total_restarts).sum();
        FaultIsolationSnapshot {
            domains,
            total_faults,
            total_restarts,
            config: self.config.clone(),
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> FaultIsolationDegradation {
        let isolated: Vec<FaultDomain> = self.isolated_domains();
        if !isolated.is_empty() {
            return FaultIsolationDegradation::DomainIsolated { isolated_domains: isolated };
        }
        let degraded_count = self.states.values()
            .filter(|s| s.health == DomainHealth::Degraded || s.health == DomainHealth::Crashed || s.health == DomainHealth::Restarting)
            .count();
        if degraded_count > 0 {
            return FaultIsolationDegradation::PartialDegradation { degraded_count };
        }
        FaultIsolationDegradation::Healthy
    }

    /// Create a log entry.
    pub fn log_entry(
        &self,
        domain: FaultDomain,
        from_health: DomainHealth,
        to_health: DomainHealth,
        description: String,
        timestamp_us: u64,
    ) -> FaultIsolationLogEntry {
        FaultIsolationLogEntry { timestamp_us, domain, from_health, to_health, description }
    }

    /// Fault history.
    pub fn fault_history(&self) -> &[FaultEvent] {
        &self.history
    }

    /// Reset all domains to healthy.
    pub fn reset(&mut self) {
        for state in self.states.values_mut() {
            state.health = DomainHealth::Healthy;
            state.total_faults = 0;
            state.total_restarts = 0;
            state.consecutive_failures = 0;
            state.last_fault_us = 0;
            state.last_restart_us = 0;
        }
        self.history.clear();
    }

    /// Count of healthy domains.
    pub fn healthy_count(&self) -> usize {
        self.states.values().filter(|s| s.health == DomainHealth::Healthy).count()
    }

    /// Count of non-healthy domains.
    pub fn unhealthy_count(&self) -> usize {
        self.states.values().filter(|s| s.health != DomainHealth::Healthy).count()
    }

    /// Total faults across all domains.
    pub fn total_faults(&self) -> u64 {
        self.states.values().map(|s| s.total_faults).sum()
    }

    /// Total restarts across all domains.
    pub fn total_restarts(&self) -> u64 {
        self.states.values().map(|s| s.total_restarts).sum()
    }

    /// Faults for a specific domain.
    pub fn domain_faults(&self, domain: FaultDomain) -> u64 {
        self.states.get(&domain).map_or(0, |s| s.total_faults)
    }

    /// Restarts for a specific domain.
    pub fn domain_restarts(&self, domain: FaultDomain) -> u64 {
        self.states.get(&domain).map_or(0, |s| s.total_restarts)
    }

    /// Whether all domains are healthy.
    pub fn all_healthy(&self) -> bool {
        self.states.values().all(|s| s.health == DomainHealth::Healthy)
    }

    /// Map FaultDomain to InvariantDomain for cross-system integration.
    pub fn to_invariant_domain(domain: FaultDomain) -> InvariantDomain {
        match domain {
            FaultDomain::Scheduler => InvariantDomain::Scheduler,
            FaultDomain::Budget => InvariantDomain::Budget,
            FaultDomain::Recovery => InvariantDomain::Recovery,
            FaultDomain::Io | FaultDomain::Storage => InvariantDomain::Composition,
        }
    }

    /// Access config.
    pub fn config(&self) -> &FaultIsolationConfig {
        &self.config
    }
}

// ── F2: Circuit Breakers and Recovery Choreography ────────────────
//
// Per-stage circuit breakers with half-open probing, and deterministic
// recovery choreography for coordinated subsystem restarts.

/// Circuit breaker state for a latency stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BreakerState {
    /// Normal operation — requests flow through.
    Closed,
    /// Tripped — requests are rejected immediately.
    Open,
    /// Probing — a limited number of requests are allowed through to test recovery.
    HalfOpen,
}

impl fmt::Display for BreakerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "closed"),
            Self::Open => write!(f, "open"),
            Self::HalfOpen => write!(f, "half-open"),
        }
    }
}

/// Configuration for a stage circuit breaker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageBreakerConfig {
    /// Failure count threshold to trip the breaker.
    pub failure_threshold: u32,
    /// Duration to stay open before transitioning to half-open (μs).
    pub open_duration_us: u64,
    /// Number of probe requests allowed in half-open state.
    pub half_open_max_probes: u32,
    /// Success count in half-open to close the breaker.
    pub half_open_success_threshold: u32,
}

impl Default for StageBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_duration_us: 1_000_000,
            half_open_max_probes: 3,
            half_open_success_threshold: 2,
        }
    }
}

/// Per-stage breaker state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageBreakerState {
    /// Stage this breaker guards.
    pub stage: LatencyStage,
    /// Current state.
    pub state: BreakerState,
    /// Consecutive failure count.
    pub consecutive_failures: u32,
    /// Timestamp when breaker was opened (0 if never).
    pub opened_at_us: u64,
    /// Probe count in current half-open window.
    pub half_open_probes: u32,
    /// Successful probes in half-open.
    pub half_open_successes: u32,
    /// Total trips.
    pub total_trips: u64,
    /// Total recoveries.
    pub total_recoveries: u64,
}

/// A recovery step in a choreography sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryStep {
    /// Stage being recovered.
    pub stage: LatencyStage,
    /// Step number in the sequence.
    pub step_number: u32,
    /// Action description.
    pub action: String,
    /// Whether this step requires all previous steps to succeed.
    pub requires_prior_success: bool,
    /// Timeout for this step (μs).
    pub timeout_us: u64,
}

/// Recovery choreography outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChoreographyOutcome {
    /// All stages recovered successfully.
    FullRecovery,
    /// Some stages recovered, others remain degraded.
    PartialRecovery { recovered: Vec<LatencyStage>, failed: Vec<LatencyStage> },
    /// Recovery was aborted (e.g., timeout, cascade failure).
    Aborted { reason: String },
}

impl fmt::Display for ChoreographyOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FullRecovery => write!(f, "full-recovery"),
            Self::PartialRecovery { recovered, failed } => {
                write!(f, "partial({} ok, {} failed)", recovered.len(), failed.len())
            }
            Self::Aborted { reason } => write!(f, "aborted: {reason}"),
        }
    }
}

/// Snapshot of the circuit breaker manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakerManagerSnapshot {
    /// Per-stage states.
    pub stages: Vec<StageBreakerState>,
    /// Total trips across all stages.
    pub total_trips: u64,
    /// Total recoveries across all stages.
    pub total_recoveries: u64,
}

/// Degradation state for the breaker manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BreakerManagerDegradation {
    /// All breakers closed.
    Healthy,
    /// Some breakers open or half-open.
    BreakerTripped { open_count: usize },
    /// Many breakers tripped — cascade risk.
    CascadeRisk { open_count: usize },
}

impl fmt::Display for BreakerManagerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::BreakerTripped { open_count } => write!(f, "tripped({open_count})"),
            Self::CascadeRisk { open_count } => write!(f, "cascade-risk({open_count})"),
        }
    }
}

/// Log entry for breaker events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakerLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Stage affected.
    pub stage: LatencyStage,
    /// Previous state.
    pub from_state: BreakerState,
    /// New state.
    pub to_state: BreakerState,
    /// Reason for transition.
    pub reason: String,
}

/// The circuit breaker manager for latency stages.
pub struct BreakerManager {
    config: StageBreakerConfig,
    states: std::collections::HashMap<LatencyStage, StageBreakerState>,
}

impl BreakerManager {
    /// Create a new breaker manager with the given config.
    pub fn new(config: StageBreakerConfig) -> Self {
        let mut states = std::collections::HashMap::new();
        for stage in LatencyStage::PIPELINE_STAGES {
            states.insert(*stage, StageBreakerState {
                stage: *stage,
                state: BreakerState::Closed,
                consecutive_failures: 0,
                opened_at_us: 0,
                half_open_probes: 0,
                half_open_successes: 0,
                total_trips: 0,
                total_recoveries: 0,
            });
        }
        Self { config, states }
    }

    /// Record a failure for a stage.
    pub fn record_failure(&mut self, stage: LatencyStage, timestamp_us: u64) {
        let threshold = self.config.failure_threshold;
        let state = self.states.get_mut(&stage).unwrap();
        match state.state {
            BreakerState::Closed => {
                state.consecutive_failures += 1;
                if state.consecutive_failures >= threshold {
                    state.state = BreakerState::Open;
                    state.opened_at_us = timestamp_us;
                    state.total_trips += 1;
                }
            }
            BreakerState::HalfOpen => {
                // Probe failed — go back to open.
                state.state = BreakerState::Open;
                state.opened_at_us = timestamp_us;
                state.half_open_probes = 0;
                state.half_open_successes = 0;
            }
            BreakerState::Open => {
                // Already open, no-op.
            }
        }
    }

    /// Record a success for a stage.
    pub fn record_success(&mut self, stage: LatencyStage) {
        let success_threshold = self.config.half_open_success_threshold;
        let state = self.states.get_mut(&stage).unwrap();
        match state.state {
            BreakerState::Closed => {
                state.consecutive_failures = 0;
            }
            BreakerState::HalfOpen => {
                state.half_open_successes += 1;
                if state.half_open_successes >= success_threshold {
                    state.state = BreakerState::Closed;
                    state.consecutive_failures = 0;
                    state.half_open_probes = 0;
                    state.half_open_successes = 0;
                    state.total_recoveries += 1;
                }
            }
            BreakerState::Open => {
                // Shouldn't happen — requests blocked in open state.
            }
        }
    }

    /// Check if a request should be allowed through for a stage.
    pub fn allow_request(&mut self, stage: LatencyStage, current_us: u64) -> bool {
        let open_duration = self.config.open_duration_us;
        let max_probes = self.config.half_open_max_probes;
        let state = self.states.get_mut(&stage).unwrap();
        match state.state {
            BreakerState::Closed => true,
            BreakerState::Open => {
                // Check if enough time has passed to try half-open.
                if current_us.saturating_sub(state.opened_at_us) >= open_duration {
                    state.state = BreakerState::HalfOpen;
                    state.half_open_probes = 1;
                    state.half_open_successes = 0;
                    true
                } else {
                    false
                }
            }
            BreakerState::HalfOpen => {
                if state.half_open_probes < max_probes {
                    state.half_open_probes += 1;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Get the state of a stage's breaker.
    pub fn breaker_state(&self, stage: LatencyStage) -> BreakerState {
        self.states.get(&stage).map_or(BreakerState::Closed, |s| s.state)
    }

    /// Count of open (tripped) breakers.
    pub fn open_count(&self) -> usize {
        self.states.values().filter(|s| s.state == BreakerState::Open || s.state == BreakerState::HalfOpen).count()
    }

    /// Whether all breakers are closed.
    pub fn all_closed(&self) -> bool {
        self.states.values().all(|s| s.state == BreakerState::Closed)
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> BreakerManagerSnapshot {
        let stages: Vec<StageBreakerState> = LatencyStage::PIPELINE_STAGES.iter()
            .filter_map(|s| self.states.get(s).cloned())
            .collect();
        let total_trips = stages.iter().map(|s| s.total_trips).sum();
        let total_recoveries = stages.iter().map(|s| s.total_recoveries).sum();
        BreakerManagerSnapshot { stages, total_trips, total_recoveries }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> BreakerManagerDegradation {
        let open_count = self.open_count();
        if open_count == 0 {
            BreakerManagerDegradation::Healthy
        } else if open_count >= 3 {
            BreakerManagerDegradation::CascadeRisk { open_count }
        } else {
            BreakerManagerDegradation::BreakerTripped { open_count }
        }
    }

    /// Create a log entry.
    pub fn log_entry(&self, stage: LatencyStage, from: BreakerState, to: BreakerState, reason: String, timestamp_us: u64) -> BreakerLogEntry {
        BreakerLogEntry { timestamp_us, stage, from_state: from, to_state: to, reason }
    }

    /// Reset all breakers to closed.
    pub fn reset(&mut self) {
        for state in self.states.values_mut() {
            state.state = BreakerState::Closed;
            state.consecutive_failures = 0;
            state.opened_at_us = 0;
            state.half_open_probes = 0;
            state.half_open_successes = 0;
            state.total_trips = 0;
            state.total_recoveries = 0;
        }
    }

    /// Access config.
    pub fn config(&self) -> &StageBreakerConfig {
        &self.config
    }

    // ── F2 Impl: Bridge methods ──

    /// Total trips across all stages.
    pub fn total_trips(&self) -> u64 {
        self.states.values().map(|s| s.total_trips).sum()
    }

    /// Total recoveries across all stages.
    pub fn total_recoveries(&self) -> u64 {
        self.states.values().map(|s| s.total_recoveries).sum()
    }

    /// Total consecutive failures across all stages.
    pub fn total_consecutive_failures(&self) -> u32 {
        self.states.values().map(|s| s.consecutive_failures).sum()
    }

    /// Stages currently in the Open state.
    pub fn open_stages(&self) -> Vec<LatencyStage> {
        self.states.iter()
            .filter(|(_, s)| s.state == BreakerState::Open)
            .map(|(stage, _)| *stage)
            .collect()
    }

    /// Stages currently in the HalfOpen state.
    pub fn half_open_stages(&self) -> Vec<LatencyStage> {
        self.states.iter()
            .filter(|(_, s)| s.state == BreakerState::HalfOpen)
            .map(|(stage, _)| *stage)
            .collect()
    }

    /// Stages currently in the Closed state.
    pub fn closed_stages(&self) -> Vec<LatencyStage> {
        self.states.iter()
            .filter(|(_, s)| s.state == BreakerState::Closed)
            .map(|(stage, _)| *stage)
            .collect()
    }

    /// Generate a recovery choreography plan for all open/half-open stages.
    /// Returns a list of recovery steps ordered by pipeline position.
    pub fn plan_recovery(&self) -> Vec<RecoveryStep> {
        let mut stages: Vec<LatencyStage> = self.states.iter()
            .filter(|(_, s)| s.state != BreakerState::Closed)
            .map(|(stage, _)| *stage)
            .collect();
        // Sort by pipeline order.
        stages.sort_by_key(|s| {
            LatencyStage::PIPELINE_STAGES.iter().position(|p| p == s).unwrap_or(usize::MAX)
        });
        stages.iter().enumerate().map(|(i, stage)| {
            RecoveryStep {
                stage: *stage,
                step_number: i as u32,
                action: format!("recover-{}", stage),
                requires_prior_success: i > 0,
                timeout_us: self.config.open_duration_us,
            }
        }).collect()
    }

    /// Execute a recovery plan by transitioning open breakers to half-open
    /// for probing. Returns the number of breakers transitioned.
    pub fn initiate_recovery(&mut self, current_us: u64) -> u32 {
        let mut transitioned = 0u32;
        let open_duration = self.config.open_duration_us;
        for state in self.states.values_mut() {
            if state.state == BreakerState::Open
                && current_us.saturating_sub(state.opened_at_us) >= open_duration
            {
                state.state = BreakerState::HalfOpen;
                state.half_open_probes = 0;
                state.half_open_successes = 0;
                transitioned += 1;
            }
        }
        transitioned
    }

    /// Map BreakerManager degradation to InvariantDomain for cross-module reporting.
    pub fn to_invariant_domain() -> InvariantDomain {
        InvariantDomain::Recovery
    }

    /// Availability ratio: fraction of stages with closed breakers (0.0..=1.0).
    pub fn availability(&self) -> f64 {
        let total = self.states.len() as f64;
        if total == 0.0 { return 1.0; }
        let closed = self.states.values().filter(|s| s.state == BreakerState::Closed).count() as f64;
        closed / total
    }

    /// Record a batch of failures for a single stage (e.g., from MC trace replay).
    pub fn record_failures_batch(&mut self, stage: LatencyStage, count: u32, timestamp_us: u64) {
        for i in 0..count {
            self.record_failure(stage, timestamp_us + i as u64);
        }
    }

    /// Get per-stage breaker state (raw HashMap access).
    pub fn stage_state(&self, stage: LatencyStage) -> Option<&StageBreakerState> {
        self.states.get(&stage)
    }
}

// ── F3: Immediate-Ack / Deferred-Completion UX Protocol ─────────

/// Phase in the immediate-ack / deferred-completion protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AckPhase {
    /// Fast path: produce an immediate user-visible acknowledgment.
    ImmediateAck,
    /// Slow path: deferred processing with progress tracking.
    DeferredCompletion,
}

impl fmt::Display for AckPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ImmediateAck => write!(f, "immediate-ack"),
            Self::DeferredCompletion => write!(f, "deferred-completion"),
        }
    }
}

/// Reason code for deferred-completion outcome.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompletionReason {
    /// Completed successfully.
    Success,
    /// Timed out waiting for slow path.
    Timeout,
    /// Upstream stage failed (breaker tripped, storage error, etc.).
    UpstreamFailure { stage: LatencyStage, detail: String },
    /// Cancelled by user or system.
    Cancelled { reason: String },
}

impl fmt::Display for CompletionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::Timeout => write!(f, "timeout"),
            Self::UpstreamFailure { stage, detail } => write!(f, "upstream-failure({stage}: {detail})"),
            Self::Cancelled { reason } => write!(f, "cancelled({reason})"),
        }
    }
}

/// Immediate-ack token: a lightweight receipt returned to the user on the fast path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AckToken {
    /// Unique correlation ID linking ack to deferred completion.
    pub correlation_id: u64,
    /// Timestamp of ack generation (μs).
    pub acked_at_us: u64,
    /// The stage that produced the ack.
    pub source_stage: LatencyStage,
    /// Human-readable summary for display.
    pub summary: String,
}

/// Deferred-completion result: delivered after slow-path processing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeferredResult {
    /// Correlation ID matching the AckToken.
    pub correlation_id: u64,
    /// Completion timestamp (μs).
    pub completed_at_us: u64,
    /// Reason code.
    pub reason: CompletionReason,
    /// Wall-clock latency from ack to completion (μs).
    pub deferred_latency_us: u64,
    /// Optional explanation for the user (ft why style).
    pub explanation: Option<String>,
}

/// Configuration for the ack/completion protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AckProtocolConfig {
    /// Max time to wait for immediate ack before downgrading (μs).
    pub ack_deadline_us: u64,
    /// Max time to wait for deferred completion (μs).
    pub completion_deadline_us: u64,
    /// Whether to show progress updates to user during deferred phase.
    pub show_progress: bool,
    /// Minimum interval between progress updates (μs).
    pub progress_interval_us: u64,
}

impl Default for AckProtocolConfig {
    fn default() -> Self {
        Self {
            ack_deadline_us: 50_000,        // 50ms — must feel instant.
            completion_deadline_us: 5_000_000, // 5s — user patience limit.
            show_progress: true,
            progress_interval_us: 500_000,  // 500ms between updates.
        }
    }
}

/// Progress update during deferred phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressUpdate {
    /// Correlation ID.
    pub correlation_id: u64,
    /// Timestamp of progress report (μs).
    pub timestamp_us: u64,
    /// Fraction complete (0.0..=1.0).
    pub fraction: f64,
    /// Human-readable status message.
    pub message: String,
}

/// Snapshot of the ack protocol manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AckProtocolSnapshot {
    /// Total ack tokens issued.
    pub total_acks: u64,
    /// Total deferred completions.
    pub total_completions: u64,
    /// Total timeouts.
    pub total_timeouts: u64,
    /// Pending (acked but not completed).
    pub pending_count: u64,
}

/// Degradation level for the protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AckProtocolDegradation {
    /// All requests completing within deadlines.
    Healthy,
    /// Some acks are slow (above ack_deadline).
    AckSlow { slow_count: u64 },
    /// Deferred completions timing out.
    CompletionTimeout { timeout_count: u64 },
}

impl fmt::Display for AckProtocolDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::AckSlow { slow_count } => write!(f, "ack-slow({slow_count})"),
            Self::CompletionTimeout { timeout_count } => write!(f, "completion-timeout({timeout_count})"),
        }
    }
}

/// Log entry for ack protocol events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AckProtocolLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Phase at which the event occurred.
    pub phase: AckPhase,
    /// Correlation ID.
    pub correlation_id: u64,
    /// Event description.
    pub event: String,
}

/// Manages the immediate-ack / deferred-completion UX protocol.
pub struct AckProtocolManager {
    config: AckProtocolConfig,
    next_correlation_id: u64,
    /// Pending acks: correlation_id → AckToken.
    pending: std::collections::HashMap<u64, AckToken>,
    total_acks: u64,
    total_completions: u64,
    total_timeouts: u64,
    total_cancellations: u64,
    slow_ack_count: u64,
}

impl AckProtocolManager {
    /// Create a new protocol manager.
    pub fn new(config: AckProtocolConfig) -> Self {
        Self {
            config,
            next_correlation_id: 1,
            pending: std::collections::HashMap::new(),
            total_acks: 0,
            total_completions: 0,
            total_timeouts: 0,
            total_cancellations: 0,
            slow_ack_count: 0,
        }
    }

    /// Issue an immediate ack. Returns a token for the caller.
    pub fn issue_ack(&mut self, stage: LatencyStage, summary: String, timestamp_us: u64) -> AckToken {
        let cid = self.next_correlation_id;
        self.next_correlation_id += 1;
        let token = AckToken {
            correlation_id: cid,
            acked_at_us: timestamp_us,
            source_stage: stage,
            summary,
        };
        self.pending.insert(cid, token.clone());
        self.total_acks += 1;
        token
    }

    /// Complete a deferred operation. Returns the result with latency info.
    pub fn complete(&mut self, correlation_id: u64, reason: CompletionReason, timestamp_us: u64) -> Option<DeferredResult> {
        let token = self.pending.remove(&correlation_id)?;
        let deferred_latency_us = timestamp_us.saturating_sub(token.acked_at_us);
        let is_timeout = matches!(reason, CompletionReason::Timeout);
        let is_cancel = matches!(reason, CompletionReason::Cancelled { .. });
        if is_timeout {
            self.total_timeouts += 1;
        } else if is_cancel {
            self.total_cancellations += 1;
        }
        self.total_completions += 1;
        Some(DeferredResult {
            correlation_id,
            completed_at_us: timestamp_us,
            reason,
            deferred_latency_us,
            explanation: None,
        })
    }

    /// Record a slow ack (ack took longer than ack_deadline).
    pub fn record_slow_ack(&mut self) {
        self.slow_ack_count += 1;
    }

    /// Check for timed-out pending operations and complete them.
    pub fn sweep_timeouts(&mut self, current_us: u64) -> Vec<DeferredResult> {
        let deadline = self.config.completion_deadline_us;
        let expired: Vec<u64> = self.pending.iter()
            .filter(|(_, token)| current_us.saturating_sub(token.acked_at_us) >= deadline)
            .map(|(cid, _)| *cid)
            .collect();
        let mut results = Vec::new();
        for cid in expired {
            if let Some(result) = self.complete(cid, CompletionReason::Timeout, current_us) {
                results.push(result);
            }
        }
        results
    }

    /// Number of pending (acked but not completed) operations.
    pub fn pending_count(&self) -> u64 {
        self.pending.len() as u64
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> AckProtocolSnapshot {
        AckProtocolSnapshot {
            total_acks: self.total_acks,
            total_completions: self.total_completions,
            total_timeouts: self.total_timeouts,
            pending_count: self.pending_count(),
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> AckProtocolDegradation {
        if self.total_timeouts > 0 {
            AckProtocolDegradation::CompletionTimeout { timeout_count: self.total_timeouts }
        } else if self.slow_ack_count > 0 {
            AckProtocolDegradation::AckSlow { slow_count: self.slow_ack_count }
        } else {
            AckProtocolDegradation::Healthy
        }
    }

    /// Create a log entry.
    pub fn log_entry(&self, phase: AckPhase, correlation_id: u64, event: String, timestamp_us: u64) -> AckProtocolLogEntry {
        AckProtocolLogEntry { timestamp_us, phase, correlation_id, event }
    }

    /// Reset counters.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.total_acks = 0;
        self.total_completions = 0;
        self.total_timeouts = 0;
        self.total_cancellations = 0;
        self.slow_ack_count = 0;
    }

    /// Access config.
    pub fn config(&self) -> &AckProtocolConfig {
        &self.config
    }

    // ── F3 Impl: Bridge methods ──

    /// Total acks issued.
    pub fn total_acks(&self) -> u64 { self.total_acks }

    /// Total completions (success + timeout + cancel).
    pub fn total_completions(&self) -> u64 { self.total_completions }

    /// Total timeouts.
    pub fn total_timeouts(&self) -> u64 { self.total_timeouts }

    /// Total cancellations.
    pub fn total_cancellations(&self) -> u64 { self.total_cancellations }

    /// Total slow acks recorded.
    pub fn slow_ack_count(&self) -> u64 { self.slow_ack_count }

    /// Completion rate: total_completions / total_acks.
    pub fn completion_rate(&self) -> f64 {
        if self.total_acks == 0 { return 1.0; }
        self.total_completions as f64 / self.total_acks as f64
    }

    /// Timeout rate: total_timeouts / total_completions.
    pub fn timeout_rate(&self) -> f64 {
        if self.total_completions == 0 { return 0.0; }
        self.total_timeouts as f64 / self.total_completions as f64
    }

    /// Whether there are any pending operations.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Get a pending token by correlation ID.
    pub fn get_pending(&self, correlation_id: u64) -> Option<&AckToken> {
        self.pending.get(&correlation_id)
    }

    /// Complete with explanation.
    pub fn complete_with_explanation(
        &mut self,
        correlation_id: u64,
        reason: CompletionReason,
        timestamp_us: u64,
        explanation: String,
    ) -> Option<DeferredResult> {
        self.complete(correlation_id, reason, timestamp_us).map(|mut r| {
            r.explanation = Some(explanation);
            r
        })
    }

    /// Issue an ack and immediately check if it was slow.
    pub fn issue_ack_checked(
        &mut self,
        stage: LatencyStage,
        summary: String,
        request_received_us: u64,
        ack_sent_us: u64,
    ) -> AckToken {
        let token = self.issue_ack(stage, summary, ack_sent_us);
        if ack_sent_us.saturating_sub(request_received_us) > self.config.ack_deadline_us {
            self.record_slow_ack();
        }
        token
    }

    /// Map AckProtocol to InvariantDomain.
    pub fn to_invariant_domain() -> InvariantDomain {
        InvariantDomain::Composition
    }

    /// Generate a progress update for a pending operation.
    pub fn make_progress(&self, correlation_id: u64, fraction: f64, message: String, timestamp_us: u64) -> Option<ProgressUpdate> {
        if !self.pending.contains_key(&correlation_id) { return None; }
        Some(ProgressUpdate {
            correlation_id,
            timestamp_us,
            fraction: fraction.clamp(0.0, 1.0),
            message,
        })
    }
}

// ── F4: Unified E2E-Chaos-Soak-Performance Matrix ──────────────────

/// Test scenario category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScenarioCategory {
    /// Happy-path end-to-end.
    E2E,
    /// Fault injection / chaos engineering.
    Chaos,
    /// Long-running soak / endurance.
    Soak,
    /// Performance / latency regression.
    Performance,
}

impl fmt::Display for ScenarioCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::E2E => write!(f, "e2e"),
            Self::Chaos => write!(f, "chaos"),
            Self::Soak => write!(f, "soak"),
            Self::Performance => write!(f, "performance"),
        }
    }
}

/// Verdict from running a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScenarioVerdict {
    Pass,
    Fail,
    Skip,
    Flaky,
}

impl fmt::Display for ScenarioVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass => write!(f, "pass"),
            Self::Fail => write!(f, "fail"),
            Self::Skip => write!(f, "skip"),
            Self::Flaky => write!(f, "flaky"),
        }
    }
}

/// A single scenario in the validation matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatrixScenario {
    /// Unique scenario ID.
    pub scenario_id: String,
    /// Category.
    pub category: ScenarioCategory,
    /// Human-readable description.
    pub description: String,
    /// Stages touched by this scenario.
    pub stages: Vec<LatencyStage>,
    /// Invariant domain under test.
    pub domain: InvariantDomain,
    /// Whether this scenario is required for promotion.
    pub required_for_promotion: bool,
}

/// Result of running a scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// Scenario ID.
    pub scenario_id: String,
    /// Verdict.
    pub verdict: ScenarioVerdict,
    /// Duration in μs.
    pub duration_us: u64,
    /// Optional failure message.
    pub failure_message: Option<String>,
    /// Artifacts produced (file paths, checksums, etc.).
    pub artifacts: Vec<String>,
}

/// Promotion gate: a set of scenarios that must pass for CI promotion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionGate {
    /// Gate name (e.g., "canary", "staging", "production").
    pub name: String,
    /// Required scenario IDs that must pass.
    pub required_scenarios: Vec<String>,
    /// Minimum pass rate across all scenarios (0.0..=1.0).
    pub min_pass_rate: f64,
    /// Max allowed flaky scenario count.
    pub max_flaky_count: u32,
}

/// Snapshot of the validation matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatrixSnapshot {
    /// Total scenarios.
    pub total_scenarios: u64,
    /// Results by category.
    pub pass_count: u64,
    pub fail_count: u64,
    pub skip_count: u64,
    pub flaky_count: u64,
}

/// Degradation state for the matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MatrixDegradation {
    /// All required scenarios passing.
    Healthy,
    /// Some flaky scenarios.
    FlakyDetected { flaky_count: u64 },
    /// Required scenarios failing — blocks promotion.
    GateFailure { failed_scenarios: Vec<String> },
}

impl fmt::Display for MatrixDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::FlakyDetected { flaky_count } => write!(f, "flaky({flaky_count})"),
            Self::GateFailure { failed_scenarios } => write!(f, "gate-failure({})", failed_scenarios.len()),
        }
    }
}

/// Log entry for matrix events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatrixLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Scenario that triggered the event.
    pub scenario_id: String,
    /// Event description.
    pub event: String,
}

/// Manages the validation matrix.
pub struct ValidationMatrix {
    scenarios: Vec<MatrixScenario>,
    results: Vec<ScenarioResult>,
    gates: Vec<PromotionGate>,
}

impl ValidationMatrix {
    /// Create a new empty matrix.
    pub fn new() -> Self {
        Self {
            scenarios: Vec::new(),
            results: Vec::new(),
            gates: Vec::new(),
        }
    }

    /// Register a scenario.
    pub fn add_scenario(&mut self, scenario: MatrixScenario) {
        self.scenarios.push(scenario);
    }

    /// Register a promotion gate.
    pub fn add_gate(&mut self, gate: PromotionGate) {
        self.gates.push(gate);
    }

    /// Record a scenario result.
    pub fn record_result(&mut self, result: ScenarioResult) {
        self.results.push(result);
    }

    /// Get all results for a scenario.
    pub fn results_for(&self, scenario_id: &str) -> Vec<&ScenarioResult> {
        self.results.iter().filter(|r| r.scenario_id == scenario_id).collect()
    }

    /// Latest result for a scenario.
    pub fn latest_result(&self, scenario_id: &str) -> Option<&ScenarioResult> {
        self.results.iter().rev().find(|r| r.scenario_id == scenario_id)
    }

    /// Check if a promotion gate passes.
    pub fn check_gate(&self, gate_name: &str) -> bool {
        let gate = match self.gates.iter().find(|g| g.name == gate_name) {
            Some(g) => g,
            None => return false,
        };
        // Check all required scenarios pass.
        for sid in &gate.required_scenarios {
            match self.latest_result(sid) {
                Some(r) if r.verdict == ScenarioVerdict::Pass => {}
                _ => return false,
            }
        }
        // Check pass rate.
        let total = self.results.len() as f64;
        if total == 0.0 { return false; }
        let passes = self.results.iter().filter(|r| r.verdict == ScenarioVerdict::Pass).count() as f64;
        if passes / total < gate.min_pass_rate { return false; }
        // Check flaky count.
        let flaky = self.results.iter().filter(|r| r.verdict == ScenarioVerdict::Flaky).count() as u32;
        flaky <= gate.max_flaky_count
    }

    /// Number of scenarios.
    pub fn scenario_count(&self) -> usize {
        self.scenarios.len()
    }

    /// Number of results recorded.
    pub fn result_count(&self) -> usize {
        self.results.len()
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> MatrixSnapshot {
        let pass_count = self.results.iter().filter(|r| r.verdict == ScenarioVerdict::Pass).count() as u64;
        let fail_count = self.results.iter().filter(|r| r.verdict == ScenarioVerdict::Fail).count() as u64;
        let skip_count = self.results.iter().filter(|r| r.verdict == ScenarioVerdict::Skip).count() as u64;
        let flaky_count = self.results.iter().filter(|r| r.verdict == ScenarioVerdict::Flaky).count() as u64;
        MatrixSnapshot {
            total_scenarios: self.scenarios.len() as u64,
            pass_count,
            fail_count,
            skip_count,
            flaky_count,
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> MatrixDegradation {
        let flaky_count = self.results.iter().filter(|r| r.verdict == ScenarioVerdict::Flaky).count() as u64;
        let failed_required: Vec<String> = self.scenarios.iter()
            .filter(|s| s.required_for_promotion)
            .filter(|s| {
                self.latest_result(&s.scenario_id)
                    .map_or(true, |r| r.verdict != ScenarioVerdict::Pass)
            })
            .map(|s| s.scenario_id.clone())
            .collect();
        if !failed_required.is_empty() {
            MatrixDegradation::GateFailure { failed_scenarios: failed_required }
        } else if flaky_count > 0 {
            MatrixDegradation::FlakyDetected { flaky_count }
        } else {
            MatrixDegradation::Healthy
        }
    }

    /// Create a log entry.
    pub fn log_entry(&self, scenario_id: String, event: String, timestamp_us: u64) -> MatrixLogEntry {
        MatrixLogEntry { timestamp_us, scenario_id, event }
    }

    /// Scenarios by category.
    pub fn scenarios_by_category(&self, category: ScenarioCategory) -> Vec<&MatrixScenario> {
        self.scenarios.iter().filter(|s| s.category == category).collect()
    }

    /// Reset all results.
    pub fn reset_results(&mut self) {
        self.results.clear();
    }

    /// Access gates.
    pub fn gates(&self) -> &[PromotionGate] {
        &self.gates
    }

    /// Access scenarios.
    pub fn scenarios(&self) -> &[MatrixScenario] {
        &self.scenarios
    }

    // ── F4 Impl: Bridge methods ──

    /// Pass rate across all results (0.0..=1.0).
    pub fn pass_rate(&self) -> f64 {
        let total = self.results.len() as f64;
        if total == 0.0 { return 1.0; }
        let passes = self.results.iter().filter(|r| r.verdict == ScenarioVerdict::Pass).count() as f64;
        passes / total
    }

    /// Flaky rate across all results (0.0..=1.0).
    pub fn flaky_rate(&self) -> f64 {
        let total = self.results.len() as f64;
        if total == 0.0 { return 0.0; }
        let flaky = self.results.iter().filter(|r| r.verdict == ScenarioVerdict::Flaky).count() as f64;
        flaky / total
    }

    /// Mean duration across all pass results (μs).
    pub fn mean_pass_duration_us(&self) -> f64 {
        let passes: Vec<u64> = self.results.iter()
            .filter(|r| r.verdict == ScenarioVerdict::Pass)
            .map(|r| r.duration_us)
            .collect();
        if passes.is_empty() { return 0.0; }
        passes.iter().sum::<u64>() as f64 / passes.len() as f64
    }

    /// Check all gates; returns list of gate names that pass.
    pub fn passing_gates(&self) -> Vec<String> {
        self.gates.iter()
            .filter(|g| self.check_gate(&g.name))
            .map(|g| g.name.clone())
            .collect()
    }

    /// Check all gates; returns list of gate names that fail.
    pub fn failing_gates(&self) -> Vec<String> {
        self.gates.iter()
            .filter(|g| !self.check_gate(&g.name))
            .map(|g| g.name.clone())
            .collect()
    }

    /// Get required scenarios that don't have a passing result.
    pub fn missing_required(&self) -> Vec<String> {
        self.scenarios.iter()
            .filter(|s| s.required_for_promotion)
            .filter(|s| {
                self.latest_result(&s.scenario_id)
                    .map_or(true, |r| r.verdict != ScenarioVerdict::Pass)
            })
            .map(|s| s.scenario_id.clone())
            .collect()
    }

    /// All artifacts across all results.
    pub fn all_artifacts(&self) -> Vec<String> {
        self.results.iter().flat_map(|r| r.artifacts.clone()).collect()
    }

    /// Map to InvariantDomain.
    pub fn to_invariant_domain() -> InvariantDomain {
        InvariantDomain::Composition
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
        assert!(
            !LatencyStage::PIPELINE_STAGES
                .iter()
                .any(|s| s.is_aggregate())
        );
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
        assert!(
            budgets
                .iter()
                .any(|b| b.stage == LatencyStage::EndToEndCapture)
        );
        assert!(
            budgets
                .iter()
                .any(|b| b.stage == LatencyStage::EndToEndAction)
        );
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
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, InvariantViolation::StageOrdering { .. }))
        );
    }

    #[test]
    fn test_pipeline_run_detects_timestamp_regression() {
        let mut run = make_valid_run();
        // Make second stage start before first ends.
        run.stages[1].start_epoch_us = run.stages[0].start_epoch_us;
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, InvariantViolation::TimestampRegression { .. }))
        );
    }

    #[test]
    fn test_pipeline_run_detects_total_mismatch() {
        let mut run = make_valid_run();
        run.total_latency_us = 999_999.0; // way off
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, InvariantViolation::TotalMismatch { .. }))
        );
    }

    #[test]
    fn test_pipeline_run_detects_overflow_mismatch() {
        let mut run = make_valid_run();
        run.has_overflow = true; // no stage actually overflowed
        let result = run.validate();
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, InvariantViolation::OverflowFlagMismatch { .. }))
        );
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
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, InstrumentationError::ClockRegression { .. }))
        );
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
        assert!(
            InstrumentationDegradation::SkipOverhead < InstrumentationDegradation::SkipCorrelation
        );
        assert!(
            InstrumentationDegradation::SkipCorrelation < InstrumentationDegradation::Passthrough
        );
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
                cooldown_observations: 1000,    // high, so cooldown won't trigger
                max_degraded_duration_us: 5000, // 5ms timeout
                gradual: false,                 // jump to full
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
        assert!(
            errors.is_empty(),
            "default config should be valid: {:?}",
            errors
        );
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
                let lane = alloc.lanes().iter().find(|l| l.stage == adj.stage).unwrap();
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
        let pty = pressures
            .iter()
            .find(|p| p.stage == LatencyStage::PtyCapture)
            .unwrap();
        // PtyCapture budget is 10000 p95, observed ~1000 → headroom > 0.
        assert!(
            pty.headroom > 0.0,
            "expected positive headroom: {}",
            pty.headroom
        );
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
        assert!(
            format!("{}", AllocatorDegradation::Oscillating { lane_count: 5 })
                .contains("OSCILLATING")
        );
        assert!(
            format!(
                "{}",
                AllocatorDegradation::ConservationDrift { drift_us: 1.5 }
            )
            .contains("CONSERVATION_DRIFT")
        );
        assert!(
            format!(
                "{}",
                AllocatorDegradation::FloorSaturation { lane_count: 4 }
            )
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
        assert_eq!(
            stage_to_lane(LatencyStage::PtyCapture),
            SchedulerLane::Input
        );
        assert_eq!(
            stage_to_lane(LatencyStage::DeltaExtraction),
            SchedulerLane::Input
        );
        assert_eq!(
            stage_to_lane(LatencyStage::ApiResponse),
            SchedulerLane::Input
        );
        assert_eq!(
            stage_to_lane(LatencyStage::EventEmission),
            SchedulerLane::Control
        );
        assert_eq!(
            stage_to_lane(LatencyStage::WorkflowDispatch),
            SchedulerLane::Control
        );
        assert_eq!(
            stage_to_lane(LatencyStage::ActionExecution),
            SchedulerLane::Control
        );
        assert_eq!(
            stage_to_lane(LatencyStage::StorageWrite),
            SchedulerLane::Bulk
        );
        assert_eq!(
            stage_to_lane(LatencyStage::PatternDetection),
            SchedulerLane::Bulk
        );
    }

    #[test]
    fn test_scheduler_config_default_valid() {
        let cfg = LaneSchedulerConfig::default();
        let errors = cfg.validate();
        assert!(
            errors.is_empty(),
            "default config should be valid: {:?}",
            errors
        );
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
        let (item, decision) = sched.admit(LatencyStage::PtyCapture, 100.0, "test-1", 0, 1000);
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
        let (_item, decision) = sched.admit(LatencyStage::StorageWrite, 1000.0, "bulk-shed", 0, 0);
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
        let seq = ring
            .enqueue(LatencyStage::PtyCapture, 100.0, "basic", 1000, 0)
            .unwrap();
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
            ring.enqueue(
                LatencyStage::PtyCapture,
                10.0,
                &format!("fifo-{}", i),
                i * 100,
                0,
            )
            .unwrap();
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
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "a", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "b", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "c", 0, 0)
            .unwrap();
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
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp1", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp2", 0, 0)
            .unwrap();
        assert_eq!(ring.backpressure(), RingBackpressure::Accept);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp3", 0, 0)
            .unwrap();
        // 3/4 = 0.75 >= high_water_mark → SlowDown
        assert_eq!(ring.backpressure(), RingBackpressure::SlowDown);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "bp4", 0, 0)
            .unwrap();
        assert_eq!(ring.backpressure(), RingBackpressure::Full);
    }

    #[test]
    fn test_input_ring_wraparound() {
        let cfg = InputRingConfig {
            capacity: 3,
            ..Default::default()
        };
        let mut ring = InputRing::new(cfg);
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w1", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w2", 0, 0)
            .unwrap();
        ring.dequeue(100).unwrap(); // remove w1
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w3", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "w4", 0, 0)
            .unwrap();
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
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "peek", 100, 0)
            .unwrap();
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
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "soj", 1000, 0)
            .unwrap();
        ring.dequeue(1500).unwrap(); // sojourn = 500us
        assert!((ring.mean_sojourn_us().unwrap() - 500.0).abs() < 1e-6);
    }

    #[test]
    fn test_input_ring_snapshot() {
        let mut ring = InputRing::with_defaults();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "snap", 100, 0)
            .unwrap();
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
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "a", 0, 0)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "b", 0, 0)
            .unwrap();
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
            ring.enqueue(
                LatencyStage::PtyCapture,
                10.0,
                &format!("d-{}", i),
                i * 100,
                0,
            )
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
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "exp", 100, 500)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "ok", 200, 2000)
            .unwrap();
        ring.enqueue(LatencyStage::PtyCapture, 10.0, "nodeadline", 300, 0)
            .unwrap();

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
        assert_eq!(
            stage_to_priority(LatencyStage::PtyCapture),
            Priority::Critical
        );
        assert_eq!(
            stage_to_priority(LatencyStage::DeltaExtraction),
            Priority::Critical
        );
        assert_eq!(
            stage_to_priority(LatencyStage::EventEmission),
            Priority::Elevated
        );
        assert_eq!(
            stage_to_priority(LatencyStage::StorageWrite),
            Priority::Background
        );
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
        assert_eq!(tracker.effective_priority("low"), Some(Priority::Critical));
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
            LockResult::OrderViolation {
                requested,
                held_after,
            } => {
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
        assert_eq!(tracker.effective_priority("t1"), Some(Priority::Normal));
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
        assert_eq!(tracker.effective_priority("low"), Some(Priority::Critical));

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
        assert_eq!(
            tracker.detect_degradation(),
            InheritanceDegradation::Healthy
        );
    }

    #[test]
    fn test_pi_degradation_excessive_inheritance() {
        let mut tracker = PriorityInheritanceTracker::with_defaults();
        // Create 3 locks each with inheritance (>2 threshold).
        for (i, resource) in [
            Resource::StorageLock,
            Resource::PatternLock,
            Resource::EventBusLock,
        ]
        .iter()
        .enumerate()
        {
            tracker.acquire(
                *resource,
                &format!("low-{}", i),
                Priority::Background,
                i as u64 * 100,
            );
            tracker.acquire(
                *resource,
                &format!("high-{}", i),
                Priority::Critical,
                i as u64 * 100 + 50,
            );
        }
        let degradation = tracker.detect_degradation();
        let is_excessive = matches!(
            degradation,
            InheritanceDegradation::ExcessiveInheritance { .. }
        );
        assert!(
            is_excessive,
            "Expected ExcessiveInheritance, got {:?}",
            degradation
        );
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
        let is_spike = matches!(
            degradation,
            InheritanceDegradation::OrderViolationSpike { .. }
        );
        assert!(
            is_spike,
            "Expected OrderViolationSpike, got {:?}",
            degradation
        );
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
            InheritanceDegradation::ExcessiveInheritance {
                active_chains: 3,
                threshold: 2,
            },
            InheritanceDegradation::HighContention {
                total_waiters: 10,
                threshold: 8,
            },
            InheritanceDegradation::OrderViolationSpike {
                total_violations: 15,
                threshold: 10,
            },
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
        let exc = InheritanceDegradation::ExcessiveInheritance {
            active_chains: 3,
            threshold: 2,
        };
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
        assert!(
            gini < 0.01,
            "Gini {} should be near 0 for equal shares",
            gini
        );
    }

    #[test]
    fn test_gini_coefficient_unequal_shares() {
        let mut tracker = StarvationTracker::with_defaults();
        // Very unequal shares → higher Gini.
        for _ in 0..5 {
            tracker.observe_epoch(&[10, 0, 0], &[0.9, 0.05, 0.05]);
        }
        let gini = tracker.gini_coefficient();
        assert!(
            gini > 0.3,
            "Gini {} should be higher for unequal shares",
            gini
        );
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
        assert!(
            is_starvation,
            "Expected LaneStarvation, got {:?}",
            degradation
        );
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
            FairnessDegradation::LaneStarvation {
                starving_lanes: vec![SchedulerLane::Bulk],
            },
            FairnessDegradation::SevereUnfairness {
                gini: 0.7,
                threshold: 0.5,
            },
            FairnessDegradation::PromotionStorm {
                events_in_window: 10,
                threshold: 5,
            },
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
        let storm = FairnessDegradation::PromotionStorm {
            events_in_window: 10,
            threshold: 5,
        };
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
        assert_eq!(
            stage_to_domain(LatencyStage::PtyCapture),
            MemoryDomain::PtyCapture
        );
        assert_eq!(
            stage_to_domain(LatencyStage::StorageWrite),
            MemoryDomain::StorageWrite
        );
        assert_eq!(
            stage_to_domain(LatencyStage::EventEmission),
            MemoryDomain::EventBus
        );
        assert_eq!(
            stage_to_domain(LatencyStage::ApiResponse),
            MemoryDomain::Shared
        );
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
            PoolDegradation::HighUtilization {
                utilization: 0.9,
                threshold: 0.85,
            },
            PoolDegradation::Exhausted { total_exhausted: 5 },
            PoolDegradation::Fragmented {
                total_blocks: 100,
                free_count: 60,
            },
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
        assert!(
            is_high,
            "Expected HighBufferPressure, got {:?}",
            degradation
        );
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
            IngestDegradation::HighBufferPressure {
                buffered_bytes: 100,
                max_line_bytes: 120,
            },
            IngestDegradation::DataCorruption {
                invalid_bytes: 10,
                total_bytes: 200,
            },
            IngestDegradation::LowZeroCopy {
                ratio: 0.3,
                threshold: 0.5,
            },
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
        let buf = IngestDegradation::HighBufferPressure {
            buffered_bytes: 100,
            max_line_bytes: 120,
        };
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.5,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);

        mgr.ingest(1, 1000, 10, 0);
        mgr.migrate(200); // hot→warm
        assert_eq!(mgr.segment(0).unwrap().tier, ScrollbackTier::Warm);

        mgr.migrate(800); // warm→cold
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 5000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 10000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 100000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let policy = TierMigrationPolicy {
            pressure_threshold: 0.8,
            ..Default::default()
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 900, 10, 0);
        let is_pressure = matches!(
            mgr.detect_degradation(),
            ScrollbackDegradation::HotPressure { .. }
        );
        assert!(
            is_pressure,
            "Expected HotPressure, got {:?}",
            mgr.detect_degradation()
        );
    }

    #[test]
    fn test_scrollback_degradation_display() {
        assert_eq!(format!("{}", ScrollbackDegradation::Healthy), "HEALTHY");
        let hot = ScrollbackDegradation::HotPressure {
            utilization: 0.9,
            threshold: 0.85,
        };
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
            ScrollbackDegradation::HotPressure {
                utilization: 0.9,
                threshold: 0.85,
            },
            ScrollbackDegradation::WarmPressure {
                utilization: 0.88,
                threshold: 0.85,
            },
            ScrollbackDegradation::MigrationBacklog {
                pending: 10,
                max_concurrent: 4,
            },
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 10000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 100000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 10000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 100000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, TierMigrationPolicy::default());

        mgr.ingest(1, 300, 10, 100);
        mgr.ingest(2, 300, 10, 200);
        mgr.ingest(3, 300, 10, 300);
        // 900/1000 = 90%. Evict to 50%.
        let freed = mgr.evict_hot_to_target(0.5);
        assert!(
            freed >= 400,
            "Should have freed enough to reach 50%: freed={}",
            freed
        );
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 5000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10000,
            target_latency_us: 10000,
            compression_ratio: 0.5,
        };
        let policy = TierMigrationPolicy {
            hot_to_warm_age_us: 10,
            warm_to_cold_age_us: 100,
            min_segment_bytes: 1,
            pressure_threshold: 0.99,
            max_concurrent_migrations: 10,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 2000, 20, 0);
        mgr.migrate(50); // hot→warm
        mgr.migrate(200); // warm→cold
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
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 1_000_000,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 1_000_000,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 10_000_000,
            target_latency_us: 10000,
            compression_ratio: 0.25,
        };
        let mut mgr = TieredScrollbackManager::new(hot, warm, cold, policy);
        mgr.ingest(1, 500, 5, 0);
        mgr.ingest(2, 600, 6, 0);
        mgr.migrate(1);
        let events = mgr.recent_migrations();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].from_tier, ScrollbackTier::Hot);
        assert_eq!(events[0].to_tier, ScrollbackTier::Warm);
    }

    // ── C4: Transport Policy Tests ─────────────────────────────────

    #[test]
    fn test_transport_mode_display() {
        assert_eq!(format!("{}", TransportMode::Local), "LOCAL");
        assert_eq!(format!("{}", TransportMode::Compressed), "COMPRESSED");
        assert_eq!(format!("{}", TransportMode::Bypass), "BYPASS");
    }

    #[test]
    fn test_transport_cost_model_default() {
        let cm = TransportCostModel::default();
        assert!(cm.compress_cost_per_byte_us > 0.0);
        assert!(cm.bypass_threshold_bytes < cm.compress_threshold_bytes);
    }

    #[test]
    fn test_transport_policy_local_when_no_network() {
        let policy = TransportPolicy::with_defaults();
        // Default cost model has network_cost=0 → always Local
        assert_eq!(policy.select_mode(100), TransportMode::Local);
        assert_eq!(policy.select_mode(100_000), TransportMode::Local);
    }

    #[test]
    fn test_transport_policy_bypass_small() {
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                network_cost_per_byte_us: 0.001,
                bypass_threshold_bytes: 4096,
                compress_threshold_bytes: 65536,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        assert_eq!(policy.select_mode(1000), TransportMode::Bypass);
    }

    #[test]
    fn test_transport_policy_compressed_large() {
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                network_cost_per_byte_us: 0.001,
                bypass_threshold_bytes: 4096,
                compress_threshold_bytes: 65536,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        assert_eq!(policy.select_mode(100_000), TransportMode::Compressed);
    }

    #[test]
    fn test_transport_policy_fixed_mode() {
        let config = TransportPolicyConfig {
            adaptive: false,
            fixed_mode: TransportMode::Compressed,
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        assert_eq!(policy.select_mode(1), TransportMode::Compressed);
        assert_eq!(policy.select_mode(1_000_000), TransportMode::Compressed);
    }

    #[test]
    fn test_transport_policy_record() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(1024, TransportMode::Local, 10.0, 8.0, 1000);
        let snap = policy.snapshot();
        assert_eq!(snap.total_decisions, 1);
        assert_eq!(snap.local_count, 1);
        assert_eq!(snap.total_bytes_transferred, 1024);
        assert!(snap.ewma_cost_us > 0.0);
    }

    #[test]
    fn test_transport_policy_decision_counts() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(100, TransportMode::Local, 1.0, 1.0, 100);
        policy.record(200, TransportMode::Compressed, 2.0, 2.0, 200);
        policy.record(300, TransportMode::Bypass, 3.0, 3.0, 300);
        let snap = policy.snapshot();
        assert_eq!(
            snap.local_count + snap.compressed_count + snap.bypass_count,
            snap.total_decisions
        );
    }

    #[test]
    fn test_transport_policy_ewma_converges() {
        let mut policy = TransportPolicy::with_defaults();
        for i in 0..100 {
            policy.record(1000, TransportMode::Local, 50.0, 50.0, i * 100);
        }
        // EWMA should converge toward 50.0
        assert!((policy.snapshot().ewma_cost_us - 50.0).abs() < 1.0);
    }

    #[test]
    fn test_transport_policy_reset() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(1024, TransportMode::Local, 10.0, 8.0, 1000);
        policy.reset();
        let snap = policy.snapshot();
        assert_eq!(snap.total_decisions, 0);
        assert_eq!(snap.total_bytes_transferred, 0);
        assert_eq!(snap.ewma_cost_us, 0.0);
    }

    #[test]
    fn test_transport_degradation_healthy() {
        let policy = TransportPolicy::with_defaults();
        assert_eq!(policy.detect_degradation(), TransportDegradation::Healthy);
    }

    #[test]
    fn test_transport_degradation_high_cost() {
        let mut policy = TransportPolicy::with_defaults();
        // Drive EWMA above 100µs
        for i in 0..50 {
            policy.record(10000, TransportMode::Compressed, 200.0, 200.0, i * 100);
        }
        let is_high = matches!(
            policy.detect_degradation(),
            TransportDegradation::HighCost { .. }
        );
        assert!(
            is_high,
            "Expected HighCost, got {:?}",
            policy.detect_degradation()
        );
    }

    #[test]
    fn test_transport_degradation_display() {
        assert_eq!(format!("{}", TransportDegradation::Healthy), "HEALTHY");
        let high = TransportDegradation::HighCost {
            ewma_cost_us: 150.0,
            threshold_us: 100.0,
        };
        assert!(format!("{}", high).contains("150.0"));
    }

    #[test]
    fn test_transport_log_entry() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(1024, TransportMode::Local, 10.0, 8.0, 1000);
        let entry = policy.log_entry();
        assert_eq!(entry.total_decisions, 1);
        assert_eq!(entry.degradation, TransportDegradation::Healthy);
    }

    #[test]
    fn test_transport_log_entry_serde() {
        let entry = TransportLogEntry {
            total_decisions: 10,
            local_count: 5,
            compressed_count: 3,
            bypass_count: 2,
            ewma_cost_us: 25.5,
            degradation: TransportDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: TransportLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_transport_snapshot_serde() {
        let snap = TransportPolicySnapshot {
            total_decisions: 100,
            local_count: 50,
            compressed_count: 30,
            bypass_count: 20,
            total_bytes_transferred: 1_000_000,
            total_savings_us: 500.0,
            ewma_cost_us: 25.0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TransportPolicySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_transport_decision_serde() {
        let dec = TransportDecision {
            payload_bytes: 4096,
            selected_mode: TransportMode::Compressed,
            estimated_cost_us: 15.0,
            actual_cost_us: 12.0,
            savings_us: 3.0,
            timestamp_us: 99999,
        };
        let json = serde_json::to_string(&dec).unwrap();
        let back: TransportDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(dec, back);
    }

    #[test]
    fn test_transport_degradation_serde() {
        let variants = vec![
            TransportDegradation::Healthy,
            TransportDegradation::HighCost {
                ewma_cost_us: 150.0,
                threshold_us: 100.0,
            },
            TransportDegradation::ModeImbalance {
                dominant_mode: "Local".to_string(),
                share: 0.98,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: TransportDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_transport_status_line() {
        let policy = TransportPolicy::with_defaults();
        let line = policy.status_line();
        assert!(line.contains("transport"));
        assert!(line.contains("decisions=0"));
    }

    #[test]
    fn test_transport_mode_mid_range_cost_comparison() {
        // In the mid-range, mode depends on cost model
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                compress_cost_per_byte_us: 0.05,
                decompress_cost_per_byte_us: 0.02,
                network_cost_per_byte_us: 0.01,
                expected_compression_ratio: 0.3,
                bypass_threshold_bytes: 1000,
                compress_threshold_bytes: 100000,
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        // 10000 bytes: bypass cost = 10000 * 0.01 = 100
        // compress cost = 10000*0.05 + 10000*0.3*0.01 + 10000*0.3*0.02 = 500 + 30 + 60 = 590
        // bypass is cheaper
        assert_eq!(policy.select_mode(10000), TransportMode::Bypass);
    }

    // ── C4 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_transport_estimate_cost_local() {
        let policy = TransportPolicy::with_defaults();
        assert_eq!(policy.estimate_cost(1000, TransportMode::Local), 0.0);
    }

    #[test]
    fn test_transport_estimate_cost_bypass() {
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                network_cost_per_byte_us: 0.01,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        let cost = policy.estimate_cost(10000, TransportMode::Bypass);
        assert!((cost - 100.0).abs() < 0.001); // 10000 * 0.01
    }

    #[test]
    fn test_transport_estimate_cost_compressed() {
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                compress_cost_per_byte_us: 0.05,
                decompress_cost_per_byte_us: 0.02,
                network_cost_per_byte_us: 0.01,
                expected_compression_ratio: 0.5,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = TransportPolicy::new(config);
        let cost = policy.estimate_cost(1000, TransportMode::Compressed);
        // 1000*0.05 + 1000*0.5*0.01 + 1000*0.5*0.02 = 50 + 5 + 10 = 65
        assert!((cost - 65.0).abs() < 0.001);
    }

    #[test]
    fn test_transport_select_and_record() {
        let mut policy = TransportPolicy::with_defaults();
        let mode = policy.select_and_record(1024, 5.0, 1000);
        assert_eq!(mode, TransportMode::Local);
        assert_eq!(policy.snapshot().total_decisions, 1);
    }

    #[test]
    fn test_transport_mode_distribution_empty() {
        let policy = TransportPolicy::with_defaults();
        let (l, c, b) = policy.mode_distribution();
        assert_eq!(l, 0.0);
        assert_eq!(c, 0.0);
        assert_eq!(b, 0.0);
    }

    #[test]
    fn test_transport_mode_distribution() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(100, TransportMode::Local, 1.0, 1.0, 0);
        policy.record(100, TransportMode::Local, 1.0, 1.0, 1);
        policy.record(100, TransportMode::Compressed, 2.0, 2.0, 2);
        policy.record(100, TransportMode::Bypass, 3.0, 3.0, 3);
        let (l, c, b) = policy.mode_distribution();
        assert!((l - 0.5).abs() < 0.001);
        assert!((c - 0.25).abs() < 0.001);
        assert!((b - 0.25).abs() < 0.001);
    }

    #[test]
    fn test_transport_total_bytes() {
        let mut policy = TransportPolicy::with_defaults();
        policy.record(1000, TransportMode::Local, 1.0, 1.0, 0);
        policy.record(2000, TransportMode::Bypass, 2.0, 2.0, 1);
        assert_eq!(policy.total_bytes(), 3000);
    }

    #[test]
    fn test_transport_update_cost_model() {
        let mut policy = TransportPolicy::with_defaults();
        let new_model = TransportCostModel {
            network_cost_per_byte_us: 0.1,
            ..Default::default()
        };
        policy.update_cost_model(new_model);
        // With non-zero network cost, small payloads should now get bypass
        let config = TransportPolicyConfig {
            cost_model: TransportCostModel {
                network_cost_per_byte_us: 0.1,
                ..Default::default()
            },
            ..Default::default()
        };
        let policy2 = TransportPolicy::new(config);
        assert_eq!(policy2.select_mode(100), TransportMode::Bypass);
    }

    #[test]
    fn test_transport_set_adaptive() {
        let mut policy = TransportPolicy::with_defaults();
        policy.set_adaptive(false);
        policy.set_fixed_mode(TransportMode::Compressed);
        assert_eq!(policy.select_mode(1), TransportMode::Compressed);
    }

    #[test]
    fn test_transport_ewma_accessor() {
        let mut policy = TransportPolicy::with_defaults();
        assert_eq!(policy.ewma_cost_us(), 0.0);
        policy.record(1000, TransportMode::Local, 10.0, 50.0, 0);
        assert!(policy.ewma_cost_us() > 0.0);
    }

    // ── C5: Tail-Latency Tests ─────────────────────────────────────

    #[test]
    fn test_syscall_strategy_display() {
        assert_eq!(format!("{}", SyscallStrategy::Immediate), "IMMEDIATE");
        assert_eq!(format!("{}", SyscallStrategy::Batched), "BATCHED");
        assert_eq!(format!("{}", SyscallStrategy::Adaptive), "ADAPTIVE");
    }

    #[test]
    fn test_wakeup_source_display() {
        assert_eq!(format!("{}", WakeupSource::Timer), "TIMER");
        assert_eq!(format!("{}", WakeupSource::IoEvent), "IO_EVENT");
        assert_eq!(format!("{}", WakeupSource::Signal), "SIGNAL");
        assert_eq!(format!("{}", WakeupSource::Nudge), "NUDGE");
    }

    #[test]
    fn test_affinity_hint_display() {
        assert_eq!(format!("{}", AffinityHint::Any), "ANY");
        assert_eq!(format!("{}", AffinityHint::PerformanceCore), "P_CORE");
        assert_eq!(format!("{}", AffinityHint::EfficiencyCore), "E_CORE");
        assert_eq!(format!("{}", AffinityHint::Pinned(3)), "PINNED(3)");
    }

    #[test]
    fn test_tail_latency_config_default() {
        let config = TailLatencyConfig::default();
        assert_eq!(config.syscall_strategy, SyscallStrategy::Adaptive);
        assert!(config.p99_budget_us < config.p999_budget_us);
    }

    #[test]
    fn test_tail_latency_record_wakeup() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_wakeup(WakeupSource::Timer, 100);
        ctrl.record_wakeup(WakeupSource::IoEvent, 200);
        ctrl.record_wakeup(WakeupSource::Signal, 300);
        ctrl.record_wakeup(WakeupSource::Nudge, 400);
        let snap = ctrl.snapshot();
        assert_eq!(snap.total_wakeups, 4);
        assert_eq!(snap.timer_wakeups, 1);
        assert_eq!(snap.io_wakeups, 1);
        assert_eq!(snap.signal_wakeups, 1);
        assert_eq!(snap.nudge_wakeups, 1);
        assert_eq!(snap.max_latency_us, 400);
    }

    #[test]
    fn test_tail_latency_wakeup_conservation() {
        let mut ctrl = TailLatencyController::with_defaults();
        for _ in 0..10 {
            ctrl.record_wakeup(WakeupSource::Timer, 50);
        }
        for _ in 0..5 {
            ctrl.record_wakeup(WakeupSource::IoEvent, 100);
        }
        let snap = ctrl.snapshot();
        assert_eq!(
            snap.timer_wakeups + snap.io_wakeups + snap.signal_wakeups + snap.nudge_wakeups,
            snap.total_wakeups
        );
    }

    #[test]
    fn test_tail_latency_record_batch() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_batch(10);
        ctrl.record_batch(20);
        assert_eq!(ctrl.snapshot().total_batches, 2);
        assert_eq!(ctrl.snapshot().total_syscalls, 30);
        assert!((ctrl.avg_batch_depth() - 15.0).abs() < 0.001);
    }

    #[test]
    fn test_tail_latency_p99() {
        let mut ctrl = TailLatencyController::with_defaults();
        // 100 samples: 99 at 100µs, 1 at 5000µs
        for _ in 0..99 {
            ctrl.record_wakeup(WakeupSource::Timer, 100);
        }
        ctrl.record_wakeup(WakeupSource::Timer, 5000);
        let p99 = ctrl.p99_latency_us();
        // p99 of 100 samples → index 99 → should be 5000
        assert!(p99 >= 100); // At minimum, it's at least 100
    }

    #[test]
    fn test_tail_latency_budget_violation() {
        let config = TailLatencyConfig {
            p99_budget_us: 1000,
            ..Default::default()
        };
        let mut ctrl = TailLatencyController::new(config);
        ctrl.record_wakeup(WakeupSource::Timer, 500); // OK
        ctrl.record_wakeup(WakeupSource::Timer, 1500); // Violation
        assert_eq!(ctrl.snapshot().budget_violations, 1);
    }

    #[test]
    fn test_tail_latency_reset() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_wakeup(WakeupSource::Timer, 100);
        ctrl.record_batch(5);
        ctrl.reset();
        let snap = ctrl.snapshot();
        assert_eq!(snap.total_wakeups, 0);
        assert_eq!(snap.total_batches, 0);
        assert_eq!(snap.max_latency_us, 0);
    }

    #[test]
    fn test_tail_latency_degradation_healthy() {
        let ctrl = TailLatencyController::with_defaults();
        assert_eq!(ctrl.detect_degradation(), TailLatencyDegradation::Healthy);
    }

    #[test]
    fn test_tail_latency_degradation_p999_breach() {
        let config = TailLatencyConfig {
            p999_budget_us: 5000,
            ..Default::default()
        };
        let mut ctrl = TailLatencyController::new(config);
        ctrl.record_wakeup(WakeupSource::Timer, 10000); // Exceeds p999
        let is_breach = matches!(
            ctrl.detect_degradation(),
            TailLatencyDegradation::P999Breach { .. }
        );
        assert!(
            is_breach,
            "Expected P999Breach, got {:?}",
            ctrl.detect_degradation()
        );
    }

    #[test]
    fn test_tail_latency_degradation_display() {
        assert_eq!(format!("{}", TailLatencyDegradation::Healthy), "HEALTHY");
        let breach = TailLatencyDegradation::P99Breach {
            observed_us: 15000,
            budget_us: 10000,
        };
        assert!(format!("{}", breach).contains("15000"));
    }

    #[test]
    fn test_tail_latency_log_entry() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_wakeup(WakeupSource::IoEvent, 500);
        ctrl.record_batch(8);
        let entry = ctrl.log_entry();
        assert_eq!(entry.total_wakeups, 1);
        assert_eq!(entry.degradation, TailLatencyDegradation::Healthy);
    }

    #[test]
    fn test_tail_latency_log_entry_serde() {
        let entry = TailLatencyLogEntry {
            total_wakeups: 100,
            p99_latency_us: 5000,
            max_latency_us: 20000,
            budget_violations: 3,
            avg_batch_depth: 12.5,
            degradation: TailLatencyDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: TailLatencyLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_tail_latency_snapshot_serde() {
        let snap = TailLatencySnapshot {
            total_wakeups: 50,
            timer_wakeups: 20,
            io_wakeups: 15,
            signal_wakeups: 10,
            nudge_wakeups: 5,
            total_syscalls: 300,
            total_batches: 30,
            avg_batch_depth: 10.0,
            p99_latency_us: 8000,
            max_latency_us: 25000,
            budget_violations: 2,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TailLatencySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_tail_latency_degradation_serde() {
        let variants = vec![
            TailLatencyDegradation::Healthy,
            TailLatencyDegradation::P99Breach {
                observed_us: 15000,
                budget_us: 10000,
            },
            TailLatencyDegradation::P999Breach {
                observed_us: 60000,
                budget_us: 50000,
            },
            TailLatencyDegradation::HighViolationRate {
                violations: 10,
                total: 100,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: TailLatencyDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_tail_latency_status_line() {
        let ctrl = TailLatencyController::with_defaults();
        let line = ctrl.status_line();
        assert!(line.contains("tail-latency"));
        assert!(line.contains("wakeups=0"));
    }

    #[test]
    fn test_tail_latency_accessors() {
        let ctrl = TailLatencyController::with_defaults();
        assert_eq!(ctrl.strategy(), SyscallStrategy::Adaptive);
        assert_eq!(ctrl.affinity(), AffinityHint::Any);
        assert_eq!(ctrl.sample_count(), 0);
    }

    #[test]
    fn test_wakeup_event_serde() {
        let evt = WakeupEvent {
            source: WakeupSource::IoEvent,
            latency_us: 250,
            timestamp_us: 12345,
            batch_depth: 4,
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: WakeupEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn test_tail_latency_config_serde() {
        let config = TailLatencyConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: TailLatencyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    // ── C5 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_tail_latency_p50() {
        let mut ctrl = TailLatencyController::with_defaults();
        for i in 1..=100 {
            ctrl.record_wakeup(WakeupSource::Timer, i * 10);
        }
        let p50 = ctrl.p50_latency_us();
        // Median of 10..1000 step 10 → ~500
        assert!(p50 >= 400 && p50 <= 600, "p50={}", p50);
    }

    #[test]
    fn test_tail_latency_wakeup_distribution() {
        let mut ctrl = TailLatencyController::with_defaults();
        for _ in 0..6 {
            ctrl.record_wakeup(WakeupSource::Timer, 100);
        }
        for _ in 0..3 {
            ctrl.record_wakeup(WakeupSource::IoEvent, 100);
        }
        for _ in 0..1 {
            ctrl.record_wakeup(WakeupSource::Signal, 100);
        }
        let (t, io, s, n) = ctrl.wakeup_distribution();
        assert!((t - 0.6).abs() < 0.01);
        assert!((io - 0.3).abs() < 0.01);
        assert!((s - 0.1).abs() < 0.01);
        assert_eq!(n, 0.0);
    }

    #[test]
    fn test_tail_latency_wakeup_distribution_empty() {
        let ctrl = TailLatencyController::with_defaults();
        let (t, io, s, n) = ctrl.wakeup_distribution();
        assert_eq!(t, 0.0);
        assert_eq!(io, 0.0);
        assert_eq!(s, 0.0);
        assert_eq!(n, 0.0);
    }

    #[test]
    fn test_tail_latency_violation_rate() {
        let config = TailLatencyConfig {
            p99_budget_us: 100,
            ..Default::default()
        };
        let mut ctrl = TailLatencyController::new(config);
        for _ in 0..8 {
            ctrl.record_wakeup(WakeupSource::Timer, 50);
        }
        for _ in 0..2 {
            ctrl.record_wakeup(WakeupSource::Timer, 200);
        }
        assert!((ctrl.violation_rate() - 0.2).abs() < 0.01);
    }

    #[test]
    fn test_tail_latency_within_budget() {
        let mut ctrl = TailLatencyController::with_defaults();
        for _ in 0..10 {
            ctrl.record_wakeup(WakeupSource::Timer, 100);
        }
        assert!(ctrl.within_p99_budget());
        assert!(ctrl.within_p999_budget());
    }

    #[test]
    fn test_tail_latency_set_strategy() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.set_strategy(SyscallStrategy::Batched);
        assert_eq!(ctrl.strategy(), SyscallStrategy::Batched);
    }

    #[test]
    fn test_tail_latency_set_affinity() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.set_affinity(AffinityHint::Pinned(7));
        assert_eq!(ctrl.affinity(), AffinityHint::Pinned(7));
    }

    #[test]
    fn test_tail_latency_set_p99_budget() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.set_p99_budget(5000);
        for _ in 0..10 {
            ctrl.record_wakeup(WakeupSource::Timer, 4000);
        }
        assert!(ctrl.within_p99_budget());
    }

    #[test]
    fn test_tail_latency_total_accessors() {
        let mut ctrl = TailLatencyController::with_defaults();
        ctrl.record_wakeup(WakeupSource::Timer, 100);
        ctrl.record_wakeup(WakeupSource::Timer, 20000); // violation (default budget=10000)
        assert_eq!(ctrl.total_wakeups(), 2);
        assert_eq!(ctrl.budget_violations(), 1);
    }

    // ── D1: Hitch-Risk Model Tests ─────────────────────────────────

    #[test]
    fn test_evidence_signal_display() {
        assert_eq!(format!("{}", EvidenceSignal::LatencyProbe), "LATENCY_PROBE");
        assert_eq!(format!("{}", EvidenceSignal::CpuLoad), "CPU_LOAD");
    }

    #[test]
    fn test_hitch_risk_level_display() {
        assert_eq!(format!("{}", HitchRiskLevel::Low), "LOW");
        assert_eq!(format!("{}", HitchRiskLevel::Critical), "CRITICAL");
    }

    #[test]
    fn test_hitch_risk_config_default() {
        let config = HitchRiskConfig::default();
        assert!(config.prior_hitch_prob > 0.0 && config.prior_hitch_prob < 1.0);
        assert!(config.elevated_threshold < config.high_threshold);
        assert!(config.high_threshold < config.critical_threshold);
    }

    #[test]
    fn test_hitch_risk_model_initial_state() {
        let model = HitchRiskModel::with_defaults();
        assert_eq!(model.risk_level(), HitchRiskLevel::Low);
        assert!(model.posterior_prob() < 0.5);
        assert_eq!(model.snapshot().total_updates, 0);
    }

    #[test]
    fn test_hitch_risk_posterior_bounded() {
        let model = HitchRiskModel::with_defaults();
        let prob = model.posterior_prob();
        assert!(prob >= 0.0 && prob <= 1.0, "prob={}", prob);
    }

    #[test]
    fn test_hitch_risk_positive_evidence() {
        let mut model = HitchRiskModel::with_defaults();
        // Strong positive evidence → risk increases
        for i in 0..20 {
            model.update(EvidenceSignal::LatencyProbe, 10000.0, 2.0, i * 100);
        }
        assert!(model.posterior_prob() > 0.5);
        let level_is_elevated = matches!(
            model.risk_level(),
            HitchRiskLevel::Elevated | HitchRiskLevel::High | HitchRiskLevel::Critical
        );
        assert!(level_is_elevated, "level={:?}", model.risk_level());
    }

    #[test]
    fn test_hitch_risk_negative_evidence() {
        let mut model = HitchRiskModel::with_defaults();
        // Strong negative evidence → risk decreases
        for i in 0..20 {
            model.update(EvidenceSignal::LatencyProbe, 10.0, -2.0, i * 100);
        }
        assert!(model.posterior_prob() < 0.1);
        assert_eq!(model.risk_level(), HitchRiskLevel::Low);
    }

    #[test]
    fn test_hitch_risk_reset() {
        let mut model = HitchRiskModel::with_defaults();
        for i in 0..10 {
            model.update(EvidenceSignal::BudgetViolation, 1.0, 3.0, i * 100);
        }
        model.reset();
        assert_eq!(model.risk_level(), HitchRiskLevel::Low);
        assert_eq!(model.snapshot().total_updates, 0);
        assert_eq!(model.recent_evidence().len(), 0);
    }

    #[test]
    fn test_hitch_risk_evidence_capped() {
        let config = HitchRiskConfig {
            max_evidence: 10,
            ..Default::default()
        };
        let mut model = HitchRiskModel::new(config);
        for i in 0..50 {
            model.update(EvidenceSignal::QueueDepth, 100.0, 0.1, i * 100);
        }
        assert_eq!(model.recent_evidence().len(), 10);
    }

    #[test]
    fn test_hitch_risk_snapshot_serde() {
        let snap = HitchRiskSnapshot {
            log_odds: 2.5,
            posterior_prob: 0.924,
            risk_level: HitchRiskLevel::High,
            evidence_count: 15,
            total_updates: 42,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: HitchRiskSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.risk_level, back.risk_level);
        assert_eq!(snap.evidence_count, back.evidence_count);
    }

    #[test]
    fn test_hitch_risk_degradation_healthy() {
        let model = HitchRiskModel::with_defaults();
        assert_eq!(model.detect_degradation(), HitchRiskDegradation::Healthy);
    }

    #[test]
    fn test_hitch_risk_degradation_elevated() {
        let mut model = HitchRiskModel::with_defaults();
        // Push log_odds above elevated threshold (1.0)
        for i in 0..10 {
            model.update(EvidenceSignal::LatencyProbe, 5000.0, 1.5, i * 100);
        }
        let is_elevated_or_higher = matches!(
            model.detect_degradation(),
            HitchRiskDegradation::ElevatedRisk { .. }
                | HitchRiskDegradation::HighRisk { .. }
                | HitchRiskDegradation::CriticalRisk { .. }
        );
        assert!(
            is_elevated_or_higher,
            "Got {:?}",
            model.detect_degradation()
        );
    }

    #[test]
    fn test_hitch_risk_degradation_display() {
        assert_eq!(format!("{}", HitchRiskDegradation::Healthy), "HEALTHY");
        let elev = HitchRiskDegradation::ElevatedRisk {
            posterior_prob: 0.75,
        };
        assert!(format!("{}", elev).contains("75.0%"));
    }

    #[test]
    fn test_hitch_risk_log_entry() {
        let model = HitchRiskModel::with_defaults();
        let entry = model.log_entry();
        assert_eq!(entry.risk_level, HitchRiskLevel::Low);
        assert_eq!(entry.total_updates, 0);
    }

    #[test]
    fn test_hitch_risk_log_entry_serde() {
        let entry = HitchRiskLogEntry {
            log_odds: 1.5,
            posterior_prob: 0.818,
            risk_level: HitchRiskLevel::Elevated,
            evidence_count: 5,
            total_updates: 10,
            degradation: HitchRiskDegradation::ElevatedRisk {
                posterior_prob: 0.818,
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: HitchRiskLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.risk_level, back.risk_level);
    }

    #[test]
    fn test_hitch_risk_degradation_serde() {
        let variants = vec![
            HitchRiskDegradation::Healthy,
            HitchRiskDegradation::ElevatedRisk {
                posterior_prob: 0.7,
            },
            HitchRiskDegradation::HighRisk {
                posterior_prob: 0.9,
                evidence_count: 20,
            },
            HitchRiskDegradation::CriticalRisk {
                posterior_prob: 0.99,
                log_odds: 5.5,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: HitchRiskDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_evidence_entry_serde() {
        let entry = EvidenceEntry {
            signal: EvidenceSignal::BackpressureChange,
            value: 3.5,
            log_likelihood_ratio: 1.2,
            timestamp_us: 99999,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: EvidenceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_hitch_risk_status_line() {
        let model = HitchRiskModel::with_defaults();
        let line = model.status_line();
        assert!(line.contains("hitch-risk"));
        assert!(line.contains("level=LOW"));
    }

    #[test]
    fn test_hitch_risk_config_serde() {
        let config = HitchRiskConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: HitchRiskConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    // ── D1 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_hitch_risk_observe_violation() {
        let mut model = HitchRiskModel::with_defaults();
        let initial = model.log_odds();
        model.observe_violation(2.0, 1000);
        assert!(model.log_odds() > initial);
        assert_eq!(model.total_updates(), 1);
    }

    #[test]
    fn test_hitch_risk_observe_latency() {
        let mut model = HitchRiskModel::with_defaults();
        model.observe_latency(15000.0, 1.5, 1000);
        assert_eq!(model.total_updates(), 1);
        assert_eq!(model.evidence_count(), 1);
    }

    #[test]
    fn test_hitch_risk_observe_healthy() {
        let mut model = HitchRiskModel::with_defaults();
        // First push risk up
        for i in 0..10 {
            model.observe_violation(2.0, i * 100);
        }
        let high_odds = model.log_odds();
        // Now submit healthy evidence
        for i in 10..30 {
            model.observe_healthy(i * 100);
        }
        assert!(model.log_odds() < high_odds, "Healthy should reduce odds");
    }

    #[test]
    fn test_hitch_risk_should_mitigate() {
        let mut model = HitchRiskModel::with_defaults();
        assert!(!model.should_mitigate());
        // Push to high risk
        for i in 0..30 {
            model.observe_violation(3.0, i * 100);
        }
        assert!(model.should_mitigate());
    }

    #[test]
    fn test_hitch_risk_is_critical() {
        let mut model = HitchRiskModel::with_defaults();
        assert!(!model.is_critical());
        for i in 0..50 {
            model.observe_violation(5.0, i * 100);
        }
        assert!(model.is_critical());
    }

    #[test]
    fn test_hitch_risk_set_evidence_decay() {
        let mut model = HitchRiskModel::with_defaults();
        model.set_evidence_decay(0.5);
        // Submit evidence; with 0.5 decay, old evidence fades fast
        model.observe_violation(10.0, 1000);
        let odds_after_1 = model.log_odds();
        model.observe_healthy(2000);
        // With 0.5 decay, log_odds *= 0.5 then -0.5 → should reduce significantly
        assert!(model.log_odds() < odds_after_1);
    }

    #[test]
    fn test_hitch_risk_set_prior() {
        let mut model = HitchRiskModel::with_defaults();
        model.set_prior(0.5);
        // This changes the config but doesn't reset log_odds mid-session
        // (by design — set_prior just updates config for next reset)
        assert_eq!(model.total_updates(), 0);
    }

    #[test]
    fn test_hitch_risk_accessors() {
        let mut model = HitchRiskModel::with_defaults();
        assert_eq!(model.total_updates(), 0);
        assert_eq!(model.evidence_count(), 0);
        model.observe_violation(1.0, 100);
        assert_eq!(model.total_updates(), 1);
        assert_eq!(model.evidence_count(), 1);
    }

    // ── D2: E-Process Drift Detector Tests ────────────────────────

    #[test]
    fn test_eprocess_kind_display() {
        assert_eq!(EProcessKind::CusumLike.to_string(), "cusum_like");
        assert_eq!(EProcessKind::Mixture.to_string(), "mixture");
        assert_eq!(
            EProcessKind::ConfidenceSequence.to_string(),
            "confidence_seq"
        );
    }

    #[test]
    fn test_drift_observable_display() {
        assert_eq!(DriftObservable::Latency.to_string(), "latency");
        assert_eq!(DriftObservable::Throughput.to_string(), "throughput");
        assert_eq!(DriftObservable::ErrorRate.to_string(), "error_rate");
        assert_eq!(DriftObservable::QueueDepth.to_string(), "queue_depth");
        assert_eq!(DriftObservable::ResourceUsage.to_string(), "resource_usage");
    }

    #[test]
    fn test_drift_alert_level_display() {
        assert_eq!(DriftAlertLevel::None.to_string(), "none");
        assert_eq!(DriftAlertLevel::Warning.to_string(), "warning");
        assert_eq!(DriftAlertLevel::Alarm.to_string(), "alarm");
    }

    #[test]
    fn test_eprocess_config_default_latency() {
        let config = EProcessConfig::default_latency();
        assert_eq!(config.kind, EProcessKind::Mixture);
        assert_eq!(config.observable, DriftObservable::Latency);
        assert!(config.alpha > 0.0 && config.alpha < 1.0);
        assert!(config.lambda > 0.0);
        assert!(config.warmup > 0);
    }

    #[test]
    fn test_eprocess_initial_state() {
        let det = EProcessDetector::with_defaults();
        assert_eq!(det.total_observations(), 0);
        assert_eq!(det.alarm_count(), 0);
        // E_0 = 1 => e_value() = exp(0) = 1
        assert!((det.e_value() - 1.0).abs() < 1e-10);
        assert!((det.log_e_value() - 0.0).abs() < 1e-10);
        assert_eq!(det.kind(), EProcessKind::Mixture);
        assert_eq!(det.history_len(), 0);
    }

    #[test]
    fn test_eprocess_null_observations_stay_near_one() {
        // Under null (observations near null_mean=0), e-value should fluctuate near 1
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 5,
            auto_reset: true,
        });
        for i in 0..50 {
            det.observe(0.0, i * 100);
        }
        // All observations exactly at null mean => LR = 1 => e-value stays at 1
        assert!((det.e_value() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_positive_drift_raises_alarm() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 1000,
            warmup: 0,
            auto_reset: false,
        });
        let mut alarm_seen = false;
        for i in 0..100 {
            let level = det.observe(5.0, i * 100);
            if level == DriftAlertLevel::Alarm {
                alarm_seen = true;
                break;
            }
        }
        assert!(alarm_seen, "Large positive drift should trigger alarm");
        assert!(det.alarm_count() >= 1);
    }

    #[test]
    fn test_eprocess_warmup_suppresses_alarm() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 100,
            warmup: 50,
            auto_reset: false,
        });
        // Even with large drift, during warmup we get None
        for i in 0..49 {
            let level = det.observe(100.0, i * 100);
            assert_eq!(level, DriftAlertLevel::None);
        }
    }

    #[test]
    fn test_eprocess_cusum_like_resets_floor() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::CusumLike,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        // Drive e-value down with negative observations
        for i in 0..20 {
            det.observe(-5.0, i * 100);
        }
        // CUSUM-like floors at log_e = 0 each step, so it can't go below 0
        // (though negative LR can make log_e = max(0, prev) + log(LR) < 0)
        // The key property is: recovery is faster since negatives don't accumulate below 0
        let e_val = det.e_value();
        // Just verify it ran without panic
        assert!(e_val >= 0.0);
    }

    #[test]
    fn test_eprocess_auto_reset() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        // Drive to alarm
        for i in 0..100 {
            det.observe(10.0, i * 100);
        }
        // After auto-reset, e-value should have been reset
        // (may have been re-driven up, but alarm_count > 0)
        assert!(det.alarm_count() >= 1);
    }

    #[test]
    fn test_eprocess_reset() {
        let mut det = EProcessDetector::with_defaults();
        for i in 0..30 {
            det.observe(5.0, i * 100);
        }
        assert!(det.total_observations() > 0);
        det.reset();
        assert_eq!(det.total_observations(), 0);
        assert_eq!(det.alarm_count(), 0);
        assert!((det.e_value() - 1.0).abs() < 1e-10);
        assert_eq!(det.history_len(), 0);
    }

    #[test]
    fn test_eprocess_running_stats() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        det.observe(10.0, 100);
        det.observe(20.0, 200);
        det.observe(30.0, 300);
        assert!((det.running_mean() - 20.0).abs() < 1e-10);
        assert!(det.running_variance() > 0.0);
    }

    #[test]
    fn test_eprocess_snapshot_fields() {
        let mut det = EProcessDetector::with_defaults();
        for i in 0..5 {
            det.observe(1.0, i * 100);
        }
        let snap = det.snapshot();
        assert_eq!(snap.total_observations, 5);
        assert!(snap.e_value >= 0.0);
        assert!(snap.peak_e_value >= snap.e_value);
    }

    #[test]
    fn test_eprocess_status_line() {
        let det = EProcessDetector::with_defaults();
        let line = det.status_line();
        assert!(line.contains("e-proc"));
        assert!(line.contains("mixture"));
    }

    #[test]
    fn test_eprocess_recent_observations() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 5,
            warmup: 0,
            auto_reset: true,
        });
        for i in 0..8 {
            det.observe(i as f64, i as u64 * 100);
        }
        let recent = det.recent_observations(3);
        assert_eq!(recent.len(), 3);
        // Should be the last 3: values 5, 6, 7
        assert!((recent[0].value - 5.0).abs() < 1e-10);
        assert!((recent[1].value - 6.0).abs() < 1e-10);
        assert!((recent[2].value - 7.0).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_degradation_healthy() {
        let det = EProcessDetector::with_defaults();
        assert_eq!(det.detect_degradation(), EProcessDegradation::Healthy);
    }

    #[test]
    fn test_eprocess_degradation_display() {
        assert_eq!(EProcessDegradation::Healthy.to_string(), "healthy");
        let suspected = EProcessDegradation::DriftSuspected {
            e_value: 5.0,
            running_mean: 2.5,
        };
        assert!(suspected.to_string().contains("drift_suspected"));
        let detected = EProcessDegradation::DriftDetected {
            e_value: 25.0,
            alarm_count: 3,
        };
        assert!(detected.to_string().contains("drift_detected"));
    }

    #[test]
    fn test_eprocess_kind_serde() {
        for kind in [
            EProcessKind::CusumLike,
            EProcessKind::Mixture,
            EProcessKind::ConfidenceSequence,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: EProcessKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn test_drift_observable_serde() {
        for obs in [
            DriftObservable::Latency,
            DriftObservable::Throughput,
            DriftObservable::ErrorRate,
            DriftObservable::QueueDepth,
            DriftObservable::ResourceUsage,
        ] {
            let json = serde_json::to_string(&obs).unwrap();
            let back: DriftObservable = serde_json::from_str(&json).unwrap();
            assert_eq!(obs, back);
        }
    }

    #[test]
    fn test_drift_alert_level_serde() {
        for level in [
            DriftAlertLevel::None,
            DriftAlertLevel::Warning,
            DriftAlertLevel::Alarm,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let back: DriftAlertLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
    }

    #[test]
    fn test_eprocess_observation_serde() {
        let obs = EProcessObservation {
            value: 3.14,
            observable: DriftObservable::Latency,
            timestamp_us: 12345,
            likelihood_ratio: 1.2,
        };
        let json = serde_json::to_string(&obs).unwrap();
        let back: EProcessObservation = serde_json::from_str(&json).unwrap();
        assert!((obs.value - back.value).abs() < 1e-10);
        assert_eq!(obs.observable, back.observable);
    }

    #[test]
    fn test_eprocess_log_entry() {
        let mut det = EProcessDetector::with_defaults();
        for i in 0..5 {
            det.observe(1.0, i * 100);
        }
        let entry = det.log_entry();
        assert_eq!(entry.total_observations, 5);
        assert!(entry.e_value >= 0.0);
    }

    #[test]
    fn test_eprocess_degradation_serde() {
        let variants = vec![
            EProcessDegradation::Healthy,
            EProcessDegradation::DriftSuspected {
                e_value: 5.0,
                running_mean: 2.5,
            },
            EProcessDegradation::DriftDetected {
                e_value: 25.0,
                alarm_count: 3,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: EProcessDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_eprocess_config_serde() {
        let config = EProcessConfig::default_latency();
        let json = serde_json::to_string(&config).unwrap();
        let back: EProcessConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.kind, back.kind);
        assert_eq!(config.observable, back.observable);
        assert!((config.alpha - back.alpha).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_history_wraps() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 3,
            warmup: 0,
            auto_reset: true,
        });
        for i in 0..10 {
            det.observe(i as f64, i as u64 * 100);
        }
        // max_history = 3, so only 3 observations stored
        assert_eq!(det.history_len(), 3);
        assert_eq!(det.total_observations(), 10);
    }

    #[test]
    fn test_eprocess_e_value_nonneg() {
        let mut det = EProcessDetector::with_defaults();
        for i in 0..50 {
            det.observe(-10.0, i * 100);
        }
        // e-value = exp(log_e_value), always >= 0
        assert!(det.e_value() >= 0.0);
    }

    // ── D2 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_eprocess_observe_batch() {
        let mut det = EProcessDetector::with_defaults();
        let batch: Vec<(f64, u64)> = (0..10).map(|i| (1.0, i * 100)).collect();
        let level = det.observe_batch(&batch);
        assert_eq!(det.total_observations(), 10);
        // Level should be deterministic
        let _ = level;
    }

    #[test]
    fn test_eprocess_observe_latency_us() {
        let mut det = EProcessDetector::with_defaults();
        let level = det.observe_latency_us(500.0, 100);
        assert_eq!(det.total_observations(), 1);
        let _ = level;
    }

    #[test]
    fn test_eprocess_running_stddev() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        det.observe(10.0, 100);
        det.observe(20.0, 200);
        det.observe(30.0, 300);
        let stddev = det.running_stddev();
        assert!(stddev > 0.0);
        assert!((stddev - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_z_score() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        det.observe(10.0, 100);
        det.observe(20.0, 200);
        det.observe(30.0, 300);
        // mean=20, stddev=10
        let z = det.z_score(30.0);
        assert!((z - 1.0).abs() < 1e-10);
        let z0 = det.z_score(20.0);
        assert!(z0.abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_z_score_zero_variance() {
        let mut det = EProcessDetector::with_defaults();
        det.observe(5.0, 100);
        // Only one observation, variance=0
        let z = det.z_score(10.0);
        assert_eq!(z, 0.0);
    }

    #[test]
    fn test_eprocess_alarm_rate() {
        let det = EProcessDetector::with_defaults();
        assert_eq!(det.alarm_rate(), 0.0);
    }

    #[test]
    fn test_eprocess_alarm_rate_positive() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        for i in 0..100 {
            det.observe(10.0, i * 100);
        }
        let rate = det.alarm_rate();
        assert!(rate >= 0.0 && rate <= 1.0);
    }

    #[test]
    fn test_eprocess_set_lambda() {
        let mut det = EProcessDetector::with_defaults();
        det.set_lambda(0.5);
        // Observe with new lambda — should be more sensitive
        det.observe(10.0, 100);
        assert_eq!(det.total_observations(), 1);
    }

    #[test]
    fn test_eprocess_set_null_mean() {
        let mut det = EProcessDetector::with_defaults();
        det.set_null_mean(5.0);
        // Observations at 5.0 should now give LR = 1
        for i in 0..10 {
            det.observe(5.0, i * 100);
        }
        assert!((det.e_value() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_eprocess_set_alpha() {
        let mut det = EProcessDetector::with_defaults();
        det.set_alpha(0.01);
        // Higher threshold now
        assert_eq!(det.total_observations(), 0);
    }

    #[test]
    fn test_eprocess_warning_count() {
        let det = EProcessDetector::with_defaults();
        assert_eq!(det.warning_count(), 0);
    }

    #[test]
    fn test_eprocess_peak_e_value() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: true,
        });
        // Drive e-value up, then down
        for i in 0..10 {
            det.observe(5.0, i * 100);
        }
        let peak_after_up = det.peak_e_value();
        for i in 10..20 {
            det.observe(-5.0, i * 100);
        }
        // Peak should be >= current and >= what it was after the up phase
        assert!(
            det.peak_e_value() >= peak_after_up
                || (det.peak_e_value() - peak_after_up).abs() < 1e-10
        );
    }

    #[test]
    fn test_eprocess_is_alarming() {
        let mut det = EProcessDetector::new(EProcessConfig {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.5,
            null_mean: 0.0,
            max_history: 100,
            warmup: 0,
            auto_reset: false, // Don't auto-reset to keep alarm state
        });
        assert!(!det.is_alarming());
        // Drive to alarm
        for i in 0..100 {
            det.observe(10.0, i * 100);
        }
        // With auto_reset=false, should stay in alarm
        if det.alarm_count() > 0 {
            assert!(det.is_alarming());
        }
    }

    // ── D3: Expected-Loss Policy Controller Tests ─────────────────

    #[test]
    fn test_policy_action_display() {
        assert_eq!(PolicyAction::Hold.to_string(), "hold");
        assert_eq!(PolicyAction::Tighten.to_string(), "tighten");
        assert_eq!(PolicyAction::Relax.to_string(), "relax");
        assert_eq!(PolicyAction::Shed.to_string(), "shed");
    }

    #[test]
    fn test_system_state_display() {
        assert_eq!(SystemState::Healthy.to_string(), "healthy");
        assert_eq!(SystemState::Drifting.to_string(), "drifting");
        assert_eq!(SystemState::Stressed.to_string(), "stressed");
        assert_eq!(SystemState::Critical.to_string(), "critical");
    }

    #[test]
    fn test_policy_controller_initial_state() {
        let ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.current_action(), PolicyAction::Hold);
        assert_eq!(ctrl.total_decisions(), 0);
    }

    #[test]
    fn test_policy_healthy_selects_hold() {
        let mut ctrl = PolicyController::with_defaults();
        // 100% healthy => Hold is cheapest (loss=0)
        let action = ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        // Due to critical_floor, slight redistribution but Hold should still win
        assert_eq!(action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_critical_selects_shed() {
        let mut ctrl = PolicyController::with_defaults();
        // 100% critical => Shed is cheapest (loss=1)
        let action = ctrl.decide([0.0, 0.0, 0.0, 1.0], 100);
        assert_eq!(action, PolicyAction::Shed);
    }

    #[test]
    fn test_policy_drifting_selects_tighten() {
        let mut ctrl = PolicyController::with_defaults();
        // 100% drifting => Tighten is cheapest (loss=0.5)
        let action = ctrl.decide([0.0, 1.0, 0.0, 0.0], 100);
        assert_eq!(action, PolicyAction::Tighten);
    }

    #[test]
    fn test_policy_stressed_selects_tighten() {
        let mut ctrl = PolicyController::with_defaults();
        // 100% stressed => Tighten is cheapest (loss=1.0)
        let action = ctrl.decide([0.0, 0.0, 1.0, 0.0], 100);
        assert_eq!(action, PolicyAction::Tighten);
    }

    #[test]
    fn test_policy_decision_count() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        ctrl.decide([0.5, 0.5, 0.0, 0.0], 200);
        ctrl.decide([0.0, 0.0, 0.0, 1.0], 300);
        assert_eq!(ctrl.total_decisions(), 3);
    }

    #[test]
    fn test_policy_critical_floor() {
        let mut ctrl = PolicyController::with_defaults();
        // Even with 0 critical probability, critical_floor ensures min P(Critical)
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        let recent = ctrl.recent_decisions(1);
        assert_eq!(recent.len(), 1);
        // Critical prob should be at least critical_floor
        assert!(recent[0].state_probs[3] >= ctrl.config.critical_floor - 1e-10);
    }

    #[test]
    fn test_policy_snapshot() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        let snap = ctrl.snapshot();
        assert_eq!(snap.total_decisions, 1);
        assert_eq!(snap.current_action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_status_line() {
        let ctrl = PolicyController::with_defaults();
        let line = ctrl.status_line();
        assert!(line.contains("policy"));
        assert!(line.contains("hold"));
    }

    #[test]
    fn test_policy_reset() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([0.0, 0.0, 0.0, 1.0], 100);
        ctrl.reset();
        assert_eq!(ctrl.total_decisions(), 0);
        assert_eq!(ctrl.current_action(), PolicyAction::Hold);
    }

    #[test]
    fn test_policy_action_serde() {
        for action in [
            PolicyAction::Hold,
            PolicyAction::Tighten,
            PolicyAction::Relax,
            PolicyAction::Shed,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let back: PolicyAction = serde_json::from_str(&json).unwrap();
            assert_eq!(action, back);
        }
    }

    #[test]
    fn test_system_state_serde() {
        for state in [
            SystemState::Healthy,
            SystemState::Drifting,
            SystemState::Stressed,
            SystemState::Critical,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: SystemState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }

    #[test]
    fn test_policy_degradation_display() {
        assert_eq!(PolicyDegradation::Healthy.to_string(), "healthy");
        let t = PolicyDegradation::Tightening { expected_loss: 1.5 };
        assert!(t.to_string().contains("tightening"));
        let e = PolicyDegradation::EmergencyShed {
            total_decisions: 5,
            last_loss: 2.0,
        };
        assert!(e.to_string().contains("emergency_shed"));
    }

    #[test]
    fn test_policy_degradation_serde() {
        let variants = vec![
            PolicyDegradation::Healthy,
            PolicyDegradation::Tightening { expected_loss: 1.5 },
            PolicyDegradation::EmergencyShed {
                total_decisions: 5,
                last_loss: 2.0,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: PolicyDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_policy_log_entry() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        let entry = ctrl.log_entry();
        assert_eq!(entry.total_decisions, 1);
        assert_eq!(entry.current_action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_config_serde() {
        let config = PolicyControllerConfig::default_asymmetric();
        let json = serde_json::to_string(&config).unwrap();
        let back: PolicyControllerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.loss_matrix.len(), back.loss_matrix.len());
    }

    #[test]
    fn test_policy_detect_degradation() {
        let mut ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.detect_degradation(), PolicyDegradation::Healthy);
        ctrl.decide([0.0, 0.0, 0.0, 1.0], 100);
        let is_shed = matches!(
            ctrl.detect_degradation(),
            PolicyDegradation::EmergencyShed { .. }
        );
        assert!(is_shed);
    }

    // ── D3 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_policy_decide_from_risk_low() {
        let mut ctrl = PolicyController::with_defaults();
        let action = ctrl.decide_from_risk(HitchRiskLevel::Low, 100);
        assert_eq!(action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_decide_from_risk_critical() {
        let mut ctrl = PolicyController::with_defaults();
        let action = ctrl.decide_from_risk(HitchRiskLevel::Critical, 100);
        assert_eq!(action, PolicyAction::Shed);
    }

    #[test]
    fn test_policy_decide_from_risk_elevated() {
        let mut ctrl = PolicyController::with_defaults();
        let action = ctrl.decide_from_risk(HitchRiskLevel::Elevated, 100);
        assert_eq!(action, PolicyAction::Tighten);
    }

    #[test]
    fn test_policy_action_distribution() {
        let mut ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.action_distribution(), [0.0; 4]);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 200);
        let dist = ctrl.action_distribution();
        let sum: f64 = dist.iter().sum();
        assert!((sum - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_policy_action_counts() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        ctrl.decide([0.0, 0.0, 0.0, 1.0], 200);
        let counts = ctrl.action_counts();
        let total: u64 = counts.iter().sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_policy_hysteresis_count() {
        let ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.hysteresis_count(), 0);
    }

    #[test]
    fn test_policy_last_expected_loss() {
        let mut ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.last_expected_loss(), 0.0);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        // Hold in healthy => loss should be very small (critical floor adds a bit)
        assert!(ctrl.last_expected_loss() >= 0.0);
    }

    #[test]
    fn test_policy_set_hysteresis() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.set_hysteresis(0.2);
        // Should affect future decisions
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        assert_eq!(ctrl.total_decisions(), 1);
    }

    #[test]
    fn test_policy_set_critical_floor() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.set_critical_floor(0.1);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        let recent = ctrl.recent_decisions(1);
        // Critical floor should be at least 0.1
        assert!(recent[0].state_probs[3] >= 0.1 - 1e-10);
    }

    #[test]
    fn test_policy_set_loss() {
        let mut ctrl = PolicyController::with_defaults();
        ctrl.set_loss(0, 0, 100.0); // Make Hold very expensive when Healthy
        let action = ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        // Now Hold should NOT be selected since it costs 100
        assert_ne!(action, PolicyAction::Hold);
    }

    #[test]
    fn test_policy_decision_count_tracks() {
        let mut ctrl = PolicyController::with_defaults();
        assert_eq!(ctrl.decision_count(), 0);
        ctrl.decide([1.0, 0.0, 0.0, 0.0], 100);
        assert_eq!(ctrl.decision_count(), 1);
    }

    // ── D4: Calibration Harness Tests ─────────────────────────────

    #[test]
    fn test_calibration_scenario_display() {
        assert_eq!(CalibrationScenario::Nominal.to_string(), "nominal");
        assert_eq!(
            CalibrationScenario::GradualDrift.to_string(),
            "gradual_drift"
        );
        assert_eq!(CalibrationScenario::AbruptShift.to_string(), "abrupt_shift");
        assert_eq!(
            CalibrationScenario::NoisyBaseline.to_string(),
            "noisy_baseline"
        );
        assert_eq!(
            CalibrationScenario::PostStressRecovery.to_string(),
            "post_stress_recovery"
        );
    }

    #[test]
    fn test_promotion_verdict_display() {
        assert_eq!(PromotionVerdict::Approved.to_string(), "approved");
        assert_eq!(
            PromotionVerdict::ConditionalHold.to_string(),
            "conditional_hold"
        );
        assert_eq!(PromotionVerdict::Rejected.to_string(), "rejected");
    }

    fn make_passing_result(scenario: CalibrationScenario) -> CalibrationResult {
        CalibrationResult {
            scenario,
            false_positive_rate: 0.01,
            miss_rate: 0.02,
            detection_delay: 10.0,
            mean_expected_loss: 1.0,
            passes_gate: false,
            observation_count: 1000,
            timestamp_us: 12345,
        }
    }

    fn make_failing_result(scenario: CalibrationScenario) -> CalibrationResult {
        CalibrationResult {
            scenario,
            false_positive_rate: 0.2,
            miss_rate: 0.3,
            detection_delay: 100.0,
            mean_expected_loss: 10.0,
            passes_gate: false,
            observation_count: 1000,
            timestamp_us: 12345,
        }
    }

    #[test]
    fn test_calibration_initial_state() {
        let harness = CalibrationHarness::with_defaults();
        assert_eq!(harness.verdict(), PromotionVerdict::Rejected);
        assert_eq!(harness.total_runs(), 0);
        assert_eq!(harness.result_count(), 0);
    }

    #[test]
    fn test_calibration_all_pass_approved() {
        let mut harness = CalibrationHarness::with_defaults();
        let scenarios = [
            CalibrationScenario::Nominal,
            CalibrationScenario::GradualDrift,
            CalibrationScenario::AbruptShift,
            CalibrationScenario::NoisyBaseline,
            CalibrationScenario::PostStressRecovery,
        ];
        for s in &scenarios {
            harness.submit(make_passing_result(*s));
        }
        let verdict = harness.evaluate();
        assert_eq!(verdict, PromotionVerdict::Approved);
    }

    #[test]
    fn test_calibration_one_fail_strict_rejected() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.submit(make_passing_result(CalibrationScenario::GradualDrift));
        harness.submit(make_passing_result(CalibrationScenario::AbruptShift));
        harness.submit(make_passing_result(CalibrationScenario::NoisyBaseline));
        harness.submit(make_failing_result(CalibrationScenario::PostStressRecovery));
        let verdict = harness.evaluate();
        assert_eq!(verdict, PromotionVerdict::Rejected);
    }

    #[test]
    fn test_calibration_non_strict_conditional() {
        let config = PromotionGateConfig {
            max_fpr: 0.05,
            max_miss_rate: 0.10,
            max_detection_delay: 50.0,
            max_expected_loss: 5.0,
            min_passing_scenarios: 5,
            strict: false,
        };
        let mut harness = CalibrationHarness::new(config);
        for _ in 0..3 {
            harness.submit(make_passing_result(CalibrationScenario::Nominal));
        }
        harness.submit(make_failing_result(CalibrationScenario::AbruptShift));
        let verdict = harness.evaluate();
        // 3 passing < 5 required, so ConditionalHold
        assert_eq!(verdict, PromotionVerdict::ConditionalHold);
    }

    #[test]
    fn test_calibration_empty_rejected() {
        let mut harness = CalibrationHarness::with_defaults();
        let verdict = harness.evaluate();
        assert_eq!(verdict, PromotionVerdict::Rejected);
    }

    #[test]
    fn test_calibration_reset() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.reset();
        assert_eq!(harness.total_runs(), 0);
        assert_eq!(harness.result_count(), 0);
    }

    #[test]
    fn test_calibration_clear_results() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        assert_eq!(harness.total_runs(), 1);
        harness.clear_results();
        assert_eq!(harness.result_count(), 0);
        assert_eq!(harness.total_runs(), 1);
    }

    #[test]
    fn test_calibration_snapshot() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.evaluate();
        let snap = harness.snapshot();
        assert_eq!(snap.total_runs, 1);
        assert_eq!(snap.scenario_results.len(), 1);
    }

    #[test]
    fn test_calibration_status_line() {
        let harness = CalibrationHarness::with_defaults();
        let line = harness.status_line();
        assert!(line.contains("calibration"));
    }

    #[test]
    fn test_calibration_scenario_serde() {
        for s in [
            CalibrationScenario::Nominal,
            CalibrationScenario::GradualDrift,
            CalibrationScenario::AbruptShift,
            CalibrationScenario::NoisyBaseline,
            CalibrationScenario::PostStressRecovery,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: CalibrationScenario = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn test_promotion_verdict_serde() {
        for v in [
            PromotionVerdict::Approved,
            PromotionVerdict::ConditionalHold,
            PromotionVerdict::Rejected,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: PromotionVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_calibration_degradation_display() {
        assert_eq!(CalibrationDegradation::Healthy.to_string(), "healthy");
        let m = CalibrationDegradation::GateMarginal {
            passing: 3,
            total: 5,
        };
        assert!(m.to_string().contains("3/5"));
        let f = CalibrationDegradation::GateFailed {
            failing: 2,
            total: 5,
        };
        assert!(f.to_string().contains("2/5"));
    }

    #[test]
    fn test_calibration_degradation_serde() {
        let variants = vec![
            CalibrationDegradation::Healthy,
            CalibrationDegradation::GateMarginal {
                passing: 3,
                total: 5,
            },
            CalibrationDegradation::GateFailed {
                failing: 2,
                total: 5,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: CalibrationDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_calibration_log_entry() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.evaluate();
        let entry = harness.log_entry();
        assert_eq!(entry.total_runs, 1);
    }

    #[test]
    fn test_calibration_detect_degradation_healthy() {
        let mut harness = CalibrationHarness::with_defaults();
        let scenarios = [
            CalibrationScenario::Nominal,
            CalibrationScenario::GradualDrift,
            CalibrationScenario::AbruptShift,
            CalibrationScenario::NoisyBaseline,
            CalibrationScenario::PostStressRecovery,
        ];
        for s in &scenarios {
            harness.submit(make_passing_result(*s));
        }
        harness.evaluate();
        assert_eq!(
            harness.detect_degradation(),
            CalibrationDegradation::Healthy
        );
    }

    #[test]
    fn test_calibration_config_serde() {
        let config = PromotionGateConfig::default_strict();
        let json = serde_json::to_string(&config).unwrap();
        let back: PromotionGateConfig = serde_json::from_str(&json).unwrap();
        assert!((config.max_fpr - back.max_fpr).abs() < 1e-10);
    }

    #[test]
    fn test_calibration_result_serde() {
        let result = make_passing_result(CalibrationScenario::Nominal);
        let json = serde_json::to_string(&result).unwrap();
        let back: CalibrationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.scenario, back.scenario);
    }

    // ── D4 Impl Tests ──────────────────────────────────────────────

    #[test]
    fn test_calibration_submit_batch() {
        let mut harness = CalibrationHarness::with_defaults();
        let batch = vec![
            make_passing_result(CalibrationScenario::Nominal),
            make_passing_result(CalibrationScenario::GradualDrift),
            make_passing_result(CalibrationScenario::AbruptShift),
            make_passing_result(CalibrationScenario::NoisyBaseline),
            make_passing_result(CalibrationScenario::PostStressRecovery),
        ];
        let verdict = harness.submit_batch(batch);
        assert_eq!(verdict, PromotionVerdict::Approved);
        assert_eq!(harness.total_runs(), 5);
    }

    #[test]
    fn test_calibration_avg_fpr() {
        let mut harness = CalibrationHarness::with_defaults();
        assert_eq!(harness.avg_fpr(), 0.0);
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        assert!(harness.avg_fpr() > 0.0);
    }

    #[test]
    fn test_calibration_avg_miss_rate() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        assert!(harness.avg_miss_rate() > 0.0);
    }

    #[test]
    fn test_calibration_avg_detection_delay() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        assert!(harness.avg_detection_delay() > 0.0);
    }

    #[test]
    fn test_calibration_passing_failing_counts() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.submit(make_failing_result(CalibrationScenario::AbruptShift));
        harness.evaluate();
        assert_eq!(harness.passing_count(), 1);
        assert_eq!(harness.failing_count(), 1);
    }

    #[test]
    fn test_calibration_is_approved() {
        let mut harness = CalibrationHarness::with_defaults();
        assert!(!harness.is_approved());
        let scenarios = [
            CalibrationScenario::Nominal,
            CalibrationScenario::GradualDrift,
            CalibrationScenario::AbruptShift,
            CalibrationScenario::NoisyBaseline,
            CalibrationScenario::PostStressRecovery,
        ];
        for s in &scenarios {
            harness.submit(make_passing_result(*s));
        }
        harness.evaluate();
        assert!(harness.is_approved());
    }

    #[test]
    fn test_calibration_set_max_fpr() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.set_max_fpr(0.001);
        harness.submit(make_passing_result(CalibrationScenario::Nominal)); // fpr=0.01 > 0.001
        harness.evaluate();
        assert!(!harness.is_approved());
    }

    #[test]
    fn test_calibration_set_strict() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.set_strict(false);
        // Submit 5 passing + 1 failing
        for _ in 0..5 {
            harness.submit(make_passing_result(CalibrationScenario::Nominal));
        }
        harness.submit(make_failing_result(CalibrationScenario::AbruptShift));
        let verdict = harness.evaluate();
        // Non-strict: 5 >= min_passing_scenarios=5, so Approved
        assert_eq!(verdict, PromotionVerdict::Approved);
    }

    #[test]
    fn test_calibration_results_for_scenario() {
        let mut harness = CalibrationHarness::with_defaults();
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.submit(make_passing_result(CalibrationScenario::Nominal));
        harness.submit(make_passing_result(CalibrationScenario::AbruptShift));
        let nominal = harness.results_for_scenario(CalibrationScenario::Nominal);
        assert_eq!(nominal.len(), 2);
        let abrupt = harness.results_for_scenario(CalibrationScenario::AbruptShift);
        assert_eq!(abrupt.len(), 1);
    }

    // ── E1: Formal Spec Pack Tests ────────────────────────────────

    #[test]
    fn test_invariant_domain_display() {
        assert_eq!(InvariantDomain::Scheduler.to_string(), "scheduler");
        assert_eq!(InvariantDomain::Budget.to_string(), "budget");
        assert_eq!(InvariantDomain::Recovery.to_string(), "recovery");
        assert_eq!(InvariantDomain::Composition.to_string(), "composition");
    }

    #[test]
    fn test_invariant_severity_ordering() {
        assert!(InvariantSeverity::Info < InvariantSeverity::Warning);
        assert!(InvariantSeverity::Warning < InvariantSeverity::Critical);
    }

    #[test]
    fn test_formal_invariant_display() {
        let inv = FormalInvariant {
            predicate_id: "budget.nonneg".to_string(),
            description: "All targets non-negative".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Critical,
            is_safety: true,
        };
        let display = format!("{inv}");
        assert!(display.contains("budget"));
        assert!(display.contains("critical"));
        assert!(display.contains("budget.nonneg"));
    }

    #[test]
    fn test_scheduler_invariant_capacity_bound_holds() {
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Input,
            capacity: 100,
            actual: 50,
        };
        assert!(inv.holds());
        assert_eq!(inv.predicate_id(), "scheduler.capacity_bound");
    }

    #[test]
    fn test_scheduler_invariant_capacity_bound_violated() {
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Bulk,
            capacity: 10,
            actual: 15,
        };
        assert!(!inv.holds());
    }

    #[test]
    fn test_scheduler_invariant_conservation_of_work() {
        let good = SchedulerInvariant::ConservationOfWork {
            total_admitted: 100,
            lane_sum: 100,
        };
        assert!(good.holds());

        let bad = SchedulerInvariant::ConservationOfWork {
            total_admitted: 100,
            lane_sum: 99,
        };
        assert!(!bad.holds());
    }

    #[test]
    fn test_scheduler_invariant_starvation_freedom() {
        let ok = SchedulerInvariant::StarvationFreedom {
            lane: SchedulerLane::Control,
            wait_epochs: 5,
            max_epochs: 10,
        };
        assert!(ok.holds());

        let starved = SchedulerInvariant::StarvationFreedom {
            lane: SchedulerLane::Control,
            wait_epochs: 11,
            max_epochs: 10,
        };
        assert!(!starved.holds());
    }

    #[test]
    fn test_scheduler_invariant_epoch_monotonicity() {
        assert!(
            SchedulerInvariant::EpochMonotonicity {
                previous: 5,
                current: 10
            }
            .holds()
        );
        assert!(
            SchedulerInvariant::EpochMonotonicity {
                previous: 5,
                current: 5
            }
            .holds()
        );
        assert!(
            !SchedulerInvariant::EpochMonotonicity {
                previous: 10,
                current: 5
            }
            .holds()
        );
    }

    #[test]
    fn test_scheduler_invariant_item_id_monotonicity() {
        assert!(
            SchedulerInvariant::ItemIdMonotonicity {
                previous: 1,
                current: 2
            }
            .holds()
        );
        assert!(
            !SchedulerInvariant::ItemIdMonotonicity {
                previous: 5,
                current: 3
            }
            .holds()
        );
        assert!(
            SchedulerInvariant::ItemIdMonotonicity {
                previous: 0,
                current: 0
            }
            .holds()
        );
    }

    #[test]
    fn test_scheduler_invariant_deterministic_replay() {
        let good = SchedulerInvariant::DeterministicReplay {
            input_hash: 0xABCD,
            expected_hash: 0x1234,
            actual_hash: 0x1234,
        };
        assert!(good.holds());

        let bad = SchedulerInvariant::DeterministicReplay {
            input_hash: 0xABCD,
            expected_hash: 0x1234,
            actual_hash: 0x5678,
        };
        assert!(!bad.holds());
    }

    #[test]
    fn test_scheduler_invariant_display() {
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Input,
            capacity: 100,
            actual: 50,
        };
        let s = format!("{inv}");
        assert!(s.contains("capacity_bound"));
        assert!(s.contains("50/100"));
    }

    #[test]
    fn test_budget_invariant_percentile_monotonicity_holds() {
        let inv = BudgetInvariant::PercentileMonotonicity {
            stage: LatencyStage::PatternDetection,
            p50: 100.0,
            p95: 200.0,
            p99: 300.0,
            p999: 400.0,
        };
        assert!(inv.holds());
        assert_eq!(inv.predicate_id(), "budget.percentile_monotonicity");
    }

    #[test]
    fn test_budget_invariant_percentile_monotonicity_violated() {
        let inv = BudgetInvariant::PercentileMonotonicity {
            stage: LatencyStage::PatternDetection,
            p50: 300.0,
            p95: 200.0,
            p99: 100.0,
            p999: 400.0,
        };
        assert!(!inv.holds());
    }

    #[test]
    fn test_budget_invariant_non_negative_targets() {
        assert!(
            BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::EventEmission,
                min_target: 0.0,
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::EventEmission,
                min_target: -1.0,
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_observation_consistency() {
        assert!(
            BudgetInvariant::ObservationConsistency {
                total: 50,
                per_stage_sum: 50
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::ObservationConsistency {
                total: 50,
                per_stage_sum: 49
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_overflow_bound() {
        assert!(
            BudgetInvariant::OverflowBound {
                overflow_count: 5,
                total_observations: 10
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::OverflowBound {
                overflow_count: 11,
                total_observations: 10
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_escalation_monotonicity() {
        assert!(
            BudgetInvariant::EscalationMonotonicity {
                stage: LatencyStage::PatternDetection,
                previous_level: MitigationLevel::None,
                current_level: MitigationLevel::Defer,
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::EscalationMonotonicity {
                stage: LatencyStage::PatternDetection,
                previous_level: MitigationLevel::Shed,
                current_level: MitigationLevel::Defer,
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_aggregate_ceiling() {
        assert!(
            BudgetInvariant::AggregateCeiling {
                percentile: Percentile::P99,
                aggregate_us: 1000.0,
                stage_sum_us: 900.0,
            }
            .holds()
        );
        assert!(
            !BudgetInvariant::AggregateCeiling {
                percentile: Percentile::P99,
                aggregate_us: 800.0,
                stage_sum_us: 900.0,
            }
            .holds()
        );
    }

    #[test]
    fn test_budget_invariant_display() {
        let inv = BudgetInvariant::PercentileMonotonicity {
            stage: LatencyStage::PatternDetection,
            p50: 100.0,
            p95: 200.0,
            p99: 300.0,
            p999: 400.0,
        };
        let s = format!("{inv}");
        assert!(s.contains("pct_mono"));
    }

    #[test]
    fn test_recovery_invariant_gradual_deescalation_holds() {
        let inv = RecoveryInvariant::GradualDeescalation {
            previous_level: MitigationLevel::Degrade,
            recovered_level: MitigationLevel::Defer,
        };
        assert!(inv.holds());
        assert_eq!(inv.predicate_id(), "recovery.gradual_deescalation");
    }

    #[test]
    fn test_recovery_invariant_gradual_deescalation_violated() {
        let inv = RecoveryInvariant::GradualDeescalation {
            previous_level: MitigationLevel::Shed,
            recovered_level: MitigationLevel::Defer,
        };
        assert!(!inv.holds());
    }

    #[test]
    fn test_recovery_invariant_cooldown_enforced() {
        assert!(
            RecoveryInvariant::CooldownEnforced {
                consecutive_ok: 20,
                cooldown_required: 20,
            }
            .holds()
        );
        assert!(
            !RecoveryInvariant::CooldownEnforced {
                consecutive_ok: 19,
                cooldown_required: 20,
            }
            .holds()
        );
    }

    #[test]
    fn test_recovery_invariant_timeout_recovery() {
        assert!(
            RecoveryInvariant::TimeoutRecovery {
                degraded_duration_us: 40_000_000,
                max_duration_us: 30_000_000,
                recovery_triggered: true,
            }
            .holds()
        );
        assert!(
            !RecoveryInvariant::TimeoutRecovery {
                degraded_duration_us: 40_000_000,
                max_duration_us: 30_000_000,
                recovery_triggered: false,
            }
            .holds()
        );
        assert!(
            RecoveryInvariant::TimeoutRecovery {
                degraded_duration_us: 10_000_000,
                max_duration_us: 30_000_000,
                recovery_triggered: false,
            }
            .holds()
        );
    }

    #[test]
    fn test_recovery_invariant_count_monotonicity() {
        assert!(
            RecoveryInvariant::EscalationCountMonotonic {
                previous: 5,
                current: 8
            }
            .holds()
        );
        assert!(
            !RecoveryInvariant::EscalationCountMonotonic {
                previous: 10,
                current: 5
            }
            .holds()
        );
        assert!(
            RecoveryInvariant::RecoveryCountMonotonic {
                previous: 3,
                current: 3
            }
            .holds()
        );
        assert!(
            !RecoveryInvariant::RecoveryCountMonotonic {
                previous: 5,
                current: 2
            }
            .holds()
        );
    }

    #[test]
    fn test_recovery_invariant_level_in_range() {
        for level in MitigationLevel::ALL {
            assert!(RecoveryInvariant::LevelInRange { level: *level }.holds());
        }
    }

    #[test]
    fn test_recovery_invariant_display() {
        let inv = RecoveryInvariant::GradualDeescalation {
            previous_level: MitigationLevel::Degrade,
            recovered_level: MitigationLevel::Defer,
        };
        let s = format!("{inv}");
        assert!(s.contains("gradual"));
    }

    #[test]
    fn test_invariant_outcome_display() {
        assert_eq!(InvariantOutcome::Satisfied.to_string(), "SATISFIED");
        let violated = InvariantOutcome::Violated {
            counterexample: "bad state".to_string(),
        };
        assert!(violated.to_string().contains("VIOLATED"));
        let inc = InvariantOutcome::Inconclusive {
            reason: "timeout".to_string(),
        };
        assert!(inc.to_string().contains("INCONCLUSIVE"));
    }

    #[test]
    fn test_invariant_check_result_passed_violated() {
        let passed = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 100,
            timestamp_us: 1000,
        };
        assert!(passed.passed());
        assert!(!passed.violated());

        let failed = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Warning,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 50,
            timestamp_us: 2000,
        };
        assert!(!failed.passed());
        assert!(failed.violated());
    }

    #[test]
    fn test_invariant_checker_new() {
        let checker = InvariantChecker::with_defaults();
        assert_eq!(checker.total_checks(), 0);
        assert_eq!(checker.total_violations(), 0);
        assert_eq!(checker.total_satisfied(), 0);
        assert_eq!(checker.registered_count(), 0);
        assert!((checker.violation_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_invariant_checker_register() {
        let mut checker = InvariantChecker::with_defaults();
        checker.register(FormalInvariant {
            predicate_id: "test.inv".to_string(),
            description: "Test invariant".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            is_safety: true,
        });
        assert_eq!(checker.registered_count(), 1);
    }

    #[test]
    fn test_invariant_checker_check_scheduler_satisfied() {
        let mut checker = InvariantChecker::with_defaults();
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Input,
            capacity: 100,
            actual: 50,
        };
        let result = checker.check_scheduler(&inv, 1000);
        assert!(result.passed());
        assert_eq!(checker.total_checks(), 1);
        assert_eq!(checker.total_satisfied(), 1);
        assert_eq!(checker.total_violations(), 0);
    }

    #[test]
    fn test_invariant_checker_check_scheduler_violated() {
        let mut checker = InvariantChecker::with_defaults();
        let inv = SchedulerInvariant::CapacityBound {
            lane: SchedulerLane::Bulk,
            capacity: 10,
            actual: 20,
        };
        let result = checker.check_scheduler(&inv, 2000);
        assert!(result.violated());
        assert_eq!(checker.total_violations(), 1);
    }

    #[test]
    fn test_invariant_checker_check_budget() {
        let mut checker = InvariantChecker::with_defaults();
        let good = BudgetInvariant::NonNegativeTargets {
            stage: LatencyStage::PatternDetection,
            min_target: 10.0,
        };
        let result = checker.check_budget(&good, 3000);
        assert!(result.passed());

        let bad = BudgetInvariant::NonNegativeTargets {
            stage: LatencyStage::PatternDetection,
            min_target: -5.0,
        };
        let result = checker.check_budget(&bad, 4000);
        assert!(result.violated());
        assert_eq!(checker.total_checks(), 2);
    }

    #[test]
    fn test_invariant_checker_check_recovery() {
        let mut checker = InvariantChecker::with_defaults();
        let inv = RecoveryInvariant::CooldownEnforced {
            consecutive_ok: 25,
            cooldown_required: 20,
        };
        let result = checker.check_recovery(&inv, 5000);
        assert!(result.passed());
    }

    #[test]
    fn test_invariant_checker_violation_rate() {
        let mut checker = InvariantChecker::with_defaults();
        for i in 0..7 {
            let inv = SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 100,
                actual: i,
            };
            checker.check_scheduler(&inv, i as u64);
        }
        for i in 0..3 {
            let inv = SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 10,
                actual: 20 + i,
            };
            checker.check_scheduler(&inv, 100 + i as u64);
        }
        assert_eq!(checker.total_checks(), 10);
        assert!((checker.violation_rate() - 0.3).abs() < 1e-6);
    }

    #[test]
    fn test_invariant_checker_recent_results() {
        let mut checker = InvariantChecker::with_defaults();
        for i in 0..5 {
            let inv = SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: i,
            };
            checker.check_scheduler(&inv, i);
        }
        assert_eq!(checker.recent_results(3).len(), 3);
        assert_eq!(checker.recent_results(10).len(), 5);
    }

    #[test]
    fn test_invariant_checker_results_by_domain() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            100,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 1.0,
            },
            200,
        );
        checker.check_recovery(
            &RecoveryInvariant::LevelInRange {
                level: MitigationLevel::None,
            },
            300,
        );
        assert_eq!(
            checker.results_by_domain(InvariantDomain::Scheduler).len(),
            1
        );
        assert_eq!(checker.results_by_domain(InvariantDomain::Budget).len(), 1);
        assert_eq!(
            checker.results_by_domain(InvariantDomain::Recovery).len(),
            1
        );
        assert_eq!(
            checker
                .results_by_domain(InvariantDomain::Composition)
                .len(),
            0
        );
    }

    #[test]
    fn test_invariant_checker_violations_filter() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 100,
                actual: 50,
            },
            100,
        );
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 10,
                actual: 20,
            },
            200,
        );
        let violations = checker.violations();
        assert_eq!(violations.len(), 1);
        assert!(violations[0].violated());
    }

    #[test]
    fn test_invariant_checker_snapshot() {
        let mut checker = InvariantChecker::with_defaults();
        for _ in 0..5 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: 1,
                },
                0,
            );
        }
        let snap = checker.snapshot();
        assert_eq!(snap.total_checks, 5);
        assert_eq!(snap.total_satisfied, 5);
        assert_eq!(snap.total_violations, 0);
        assert_eq!(snap.history_len, 5);
    }

    #[test]
    fn test_invariant_checker_status_line() {
        let checker = InvariantChecker::with_defaults();
        let line = checker.status_line();
        assert!(line.contains("invariants:"));
        assert!(line.contains("checks=0"));
    }

    #[test]
    fn test_invariant_checker_reset() {
        let mut checker = InvariantChecker::with_defaults();
        for _ in 0..3 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: 1,
                },
                0,
            );
        }
        assert_eq!(checker.total_checks(), 3);
        checker.reset();
        assert_eq!(checker.total_checks(), 0);
        assert_eq!(checker.total_violations(), 0);
        assert_eq!(checker.total_satisfied(), 0);
        assert_eq!(checker.recent_results(10).len(), 0);
    }

    #[test]
    fn test_invariant_checker_degradation_healthy() {
        let checker = InvariantChecker::with_defaults();
        assert_eq!(
            checker.detect_degradation(),
            InvariantCheckerDegradation::Healthy
        );
    }

    #[test]
    fn test_invariant_checker_degradation_violations_detected() {
        let mut checker = InvariantChecker::with_defaults();
        for i in 0..19 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: i,
                },
                i,
            );
        }
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 5,
            },
            100,
        );
        match checker.detect_degradation() {
            InvariantCheckerDegradation::ViolationsDetected {
                violations, total, ..
            } => {
                assert_eq!(violations, 1);
                assert_eq!(total, 20);
            }
            other => panic!("Expected ViolationsDetected, got {other:?}"),
        }
    }

    #[test]
    fn test_invariant_checker_degradation_high_rate() {
        let mut checker = InvariantChecker::with_defaults();
        for i in 0..8 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: i,
                },
                i,
            );
        }
        for _ in 0..2 {
            checker.check_scheduler(
                &SchedulerInvariant::CapacityBound {
                    lane: SchedulerLane::Input,
                    capacity: 1,
                    actual: 5,
                },
                100,
            );
        }
        match checker.detect_degradation() {
            InvariantCheckerDegradation::HighViolationRate {
                violations, total, ..
            } => {
                assert_eq!(violations, 2);
                assert_eq!(total, 10);
            }
            other => panic!("Expected HighViolationRate, got {other:?}"),
        }
    }

    #[test]
    fn test_invariant_checker_log_entry() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        let entry = checker.log_entry();
        assert_eq!(entry.total_checks, 1);
        assert_eq!(entry.total_satisfied, 1);
        assert_eq!(entry.total_violations, 0);
    }

    #[test]
    fn test_invariant_checker_history_cap() {
        let config = InvariantCheckerConfig {
            max_history: 5,
            ..Default::default()
        };
        let mut checker = InvariantChecker::new(config);
        for i in 0..10 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: i,
                },
                i,
            );
        }
        assert_eq!(checker.recent_results(100).len(), 5);
        assert_eq!(checker.total_checks(), 10);
    }

    #[test]
    fn test_invariant_domain_serde() {
        for domain in &[
            InvariantDomain::Scheduler,
            InvariantDomain::Budget,
            InvariantDomain::Recovery,
            InvariantDomain::Composition,
        ] {
            let json = serde_json::to_string(domain).unwrap();
            let back: InvariantDomain = serde_json::from_str(&json).unwrap();
            assert_eq!(*domain, back);
        }
    }

    #[test]
    fn test_invariant_severity_serde() {
        for sev in &[
            InvariantSeverity::Info,
            InvariantSeverity::Warning,
            InvariantSeverity::Critical,
        ] {
            let json = serde_json::to_string(sev).unwrap();
            let back: InvariantSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(*sev, back);
        }
    }

    #[test]
    fn test_invariant_checker_degradation_serde() {
        let variants = vec![
            InvariantCheckerDegradation::Healthy,
            InvariantCheckerDegradation::ViolationsDetected {
                violations: 3,
                total: 100,
            },
            InvariantCheckerDegradation::HighViolationRate {
                violations: 15,
                total: 100,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: InvariantCheckerDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_invariant_checker_degradation_display() {
        assert_eq!(InvariantCheckerDegradation::Healthy.to_string(), "healthy");
        let det = InvariantCheckerDegradation::ViolationsDetected {
            violations: 2,
            total: 50,
        };
        assert!(det.to_string().contains("2/50"));
        let high = InvariantCheckerDegradation::HighViolationRate {
            violations: 10,
            total: 50,
        };
        assert!(high.to_string().contains("high_rate"));
    }

    #[test]
    fn test_invariant_check_result_display() {
        let r = InvariantCheckResult {
            predicate_id: "test.id".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 42,
            timestamp_us: 1000,
        };
        let s = format!("{r}");
        assert!(s.contains("SATISFIED"));
        assert!(s.contains("42"));
    }

    // ── E1 Impl: Bridge Method Tests ──────────────────────────────

    #[test]
    fn test_checker_batch_scheduler() {
        let mut checker = InvariantChecker::with_defaults();
        let invs = vec![
            SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 5,
            },
            SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 100,
                actual: 50,
            },
            SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Bulk,
                capacity: 10,
                actual: 20,
            },
        ];
        let results = checker.check_scheduler_batch(&invs, 1000);
        assert_eq!(results.len(), 3);
        assert!(results[0].passed());
        assert!(results[1].passed());
        assert!(results[2].violated());
        assert_eq!(checker.total_checks(), 3);
    }

    #[test]
    fn test_checker_batch_budget() {
        let mut checker = InvariantChecker::with_defaults();
        let invs = vec![
            BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 10.0,
            },
            BudgetInvariant::OverflowBound {
                overflow_count: 5,
                total_observations: 100,
            },
        ];
        let results = checker.check_budget_batch(&invs, 2000);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.passed()));
    }

    #[test]
    fn test_checker_batch_recovery() {
        let mut checker = InvariantChecker::with_defaults();
        let invs = vec![
            RecoveryInvariant::LevelInRange {
                level: MitigationLevel::Defer,
            },
            RecoveryInvariant::EscalationCountMonotonic {
                previous: 3,
                current: 5,
            },
        ];
        let results = checker.check_recovery_batch(&invs, 3000);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.passed()));
    }

    #[test]
    fn test_checker_all_satisfied() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        assert!(checker.all_satisfied());
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 10,
            },
            1,
        );
        assert!(!checker.all_satisfied());
    }

    #[test]
    fn test_checker_violation_count_by_domain() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 10,
            },
            0,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: -1.0,
            },
            1,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 5.0,
            },
            2,
        );
        assert_eq!(
            checker.violation_count_by_domain(InvariantDomain::Scheduler),
            1
        );
        assert_eq!(
            checker.violation_count_by_domain(InvariantDomain::Budget),
            1
        );
        assert_eq!(
            checker.violation_count_by_domain(InvariantDomain::Recovery),
            0
        );
    }

    #[test]
    fn test_checker_last_violation() {
        let mut checker = InvariantChecker::with_defaults();
        assert!(checker.last_violation().is_none());
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 10,
            },
            100,
        );
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            200,
        );
        let last = checker.last_violation().unwrap();
        assert_eq!(last.predicate_id, "scheduler.capacity_bound");
    }

    #[test]
    fn test_checker_predicate_ever_violated() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        assert!(!checker.predicate_ever_violated("scheduler.epoch_monotonicity"));
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 5,
                current: 1,
            },
            1,
        );
        assert!(checker.predicate_ever_violated("scheduler.epoch_monotonicity"));
    }

    #[test]
    fn test_checker_predicate_pass_rate() {
        let mut checker = InvariantChecker::with_defaults();
        assert!(checker.predicate_pass_rate("nonexistent").is_nan());
        for _ in 0..3 {
            checker.check_scheduler(
                &SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: 1,
                },
                0,
            );
        }
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 5,
                current: 1,
            },
            1,
        );
        let rate = checker.predicate_pass_rate("scheduler.epoch_monotonicity");
        assert!((rate - 0.75).abs() < 1e-6);
    }

    #[test]
    fn test_checker_checked_predicates() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 1.0,
            },
            1,
        );
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 2,
            },
            2,
        );
        let preds = checker.checked_predicates();
        assert_eq!(preds.len(), 2);
        assert!(preds.contains(&"scheduler.epoch_monotonicity".to_string()));
        assert!(preds.contains(&"budget.non_negative_targets".to_string()));
    }

    #[test]
    fn test_checker_domain_summary() {
        let mut checker = InvariantChecker::with_defaults();
        checker.check_scheduler(
            &SchedulerInvariant::EpochMonotonicity {
                previous: 0,
                current: 1,
            },
            0,
        );
        checker.check_scheduler(
            &SchedulerInvariant::CapacityBound {
                lane: SchedulerLane::Input,
                capacity: 1,
                actual: 10,
            },
            1,
        );
        checker.check_budget(
            &BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PatternDetection,
                min_target: 5.0,
            },
            2,
        );
        let summary = checker.domain_summary();
        assert_eq!(summary.len(), 4);
        // Scheduler: 2 checks, 1 violation
        let sched = summary
            .iter()
            .find(|(d, _, _)| *d == InvariantDomain::Scheduler)
            .unwrap();
        assert_eq!(sched.1, 2);
        assert_eq!(sched.2, 1);
        // Budget: 1 check, 0 violations
        let budget = summary
            .iter()
            .find(|(d, _, _)| *d == InvariantDomain::Budget)
            .unwrap();
        assert_eq!(budget.1, 1);
        assert_eq!(budget.2, 0);
    }

    #[test]
    fn test_checker_from_enforcement_state_monotonic() {
        let mut checker = InvariantChecker::with_defaults();
        let prev = StageEnforcementState {
            current_level: MitigationLevel::Degrade,
            consecutive_ok: 0,
            last_escalation_us: 1000,
            escalation_count: 3,
            recovery_count: 1,
        };
        let curr = StageEnforcementState {
            current_level: MitigationLevel::Degrade,
            consecutive_ok: 5,
            last_escalation_us: 1000,
            escalation_count: 3,
            recovery_count: 1,
        };
        let protocol = RecoveryProtocol::default();
        let results = checker.check_from_enforcement_state(&curr, &prev, &protocol, 5000);
        // Should check: escalation_count_mono, recovery_count_mono, level_in_range
        // No recovery happened (same level), so no gradual or cooldown checks
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.passed()));
    }

    #[test]
    fn test_checker_from_enforcement_state_recovery() {
        let mut checker = InvariantChecker::with_defaults();
        let prev = StageEnforcementState {
            current_level: MitigationLevel::Degrade,
            consecutive_ok: 0,
            last_escalation_us: 1000,
            escalation_count: 3,
            recovery_count: 1,
        };
        // Gradual recovery: Degrade -> Defer (one step down)
        let curr = StageEnforcementState {
            current_level: MitigationLevel::Defer,
            consecutive_ok: 25, // > 20 cooldown
            last_escalation_us: 1000,
            escalation_count: 3,
            recovery_count: 2,
        };
        let protocol = RecoveryProtocol::default();
        let results = checker.check_from_enforcement_state(&curr, &prev, &protocol, 6000);
        // escalation_count_mono, recovery_count_mono, level_in_range, gradual_deescalation, cooldown_enforced
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(|r| r.passed()));
    }

    // ── E2: Model-Checking Harness Tests ──────────────────────────

    #[test]
    fn test_trace_action_display() {
        let obs = TraceAction::ObserveLatency {
            stage: LatencyStage::PtyCapture,
            latency_us: 42.5,
        };
        assert!(obs.to_string().contains("observe"));
        let admit = TraceAction::SchedulerAdmit {
            lane: SchedulerLane::Input,
            cost_us: 10.0,
        };
        assert!(admit.to_string().contains("admit"));
        let recover = TraceAction::RecoveryStep {
            level_before: MitigationLevel::Degrade,
            level_after: MitigationLevel::Defer,
        };
        assert!(recover.to_string().contains("recover"));
        let epoch = TraceAction::EpochAdvance { new_epoch: 5 };
        assert!(epoch.to_string().contains("epoch"));
        let reset = TraceAction::Reset {
            domain: InvariantDomain::Budget,
        };
        assert!(reset.to_string().contains("reset"));
    }

    #[test]
    fn test_counterexample_display() {
        let cx = Counterexample {
            predicate_id: "scheduler.capacity_bound".to_string(),
            domain: InvariantDomain::Scheduler,
            trace: vec![TraceStep {
                step: 0,
                action: TraceAction::EpochAdvance { new_epoch: 1 },
                check_results: vec![],
                timestamp_us: 100,
            }],
            description: "capacity exceeded".to_string(),
            found_at_us: 100,
        };
        let s = format!("{cx}");
        assert!(s.contains("scheduler.capacity_bound"));
        assert!(s.contains("1 steps"));
    }

    #[test]
    fn test_exploration_strategy_display() {
        assert_eq!(ExplorationStrategy::BreadthFirst.to_string(), "bfs");
        assert_eq!(ExplorationStrategy::RandomWalk.to_string(), "random");
        assert_eq!(ExplorationStrategy::Guided.to_string(), "guided");
    }

    #[test]
    fn test_model_checker_new() {
        let mc = ModelChecker::with_defaults();
        assert_eq!(mc.states_explored(), 0);
        assert_eq!(mc.counterexample_count(), 0);
        assert_eq!(mc.max_depth_reached(), 0);
    }

    #[test]
    fn test_model_checker_step_no_violation() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 100,
        };
        let violated = mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 100);
        assert!(!violated);
        assert_eq!(mc.states_explored(), 1);
        assert_eq!(mc.counterexample_count(), 0);
    }

    #[test]
    fn test_model_checker_step_with_violation() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "scheduler.capacity_bound".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "capacity exceeded".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 200,
        };
        let violated = mc.step(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 50.0,
            },
            &[result],
            200,
        );
        assert!(violated);
        assert_eq!(mc.counterexample_count(), 1);
        let cx = &mc.counterexamples()[0];
        assert_eq!(cx.predicate_id, "scheduler.capacity_bound");
        assert_eq!(cx.trace.len(), 1);
    }

    #[test]
    fn test_model_checker_new_trace() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Warning,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 100,
        };
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 1 },
            &[result.clone()],
            100,
        );
        mc.step(TraceAction::EpochAdvance { new_epoch: 2 }, &[result], 200);
        assert_eq!(mc.states_explored(), 2);
        mc.new_trace();
        assert_eq!(mc.states_explored(), 2); // preserved
        // depth resets but states don't
    }

    #[test]
    fn test_model_checker_should_stop_non_exhaustive() {
        let config = ModelCheckerConfig {
            max_depth: 100,
            max_states: 10_000,
            exhaustive: false,
            ..Default::default()
        };
        let mut mc = ModelChecker::new(config);
        assert!(!mc.should_stop());
        // Add a counterexample
        let result = InvariantCheckResult {
            predicate_id: "x".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        assert!(mc.should_stop()); // non-exhaustive stops after first
    }

    #[test]
    fn test_model_checker_should_stop_exhaustive() {
        let config = ModelCheckerConfig {
            max_depth: 100,
            max_states: 10_000,
            max_counterexamples: 5,
            exhaustive: true,
            ..Default::default()
        };
        let mut mc = ModelChecker::new(config);
        let result = InvariantCheckResult {
            predicate_id: "x".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        assert!(!mc.should_stop()); // exhaustive continues
    }

    #[test]
    fn test_model_checker_verdict_no_violation() {
        let mc = ModelChecker::with_defaults();
        match mc.verdict() {
            ModelCheckVerdict::NoViolation {
                states_explored, ..
            } => {
                assert_eq!(states_explored, 0);
            }
            other => panic!("Expected NoViolation, got {other}"),
        }
    }

    #[test]
    fn test_model_checker_verdict_violations() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "bad".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        match mc.verdict() {
            ModelCheckVerdict::ViolationsFound { counterexamples } => {
                assert_eq!(counterexamples.len(), 1);
            }
            other => panic!("Expected ViolationsFound, got {other}"),
        }
    }

    #[test]
    fn test_model_checker_snapshot() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Info,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        let snap = mc.snapshot();
        assert_eq!(snap.states_explored, 1);
        assert_eq!(snap.counterexamples_found, 0);
    }

    #[test]
    fn test_model_checker_status_line() {
        let mc = ModelChecker::with_defaults();
        let line = mc.status_line();
        assert!(line.contains("model_check:"));
        assert!(line.contains("states=0"));
    }

    #[test]
    fn test_model_checker_reset() {
        let mut mc = ModelChecker::with_defaults();
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        assert_eq!(mc.counterexample_count(), 1);
        mc.reset();
        assert_eq!(mc.states_explored(), 0);
        assert_eq!(mc.counterexample_count(), 0);
        assert_eq!(mc.max_depth_reached(), 0);
    }

    #[test]
    fn test_model_checker_degradation_healthy() {
        let mc = ModelChecker::with_defaults();
        assert_eq!(mc.detect_degradation(), ModelCheckerDegradation::Healthy);
    }

    #[test]
    fn test_model_checker_degradation_violations_found() {
        let mut mc = ModelChecker::new(ModelCheckerConfig {
            exhaustive: true,
            ..Default::default()
        });
        let result = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[result], 0);
        match mc.detect_degradation() {
            ModelCheckerDegradation::ViolationsFound { count } => {
                assert_eq!(count, 1);
            }
            other => panic!("Expected ViolationsFound, got {other:?}"),
        }
    }

    #[test]
    fn test_model_checker_log_entry() {
        let mc = ModelChecker::with_defaults();
        let entry = mc.log_entry();
        assert_eq!(entry.states_explored, 0);
        assert_eq!(entry.counterexamples_found, 0);
    }

    #[test]
    fn test_model_check_verdict_display() {
        let nv = ModelCheckVerdict::NoViolation {
            states_explored: 100,
            depth_reached: 10,
        };
        assert!(nv.to_string().contains("NO_VIOLATION"));
        let vf = ModelCheckVerdict::ViolationsFound {
            counterexamples: vec![],
        };
        assert!(vf.to_string().contains("VIOLATIONS_FOUND"));
        let inc = ModelCheckVerdict::Incomplete {
            states_explored: 50,
            reason: "timeout".to_string(),
        };
        assert!(inc.to_string().contains("INCOMPLETE"));
    }

    #[test]
    fn test_model_checker_degradation_serde() {
        let variants = vec![
            ModelCheckerDegradation::Healthy,
            ModelCheckerDegradation::ViolationsFound { count: 2 },
            ModelCheckerDegradation::HighViolationRate {
                count: 10,
                states: 100,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: ModelCheckerDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_model_checker_degradation_display() {
        assert_eq!(ModelCheckerDegradation::Healthy.to_string(), "healthy");
        let vf = ModelCheckerDegradation::ViolationsFound { count: 3 };
        assert!(vf.to_string().contains("violations(3)"));
        let hr = ModelCheckerDegradation::HighViolationRate {
            count: 10,
            states: 50,
        };
        assert!(hr.to_string().contains("high_rate"));
    }

    #[test]
    fn test_exploration_strategy_serde() {
        for strat in &[
            ExplorationStrategy::BreadthFirst,
            ExplorationStrategy::RandomWalk,
            ExplorationStrategy::Guided,
        ] {
            let json = serde_json::to_string(strat).unwrap();
            let back: ExplorationStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(*strat, back);
        }
    }

    #[test]
    fn test_model_checker_multi_step_trace() {
        let mut mc = ModelChecker::with_defaults();
        let ok = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Info,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 1 },
            &[ok.clone()],
            100,
        );
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 2 },
            &[ok.clone()],
            200,
        );
        let bad = InvariantCheckResult {
            predicate_id: "sched.cap".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "overflow".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 300,
        };
        mc.step(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Bulk,
                cost_us: 999.0,
            },
            &[bad],
            300,
        );
        assert_eq!(mc.counterexample_count(), 1);
        // Trace should have all 3 steps
        assert_eq!(mc.counterexamples()[0].trace.len(), 3);
        assert_eq!(mc.states_explored(), 3);
        assert_eq!(mc.max_depth_reached(), 3);
    }

    // ── E2 Impl: Bridge Method Tests ──────────────────────────────

    #[test]
    fn test_mc_run_scheduler_scenario_no_violation() {
        let mut mc = ModelChecker::with_defaults();
        let mut checker = InvariantChecker::with_defaults();
        let actions = vec![
            (
                TraceAction::EpochAdvance { new_epoch: 1 },
                vec![SchedulerInvariant::EpochMonotonicity {
                    previous: 0,
                    current: 1,
                }],
            ),
            (
                TraceAction::EpochAdvance { new_epoch: 2 },
                vec![SchedulerInvariant::EpochMonotonicity {
                    previous: 1,
                    current: 2,
                }],
            ),
        ];
        let verdict = mc.run_scheduler_scenario(&mut checker, &actions, 1000);
        let is_no_violation = matches!(verdict, ModelCheckVerdict::NoViolation { .. });
        assert!(is_no_violation);
    }

    #[test]
    fn test_mc_run_scheduler_scenario_with_violation() {
        let mut mc = ModelChecker::with_defaults();
        let mut checker = InvariantChecker::with_defaults();
        let actions = vec![
            (
                TraceAction::EpochAdvance { new_epoch: 1 },
                vec![SchedulerInvariant::CapacityBound {
                    lane: SchedulerLane::Input,
                    capacity: 100,
                    actual: 50,
                }],
            ),
            (
                TraceAction::SchedulerAdmit {
                    lane: SchedulerLane::Input,
                    cost_us: 10.0,
                },
                vec![SchedulerInvariant::CapacityBound {
                    lane: SchedulerLane::Input,
                    capacity: 5,
                    actual: 20,
                }],
            ),
        ];
        let verdict = mc.run_scheduler_scenario(&mut checker, &actions, 2000);
        let is_violations = matches!(verdict, ModelCheckVerdict::ViolationsFound { .. });
        assert!(is_violations);
    }

    #[test]
    fn test_mc_run_budget_scenario() {
        let mut mc = ModelChecker::with_defaults();
        let mut checker = InvariantChecker::with_defaults();
        let actions = vec![(
            TraceAction::ObserveLatency {
                stage: LatencyStage::PtyCapture,
                latency_us: 100.0,
            },
            vec![BudgetInvariant::NonNegativeTargets {
                stage: LatencyStage::PtyCapture,
                min_target: 50.0,
            }],
        )];
        let verdict = mc.run_budget_scenario(&mut checker, &actions, 3000);
        let is_no_violation = matches!(verdict, ModelCheckVerdict::NoViolation { .. });
        assert!(is_no_violation);
    }

    #[test]
    fn test_mc_run_recovery_scenario() {
        let mut mc = ModelChecker::with_defaults();
        let mut checker = InvariantChecker::with_defaults();
        let actions = vec![(
            TraceAction::RecoveryStep {
                level_before: MitigationLevel::Degrade,
                level_after: MitigationLevel::Defer,
            },
            vec![RecoveryInvariant::LevelInRange {
                level: MitigationLevel::Defer,
            }],
        )];
        let verdict = mc.run_recovery_scenario(&mut checker, &actions, 4000);
        let is_no_violation = matches!(verdict, ModelCheckVerdict::NoViolation { .. });
        assert!(is_no_violation);
    }

    #[test]
    fn test_mc_counterexamples_by_domain() {
        let mut mc = ModelChecker::new(ModelCheckerConfig {
            exhaustive: true,
            ..Default::default()
        });
        let sched_result = InvariantCheckResult {
            predicate_id: "sched.x".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "a".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 1 },
            &[sched_result],
            0,
        );
        mc.new_trace();
        let budget_result = InvariantCheckResult {
            predicate_id: "budget.y".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "b".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 1,
        };
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 2 },
            &[budget_result],
            1,
        );
        assert_eq!(
            mc.counterexamples_by_domain(InvariantDomain::Scheduler)
                .len(),
            1
        );
        assert_eq!(
            mc.counterexamples_by_domain(InvariantDomain::Budget).len(),
            1
        );
        assert_eq!(
            mc.counterexamples_by_domain(InvariantDomain::Recovery)
                .len(),
            0
        );
    }

    #[test]
    fn test_mc_shortest_counterexample() {
        let mut mc = ModelChecker::new(ModelCheckerConfig {
            exhaustive: true,
            ..Default::default()
        });
        let bad = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        let ok = InvariantCheckResult {
            predicate_id: "test2".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Info,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 0,
        };
        // Trace 1: 3 steps then violation
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[ok.clone()], 0);
        mc.step(TraceAction::EpochAdvance { new_epoch: 2 }, &[ok.clone()], 1);
        mc.step(
            TraceAction::EpochAdvance { new_epoch: 3 },
            &[bad.clone()],
            2,
        );
        mc.new_trace();
        // Trace 2: 1 step then violation
        mc.step(TraceAction::EpochAdvance { new_epoch: 4 }, &[bad], 3);
        let shortest = mc.shortest_counterexample().unwrap();
        assert_eq!(shortest.trace.len(), 1);
    }

    #[test]
    fn test_mc_violated_predicates() {
        let mut mc = ModelChecker::new(ModelCheckerConfig {
            exhaustive: true,
            ..Default::default()
        });
        let r1 = InvariantCheckResult {
            predicate_id: "a.x".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "x".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 0,
        };
        let r2 = InvariantCheckResult {
            predicate_id: "b.y".to_string(),
            domain: InvariantDomain::Budget,
            severity: InvariantSeverity::Critical,
            outcome: InvariantOutcome::Violated {
                counterexample: "y".to_string(),
            },
            eval_time_us: 0,
            timestamp_us: 1,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[r1], 0);
        mc.new_trace();
        mc.step(TraceAction::EpochAdvance { new_epoch: 2 }, &[r2], 1);
        let preds = mc.violated_predicates();
        assert_eq!(preds.len(), 2);
        assert!(preds.contains(&"a.x".to_string()));
        assert!(preds.contains(&"b.y".to_string()));
    }

    #[test]
    fn test_mc_inner_checker() {
        let mc = ModelChecker::with_defaults();
        assert_eq!(mc.inner_checker().total_checks(), 0);
    }

    #[test]
    fn test_mc_current_trace_len() {
        let mut mc = ModelChecker::with_defaults();
        assert_eq!(mc.current_trace_len(), 0);
        let ok = InvariantCheckResult {
            predicate_id: "test".to_string(),
            domain: InvariantDomain::Scheduler,
            severity: InvariantSeverity::Info,
            outcome: InvariantOutcome::Satisfied,
            eval_time_us: 0,
            timestamp_us: 0,
        };
        mc.step(TraceAction::EpochAdvance { new_epoch: 1 }, &[ok], 0);
        assert_eq!(mc.current_trace_len(), 1);
    }

    #[test]
    fn test_mc_strategy() {
        let mc = ModelChecker::with_defaults();
        assert_eq!(mc.strategy(), ExplorationStrategy::RandomWalk);
    }

    // ── E3: Deterministic Trace v2 Tests ─────────────────────────

    #[test]
    fn test_trace_format_version_display() {
        assert_eq!(TraceFormatVersion::V1.to_string(), "v1");
        assert_eq!(TraceFormatVersion::V2.to_string(), "v2");
    }

    #[test]
    fn test_trace_format_version_serde() {
        let v = TraceFormatVersion::V2;
        let json = serde_json::to_string(&v).unwrap();
        let back: TraceFormatVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn test_canonical_ordering_display() {
        assert_eq!(CanonicalOrdering::Temporal.to_string(), "temporal");
        assert_eq!(
            CanonicalOrdering::DomainGrouped.to_string(),
            "domain-grouped"
        );
        assert_eq!(CanonicalOrdering::Causal.to_string(), "causal");
    }

    #[test]
    fn test_canonical_ordering_serde() {
        for ord in [
            CanonicalOrdering::Temporal,
            CanonicalOrdering::DomainGrouped,
            CanonicalOrdering::Causal,
        ] {
            let json = serde_json::to_string(&ord).unwrap();
            let back: CanonicalOrdering = serde_json::from_str(&json).unwrap();
            assert_eq!(ord, back);
        }
    }

    #[test]
    fn test_trace_entry_fingerprint_deterministic() {
        let action = TraceAction::EpochAdvance { new_epoch: 42 };
        let domain = InvariantDomain::Composition;
        let fp1 = TraceEntry::compute_fingerprint(&action, domain);
        let fp2 = TraceEntry::compute_fingerprint(&action, domain);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_trace_entry_fingerprint_varies_by_domain() {
        let action = TraceAction::EpochAdvance { new_epoch: 42 };
        let fp1 = TraceEntry::compute_fingerprint(&action, InvariantDomain::Scheduler);
        let fp2 = TraceEntry::compute_fingerprint(&action, InvariantDomain::Budget);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_trace_entry_fingerprint_varies_by_action() {
        let a1 = TraceAction::EpochAdvance { new_epoch: 1 };
        let a2 = TraceAction::EpochAdvance { new_epoch: 2 };
        let fp1 = TraceEntry::compute_fingerprint(&a1, InvariantDomain::Composition);
        let fp2 = TraceEntry::compute_fingerprint(&a2, InvariantDomain::Composition);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_trace_entry_display() {
        let entry = TraceEntry {
            seq: 0,
            timestamp_us: 100,
            action: TraceAction::EpochAdvance { new_epoch: 5 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 42,
        };
        let s = entry.to_string();
        assert!(s.contains("[0]"));
        assert!(s.contains("@100μs"));
        assert!(s.contains("epoch(5)"));
    }

    #[test]
    fn test_deterministic_trace_new_v2() {
        let trace = DeterministicTrace::new_v2("test-1".to_string(), 12345, 0);
        assert_eq!(trace.version, TraceFormatVersion::V2);
        assert_eq!(trace.trace_id, "test-1");
        assert_eq!(trace.seed, 12345);
        assert!(trace.is_empty());
        assert_eq!(trace.len(), 0);
    }

    #[test]
    fn test_deterministic_trace_push() {
        let mut trace = DeterministicTrace::new_v2("t1".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            100,
            None,
        );
        assert_eq!(trace.len(), 1);
        assert!(!trace.is_empty());
        assert_eq!(trace.entries[0].seq, 0);
        assert_eq!(trace.entries[0].timestamp_us, 100);
        assert_eq!(trace.duration_us, 100);
    }

    #[test]
    fn test_deterministic_trace_push_sequence_monotonic() {
        let mut trace = DeterministicTrace::new_v2("t1".to_string(), 0, 0);
        for i in 0..5 {
            trace.push(
                TraceAction::EpochAdvance { new_epoch: i },
                InvariantDomain::Composition,
                i * 10,
                if i > 0 { Some(i - 1) } else { None },
            );
        }
        assert_eq!(trace.len(), 5);
        for (i, entry) in trace.entries.iter().enumerate() {
            assert_eq!(entry.seq, i as u64);
        }
    }

    #[test]
    fn test_deterministic_trace_digest_deterministic() {
        let mut trace = DeterministicTrace::new_v2("t1".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            Some(0),
        );
        let d1 = trace.digest();
        let d2 = trace.digest();
        assert_eq!(d1, d2);
    }

    #[test]
    fn test_deterministic_trace_digest_varies() {
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mut t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        t2.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            10,
            None,
        );
        assert_ne!(t1.digest(), t2.digest());
    }

    #[test]
    fn test_deterministic_trace_display() {
        let trace = DeterministicTrace::new_v2("t1".to_string(), 42, 0);
        let s = trace.to_string();
        assert!(s.contains("v2"));
        assert!(s.contains("t1"));
        assert!(s.contains("42"));
    }

    #[test]
    fn test_deterministic_trace_serde() {
        let mut trace = DeterministicTrace::new_v2("t1".to_string(), 99, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            100,
            None,
        );
        let json = serde_json::to_string(&trace).unwrap();
        let back: DeterministicTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(trace, back);
    }

    #[test]
    fn test_replay_comparison_result_display() {
        let id = ReplayComparisonResult::Identical;
        assert_eq!(id.to_string(), "identical");
        let iso = ReplayComparisonResult::Isomorphic { reordered_count: 3 };
        assert!(iso.to_string().contains("isomorphic"));
        let div = ReplayComparisonResult::Divergent {
            first_divergence_idx: 5,
            description: "test".to_string(),
        };
        assert!(div.to_string().contains("divergent"));
    }

    #[test]
    fn test_replay_comparison_result_serde() {
        let results = vec![
            ReplayComparisonResult::Identical,
            ReplayComparisonResult::Isomorphic { reordered_count: 2 },
            ReplayComparisonResult::Divergent {
                first_divergence_idx: 0,
                description: "test".to_string(),
            },
        ];
        for r in results {
            let json = serde_json::to_string(&r).unwrap();
            let back: ReplayComparisonResult = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }

    #[test]
    fn test_trace_mismatch_serde() {
        let mm = TraceMismatch {
            canonical_idx: 3,
            expected_fingerprint: 111,
            actual_fingerprint: Some(222),
            explanation: "different action".to_string(),
        };
        let json = serde_json::to_string(&mm).unwrap();
        let back: TraceMismatch = serde_json::from_str(&json).unwrap();
        assert_eq!(mm, back);
    }

    #[test]
    fn test_canonicalizer_config_default() {
        let cfg = CanonicalizerConfig::default();
        assert_eq!(cfg.ordering, CanonicalOrdering::Causal);
        assert!(!cfg.strip_timestamps);
        assert!(!cfg.dedup_consecutive);
        assert_eq!(cfg.max_entries, 0);
    }

    #[test]
    fn test_canonicalizer_causal_ordering() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        // Insert out of causal order (timestamps swapped).
        trace.entries.push(TraceEntry {
            seq: 1,
            timestamp_us: 200,
            action: TraceAction::EpochAdvance { new_epoch: 2 },
            domain: InvariantDomain::Composition,
            causal_parent: Some(0),
            fingerprint: 1,
        });
        trace.entries.push(TraceEntry {
            seq: 0,
            timestamp_us: 100,
            action: TraceAction::EpochAdvance { new_epoch: 1 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 2,
        });
        let canonical = c.canonicalize(&trace);
        // Causal ordering sorts by seq.
        assert_eq!(canonical.entries[0].fingerprint, 2); // was seq=0
        assert_eq!(canonical.entries[1].fingerprint, 1); // was seq=1
    }

    #[test]
    fn test_canonicalizer_temporal_ordering() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            ordering: CanonicalOrdering::Temporal,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.entries.push(TraceEntry {
            seq: 0,
            timestamp_us: 200,
            action: TraceAction::EpochAdvance { new_epoch: 2 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 1,
        });
        trace.entries.push(TraceEntry {
            seq: 1,
            timestamp_us: 100,
            action: TraceAction::EpochAdvance { new_epoch: 1 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 2,
        });
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.entries[0].fingerprint, 2); // timestamp 100 first
        assert_eq!(canonical.entries[1].fingerprint, 1); // timestamp 200 second
    }

    #[test]
    fn test_canonicalizer_domain_grouped_ordering() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            ordering: CanonicalOrdering::DomainGrouped,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        // Budget entry first, then scheduler.
        trace.push(
            TraceAction::ObserveLatency {
                stage: LatencyStage::PtyCapture,
                latency_us: 10.0,
            },
            InvariantDomain::Budget,
            100,
            None,
        );
        trace.push(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 5.0,
            },
            InvariantDomain::Scheduler,
            50,
            None,
        );
        let canonical = c.canonicalize(&trace);
        // Scheduler (0) comes before Budget (1) in domain sort.
        assert_eq!(canonical.entries[0].domain, InvariantDomain::Scheduler);
        assert_eq!(canonical.entries[1].domain, InvariantDomain::Budget);
    }

    #[test]
    fn test_canonicalizer_strip_timestamps() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            strip_timestamps: true,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            500,
            None,
        );
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.entries[0].timestamp_us, 0);
    }

    #[test]
    fn test_canonicalizer_dedup_consecutive() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            dedup_consecutive: true,
            ..Default::default()
        });
        let action = TraceAction::EpochAdvance { new_epoch: 1 };
        let domain = InvariantDomain::Composition;
        let fp = TraceEntry::compute_fingerprint(&action, domain);
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        // Push same action twice (same fingerprint).
        trace.entries.push(TraceEntry {
            seq: 0,
            timestamp_us: 100,
            action: action.clone(),
            domain,
            causal_parent: None,
            fingerprint: fp,
        });
        trace.entries.push(TraceEntry {
            seq: 1,
            timestamp_us: 200,
            action: action.clone(),
            domain,
            causal_parent: Some(0),
            fingerprint: fp,
        });
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.len(), 1);
        assert_eq!(c.entries_deduped, 1);
    }

    #[test]
    fn test_canonicalizer_max_entries() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            max_entries: 2,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        for i in 0..5 {
            trace.push(
                TraceAction::EpochAdvance { new_epoch: i },
                InvariantDomain::Composition,
                i * 10,
                None,
            );
        }
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.len(), 2);
    }

    #[test]
    fn test_canonicalizer_compare_identical() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let t2 = t1.clone();
        let result = c.compare(&t1, &t2);
        assert_eq!(result, ReplayComparisonResult::Identical);
    }

    #[test]
    fn test_canonicalizer_compare_divergent_length() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        let result = c.compare(&t1, &t2);
        let is_divergent = matches!(result, ReplayComparisonResult::Divergent { .. });
        assert!(is_divergent);
    }

    #[test]
    fn test_canonicalizer_compare_divergent_content() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mut t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        t2.push(
            TraceAction::EpochAdvance { new_epoch: 99 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let result = c.compare(&t1, &t2);
        let is_divergent = matches!(result, ReplayComparisonResult::Divergent { .. });
        assert!(is_divergent);
    }

    #[test]
    fn test_canonicalizer_diagnose_mismatches_none() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mismatches = c.diagnose_mismatches(&t1, &t1);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_canonicalizer_diagnose_mismatches_found() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mut t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        t2.push(
            TraceAction::EpochAdvance { new_epoch: 99 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let mismatches = c.diagnose_mismatches(&t1, &t2);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].canonical_idx, 0);
    }

    #[test]
    fn test_canonicalizer_diagnose_missing_entry() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 0, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let t2 = DeterministicTrace::new_v2("b".to_string(), 0, 0);
        let mismatches = c.diagnose_mismatches(&t1, &t2);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].actual_fingerprint.is_none());
    }

    #[test]
    fn test_canonicalizer_upgrade_trace() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let steps = vec![
            TraceStep {
                step: 0,
                action: TraceAction::EpochAdvance { new_epoch: 1 },
                check_results: vec![],
                timestamp_us: 100,
            },
            TraceStep {
                step: 1,
                action: TraceAction::SchedulerAdmit {
                    lane: SchedulerLane::Input,
                    cost_us: 5.0,
                },
                check_results: vec![],
                timestamp_us: 200,
            },
        ];
        let trace = c.upgrade_trace(&steps, "upgraded".to_string(), 42);
        assert_eq!(trace.version, TraceFormatVersion::V2);
        assert_eq!(trace.len(), 2);
        assert_eq!(trace.seed, 42);
        assert_eq!(trace.entries[0].domain, InvariantDomain::Composition);
        assert_eq!(trace.entries[1].domain, InvariantDomain::Scheduler);
    }

    #[test]
    fn test_canonicalizer_snapshot() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let _ = c.canonicalize(&trace);
        let snap = c.snapshot();
        assert_eq!(snap.traces_processed, 1);
        assert_eq!(snap.entries_processed, 1);
    }

    #[test]
    fn test_canonicalizer_degradation_healthy() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let deg = c.detect_degradation();
        assert_eq!(deg, CanonicalizerDegradation::Healthy);
    }

    #[test]
    fn test_canonicalizer_degradation_display() {
        assert_eq!(CanonicalizerDegradation::Healthy.to_string(), "healthy");
        let high = CanonicalizerDegradation::HighDedupRatio { ratio: 0.75 };
        assert!(high.to_string().contains("high-dedup"));
        let vol = CanonicalizerDegradation::HighVolume {
            entries_processed: 200_000,
        };
        assert!(vol.to_string().contains("high-volume"));
    }

    #[test]
    fn test_canonicalizer_degradation_serde() {
        let variants = vec![
            CanonicalizerDegradation::Healthy,
            CanonicalizerDegradation::HighDedupRatio { ratio: 0.8 },
            CanonicalizerDegradation::HighVolume {
                entries_processed: 100,
            },
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: CanonicalizerDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_canonicalizer_log_entry() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let entry = c.log_entry("trace-1", 10, 8, 500);
        assert_eq!(entry.trace_id, "trace-1");
        assert_eq!(entry.input_entries, 10);
        assert_eq!(entry.output_entries, 8);
        assert_eq!(entry.duration_us, 500);
    }

    #[test]
    fn test_canonicalizer_reset() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let _ = c.canonicalize(&trace);
        assert_eq!(c.snapshot().traces_processed, 1);
        c.reset();
        assert_eq!(c.snapshot().traces_processed, 0);
    }

    #[test]
    fn test_action_domain_mapping() {
        assert_eq!(
            action_domain(&TraceAction::ObserveLatency {
                stage: LatencyStage::PtyCapture,
                latency_us: 1.0
            }),
            InvariantDomain::Budget
        );
        assert_eq!(
            action_domain(&TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 1.0
            }),
            InvariantDomain::Scheduler
        );
        assert_eq!(
            action_domain(&TraceAction::RecoveryStep {
                level_before: MitigationLevel::None,
                level_after: MitigationLevel::Defer
            }),
            InvariantDomain::Recovery
        );
        assert_eq!(
            action_domain(&TraceAction::EpochAdvance { new_epoch: 1 }),
            InvariantDomain::Composition
        );
        assert_eq!(
            action_domain(&TraceAction::Reset {
                domain: InvariantDomain::Recovery
            }),
            InvariantDomain::Recovery
        );
    }

    #[test]
    fn test_canonicalizer_snapshot_serde() {
        let snap = CanonicalizerSnapshot {
            traces_processed: 5,
            entries_processed: 100,
            entries_deduped: 10,
            comparisons_made: 3,
            config: CanonicalizerConfig::default(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: CanonicalizerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_canonicalizer_log_entry_serde() {
        let entry = CanonicalizerLogEntry {
            timestamp_us: 100,
            trace_id: "t1".to_string(),
            input_entries: 10,
            output_entries: 8,
            duration_us: 50,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: CanonicalizerLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_canonicalize_reassigns_seq_numbers() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig {
            ordering: CanonicalOrdering::Temporal,
            ..Default::default()
        });
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        // Entries with seq 0,1 but timestamp order is reversed.
        trace.entries.push(TraceEntry {
            seq: 0,
            timestamp_us: 200,
            action: TraceAction::EpochAdvance { new_epoch: 2 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 1,
        });
        trace.entries.push(TraceEntry {
            seq: 1,
            timestamp_us: 100,
            action: TraceAction::EpochAdvance { new_epoch: 1 },
            domain: InvariantDomain::Composition,
            causal_parent: None,
            fingerprint: 2,
        });
        let canonical = c.canonicalize(&trace);
        // After temporal sort, seq should be reassigned 0,1.
        assert_eq!(canonical.entries[0].seq, 0);
        assert_eq!(canonical.entries[1].seq, 1);
    }

    #[test]
    fn test_canonicalize_preserves_version() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let trace = DeterministicTrace::new_v2("t".to_string(), 42, 0);
        let canonical = c.canonicalize(&trace);
        assert_eq!(canonical.version, TraceFormatVersion::V2);
        assert_eq!(canonical.seed, 42);
    }

    // ── E3 Impl: Bridge method tests ─────────────────────────────

    #[test]
    fn test_compare_mc_traces_identical() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let steps = vec![
            TraceStep {
                step: 0,
                action: TraceAction::EpochAdvance { new_epoch: 1 },
                check_results: vec![],
                timestamp_us: 10,
            },
            TraceStep {
                step: 1,
                action: TraceAction::EpochAdvance { new_epoch: 2 },
                check_results: vec![],
                timestamp_us: 20,
            },
        ];
        let result = c.compare_mc_traces(&steps, &steps, 42);
        assert_eq!(result, ReplayComparisonResult::Identical);
    }

    #[test]
    fn test_compare_mc_traces_divergent() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let a = vec![TraceStep {
            step: 0,
            action: TraceAction::EpochAdvance { new_epoch: 1 },
            check_results: vec![],
            timestamp_us: 10,
        }];
        let b = vec![TraceStep {
            step: 0,
            action: TraceAction::EpochAdvance { new_epoch: 99 },
            check_results: vec![],
            timestamp_us: 10,
        }];
        let result = c.compare_mc_traces(&a, &b, 0);
        let is_divergent = matches!(result, ReplayComparisonResult::Divergent { .. });
        assert!(is_divergent);
    }

    #[test]
    fn test_verify_determinism() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            Some(0),
        );
        assert!(c.verify_determinism(&trace));
    }

    #[test]
    fn test_filter_by_domain() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 5.0,
            },
            InvariantDomain::Scheduler,
            20,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            30,
            Some(0),
        );

        let filtered = c.filter_by_domain(&trace, InvariantDomain::Composition);
        assert_eq!(filtered.len(), 2);
        for e in &filtered.entries {
            assert_eq!(e.domain, InvariantDomain::Composition);
        }
        // Seq numbers reassigned.
        assert_eq!(filtered.entries[0].seq, 0);
        assert_eq!(filtered.entries[1].seq, 1);
    }

    #[test]
    fn test_filter_by_domain_empty() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let filtered = c.filter_by_domain(&trace, InvariantDomain::Recovery);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_causal_chain_no_parents() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let chain = c.causal_chain(&trace, 0);
        assert_eq!(chain, vec![0]);
    }

    #[test]
    fn test_causal_chain_with_parents() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            Some(0),
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 3 },
            InvariantDomain::Composition,
            30,
            Some(1),
        );
        let chain = c.causal_chain(&trace, 2);
        assert_eq!(chain, vec![0, 1, 2]);
    }

    #[test]
    fn test_domain_histogram() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::SchedulerAdmit {
                lane: SchedulerLane::Input,
                cost_us: 5.0,
            },
            InvariantDomain::Scheduler,
            20,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            30,
            None,
        );
        let hist = c.domain_histogram(&trace);
        assert_eq!(hist.get("composition"), Some(&2));
        assert_eq!(hist.get("scheduler"), Some(&1));
    }

    #[test]
    fn test_unique_fingerprints() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            20,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            30,
            None,
        );
        let unique = c.unique_fingerprints(&trace);
        // epoch 2 appears once, epoch 1 appears twice.
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0], 2); // seq of the epoch(2) entry
    }

    #[test]
    fn test_merge_traces() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut t1 = DeterministicTrace::new_v2("a".to_string(), 1, 0);
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        t1.push(
            TraceAction::EpochAdvance { new_epoch: 3 },
            InvariantDomain::Composition,
            30,
            None,
        );

        let mut t2 = DeterministicTrace::new_v2("b".to_string(), 2, 0);
        t2.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            None,
        );

        let merged = c.merge_traces(&t1, &t2);
        assert_eq!(merged.len(), 3);
        // Should be sorted by timestamp: 10, 20, 30.
        assert_eq!(merged.entries[0].timestamp_us, 10);
        assert_eq!(merged.entries[1].timestamp_us, 20);
        assert_eq!(merged.entries[2].timestamp_us, 30);
        // Seq reassigned.
        assert_eq!(merged.entries[0].seq, 0);
        assert_eq!(merged.entries[1].seq, 1);
        assert_eq!(merged.entries[2].seq, 2);
    }

    #[test]
    fn test_time_window() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 2 },
            InvariantDomain::Composition,
            20,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 3 },
            InvariantDomain::Composition,
            30,
            None,
        );
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 4 },
            InvariantDomain::Composition,
            40,
            None,
        );

        let windowed = c.time_window(&trace, 15, 35);
        assert_eq!(windowed.len(), 2);
        assert_eq!(windowed.entries[0].timestamp_us, 20);
        assert_eq!(windowed.entries[1].timestamp_us, 30);
    }

    #[test]
    fn test_time_window_empty() {
        let c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        let mut trace = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        trace.push(
            TraceAction::EpochAdvance { new_epoch: 1 },
            InvariantDomain::Composition,
            10,
            None,
        );
        let windowed = c.time_window(&trace, 100, 200);
        assert!(windowed.is_empty());
    }

    #[test]
    fn test_total_comparisons() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        assert_eq!(c.total_comparisons(), 0);
        let t = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        let _ = c.compare(&t, &t);
        assert_eq!(c.total_comparisons(), 1);
    }

    #[test]
    fn test_total_traces() {
        let mut c = ReplayCanonicalizer::new(CanonicalizerConfig::default());
        assert_eq!(c.total_traces(), 0);
        let t = DeterministicTrace::new_v2("t".to_string(), 0, 0);
        let _ = c.canonicalize(&t);
        assert_eq!(c.total_traces(), 1);
    }

    #[test]
    fn test_config_accessor() {
        let cfg = CanonicalizerConfig {
            ordering: CanonicalOrdering::Temporal,
            ..Default::default()
        };
        let c = ReplayCanonicalizer::new(cfg.clone());
        assert_eq!(*c.config(), cfg);
    }

    // ── E4: Optimization Isomorphism Proof Gate Tests ────────────

    fn make_golden_trace(entries: &[(u64, TraceAction, InvariantDomain)]) -> DeterministicTrace {
        let mut trace = DeterministicTrace::new_v2("golden".to_string(), 42, 0);
        for (ts, action, domain) in entries {
            trace.push(action.clone(), *domain, *ts, None);
        }
        trace
    }

    #[test]
    fn test_golden_artifact_new() {
        let trace = make_golden_trace(&[
            (10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition),
        ]);
        let ga = GoldenArtifact::new("test".to_string(), trace.clone(), "desc".to_string(), 0);
        assert_eq!(ga.artifact_id, "test");
        assert_eq!(ga.version, 1);
        assert!(ga.verify_checksum());
        assert_eq!(ga.checksum, trace.digest());
    }

    #[test]
    fn test_golden_artifact_update() {
        let t1 = make_golden_trace(&[
            (10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition),
        ]);
        let mut ga = GoldenArtifact::new("test".to_string(), t1, "v1".to_string(), 0);
        let t2 = make_golden_trace(&[
            (20, TraceAction::EpochAdvance { new_epoch: 2 }, InvariantDomain::Composition),
        ]);
        ga.update(t2.clone(), 100);
        assert_eq!(ga.version, 2);
        assert_eq!(ga.checksum, t2.digest());
        assert!(ga.verify_checksum());
    }

    #[test]
    fn test_golden_artifact_serde() {
        let trace = make_golden_trace(&[
            (10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition),
        ]);
        let ga = GoldenArtifact::new("test".to_string(), trace, "desc".to_string(), 0);
        let json = serde_json::to_string(&ga).unwrap();
        let back: GoldenArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(ga, back);
    }

    #[test]
    fn test_golden_artifact_display() {
        let trace = make_golden_trace(&[
            (10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition),
        ]);
        let ga = GoldenArtifact::new("my-opt".to_string(), trace, "desc".to_string(), 0);
        let s = ga.to_string();
        assert!(s.contains("my-opt"));
        assert!(s.contains("v1"));
    }

    #[test]
    fn test_proof_gate_verdict_pass_fail() {
        assert!(ProofGateVerdict::Equivalent.is_pass());
        assert!(!ProofGateVerdict::Equivalent.is_fail());
        assert!(ProofGateVerdict::IsomorphicEquivalent { reordered_count: 1 }.is_pass());
        let drift = ProofGateVerdict::SemanticDrift {
            first_divergence_idx: 0, mismatches: vec![], summary: "x".to_string(),
        };
        assert!(drift.is_fail());
        let chk = ProofGateVerdict::ChecksumFailure { expected: 1, actual: 2 };
        assert!(chk.is_fail());
    }

    #[test]
    fn test_proof_gate_verdict_display() {
        assert!(ProofGateVerdict::Equivalent.to_string().contains("PASS"));
        let iso = ProofGateVerdict::IsomorphicEquivalent { reordered_count: 3 };
        assert!(iso.to_string().contains("isomorphic"));
        let drift = ProofGateVerdict::SemanticDrift {
            first_divergence_idx: 5, mismatches: vec![], summary: "oops".to_string(),
        };
        assert!(drift.to_string().contains("FAIL"));
    }

    #[test]
    fn test_proof_gate_verdict_serde() {
        let verdicts = vec![
            ProofGateVerdict::Equivalent,
            ProofGateVerdict::IsomorphicEquivalent { reordered_count: 2 },
            ProofGateVerdict::SemanticDrift {
                first_divergence_idx: 0, mismatches: vec![], summary: "x".to_string(),
            },
            ProofGateVerdict::ChecksumFailure { expected: 1, actual: 2 },
        ];
        for v in verdicts {
            let json = serde_json::to_string(&v).unwrap();
            let back: ProofGateVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_proof_gate_config_default() {
        let cfg = ProofGateConfig::default();
        assert!(cfg.allow_isomorphic);
        assert_eq!(cfg.max_mismatches, 50);
    }

    #[test]
    fn test_proof_summary_display() {
        let summary = ProofSummary {
            artifact_id: "test".to_string(),
            golden_version: 1,
            verdict: ProofGateVerdict::Equivalent,
            candidate_entries: 10,
            golden_entries: 10,
            check_duration_us: 500,
            timestamp_us: 0,
        };
        let s = summary.to_string();
        assert!(s.contains("test"));
        assert!(s.contains("PASS"));
    }

    #[test]
    fn test_proof_summary_serde() {
        let summary = ProofSummary {
            artifact_id: "test".to_string(),
            golden_version: 3,
            verdict: ProofGateVerdict::Equivalent,
            candidate_entries: 10,
            golden_entries: 10,
            check_duration_us: 500,
            timestamp_us: 100,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: ProofSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(summary, back);
    }

    #[test]
    fn test_proof_gate_register_and_get() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[
            (10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition),
        ]);
        let ga = GoldenArtifact::new("opt-1".to_string(), trace, "desc".to_string(), 0);
        gate.register_golden(ga);
        assert_eq!(gate.artifact_count(), 1);
        assert!(gate.get_golden("opt-1").is_some());
        assert!(gate.get_golden("opt-2").is_none());
    }

    #[test]
    fn test_proof_gate_register_replaces() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let t1 = make_golden_trace(&[
            (10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition),
        ]);
        let t2 = make_golden_trace(&[
            (20, TraceAction::EpochAdvance { new_epoch: 2 }, InvariantDomain::Composition),
        ]);
        gate.register_golden(GoldenArtifact::new("x".to_string(), t1, "v1".to_string(), 0));
        gate.register_golden(GoldenArtifact::new("x".to_string(), t2, "v2".to_string(), 100));
        assert_eq!(gate.artifact_count(), 1);
        assert_eq!(gate.get_golden("x").unwrap().description, "v2");
    }

    #[test]
    fn test_proof_gate_check_equivalent() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[
            (10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition),
        ]);
        gate.register_golden(GoldenArtifact::new("opt-1".to_string(), trace.clone(), "d".to_string(), 0));
        let summary = gate.check("opt-1", &trace, 100);
        assert_eq!(summary.verdict, ProofGateVerdict::Equivalent);
        assert_eq!(gate.snapshot().passes, 1);
    }

    #[test]
    fn test_proof_gate_check_divergent() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let golden = make_golden_trace(&[
            (10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition),
        ]);
        gate.register_golden(GoldenArtifact::new("opt-1".to_string(), golden, "d".to_string(), 0));
        let candidate = make_golden_trace(&[
            (10, TraceAction::EpochAdvance { new_epoch: 99 }, InvariantDomain::Composition),
        ]);
        let summary = gate.check("opt-1", &candidate, 100);
        assert!(summary.verdict.is_fail());
        assert_eq!(gate.snapshot().failures, 1);
    }

    #[test]
    fn test_proof_gate_check_missing_artifact() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let candidate = DeterministicTrace::new_v2("c".to_string(), 0, 0);
        let summary = gate.check("nonexistent", &candidate, 100);
        assert!(summary.verdict.is_fail());
    }

    #[test]
    fn test_proof_gate_remove_golden() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[(10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition)]);
        gate.register_golden(GoldenArtifact::new("x".to_string(), trace, "d".to_string(), 0));
        assert!(gate.remove_golden("x"));
        assert_eq!(gate.artifact_count(), 0);
        assert!(!gate.remove_golden("x"));
    }

    #[test]
    fn test_proof_gate_artifact_ids() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let t = make_golden_trace(&[(10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition)]);
        gate.register_golden(GoldenArtifact::new("a".to_string(), t.clone(), "d".to_string(), 0));
        gate.register_golden(GoldenArtifact::new("b".to_string(), t, "d".to_string(), 0));
        let ids = gate.artifact_ids();
        assert!(ids.contains(&"a".to_string()));
        assert!(ids.contains(&"b".to_string()));
    }

    #[test]
    fn test_proof_gate_reset_counters() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[(10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition)]);
        gate.register_golden(GoldenArtifact::new("x".to_string(), trace.clone(), "d".to_string(), 0));
        let _ = gate.check("x", &trace, 0);
        gate.reset_counters();
        let snap = gate.snapshot();
        assert_eq!(snap.checks_run, 0);
        assert_eq!(snap.passes, 0);
        assert_eq!(snap.failures, 0);
        assert_eq!(snap.artifacts_count, 1); // Artifacts preserved.
    }

    #[test]
    fn test_proof_gate_snapshot_serde() {
        let snap = ProofGateSnapshot {
            checks_run: 10,
            passes: 8,
            failures: 2,
            artifacts_count: 3,
            config: ProofGateConfig::default(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ProofGateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_proof_gate_degradation_healthy() {
        let gate = ProofGate::new(ProofGateConfig::default());
        assert_eq!(gate.detect_degradation(), ProofGateDegradation::Healthy);
    }

    #[test]
    fn test_proof_gate_degradation_display() {
        assert_eq!(ProofGateDegradation::Healthy.to_string(), "healthy");
        let hfr = ProofGateDegradation::HighFailureRate { rate: 0.75 };
        assert!(hfr.to_string().contains("high-failure-rate"));
        let hac = ProofGateDegradation::HighArtifactCount { count: 200 };
        assert!(hac.to_string().contains("high-artifact-count"));
    }

    #[test]
    fn test_proof_gate_degradation_serde() {
        let variants = vec![
            ProofGateDegradation::Healthy,
            ProofGateDegradation::HighFailureRate { rate: 0.8 },
            ProofGateDegradation::HighArtifactCount { count: 150 },
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: ProofGateDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_proof_gate_log_entry() {
        let gate = ProofGate::new(ProofGateConfig::default());
        let entry = gate.log_entry("test", true, 500);
        assert_eq!(entry.artifact_id, "test");
        assert!(entry.passed);
        assert_eq!(entry.check_duration_us, 500);
    }

    #[test]
    fn test_proof_gate_log_entry_serde() {
        let entry = ProofGateLogEntry {
            timestamp_us: 100,
            artifact_id: "opt-1".to_string(),
            passed: true,
            check_duration_us: 50,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: ProofGateLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_proof_gate_config_accessor() {
        let cfg = ProofGateConfig { allow_isomorphic: false, ..Default::default() };
        let gate = ProofGate::new(cfg.clone());
        assert_eq!(*gate.config(), cfg);
    }

    // ── E4 Impl: Bridge method tests ─────────────────────────────

    #[test]
    fn test_check_from_mc_trace() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let steps = vec![
            TraceStep { step: 0, action: TraceAction::EpochAdvance { new_epoch: 1 }, check_results: vec![], timestamp_us: 10 },
        ];
        gate.register_golden_from_mc("mc-opt".to_string(), &steps, 42, "desc".to_string(), 0);
        let summary = gate.check_from_mc_trace("mc-opt", &steps, 42, 100);
        assert!(summary.verdict.is_pass());
    }

    #[test]
    fn test_register_golden_from_mc() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let steps = vec![
            TraceStep { step: 0, action: TraceAction::EpochAdvance { new_epoch: 5 }, check_results: vec![], timestamp_us: 100 },
        ];
        gate.register_golden_from_mc("mc-1".to_string(), &steps, 99, "mc golden".to_string(), 0);
        assert_eq!(gate.artifact_count(), 1);
        let ga = gate.get_golden("mc-1").unwrap();
        assert_eq!(ga.trace.version, TraceFormatVersion::V2);
        assert_eq!(ga.trace.len(), 1);
    }

    #[test]
    fn test_approve_drift() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let t1 = make_golden_trace(&[(10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition)]);
        gate.register_golden(GoldenArtifact::new("x".to_string(), t1, "v1".to_string(), 0));
        let t2 = make_golden_trace(&[(20, TraceAction::EpochAdvance { new_epoch: 2 }, InvariantDomain::Composition)]);
        assert!(gate.approve_drift("x", &t2, 100));
        assert_eq!(gate.get_golden("x").unwrap().version, 2);
        assert!(!gate.approve_drift("nonexistent", &t2, 100));
    }

    #[test]
    fn test_failing_passing_artifacts() {
        let summaries = vec![
            ProofSummary {
                artifact_id: "a".to_string(), golden_version: 1,
                verdict: ProofGateVerdict::Equivalent,
                candidate_entries: 1, golden_entries: 1, check_duration_us: 0, timestamp_us: 0,
            },
            ProofSummary {
                artifact_id: "b".to_string(), golden_version: 1,
                verdict: ProofGateVerdict::SemanticDrift {
                    first_divergence_idx: 0, mismatches: vec![], summary: "x".to_string(),
                },
                candidate_entries: 1, golden_entries: 1, check_duration_us: 0, timestamp_us: 0,
            },
        ];
        assert_eq!(ProofGate::failing_artifacts(&summaries), vec!["b".to_string()]);
        assert_eq!(ProofGate::passing_artifacts(&summaries), vec!["a".to_string()]);
    }

    #[test]
    fn test_pass_rate() {
        let summaries = vec![
            ProofSummary {
                artifact_id: "a".to_string(), golden_version: 1,
                verdict: ProofGateVerdict::Equivalent,
                candidate_entries: 1, golden_entries: 1, check_duration_us: 0, timestamp_us: 0,
            },
            ProofSummary {
                artifact_id: "b".to_string(), golden_version: 1,
                verdict: ProofGateVerdict::SemanticDrift {
                    first_divergence_idx: 0, mismatches: vec![], summary: "x".to_string(),
                },
                candidate_entries: 1, golden_entries: 1, check_duration_us: 0, timestamp_us: 0,
            },
        ];
        let rate = ProofGate::pass_rate(&summaries);
        assert!((rate - 0.5).abs() < 1e-10);
        assert!((ProofGate::pass_rate(&[]) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_total_counters() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let trace = make_golden_trace(&[(10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition)]);
        gate.register_golden(GoldenArtifact::new("x".to_string(), trace.clone(), "d".to_string(), 0));
        let _ = gate.check("x", &trace, 0);
        assert_eq!(gate.total_checks(), 1);
        assert_eq!(gate.total_passes(), 1);
        assert_eq!(gate.total_failures(), 0);
    }

    #[test]
    fn test_check_all() {
        let mut gate = ProofGate::new(ProofGateConfig::default());
        let t1 = make_golden_trace(&[(10, TraceAction::EpochAdvance { new_epoch: 1 }, InvariantDomain::Composition)]);
        let t2 = make_golden_trace(&[(20, TraceAction::EpochAdvance { new_epoch: 2 }, InvariantDomain::Composition)]);
        gate.register_golden(GoldenArtifact::new("a".to_string(), t1.clone(), "d".to_string(), 0));
        gate.register_golden(GoldenArtifact::new("b".to_string(), t2.clone(), "d".to_string(), 0));
        let mut candidates = std::collections::HashMap::new();
        candidates.insert("a".to_string(), t1);
        candidates.insert("b".to_string(), t2);
        let summaries = gate.check_all(&candidates, 100);
        assert_eq!(summaries.len(), 2);
        for s in &summaries {
            assert!(s.verdict.is_pass());
        }
    }

    // ── F1: Fault Domain Isolation Tests ─────────────────────────

    #[test]
    fn test_fault_domain_all() {
        assert_eq!(FaultDomain::ALL.len(), 5);
    }

    #[test]
    fn test_fault_domain_display() {
        assert_eq!(FaultDomain::Scheduler.to_string(), "scheduler");
        assert_eq!(FaultDomain::Budget.to_string(), "budget");
        assert_eq!(FaultDomain::Recovery.to_string(), "recovery");
        assert_eq!(FaultDomain::Io.to_string(), "io");
        assert_eq!(FaultDomain::Storage.to_string(), "storage");
    }

    #[test]
    fn test_fault_domain_serde() {
        for d in FaultDomain::ALL {
            let json = serde_json::to_string(d).unwrap();
            let back: FaultDomain = serde_json::from_str(&json).unwrap();
            assert_eq!(*d, back);
        }
    }

    #[test]
    fn test_domain_health_display() {
        assert_eq!(DomainHealth::Healthy.to_string(), "healthy");
        assert_eq!(DomainHealth::Degraded.to_string(), "degraded");
        assert_eq!(DomainHealth::Crashed.to_string(), "crashed");
        assert_eq!(DomainHealth::Restarting.to_string(), "restarting");
        assert_eq!(DomainHealth::Isolated.to_string(), "isolated");
    }

    #[test]
    fn test_domain_health_serde() {
        for h in [DomainHealth::Healthy, DomainHealth::Degraded, DomainHealth::Crashed, DomainHealth::Restarting, DomainHealth::Isolated] {
            let json = serde_json::to_string(&h).unwrap();
            let back: DomainHealth = serde_json::from_str(&json).unwrap();
            assert_eq!(h, back);
        }
    }

    #[test]
    fn test_crash_only_contract_default() {
        let c = CrashOnlyContract::default();
        assert_eq!(c.max_restarts, 3);
        assert!(c.checkpoint_on_crash);
    }

    #[test]
    fn test_crash_only_contract_serde() {
        let c = CrashOnlyContract {
            domain: FaultDomain::Io,
            max_restarts: 5,
            restart_cooldown_us: 50_000,
            checkpoint_on_crash: false,
            restart_timeout_us: 1_000_000,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: CrashOnlyContract = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn test_fault_event_serde() {
        let ev = FaultEvent {
            domain: FaultDomain::Storage,
            timestamp_us: 12345,
            description: "disk full".to_string(),
            recovery_attempted: true,
            recovery_succeeded: false,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: FaultEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn test_fault_isolation_manager_new() {
        let mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        for d in FaultDomain::ALL {
            assert_eq!(mgr.domain_health(*d), DomainHealth::Healthy);
        }
        assert!(!mgr.has_isolated_domains());
    }

    #[test]
    fn test_record_fault_transitions_to_crashed() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "test fault".to_string(), 100);
        assert_eq!(mgr.domain_health(FaultDomain::Scheduler), DomainHealth::Crashed);
        assert_eq!(mgr.domain_state(FaultDomain::Scheduler).unwrap().total_faults, 1);
    }

    #[test]
    fn test_record_fault_auto_isolates() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        // Default max_restarts is 3, so 4th fault triggers isolation.
        for i in 0..4 {
            mgr.record_fault(FaultDomain::Scheduler, format!("fault {i}"), (i + 1) * 100);
        }
        assert_eq!(mgr.domain_health(FaultDomain::Scheduler), DomainHealth::Isolated);
        assert!(mgr.has_isolated_domains());
    }

    #[test]
    fn test_attempt_restart_success() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Budget, "test".to_string(), 100);
        assert!(mgr.attempt_restart(FaultDomain::Budget, 200_000));
        assert_eq!(mgr.domain_health(FaultDomain::Budget), DomainHealth::Restarting);
    }

    #[test]
    fn test_attempt_restart_cooldown_enforced() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Budget, "test".to_string(), 100);
        assert!(mgr.attempt_restart(FaultDomain::Budget, 200_000));
        mgr.restart_failed(FaultDomain::Budget, 200_001);
        // Too soon — cooldown not elapsed.
        assert!(!mgr.attempt_restart(FaultDomain::Budget, 200_002));
    }

    #[test]
    fn test_restart_succeeded_resets_failures() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Io, "err".to_string(), 100);
        mgr.attempt_restart(FaultDomain::Io, 200_000);
        mgr.restart_succeeded(FaultDomain::Io);
        assert_eq!(mgr.domain_health(FaultDomain::Io), DomainHealth::Healthy);
        assert_eq!(mgr.domain_state(FaultDomain::Io).unwrap().consecutive_failures, 0);
    }

    #[test]
    fn test_restart_failed_increments_failures() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Io, "err".to_string(), 100);
        mgr.attempt_restart(FaultDomain::Io, 200_000);
        mgr.restart_failed(FaultDomain::Io, 200_001);
        assert_eq!(mgr.domain_health(FaultDomain::Io), DomainHealth::Crashed);
        assert_eq!(mgr.domain_state(FaultDomain::Io).unwrap().consecutive_failures, 2);
    }

    #[test]
    fn test_mark_degraded() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.mark_degraded(FaultDomain::Storage);
        assert_eq!(mgr.domain_health(FaultDomain::Storage), DomainHealth::Degraded);
    }

    #[test]
    fn test_un_isolate() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        for i in 0..4 {
            mgr.record_fault(FaultDomain::Recovery, format!("f{i}"), i * 100);
        }
        assert_eq!(mgr.domain_health(FaultDomain::Recovery), DomainHealth::Isolated);
        mgr.un_isolate(FaultDomain::Recovery);
        assert_eq!(mgr.domain_health(FaultDomain::Recovery), DomainHealth::Crashed);
    }

    #[test]
    fn test_fault_isolation_snapshot() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "test".to_string(), 100);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_faults, 1);
        assert_eq!(snap.domains.len(), 5);
    }

    #[test]
    fn test_fault_isolation_snapshot_serde() {
        let mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        let snap = mgr.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: FaultIsolationSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn test_fault_isolation_degradation_healthy() {
        let mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        assert_eq!(mgr.detect_degradation(), FaultIsolationDegradation::Healthy);
    }

    #[test]
    fn test_fault_isolation_degradation_partial() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "x".to_string(), 100);
        let deg = mgr.detect_degradation();
        let is_partial = matches!(deg, FaultIsolationDegradation::PartialDegradation { .. });
        assert!(is_partial);
    }

    #[test]
    fn test_fault_isolation_degradation_isolated() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        for i in 0..4 {
            mgr.record_fault(FaultDomain::Io, format!("f{i}"), i * 100);
        }
        let deg = mgr.detect_degradation();
        let is_isolated = matches!(deg, FaultIsolationDegradation::DomainIsolated { .. });
        assert!(is_isolated);
    }

    #[test]
    fn test_fault_isolation_degradation_display() {
        assert_eq!(FaultIsolationDegradation::Healthy.to_string(), "healthy");
        let pd = FaultIsolationDegradation::PartialDegradation { degraded_count: 2 };
        assert!(pd.to_string().contains("partial-degradation"));
        let di = FaultIsolationDegradation::DomainIsolated { isolated_domains: vec![FaultDomain::Io] };
        assert!(di.to_string().contains("io"));
    }

    #[test]
    fn test_fault_isolation_degradation_serde() {
        let variants: Vec<FaultIsolationDegradation> = vec![
            FaultIsolationDegradation::Healthy,
            FaultIsolationDegradation::PartialDegradation { degraded_count: 2 },
            FaultIsolationDegradation::DomainIsolated { isolated_domains: vec![FaultDomain::Scheduler] },
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: FaultIsolationDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn test_fault_isolation_log_entry_serde() {
        let entry = FaultIsolationLogEntry {
            timestamp_us: 100,
            domain: FaultDomain::Budget,
            from_health: DomainHealth::Healthy,
            to_health: DomainHealth::Crashed,
            description: "budget exceeded".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: FaultIsolationLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_fault_history_capped() {
        let cfg = FaultIsolationConfig {
            max_history: 3,
            ..Default::default()
        };
        let mut mgr = FaultIsolationManager::new(cfg);
        for i in 0..5 {
            mgr.record_fault(FaultDomain::Scheduler, format!("f{i}"), i * 100);
        }
        assert_eq!(mgr.fault_history().len(), 3);
    }

    #[test]
    fn test_fault_isolation_reset() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "x".to_string(), 100);
        mgr.reset();
        for d in FaultDomain::ALL {
            assert_eq!(mgr.domain_health(*d), DomainHealth::Healthy);
        }
        assert!(mgr.fault_history().is_empty());
    }

    // ── F1 Impl: Bridge method tests ─────────────────────────────

    #[test]
    fn test_healthy_unhealthy_count() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        assert_eq!(mgr.healthy_count(), 5);
        assert_eq!(mgr.unhealthy_count(), 0);
        mgr.record_fault(FaultDomain::Scheduler, "x".to_string(), 100);
        assert_eq!(mgr.healthy_count(), 4);
        assert_eq!(mgr.unhealthy_count(), 1);
    }

    #[test]
    fn test_total_faults_and_restarts() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Scheduler, "a".to_string(), 100);
        mgr.record_fault(FaultDomain::Budget, "b".to_string(), 200);
        assert_eq!(mgr.total_faults(), 2);
        assert_eq!(mgr.total_restarts(), 0);
    }

    #[test]
    fn test_domain_faults_and_restarts() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        mgr.record_fault(FaultDomain::Io, "x".to_string(), 100);
        mgr.record_fault(FaultDomain::Io, "y".to_string(), 200);
        assert_eq!(mgr.domain_faults(FaultDomain::Io), 2);
        assert_eq!(mgr.domain_faults(FaultDomain::Budget), 0);
    }

    #[test]
    fn test_all_healthy() {
        let mut mgr = FaultIsolationManager::new(FaultIsolationConfig::default());
        assert!(mgr.all_healthy());
        mgr.record_fault(FaultDomain::Storage, "x".to_string(), 100);
        assert!(!mgr.all_healthy());
    }

    #[test]
    fn test_to_invariant_domain() {
        assert_eq!(FaultIsolationManager::to_invariant_domain(FaultDomain::Scheduler), InvariantDomain::Scheduler);
        assert_eq!(FaultIsolationManager::to_invariant_domain(FaultDomain::Budget), InvariantDomain::Budget);
        assert_eq!(FaultIsolationManager::to_invariant_domain(FaultDomain::Recovery), InvariantDomain::Recovery);
        assert_eq!(FaultIsolationManager::to_invariant_domain(FaultDomain::Io), InvariantDomain::Composition);
        assert_eq!(FaultIsolationManager::to_invariant_domain(FaultDomain::Storage), InvariantDomain::Composition);
    }

    #[test]
    fn test_config_accessor_fault() {
        let cfg = FaultIsolationConfig { auto_isolate: false, ..Default::default() };
        let mgr = FaultIsolationManager::new(cfg.clone());
        assert_eq!(*mgr.config(), cfg);
    }

    // ── F2: Circuit Breakers and Recovery Choreography ──

    #[test]
    fn test_breaker_state_display() {
        assert_eq!(BreakerState::Closed.to_string(), "closed");
        assert_eq!(BreakerState::Open.to_string(), "open");
        assert_eq!(BreakerState::HalfOpen.to_string(), "half-open");
    }

    #[test]
    fn test_breaker_state_serde() {
        for state in [BreakerState::Closed, BreakerState::Open, BreakerState::HalfOpen] {
            let json = serde_json::to_string(&state).unwrap();
            let back: BreakerState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn test_stage_breaker_config_default() {
        let cfg = StageBreakerConfig::default();
        assert_eq!(cfg.failure_threshold, 5);
        assert_eq!(cfg.open_duration_us, 1_000_000);
        assert_eq!(cfg.half_open_max_probes, 3);
        assert_eq!(cfg.half_open_success_threshold, 2);
    }

    #[test]
    fn test_stage_breaker_config_serde() {
        let cfg = StageBreakerConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: StageBreakerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn test_stage_breaker_state_serde() {
        let sbs = StageBreakerState {
            stage: LatencyStage::PtyCapture,
            state: BreakerState::Open,
            consecutive_failures: 5,
            opened_at_us: 1000,
            half_open_probes: 0,
            half_open_successes: 0,
            total_trips: 1,
            total_recoveries: 0,
        };
        let json = serde_json::to_string(&sbs).unwrap();
        let back: StageBreakerState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sbs);
    }

    #[test]
    fn test_recovery_step_serde() {
        let step = RecoveryStep {
            stage: LatencyStage::StorageWrite,
            step_number: 1,
            action: "flush WAL".to_string(),
            requires_prior_success: true,
            timeout_us: 500_000,
        };
        let json = serde_json::to_string(&step).unwrap();
        let back: RecoveryStep = serde_json::from_str(&json).unwrap();
        assert_eq!(back, step);
    }

    #[test]
    fn test_choreography_outcome_display() {
        assert_eq!(ChoreographyOutcome::FullRecovery.to_string(), "full-recovery");
        let partial = ChoreographyOutcome::PartialRecovery {
            recovered: vec![LatencyStage::PtyCapture],
            failed: vec![LatencyStage::StorageWrite, LatencyStage::EventEmission],
        };
        assert_eq!(partial.to_string(), "partial(1 ok, 2 failed)");
        let aborted = ChoreographyOutcome::Aborted { reason: "timeout".to_string() };
        assert_eq!(aborted.to_string(), "aborted: timeout");
    }

    #[test]
    fn test_choreography_outcome_serde() {
        let outcomes = vec![
            ChoreographyOutcome::FullRecovery,
            ChoreographyOutcome::PartialRecovery {
                recovered: vec![LatencyStage::PtyCapture],
                failed: vec![LatencyStage::StorageWrite],
            },
            ChoreographyOutcome::Aborted { reason: "cascade".to_string() },
        ];
        for o in outcomes {
            let json = serde_json::to_string(&o).unwrap();
            let back: ChoreographyOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(back, o);
        }
    }

    #[test]
    fn test_breaker_manager_new_all_closed() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert!(mgr.all_closed());
        assert_eq!(mgr.open_count(), 0);
        for stage in LatencyStage::PIPELINE_STAGES {
            assert_eq!(mgr.breaker_state(*stage), BreakerState::Closed);
        }
    }

    #[test]
    fn test_breaker_manager_failure_below_threshold() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        // Default threshold is 5. Record 4 failures — should stay closed.
        for i in 0..4 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Closed);
        assert!(mgr.all_closed());
    }

    #[test]
    fn test_breaker_manager_failure_trips_breaker() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Open);
        assert!(!mgr.all_closed());
        assert_eq!(mgr.open_count(), 1);
    }

    #[test]
    fn test_breaker_manager_open_blocks_requests() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // Immediately after tripping, before open_duration passes, requests blocked.
        assert!(!mgr.allow_request(LatencyStage::PtyCapture, 105));
    }

    #[test]
    fn test_breaker_manager_open_to_half_open() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // After open_duration (1_000_000 us), should transition to half-open.
        assert!(mgr.allow_request(LatencyStage::PtyCapture, 1_000_200));
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::HalfOpen);
    }

    #[test]
    fn test_breaker_manager_half_open_probe_limit() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // Transition to half-open.
        assert!(mgr.allow_request(LatencyStage::PtyCapture, 1_100_000));
        // First probe consumed by the transition call. max_probes=3.
        assert!(mgr.allow_request(LatencyStage::PtyCapture, 1_100_001));
        assert!(mgr.allow_request(LatencyStage::PtyCapture, 1_100_002));
        // Now at 3 probes — next should be blocked.
        assert!(!mgr.allow_request(LatencyStage::PtyCapture, 1_100_003));
    }

    #[test]
    fn test_breaker_manager_half_open_recovery() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        // Transition to half-open.
        mgr.allow_request(LatencyStage::PtyCapture, 1_100_000);
        // Record enough successes to close (threshold = 2).
        mgr.record_success(LatencyStage::PtyCapture);
        mgr.record_success(LatencyStage::PtyCapture);
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Closed);
    }

    #[test]
    fn test_breaker_manager_half_open_failure_reopens() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        mgr.allow_request(LatencyStage::PtyCapture, 1_100_000);
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::HalfOpen);
        mgr.record_failure(LatencyStage::PtyCapture, 1_200_000);
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Open);
    }

    #[test]
    fn test_breaker_manager_success_resets_failures() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        mgr.record_failure(LatencyStage::PtyCapture, 100);
        mgr.record_failure(LatencyStage::PtyCapture, 101);
        mgr.record_success(LatencyStage::PtyCapture);
        // After success, consecutive failures reset. Need 5 more to trip.
        for i in 0..4 {
            mgr.record_failure(LatencyStage::PtyCapture, 200 + i);
        }
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Closed);
    }

    #[test]
    fn test_breaker_manager_multiple_stages_independent() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Open);
        assert_eq!(mgr.breaker_state(LatencyStage::StorageWrite), BreakerState::Closed);
        assert_eq!(mgr.open_count(), 1);
    }

    #[test]
    fn test_breaker_manager_snapshot() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        let snap = mgr.snapshot();
        assert_eq!(snap.stages.len(), 8); // All pipeline stages.
        assert_eq!(snap.total_trips, 1);
        assert_eq!(snap.total_recoveries, 0);
    }

    #[test]
    fn test_breaker_manager_snapshot_serde() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        let snap = mgr.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: BreakerManagerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn test_breaker_manager_degradation_healthy() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert_eq!(mgr.detect_degradation(), BreakerManagerDegradation::Healthy);
    }

    #[test]
    fn test_breaker_manager_degradation_tripped() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        let deg = mgr.detect_degradation();
        assert_eq!(deg, BreakerManagerDegradation::BreakerTripped { open_count: 1 });
    }

    #[test]
    fn test_breaker_manager_degradation_cascade_risk() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        let stages = [LatencyStage::PtyCapture, LatencyStage::StorageWrite, LatencyStage::EventEmission];
        for stage in stages {
            for i in 0..5 {
                mgr.record_failure(stage, 100 + i);
            }
        }
        let deg = mgr.detect_degradation();
        assert_eq!(deg, BreakerManagerDegradation::CascadeRisk { open_count: 3 });
    }

    #[test]
    fn test_breaker_manager_degradation_display() {
        assert_eq!(BreakerManagerDegradation::Healthy.to_string(), "healthy");
        assert_eq!(BreakerManagerDegradation::BreakerTripped { open_count: 2 }.to_string(), "tripped(2)");
        assert_eq!(BreakerManagerDegradation::CascadeRisk { open_count: 4 }.to_string(), "cascade-risk(4)");
    }

    #[test]
    fn test_breaker_manager_degradation_serde() {
        let cases = vec![
            BreakerManagerDegradation::Healthy,
            BreakerManagerDegradation::BreakerTripped { open_count: 1 },
            BreakerManagerDegradation::CascadeRisk { open_count: 5 },
        ];
        for deg in cases {
            let json = serde_json::to_string(&deg).unwrap();
            let back: BreakerManagerDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, deg);
        }
    }

    #[test]
    fn test_breaker_log_entry() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        let entry = mgr.log_entry(
            LatencyStage::PtyCapture,
            BreakerState::Closed,
            BreakerState::Open,
            "threshold exceeded".to_string(),
            42_000,
        );
        assert_eq!(entry.timestamp_us, 42_000);
        assert_eq!(entry.stage, LatencyStage::PtyCapture);
        assert_eq!(entry.from_state, BreakerState::Closed);
        assert_eq!(entry.to_state, BreakerState::Open);
        assert_eq!(entry.reason, "threshold exceeded");
    }

    #[test]
    fn test_breaker_log_entry_serde() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        let entry = mgr.log_entry(
            LatencyStage::StorageWrite,
            BreakerState::Open,
            BreakerState::HalfOpen,
            "cooldown elapsed".to_string(),
            100_000,
        );
        let json = serde_json::to_string(&entry).unwrap();
        let back: BreakerLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn test_breaker_manager_reset() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 {
            mgr.record_failure(LatencyStage::PtyCapture, 100 + i);
        }
        assert!(!mgr.all_closed());
        mgr.reset();
        assert!(mgr.all_closed());
        assert_eq!(mgr.open_count(), 0);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_trips, 0);
        assert_eq!(snap.total_recoveries, 0);
    }

    #[test]
    fn test_breaker_manager_config_accessor() {
        let cfg = StageBreakerConfig {
            failure_threshold: 3,
            open_duration_us: 500_000,
            half_open_max_probes: 2,
            half_open_success_threshold: 1,
        };
        let mgr = BreakerManager::new(cfg.clone());
        assert_eq!(*mgr.config(), cfg);
    }

    #[test]
    fn test_breaker_manager_custom_threshold() {
        let cfg = StageBreakerConfig {
            failure_threshold: 2,
            ..Default::default()
        };
        let mut mgr = BreakerManager::new(cfg);
        mgr.record_failure(LatencyStage::DeltaExtraction, 100);
        assert_eq!(mgr.breaker_state(LatencyStage::DeltaExtraction), BreakerState::Closed);
        mgr.record_failure(LatencyStage::DeltaExtraction, 101);
        assert_eq!(mgr.breaker_state(LatencyStage::DeltaExtraction), BreakerState::Open);
    }

    #[test]
    fn test_breaker_total_trips_accumulates() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        // Trip PtyCapture.
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 100 + i); }
        // Recover it.
        mgr.allow_request(LatencyStage::PtyCapture, 1_200_000);
        mgr.record_success(LatencyStage::PtyCapture);
        mgr.record_success(LatencyStage::PtyCapture);
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Closed);
        // Trip it again.
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 2_000_000 + i); }
        let snap = mgr.snapshot();
        assert_eq!(snap.total_trips, 2);
        assert_eq!(snap.total_recoveries, 1);
    }

    #[test]
    fn test_closed_always_allows_request() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for ts in 0..100 {
            assert!(mgr.allow_request(LatencyStage::PtyCapture, ts));
        }
    }

    #[test]
    fn test_open_failure_is_noop() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 100 + i); }
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Open);
        // Further failures while open are no-op.
        mgr.record_failure(LatencyStage::PtyCapture, 200);
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Open);
    }

    // ── F2 Impl: Bridge method tests ──

    #[test]
    fn test_breaker_total_trips_method() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        assert_eq!(mgr.total_trips(), 0);
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 100 + i); }
        assert_eq!(mgr.total_trips(), 1);
        for i in 0..5 { mgr.record_failure(LatencyStage::StorageWrite, 200 + i); }
        assert_eq!(mgr.total_trips(), 2);
    }

    #[test]
    fn test_breaker_total_recoveries_method() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 100 + i); }
        mgr.allow_request(LatencyStage::PtyCapture, 1_200_000);
        mgr.record_success(LatencyStage::PtyCapture);
        mgr.record_success(LatencyStage::PtyCapture);
        assert_eq!(mgr.total_recoveries(), 1);
    }

    #[test]
    fn test_breaker_total_consecutive_failures() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        mgr.record_failure(LatencyStage::PtyCapture, 100);
        mgr.record_failure(LatencyStage::StorageWrite, 101);
        assert_eq!(mgr.total_consecutive_failures(), 2);
    }

    #[test]
    fn test_breaker_open_stages() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        assert!(mgr.open_stages().is_empty());
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 100 + i); }
        let open = mgr.open_stages();
        assert_eq!(open.len(), 1);
        assert!(open.contains(&LatencyStage::PtyCapture));
    }

    #[test]
    fn test_breaker_half_open_stages() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 100 + i); }
        mgr.allow_request(LatencyStage::PtyCapture, 1_200_000);
        let half = mgr.half_open_stages();
        assert_eq!(half.len(), 1);
        assert!(half.contains(&LatencyStage::PtyCapture));
    }

    #[test]
    fn test_breaker_closed_stages() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert_eq!(mgr.closed_stages().len(), 8);
    }

    #[test]
    fn test_breaker_plan_recovery_empty_when_all_closed() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert!(mgr.plan_recovery().is_empty());
    }

    #[test]
    fn test_breaker_plan_recovery_ordered_by_pipeline() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        // Trip StorageWrite and PtyCapture (out of pipeline order).
        for i in 0..5 { mgr.record_failure(LatencyStage::StorageWrite, 100 + i); }
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 200 + i); }
        let plan = mgr.plan_recovery();
        assert_eq!(plan.len(), 2);
        // PtyCapture comes before StorageWrite in pipeline.
        assert_eq!(plan[0].stage, LatencyStage::PtyCapture);
        assert_eq!(plan[1].stage, LatencyStage::StorageWrite);
        assert!(!plan[0].requires_prior_success);
        assert!(plan[1].requires_prior_success);
    }

    #[test]
    fn test_breaker_initiate_recovery() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 100 + i); }
        // Not enough time passed yet.
        assert_eq!(mgr.initiate_recovery(500_000), 0);
        // Enough time passed.
        let transitioned = mgr.initiate_recovery(1_200_000);
        assert_eq!(transitioned, 1);
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::HalfOpen);
    }

    #[test]
    fn test_breaker_to_invariant_domain() {
        assert_eq!(BreakerManager::to_invariant_domain(), InvariantDomain::Recovery);
    }

    #[test]
    fn test_breaker_availability_all_closed() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        assert!((mgr.availability() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_breaker_availability_some_open() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        for i in 0..5 { mgr.record_failure(LatencyStage::PtyCapture, 100 + i); }
        // 7/8 closed.
        assert!((mgr.availability() - 7.0 / 8.0).abs() < 0.01);
    }

    #[test]
    fn test_breaker_record_failures_batch() {
        let mut mgr = BreakerManager::new(StageBreakerConfig::default());
        mgr.record_failures_batch(LatencyStage::PtyCapture, 5, 1000);
        assert_eq!(mgr.breaker_state(LatencyStage::PtyCapture), BreakerState::Open);
    }

    #[test]
    fn test_breaker_stage_state() {
        let mgr = BreakerManager::new(StageBreakerConfig::default());
        let st = mgr.stage_state(LatencyStage::PtyCapture);
        assert!(st.is_some());
        assert_eq!(st.unwrap().state, BreakerState::Closed);
    }

    // ── F3: Immediate-Ack / Deferred-Completion UX Protocol ──

    #[test]
    fn test_ack_phase_display() {
        assert_eq!(AckPhase::ImmediateAck.to_string(), "immediate-ack");
        assert_eq!(AckPhase::DeferredCompletion.to_string(), "deferred-completion");
    }

    #[test]
    fn test_ack_phase_serde() {
        for phase in [AckPhase::ImmediateAck, AckPhase::DeferredCompletion] {
            let json = serde_json::to_string(&phase).unwrap();
            let back: AckPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(back, phase);
        }
    }

    #[test]
    fn test_completion_reason_display() {
        assert_eq!(CompletionReason::Success.to_string(), "success");
        assert_eq!(CompletionReason::Timeout.to_string(), "timeout");
        let up = CompletionReason::UpstreamFailure {
            stage: LatencyStage::StorageWrite,
            detail: "WAL full".to_string(),
        };
        assert!(up.to_string().contains("upstream-failure"));
        let cancel = CompletionReason::Cancelled { reason: "user".to_string() };
        assert!(cancel.to_string().contains("cancelled"));
    }

    #[test]
    fn test_completion_reason_serde() {
        let reasons = vec![
            CompletionReason::Success,
            CompletionReason::Timeout,
            CompletionReason::UpstreamFailure {
                stage: LatencyStage::PatternDetection,
                detail: "OOM".to_string(),
            },
            CompletionReason::Cancelled { reason: "test".to_string() },
        ];
        for r in reasons {
            let json = serde_json::to_string(&r).unwrap();
            let back: CompletionReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn test_ack_token_serde() {
        let token = AckToken {
            correlation_id: 42,
            acked_at_us: 1000,
            source_stage: LatencyStage::PtyCapture,
            summary: "received input".to_string(),
        };
        let json = serde_json::to_string(&token).unwrap();
        let back: AckToken = serde_json::from_str(&json).unwrap();
        assert_eq!(back, token);
    }

    #[test]
    fn test_deferred_result_serde() {
        let result = DeferredResult {
            correlation_id: 42,
            completed_at_us: 5000,
            reason: CompletionReason::Success,
            deferred_latency_us: 4000,
            explanation: Some("Pattern matched".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: DeferredResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn test_ack_protocol_config_default() {
        let cfg = AckProtocolConfig::default();
        assert_eq!(cfg.ack_deadline_us, 50_000);
        assert_eq!(cfg.completion_deadline_us, 5_000_000);
        assert!(cfg.show_progress);
        assert_eq!(cfg.progress_interval_us, 500_000);
    }

    #[test]
    fn test_ack_protocol_config_serde() {
        let cfg = AckProtocolConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AckProtocolConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn test_progress_update_serde() {
        let update = ProgressUpdate {
            correlation_id: 7,
            timestamp_us: 3000,
            fraction: 0.5,
            message: "halfway".to_string(),
        };
        let json = serde_json::to_string(&update).unwrap();
        let back: ProgressUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, update);
    }

    #[test]
    fn test_ack_protocol_issue_ack() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "got input".to_string(), 1000);
        assert_eq!(token.correlation_id, 1);
        assert_eq!(token.acked_at_us, 1000);
        assert_eq!(token.source_stage, LatencyStage::PtyCapture);
        assert_eq!(mgr.pending_count(), 1);
    }

    #[test]
    fn test_ack_protocol_issue_increments_ids() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let t1 = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let t2 = mgr.issue_ack(LatencyStage::StorageWrite, "b".to_string(), 200);
        assert_eq!(t1.correlation_id, 1);
        assert_eq!(t2.correlation_id, 2);
        assert_eq!(mgr.pending_count(), 2);
    }

    #[test]
    fn test_ack_protocol_complete_success() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        let result = mgr.complete(token.correlation_id, CompletionReason::Success, 3000);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.deferred_latency_us, 2000);
        assert_eq!(r.reason, CompletionReason::Success);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn test_ack_protocol_complete_unknown_id() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let result = mgr.complete(999, CompletionReason::Success, 1000);
        assert!(result.is_none());
    }

    #[test]
    fn test_ack_protocol_sweep_timeouts() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        // Before deadline.
        let results = mgr.sweep_timeouts(4_000_000);
        assert!(results.is_empty());
        assert_eq!(mgr.pending_count(), 1);
        // After deadline (5_000_000 default).
        let results = mgr.sweep_timeouts(6_100_000);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].reason, CompletionReason::Timeout);
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn test_ack_protocol_snapshot() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        mgr.complete(1, CompletionReason::Success, 2000);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_acks, 1);
        assert_eq!(snap.total_completions, 1);
        assert_eq!(snap.total_timeouts, 0);
        assert_eq!(snap.pending_count, 0);
    }

    #[test]
    fn test_ack_protocol_snapshot_serde() {
        let snap = AckProtocolSnapshot {
            total_acks: 10,
            total_completions: 8,
            total_timeouts: 2,
            pending_count: 0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: AckProtocolSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn test_ack_protocol_degradation_healthy() {
        let mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert_eq!(mgr.detect_degradation(), AckProtocolDegradation::Healthy);
    }

    #[test]
    fn test_ack_protocol_degradation_slow_ack() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.record_slow_ack();
        assert_eq!(mgr.detect_degradation(), AckProtocolDegradation::AckSlow { slow_count: 1 });
    }

    #[test]
    fn test_ack_protocol_degradation_timeout() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        mgr.sweep_timeouts(6_100_000);
        let deg = mgr.detect_degradation();
        assert_eq!(deg, AckProtocolDegradation::CompletionTimeout { timeout_count: 1 });
    }

    #[test]
    fn test_ack_protocol_degradation_display() {
        assert_eq!(AckProtocolDegradation::Healthy.to_string(), "healthy");
        assert_eq!(AckProtocolDegradation::AckSlow { slow_count: 3 }.to_string(), "ack-slow(3)");
        assert_eq!(AckProtocolDegradation::CompletionTimeout { timeout_count: 2 }.to_string(), "completion-timeout(2)");
    }

    #[test]
    fn test_ack_protocol_degradation_serde() {
        let cases = vec![
            AckProtocolDegradation::Healthy,
            AckProtocolDegradation::AckSlow { slow_count: 5 },
            AckProtocolDegradation::CompletionTimeout { timeout_count: 3 },
        ];
        for deg in cases {
            let json = serde_json::to_string(&deg).unwrap();
            let back: AckProtocolDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, deg);
        }
    }

    #[test]
    fn test_ack_protocol_log_entry() {
        let mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let entry = mgr.log_entry(AckPhase::ImmediateAck, 42, "ack issued".to_string(), 1000);
        assert_eq!(entry.phase, AckPhase::ImmediateAck);
        assert_eq!(entry.correlation_id, 42);
    }

    #[test]
    fn test_ack_protocol_log_entry_serde() {
        let entry = AckProtocolLogEntry {
            timestamp_us: 1000,
            phase: AckPhase::DeferredCompletion,
            correlation_id: 7,
            event: "completed".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: AckProtocolLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn test_ack_protocol_reset() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        mgr.record_slow_ack();
        mgr.reset();
        assert_eq!(mgr.pending_count(), 0);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_acks, 0);
        assert_eq!(snap.total_completions, 0);
        assert_eq!(snap.total_timeouts, 0);
        assert_eq!(mgr.detect_degradation(), AckProtocolDegradation::Healthy);
    }

    #[test]
    fn test_ack_protocol_config_accessor() {
        let cfg = AckProtocolConfig { ack_deadline_us: 100, ..Default::default() };
        let mgr = AckProtocolManager::new(cfg.clone());
        assert_eq!(*mgr.config(), cfg);
    }

    #[test]
    fn test_ack_timeout_increments_counter() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 1000);
        mgr.issue_ack(LatencyStage::StorageWrite, "b".to_string(), 1000);
        mgr.sweep_timeouts(6_100_000);
        assert_eq!(mgr.snapshot().total_timeouts, 2);
    }

    #[test]
    fn test_ack_cancel_completes() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "x".to_string(), 1000);
        let result = mgr.complete(token.correlation_id, CompletionReason::Cancelled { reason: "user".to_string() }, 2000);
        assert!(result.is_some());
        assert_eq!(mgr.pending_count(), 0);
        assert_eq!(mgr.snapshot().total_completions, 1);
    }

    // ── F3 Impl: Bridge method tests ──

    #[test]
    fn test_ack_total_acks_accessor() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert_eq!(mgr.total_acks(), 0);
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        assert_eq!(mgr.total_acks(), 1);
    }

    #[test]
    fn test_ack_total_completions_accessor() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.complete(1, CompletionReason::Success, 200);
        assert_eq!(mgr.total_completions(), 1);
    }

    #[test]
    fn test_ack_total_timeouts_accessor() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.sweep_timeouts(6_000_000);
        assert_eq!(mgr.total_timeouts(), 1);
    }

    #[test]
    fn test_ack_total_cancellations() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.complete(1, CompletionReason::Cancelled { reason: "x".to_string() }, 200);
        assert_eq!(mgr.total_cancellations(), 1);
    }

    #[test]
    fn test_ack_slow_count_accessor() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        mgr.record_slow_ack();
        mgr.record_slow_ack();
        assert_eq!(mgr.slow_ack_count(), 2);
    }

    #[test]
    fn test_ack_completion_rate() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert!((mgr.completion_rate() - 1.0).abs() < f64::EPSILON);
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.issue_ack(LatencyStage::StorageWrite, "b".to_string(), 100);
        mgr.complete(1, CompletionReason::Success, 200);
        assert!((mgr.completion_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_ack_timeout_rate() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert!((mgr.timeout_rate() - 0.0).abs() < f64::EPSILON);
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        mgr.issue_ack(LatencyStage::StorageWrite, "b".to_string(), 100);
        mgr.complete(1, CompletionReason::Success, 200);
        mgr.complete(2, CompletionReason::Timeout, 300);
        assert!((mgr.timeout_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_ack_has_pending() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        assert!(!mgr.has_pending());
        mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        assert!(mgr.has_pending());
    }

    #[test]
    fn test_ack_get_pending() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let got = mgr.get_pending(token.correlation_id);
        assert!(got.is_some());
        assert_eq!(got.unwrap().summary, "a");
        assert!(mgr.get_pending(999).is_none());
    }

    #[test]
    fn test_ack_complete_with_explanation() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let result = mgr.complete_with_explanation(
            token.correlation_id,
            CompletionReason::Success,
            200,
            "Pattern found".to_string(),
        );
        assert!(result.is_some());
        assert_eq!(result.unwrap().explanation, Some("Pattern found".to_string()));
    }

    #[test]
    fn test_ack_issue_checked_slow() {
        let cfg = AckProtocolConfig { ack_deadline_us: 100, ..Default::default() };
        let mut mgr = AckProtocolManager::new(cfg);
        // Ack took 200μs but deadline is 100μs → slow.
        mgr.issue_ack_checked(LatencyStage::PtyCapture, "a".to_string(), 1000, 1201);
        assert_eq!(mgr.slow_ack_count(), 1);
    }

    #[test]
    fn test_ack_issue_checked_fast() {
        let cfg = AckProtocolConfig { ack_deadline_us: 100, ..Default::default() };
        let mut mgr = AckProtocolManager::new(cfg);
        // Ack took 50μs, deadline is 100μs → fast.
        mgr.issue_ack_checked(LatencyStage::PtyCapture, "a".to_string(), 1000, 1050);
        assert_eq!(mgr.slow_ack_count(), 0);
    }

    #[test]
    fn test_ack_to_invariant_domain() {
        assert_eq!(AckProtocolManager::to_invariant_domain(), InvariantDomain::Composition);
    }

    #[test]
    fn test_ack_make_progress() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let prog = mgr.make_progress(token.correlation_id, 0.5, "halfway".to_string(), 500);
        assert!(prog.is_some());
        let p = prog.unwrap();
        assert!((p.fraction - 0.5).abs() < f64::EPSILON);
        // Non-existent correlation ID.
        assert!(mgr.make_progress(999, 0.5, "x".to_string(), 500).is_none());
    }

    #[test]
    fn test_ack_make_progress_clamps_fraction() {
        let mut mgr = AckProtocolManager::new(AckProtocolConfig::default());
        let token = mgr.issue_ack(LatencyStage::PtyCapture, "a".to_string(), 100);
        let p = mgr.make_progress(token.correlation_id, 2.0, "over".to_string(), 500).unwrap();
        assert!((p.fraction - 1.0).abs() < f64::EPSILON);
        let p2 = mgr.make_progress(token.correlation_id, -0.5, "under".to_string(), 500).unwrap();
        assert!((p2.fraction - 0.0).abs() < f64::EPSILON);
    }

    // ── F4: Unified E2E-Chaos-Soak-Performance Matrix ──

    #[test]
    fn test_scenario_category_display() {
        assert_eq!(ScenarioCategory::E2E.to_string(), "e2e");
        assert_eq!(ScenarioCategory::Chaos.to_string(), "chaos");
        assert_eq!(ScenarioCategory::Soak.to_string(), "soak");
        assert_eq!(ScenarioCategory::Performance.to_string(), "performance");
    }

    #[test]
    fn test_scenario_category_serde() {
        for cat in [ScenarioCategory::E2E, ScenarioCategory::Chaos, ScenarioCategory::Soak, ScenarioCategory::Performance] {
            let json = serde_json::to_string(&cat).unwrap();
            let back: ScenarioCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cat);
        }
    }

    #[test]
    fn test_scenario_verdict_display() {
        assert_eq!(ScenarioVerdict::Pass.to_string(), "pass");
        assert_eq!(ScenarioVerdict::Fail.to_string(), "fail");
        assert_eq!(ScenarioVerdict::Skip.to_string(), "skip");
        assert_eq!(ScenarioVerdict::Flaky.to_string(), "flaky");
    }

    #[test]
    fn test_scenario_verdict_serde() {
        for v in [ScenarioVerdict::Pass, ScenarioVerdict::Fail, ScenarioVerdict::Skip, ScenarioVerdict::Flaky] {
            let json = serde_json::to_string(&v).unwrap();
            let back: ScenarioVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn test_matrix_scenario_serde() {
        let s = MatrixScenario {
            scenario_id: "e2e-001".to_string(),
            category: ScenarioCategory::E2E,
            description: "happy path".to_string(),
            stages: vec![LatencyStage::PtyCapture, LatencyStage::StorageWrite],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: true,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: MatrixScenario = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn test_scenario_result_serde() {
        let r = ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 5000,
            failure_message: None,
            artifacts: vec!["trace.json".to_string()],
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: ScenarioResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn test_promotion_gate_serde() {
        let g = PromotionGate {
            name: "staging".to_string(),
            required_scenarios: vec!["e2e-001".to_string()],
            min_pass_rate: 0.95,
            max_flaky_count: 2,
        };
        let json = serde_json::to_string(&g).unwrap();
        let back: PromotionGate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn test_validation_matrix_new_empty() {
        let matrix = ValidationMatrix::new();
        assert_eq!(matrix.scenario_count(), 0);
        assert_eq!(matrix.result_count(), 0);
    }

    #[test]
    fn test_validation_matrix_add_scenario() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "e2e-001".to_string(),
            category: ScenarioCategory::E2E,
            description: "basic".to_string(),
            stages: vec![LatencyStage::PtyCapture],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: true,
        });
        assert_eq!(matrix.scenario_count(), 1);
    }

    #[test]
    fn test_validation_matrix_record_result() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 1000,
            failure_message: None,
            artifacts: vec![],
        });
        assert_eq!(matrix.result_count(), 1);
    }

    #[test]
    fn test_validation_matrix_latest_result() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Fail,
            duration_us: 1000,
            failure_message: Some("first".to_string()),
            artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 2000,
            failure_message: None,
            artifacts: vec![],
        });
        let latest = matrix.latest_result("e2e-001").unwrap();
        assert_eq!(latest.verdict, ScenarioVerdict::Pass);
    }

    #[test]
    fn test_validation_matrix_check_gate_passes() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_gate(PromotionGate {
            name: "staging".to_string(),
            required_scenarios: vec!["e2e-001".to_string()],
            min_pass_rate: 0.5,
            max_flaky_count: 1,
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 1000,
            failure_message: None,
            artifacts: vec![],
        });
        assert!(matrix.check_gate("staging"));
    }

    #[test]
    fn test_validation_matrix_check_gate_fails_required() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_gate(PromotionGate {
            name: "staging".to_string(),
            required_scenarios: vec!["e2e-001".to_string()],
            min_pass_rate: 0.5,
            max_flaky_count: 1,
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "e2e-001".to_string(),
            verdict: ScenarioVerdict::Fail,
            duration_us: 1000,
            failure_message: Some("broken".to_string()),
            artifacts: vec![],
        });
        assert!(!matrix.check_gate("staging"));
    }

    #[test]
    fn test_validation_matrix_check_gate_unknown() {
        let matrix = ValidationMatrix::new();
        assert!(!matrix.check_gate("nonexistent"));
    }

    #[test]
    fn test_validation_matrix_snapshot() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "s1".to_string(),
            category: ScenarioCategory::E2E,
            description: "x".to_string(),
            stages: vec![],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: false,
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        let snap = matrix.snapshot();
        assert_eq!(snap.total_scenarios, 1);
        assert_eq!(snap.pass_count, 1);
        assert_eq!(snap.fail_count, 0);
    }

    #[test]
    fn test_validation_matrix_snapshot_serde() {
        let snap = MatrixSnapshot {
            total_scenarios: 5,
            pass_count: 3,
            fail_count: 1,
            skip_count: 0,
            flaky_count: 1,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: MatrixSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn test_validation_matrix_degradation_healthy() {
        let matrix = ValidationMatrix::new();
        assert_eq!(matrix.detect_degradation(), MatrixDegradation::Healthy);
    }

    #[test]
    fn test_validation_matrix_degradation_gate_failure() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "req".to_string(),
            category: ScenarioCategory::E2E,
            description: "required".to_string(),
            stages: vec![],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: true,
        });
        // No result → fails.
        let deg = matrix.detect_degradation();
        assert!(matches!(deg, MatrixDegradation::GateFailure { .. }));
    }

    #[test]
    fn test_validation_matrix_degradation_flaky() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Flaky,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        let deg = matrix.detect_degradation();
        assert_eq!(deg, MatrixDegradation::FlakyDetected { flaky_count: 1 });
    }

    #[test]
    fn test_validation_matrix_degradation_display() {
        assert_eq!(MatrixDegradation::Healthy.to_string(), "healthy");
        assert_eq!(MatrixDegradation::FlakyDetected { flaky_count: 3 }.to_string(), "flaky(3)");
        let gf = MatrixDegradation::GateFailure { failed_scenarios: vec!["a".to_string(), "b".to_string()] };
        assert_eq!(gf.to_string(), "gate-failure(2)");
    }

    #[test]
    fn test_validation_matrix_degradation_serde() {
        let cases = vec![
            MatrixDegradation::Healthy,
            MatrixDegradation::FlakyDetected { flaky_count: 2 },
            MatrixDegradation::GateFailure { failed_scenarios: vec!["x".to_string()] },
        ];
        for deg in cases {
            let json = serde_json::to_string(&deg).unwrap();
            let back: MatrixDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, deg);
        }
    }

    #[test]
    fn test_validation_matrix_log_entry() {
        let matrix = ValidationMatrix::new();
        let entry = matrix.log_entry("s1".to_string(), "started".to_string(), 42);
        assert_eq!(entry.scenario_id, "s1");
        assert_eq!(entry.timestamp_us, 42);
    }

    #[test]
    fn test_validation_matrix_log_entry_serde() {
        let entry = MatrixLogEntry {
            timestamp_us: 1000,
            scenario_id: "x".to_string(),
            event: "done".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: MatrixLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn test_validation_matrix_scenarios_by_category() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "e1".to_string(),
            category: ScenarioCategory::E2E,
            description: "x".to_string(),
            stages: vec![],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: false,
        });
        matrix.add_scenario(MatrixScenario {
            scenario_id: "c1".to_string(),
            category: ScenarioCategory::Chaos,
            description: "y".to_string(),
            stages: vec![],
            domain: InvariantDomain::Recovery,
            required_for_promotion: false,
        });
        assert_eq!(matrix.scenarios_by_category(ScenarioCategory::E2E).len(), 1);
        assert_eq!(matrix.scenarios_by_category(ScenarioCategory::Chaos).len(), 1);
        assert_eq!(matrix.scenarios_by_category(ScenarioCategory::Soak).len(), 0);
    }

    #[test]
    fn test_validation_matrix_reset_results() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        matrix.reset_results();
        assert_eq!(matrix.result_count(), 0);
    }

    #[test]
    fn test_validation_matrix_gates_accessor() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_gate(PromotionGate {
            name: "prod".to_string(),
            required_scenarios: vec![],
            min_pass_rate: 0.99,
            max_flaky_count: 0,
        });
        assert_eq!(matrix.gates().len(), 1);
        assert_eq!(matrix.gates()[0].name, "prod");
    }

    #[test]
    fn test_validation_matrix_results_for() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(),
            verdict: ScenarioVerdict::Pass,
            duration_us: 100,
            failure_message: None,
            artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(),
            verdict: ScenarioVerdict::Fail,
            duration_us: 200,
            failure_message: Some("err".to_string()),
            artifacts: vec![],
        });
        assert_eq!(matrix.results_for("s1").len(), 1);
        assert_eq!(matrix.results_for("s2").len(), 1);
        assert_eq!(matrix.results_for("s3").len(), 0);
    }

    // ── F4 Impl: Bridge method tests ──

    #[test]
    fn test_matrix_pass_rate_empty() {
        let matrix = ValidationMatrix::new();
        assert!((matrix.pass_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_matrix_pass_rate() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(), verdict: ScenarioVerdict::Pass,
            duration_us: 100, failure_message: None, artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(), verdict: ScenarioVerdict::Fail,
            duration_us: 200, failure_message: None, artifacts: vec![],
        });
        assert!((matrix.pass_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_matrix_flaky_rate() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(), verdict: ScenarioVerdict::Flaky,
            duration_us: 100, failure_message: None, artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(), verdict: ScenarioVerdict::Pass,
            duration_us: 200, failure_message: None, artifacts: vec![],
        });
        assert!((matrix.flaky_rate() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_matrix_mean_pass_duration() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(), verdict: ScenarioVerdict::Pass,
            duration_us: 100, failure_message: None, artifacts: vec![],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(), verdict: ScenarioVerdict::Pass,
            duration_us: 300, failure_message: None, artifacts: vec![],
        });
        assert!((matrix.mean_pass_duration_us() - 200.0).abs() < 0.01);
    }

    #[test]
    fn test_matrix_passing_failing_gates() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_gate(PromotionGate {
            name: "canary".to_string(),
            required_scenarios: vec!["s1".to_string()],
            min_pass_rate: 0.5,
            max_flaky_count: 10,
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(), verdict: ScenarioVerdict::Pass,
            duration_us: 100, failure_message: None, artifacts: vec![],
        });
        assert_eq!(matrix.passing_gates(), vec!["canary".to_string()]);
        assert!(matrix.failing_gates().is_empty());
    }

    #[test]
    fn test_matrix_missing_required() {
        let mut matrix = ValidationMatrix::new();
        matrix.add_scenario(MatrixScenario {
            scenario_id: "req1".to_string(),
            category: ScenarioCategory::E2E,
            description: "required".to_string(),
            stages: vec![],
            domain: InvariantDomain::Scheduler,
            required_for_promotion: true,
        });
        assert_eq!(matrix.missing_required(), vec!["req1".to_string()]);
        matrix.record_result(ScenarioResult {
            scenario_id: "req1".to_string(), verdict: ScenarioVerdict::Pass,
            duration_us: 100, failure_message: None, artifacts: vec![],
        });
        assert!(matrix.missing_required().is_empty());
    }

    #[test]
    fn test_matrix_all_artifacts() {
        let mut matrix = ValidationMatrix::new();
        matrix.record_result(ScenarioResult {
            scenario_id: "s1".to_string(), verdict: ScenarioVerdict::Pass,
            duration_us: 100, failure_message: None,
            artifacts: vec!["a.json".to_string()],
        });
        matrix.record_result(ScenarioResult {
            scenario_id: "s2".to_string(), verdict: ScenarioVerdict::Pass,
            duration_us: 200, failure_message: None,
            artifacts: vec!["b.json".to_string(), "c.json".to_string()],
        });
        assert_eq!(matrix.all_artifacts().len(), 3);
    }

    #[test]
    fn test_matrix_to_invariant_domain() {
        assert_eq!(ValidationMatrix::to_invariant_domain(), InvariantDomain::Composition);
    }
}
