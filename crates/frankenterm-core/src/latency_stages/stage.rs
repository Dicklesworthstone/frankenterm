//! Pipeline stage identity and path constants.

use serde::{Deserialize, Serialize};
use std::fmt;

/// All stages on the critical path from PTY output to visible response.
///
/// Stages are ordered by their position in the pipeline. Each stage
/// represents a distinct latency-contributing operation with its own
/// budget, failure modes, and measurement points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum LatencyStage {
    /// PTY read -> raw bytes available.
    /// Dominated by kernel scheduling and PTY buffer flush timing.
    PtyCapture,

    /// Raw snapshot -> delta extraction via overlap matching.
    /// CPU-bound: string comparison against previous snapshot.
    DeltaExtraction,

    /// Delta -> persisted segment in SQLite.
    /// I/O-bound: WAL write + FTS trigger indexing.
    StorageWrite,

    /// Persisted segment -> pattern detection results.
    /// CPU-bound: Bloom filter -> Aho-Corasick -> regex extraction.
    PatternDetection,

    /// Detection -> event record persisted + bus fanout.
    /// Mixed: SQLite INSERT + broadcast channel send.
    EventEmission,

    /// Event -> workflow plan generated.
    /// CPU-bound: descriptor matching + plan construction.
    WorkflowDispatch,

    /// Workflow step -> action executed (send-text, wait-for, etc.).
    /// Variable: depends on action type and external I/O.
    ActionExecution,

    /// Request received -> JSON response serialized.
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

    /// Stages that compose the capture path (PTY -> event recorded).
    pub const CAPTURE_PATH: &[Self] = &[
        Self::PtyCapture,
        Self::DeltaExtraction,
        Self::StorageWrite,
        Self::PatternDetection,
        Self::EventEmission,
    ];

    /// Stages that compose the action path (event -> action complete).
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
