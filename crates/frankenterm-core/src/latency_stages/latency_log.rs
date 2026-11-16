//! Structured logging contract for latency-stage decisions.

use serde::{Deserialize, Serialize};

/// Required fields for every latency log entry.
///
/// This struct defines the structured logging contract for the AARSP
/// latency pipeline. Every log entry at critical decision points and
/// stage boundaries must include these fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
