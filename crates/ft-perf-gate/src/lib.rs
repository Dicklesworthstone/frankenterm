//! Performance proof-gate substrate for FrankenTerm evidence streams.
//!
//! This crate is intentionally small and leaf-clean. It supplies shared data
//! contracts for the statistical proof gates tracked by `ft-tf6g3.30`:
//! sequential regression decisions, conformal SLO bands, KL-divergence regime
//! shift checks, and causal attribution. Downstream leaves can compose or
//! extend the gate implementations without changing the evidence stream or
//! telemetry event shape.
//!
//! ```
//! use ft_perf_gate::{EvidenceSample, EvidenceStream, GateDecision, VecEvidenceStream};
//!
//! let samples = vec![EvidenceSample::new("robot.p95", 4.2, "ms", 1, 1_000)];
//! let mut stream = VecEvidenceStream::new(samples);
//! let first = stream.next_sample().unwrap().unwrap();
//! assert_eq!(first.claim_id, "robot.p95");
//!
//! let decision = GateDecision::Accept {
//!     reason: "below target".into(),
//!     confidence: Some(0.99),
//! };
//! assert!(decision.is_terminal());
//! ```

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use thiserror::Error;

pub mod causal_attribution;
pub mod conformal;
pub mod regime_shift;
pub mod snc;
pub mod sprt;

pub use causal_attribution::{
    AttributionCandidate, AttributionReport, CAUSAL_GRAPH_SCHEMA_VERSION, CausalAttributionConfig,
    CausalEdge, CausalGraphReport, CausalVariable, CausalVariableRole,
    REGRESSION_ATTRIBUTION_SCHEMA_VERSION, RegressionAttributionReport, SeparatingSet,
    attribute_regression_event, infer_pc_skeleton,
};
pub use conformal::{ConformalBand, ConformalConfig};
pub use regime_shift::{RegimeShiftConfig, RegimeShiftReport};
pub use snc::{
    HillEstimate, MgfServiceCurve, SncBound, SncConfig, compute_snc_bound, hill_estimate,
};
pub use sprt::{SprtConfig, SprtReport};

/// Schema marker for per-claim evidence samples consumed by proof gates.
pub const EVIDENCE_SAMPLE_SCHEMA_VERSION: &str = "ft.perf.evidence-sample.v1";

/// Schema marker for proof-gate telemetry events emitted for Robot Mode.
pub const GATE_METRIC_EVENT_SCHEMA_VERSION: &str = "ft.perf.gate-event.v1";

const INVALID_EVIDENCE_REASON: &str = "sample failed required field validation";

/// One normalized measurement in a per-claim evidence JSONL stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceSample {
    /// Stable schema version for JSONL compatibility.
    pub schema_version: String,
    /// Event timestamp in Unix milliseconds.
    pub ts_ms: u64,
    /// Stable claim identifier, for example `robot.p95`.
    pub claim_id: String,
    /// Numeric measurement value.
    pub metric_value: f64,
    /// Unit of `metric_value`, for example `ms`, `bytes`, or `count`.
    pub metric_unit: String,
    /// Number of observations represented by this sample.
    pub sample_size: u64,
    /// Commit SHA that produced the sample, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    /// Stable hardware fingerprint, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_fingerprint: Option<String>,
    /// Runner SKU or target class, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_sku: Option<String>,
    /// Workload class used for grouping comparable samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_class: Option<String>,
    /// Extensible string metadata kept deterministic for JSON output.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl EvidenceSample {
    /// Build a minimal sample for tests, doctests, and synthetic gates.
    #[must_use]
    pub fn new(
        claim_id: impl Into<String>,
        metric_value: f64,
        metric_unit: impl Into<String>,
        sample_size: u64,
        ts_ms: u64,
    ) -> Self {
        Self {
            schema_version: EVIDENCE_SAMPLE_SCHEMA_VERSION.to_string(),
            ts_ms,
            claim_id: claim_id.into(),
            metric_value,
            metric_unit: metric_unit.into(),
            sample_size,
            commit_sha: None,
            hardware_fingerprint: None,
            runner_sku: None,
            workload_class: None,
            tags: BTreeMap::new(),
        }
    }

    /// Return true when this sample is finite and has a non-empty claim/unit.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.schema_version == EVIDENCE_SAMPLE_SCHEMA_VERSION
            && !self.claim_id.trim().is_empty()
            && self.metric_value.is_finite()
            && !self.metric_unit.trim().is_empty()
            && self.sample_size > 0
    }
}

/// Pull-based abstraction over per-claim evidence JSONL.
///
/// Implementations are intentionally synchronous: the proof-gate algorithms
/// work over bounded artifacts and should be runnable in CI, doctests, and
/// Robot Mode status surfaces without owning an async runtime.
pub trait EvidenceStream {
    /// Stream-specific error type.
    type Error;

    /// Return the next sample, or `Ok(None)` at the end of the stream.
    fn next_sample(&mut self) -> Result<Option<EvidenceSample>, Self::Error>;

    /// Drain at most `limit` samples from the stream.
    fn collect_limited(&mut self, limit: usize) -> Result<Vec<EvidenceSample>, Self::Error> {
        let mut samples = Vec::new();
        while samples.len() < limit {
            let Some(sample) = self.next_sample()? else {
                break;
            };
            samples.push(sample);
        }
        Ok(samples)
    }
}

/// Errors raised while parsing or validating evidence streams.
#[derive(Debug, Error)]
pub enum PerfGateError {
    /// A JSONL row did not deserialize into an evidence sample.
    #[error("invalid evidence JSON at line {line}: {source}")]
    Json {
        /// 1-based line number.
        line: usize,
        /// Underlying JSON error.
        source: serde_json::Error,
    },
    /// A deserialized evidence sample failed semantic validation.
    #[error("invalid evidence sample at line {line}: {reason}")]
    InvalidEvidence {
        /// 1-based line number.
        line: usize,
        /// Operator-facing reason.
        reason: &'static str,
    },
}

/// In-memory JSONL evidence stream for small proof fixtures and CI summaries.
#[derive(Debug, Clone)]
pub struct JsonlEvidenceStream {
    lines: VecDeque<String>,
    line_number: usize,
}

impl JsonlEvidenceStream {
    /// Create a stream from newline-delimited JSON evidence rows.
    #[must_use]
    pub fn from_text(input: &str) -> Self {
        Self {
            lines: input.lines().map(str::to_owned).collect(),
            line_number: 0,
        }
    }
}

impl EvidenceStream for JsonlEvidenceStream {
    type Error = PerfGateError;

    fn next_sample(&mut self) -> Result<Option<EvidenceSample>, Self::Error> {
        while let Some(line) = self.lines.pop_front() {
            self.line_number += 1;
            if line.trim().is_empty() {
                continue;
            }
            let sample: EvidenceSample =
                serde_json::from_str(&line).map_err(|source| PerfGateError::Json {
                    line: self.line_number,
                    source,
                })?;
            if sample.is_well_formed() {
                return Ok(Some(sample));
            }
            return Err(PerfGateError::InvalidEvidence {
                line: self.line_number,
                reason: INVALID_EVIDENCE_REASON,
            });
        }
        Ok(None)
    }
}

/// In-memory evidence stream for doctests and small proof fixtures.
#[derive(Debug, Clone)]
pub struct VecEvidenceStream {
    samples: Vec<EvidenceSample>,
    next_index: usize,
}

impl VecEvidenceStream {
    /// Create a stream over `samples` in insertion order.
    #[must_use]
    pub fn new(samples: Vec<EvidenceSample>) -> Self {
        Self {
            samples,
            next_index: 0,
        }
    }
}

impl EvidenceStream for VecEvidenceStream {
    type Error = std::convert::Infallible;

    fn next_sample(&mut self) -> Result<Option<EvidenceSample>, Self::Error> {
        let sample = self.samples.get(self.next_index).cloned();
        if sample.is_some() {
            self.next_index += 1;
        }
        Ok(sample)
    }
}

/// Canonical proof-gate decisions consumed by Robot Mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateDecision {
    /// The claim is accepted under the gate's current evidence and threshold.
    Accept {
        /// Human-readable explanation for operators.
        reason: String,
        /// Optional confidence or posterior probability.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
    /// The claim is rejected under the gate's current evidence and threshold.
    Reject {
        /// Human-readable explanation for operators.
        reason: String,
        /// Optional confidence or posterior probability.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
    /// More evidence is required before a sound decision can be made.
    Continue {
        /// Human-readable explanation for operators.
        reason: String,
        /// Minimum additional samples requested by the gate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        needed_samples: Option<u64>,
    },
    /// Evidence appears to come from a different regime and should gate decisions.
    RegimeShift {
        /// Human-readable explanation for operators.
        reason: String,
        /// Divergence or distance score that triggered the shift.
        divergence: f64,
    },
    /// Evidence exists, but not enough confidence to accept or reject.
    LowConfidence {
        /// Human-readable explanation for operators.
        reason: String,
        /// Optional confidence or posterior probability.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
}

impl GateDecision {
    /// Stable decision kind for metric labels and Robot Mode status.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Accept { .. } => "accept",
            Self::Reject { .. } => "reject",
            Self::Continue { .. } => "continue",
            Self::RegimeShift { .. } => "regime_shift",
            Self::LowConfidence { .. } => "low_confidence",
        }
    }

    /// Return true when the gate should stop consuming evidence.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Accept { .. } | Self::Reject { .. } | Self::RegimeShift { .. }
        )
    }

    /// Extract the operator-facing reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Accept { reason, .. }
            | Self::Reject { reason, .. }
            | Self::Continue { reason, .. }
            | Self::RegimeShift { reason, .. }
            | Self::LowConfidence { reason, .. } => reason,
        }
    }

    /// Build a structured event ready for `ft robot perf gate-status`.
    #[must_use]
    pub fn to_metric_event(
        &self,
        gate_id: impl Into<String>,
        claim_id: impl Into<String>,
        sample_count: u64,
        emitted_at_ms: u64,
    ) -> GateMetricEvent {
        GateMetricEvent {
            schema_version: GATE_METRIC_EVENT_SCHEMA_VERSION.to_string(),
            gate_id: gate_id.into(),
            claim_id: claim_id.into(),
            decision_kind: self.kind().to_string(),
            decision: self.clone(),
            reason: self.reason().to_string(),
            sample_count,
            emitted_at_ms,
            details: BTreeMap::new(),
        }
    }
}

/// Structured metric emitted whenever a proof gate makes a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateMetricEvent {
    /// Stable schema version for Robot Mode consumers.
    pub schema_version: String,
    /// Gate identifier, for example `sprt` or `conformal`.
    pub gate_id: String,
    /// Claim identifier evaluated by the gate.
    pub claim_id: String,
    /// Stable decision kind for low-cardinality metrics.
    pub decision_kind: String,
    /// Full decision payload.
    pub decision: GateDecision,
    /// Operator-facing reason duplicated for compact status tables.
    pub reason: String,
    /// Number of samples consumed by this gate decision.
    pub sample_count: u64,
    /// Event timestamp in Unix milliseconds.
    pub emitted_at_ms: u64,
    /// Extensible deterministic metadata for downstream Robot Mode surfaces.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_sample_validates_required_fields() {
        let sample = EvidenceSample::new("robot.p95", 1.0, "ms", 3, 42);
        assert!(sample.is_well_formed());

        let mut invalid = sample.clone();
        invalid.metric_value = f64::NAN;
        assert!(!invalid.is_well_formed());
    }

    #[test]
    fn vector_stream_drains_in_order() -> Result<(), String> {
        let samples = vec![
            EvidenceSample::new("a", 1.0, "ms", 1, 1),
            EvidenceSample::new("a", 2.0, "ms", 1, 2),
        ];
        let mut stream = VecEvidenceStream::new(samples);
        let drained = match stream.collect_limited(8) {
            Ok(samples) => samples,
            Err(err) => match err {},
        };
        assert_eq!(drained.len(), 2);
        let mut drained_iter = drained.iter();
        let first = drained_iter
            .next()
            .ok_or_else(|| "first sample missing after len check".to_string())?;
        let second = drained_iter
            .next()
            .ok_or_else(|| "second sample missing after len check".to_string())?;
        assert!((first.metric_value - 1.0).abs() < f64::EPSILON);
        assert!((second.metric_value - 2.0).abs() < f64::EPSILON);
        let next = match stream.next_sample() {
            Ok(next) => next,
            Err(err) => match err {},
        };
        assert!(next.is_none());
        Ok(())
    }

    #[test]
    fn metric_event_preserves_decision_contract() {
        let decision = GateDecision::Reject {
            reason: "regression".into(),
            confidence: Some(0.98),
        };
        let event = decision.to_metric_event("sprt", "robot.p95", 12, 99);
        assert_eq!(event.schema_version, GATE_METRIC_EVENT_SCHEMA_VERSION);
        assert_eq!(event.decision_kind, "reject");
        assert_eq!(event.reason, "regression");
        assert!(event.decision.is_terminal());
    }

    #[test]
    fn jsonl_stream_rejects_invalid_rows() -> Result<(), String> {
        let invalid = r#"{"schema_version":"bad","ts_ms":1,"claim_id":"x","metric_value":1.0,"metric_unit":"ms","sample_size":1}"#;
        let mut stream = JsonlEvidenceStream::from_text(invalid);
        let err = match stream.next_sample() {
            Ok(sample) => return Err(format!("expected error, got {sample:?}")),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            PerfGateError::InvalidEvidence { line: 1, .. }
        ));
        Ok(())
    }

    #[test]
    fn jsonl_stream_parses_valid_rows() -> Result<(), String> {
        let sample = EvidenceSample::new("robot.p95", 4.2, "ms", 1, 1_000);
        let json = serde_json::to_string(&sample).map_err(|err| err.to_string())?;
        let mut stream = JsonlEvidenceStream::from_text(&json);
        let parsed = stream
            .next_sample()
            .map_err(|err| err.to_string())?
            .ok_or_else(|| "expected one parsed sample".to_string())?;
        assert_eq!(parsed.claim_id, sample.claim_id);
        assert!((parsed.metric_value - sample.metric_value).abs() < f64::EPSILON);
        assert!(
            stream
                .next_sample()
                .map_err(|err| err.to_string())?
                .is_none()
        );
        Ok(())
    }
}
