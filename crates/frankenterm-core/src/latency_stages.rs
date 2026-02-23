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
}
