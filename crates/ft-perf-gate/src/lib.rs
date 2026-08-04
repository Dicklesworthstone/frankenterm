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
use std::{
    collections::{BTreeMap, VecDeque},
    env,
};
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
pub use snc::{HillEstimate, SncBound, SncConfig, compute_snc_bound, hill_estimate};
pub use sprt::{SprtConfig, SprtReport};

/// Schema marker for per-claim evidence samples consumed by proof gates.
pub const EVIDENCE_SAMPLE_SCHEMA_VERSION: &str = "ft.perf.evidence-sample.v1";

/// Schema marker for proof-gate telemetry events emitted for Robot Mode.
pub const GATE_METRIC_EVENT_SCHEMA_VERSION: &str = "ft.perf.gate-event.v1";

/// Environment variable selecting the regression-decision driver.
pub const FT_PERF_GATE_MODE_ENV: &str = "FT_PERF_GATE_MODE";

/// Environment variable selecting the optional distribution-band driver.
pub const FT_PERF_GATE_BANDS_ENV: &str = "FT_PERF_GATE_BANDS";

/// Default hard sample cap for the additive perf-gate driver.
pub const PERF_GATE_DEFAULT_MAX_SAMPLES: usize = 1_024;

const INVALID_EVIDENCE_REASON: &str = "sample failed required field validation";

/// Regression-decision mode wired by [`FT_PERF_GATE_MODE_ENV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfGateMode {
    /// Legacy fixed-sample mean threshold. This is the default.
    Fixed,
    /// Wald sequential probability-ratio test.
    Sprt,
    /// Howard-et-al anytime-valid confidence sequence.
    Anytime,
}

impl Default for PerfGateMode {
    fn default() -> Self {
        Self::Fixed
    }
}

impl PerfGateMode {
    /// Parse a mode from an optional environment value.
    pub fn from_env_value(value: Option<&str>) -> Result<Self, GateDecision> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::Fixed);
        };
        match value {
            "fixed" => Ok(Self::Fixed),
            "sprt" => Ok(Self::Sprt),
            "anytime" => Ok(Self::Anytime),
            other => Err(GateDecision::LowConfidence {
                reason: format!(
                    "{FT_PERF_GATE_MODE_ENV} must be one of fixed|sprt|anytime; got {other:?}"
                ),
                confidence: None,
            }),
        }
    }

    /// Parse a mode from [`FT_PERF_GATE_MODE_ENV`].
    pub fn from_env() -> Result<Self, GateDecision> {
        let value = env::var(FT_PERF_GATE_MODE_ENV).ok();
        Self::from_env_value(value.as_deref())
    }
}

/// Distribution-band mode wired by [`FT_PERF_GATE_BANDS_ENV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfGateBandMode {
    /// Legacy fixed 10% mean threshold only. This is the default.
    Fixed,
    /// Split-conformal upper band clamped by the legacy fixed threshold.
    Conformal,
}

impl Default for PerfGateBandMode {
    fn default() -> Self {
        Self::Fixed
    }
}

impl PerfGateBandMode {
    /// Parse a band mode from an optional environment value.
    pub fn from_env_value(value: Option<&str>) -> Result<Self, GateDecision> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::Fixed);
        };
        match value {
            "fixed" => Ok(Self::Fixed),
            "conformal" => Ok(Self::Conformal),
            other => Err(GateDecision::LowConfidence {
                reason: format!("{FT_PERF_GATE_BANDS_ENV} must be fixed|conformal; got {other:?}"),
                confidence: None,
            }),
        }
    }

    /// Parse a band mode from [`FT_PERF_GATE_BANDS_ENV`].
    pub fn from_env() -> Result<Self, GateDecision> {
        let value = env::var(FT_PERF_GATE_BANDS_ENV).ok();
        Self::from_env_value(value.as_deref())
    }
}

/// Configuration for the additive round-4 perf-gate driver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerfGateDriverConfig {
    /// Baseline fixed-threshold value from the accepted arm.
    pub baseline: f64,
    /// Relative regression threshold; `0.10` is the legacy 10% gate.
    pub relative_threshold: f64,
    /// Minimum samples before a terminal statistical decision is allowed.
    pub min_samples: usize,
    /// Hard cap on samples consumed by any mode.
    pub max_samples: usize,
    /// Confidence carried by the legacy fixed gate.
    pub confidence: Option<f64>,
    /// Type-I error rate for sequential/anytime modes and conformal bands.
    pub alpha: f64,
    /// Type-II error rate for Wald SPRT.
    pub beta: f64,
    /// Measurement-noise scale for sequential/anytime modes.
    pub sigma: f64,
    /// Regression-decision mode.
    pub mode: PerfGateMode,
    /// Optional distribution-band mode.
    pub bands: PerfGateBandMode,
    /// Split-conformal calibration settings for [`PerfGateBandMode::Conformal`].
    pub conformal: conformal::SplitConformalConfig,
}

impl PerfGateDriverConfig {
    /// Build a config whose default path is the legacy fixed 10% gate.
    #[must_use]
    pub fn fixed(baseline: f64) -> Self {
        Self {
            baseline,
            relative_threshold: 0.10,
            min_samples: 2,
            max_samples: PERF_GATE_DEFAULT_MAX_SAMPLES,
            confidence: Some(0.95),
            alpha: 0.05,
            beta: 0.05,
            sigma: 1.0,
            mode: PerfGateMode::Fixed,
            bands: PerfGateBandMode::Fixed,
            conformal: conformal::SplitConformalConfig::default(),
        }
    }

    /// Build a config from the default fixed gate plus `FT_PERF_GATE_*`.
    pub fn from_env(baseline: f64) -> Result<Self, GateDecision> {
        let mut config = Self::fixed(baseline);
        config.mode = PerfGateMode::from_env()?;
        config.bands = PerfGateBandMode::from_env()?;
        Ok(config)
    }

    fn fixed_config(&self) -> sprt::SprtConfig {
        sprt::SprtConfig {
            baseline: self.baseline,
            relative_threshold: self.relative_threshold,
            min_samples: self.min_samples,
            confidence: self.confidence,
        }
    }

    fn wald_config(&self) -> sprt::WaldSprtConfig {
        sprt::WaldSprtConfig {
            mu_null: self.baseline,
            mu_alt: self.baseline * (1.0 + self.relative_threshold),
            sigma: self.sigma,
            alpha: self.alpha,
            beta: self.beta,
            min_samples: self.min_samples,
            max_samples: self.max_samples,
        }
    }

    fn anytime_config(&self) -> sprt::AnytimeValidCiConfig {
        sprt::AnytimeValidCiConfig {
            sigma: self.sigma,
            alpha: self.alpha,
            threshold: self.baseline * (1.0 + self.relative_threshold),
            test_kind: sprt::AnytimeValidTest::UpperBoundMustHold,
            min_samples: self.min_samples,
            max_samples: self.max_samples,
        }
    }
}

/// Structured result from the additive round-4 perf-gate driver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerfGateDriverReport {
    /// Claim identifier being evaluated.
    pub claim_id: String,
    /// Regression-decision mode used for this report.
    pub mode: PerfGateMode,
    /// Distribution-band mode used for this report.
    pub bands: PerfGateBandMode,
    /// Candidate samples consumed after the hard cap.
    pub sample_count: usize,
    /// Baseline calibration samples consumed after the hard cap.
    pub baseline_sample_count: usize,
    /// Candidate arithmetic mean over consumed samples.
    pub mean: f64,
    /// Baseline value used by the fixed threshold.
    pub baseline: f64,
    /// Legacy fixed upper threshold, normally `baseline * 1.10`.
    pub legacy_upper_bound: f64,
    /// Optional clamped conformal band.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<ConformalBand>,
    /// Final fail-closed gate decision.
    pub decision: GateDecision,
    /// True only when the final decision is [`GateDecision::Accept`].
    pub keep_candidate: bool,
}

/// Evaluate candidate evidence through the round-4 driver.
#[must_use]
pub fn evaluate_perf_gate_driver(
    candidate_samples: &[EvidenceSample],
    baseline_samples: &[EvidenceSample],
    config: &PerfGateDriverConfig,
) -> PerfGateDriverReport {
    let max_samples = config.max_samples.max(1);
    let candidate_samples = capped_samples(candidate_samples, max_samples);
    let baseline_samples = capped_samples(baseline_samples, max_samples);
    let claim_id = candidate_samples
        .first()
        .or_else(|| baseline_samples.first())
        .map_or_else(|| "unknown".to_string(), |sample| sample.claim_id.clone());
    let legacy_upper_bound = config.baseline * (1.0 + config.relative_threshold);
    let mean = mean_metric_value(candidate_samples).unwrap_or(f64::NAN);

    let primary = validate_driver_config(config)
        .unwrap_or_else(|| evaluate_driver_mode(candidate_samples, config));
    let (decision, band) = if matches!(primary, GateDecision::Accept { .. })
        && config.bands == PerfGateBandMode::Conformal
    {
        match evaluate_conformal_upper_band(candidate_samples, baseline_samples, config) {
            Ok((band, decision)) => (decision, Some(band)),
            Err(decision) => (decision, None),
        }
    } else {
        (primary, None)
    };
    let keep_candidate = matches!(decision, GateDecision::Accept { .. });

    PerfGateDriverReport {
        claim_id,
        mode: config.mode,
        bands: config.bands,
        sample_count: candidate_samples.len(),
        baseline_sample_count: baseline_samples.len(),
        mean,
        baseline: config.baseline,
        legacy_upper_bound,
        band,
        decision,
        keep_candidate,
    }
}

fn capped_samples(samples: &[EvidenceSample], max_samples: usize) -> &[EvidenceSample] {
    let end = samples.len().min(max_samples);
    &samples[..end]
}

fn validate_driver_config(config: &PerfGateDriverConfig) -> Option<GateDecision> {
    if !config.baseline.is_finite()
        || config.baseline <= 0.0
        || !config.relative_threshold.is_finite()
        || config.relative_threshold < 0.0
        || config.min_samples == 0
        || config.max_samples == 0
    {
        return Some(GateDecision::LowConfidence {
            reason: "perf-gate driver config malformed (baseline, threshold, or sample bounds)"
                .to_string(),
            confidence: None,
        });
    }
    None
}

fn evaluate_driver_mode(
    candidate_samples: &[EvidenceSample],
    config: &PerfGateDriverConfig,
) -> GateDecision {
    match config.mode {
        PerfGateMode::Fixed => {
            sprt::evaluate_samples(candidate_samples, &config.fixed_config()).decision
        }
        PerfGateMode::Sprt => {
            sprt::evaluate_wald_sprt(candidate_samples, &config.wald_config()).decision
        }
        PerfGateMode::Anytime => {
            sprt::evaluate_anytime_valid_ci(candidate_samples, &config.anytime_config()).decision
        }
    }
}

fn evaluate_conformal_upper_band(
    candidate_samples: &[EvidenceSample],
    baseline_samples: &[EvidenceSample],
    config: &PerfGateDriverConfig,
) -> Result<(ConformalBand, GateDecision), GateDecision> {
    let mut band = conformal::fit_split_conformal_band(baseline_samples, &config.conformal)?;
    let legacy_upper = config.baseline * (1.0 + config.relative_threshold);
    if !legacy_upper.is_finite() {
        return Err(GateDecision::LowConfidence {
            reason: "legacy fixed upper bound is not finite".to_string(),
            confidence: None,
        });
    }
    band.upper = band.upper.min(legacy_upper);
    if band.lower > band.upper {
        return Err(GateDecision::LowConfidence {
            reason: "clamped conformal band lower bound exceeds legacy upper bound".to_string(),
            confidence: None,
        });
    }
    let Some(candidate_max) = max_metric_value(candidate_samples) else {
        return Err(GateDecision::LowConfidence {
            reason: "candidate samples must be non-empty and finite for conformal band gating"
                .to_string(),
            confidence: None,
        });
    };
    if candidate_max <= band.upper {
        Ok((
            band,
            GateDecision::Accept {
                reason: "candidate samples stay within clamped conformal upper band".to_string(),
                confidence: Some(1.0 - config.conformal.alpha),
            },
        ))
    } else {
        Ok((
            band,
            GateDecision::Reject {
                reason: "candidate sample exceeds clamped conformal upper band".to_string(),
                confidence: Some(1.0 - config.conformal.alpha),
            },
        ))
    }
}

fn mean_metric_value(samples: &[EvidenceSample]) -> Option<f64> {
    let len = u32::try_from(samples.len()).ok()?;
    if len == 0 {
        return None;
    }
    Some(
        samples
            .iter()
            .map(|sample| sample.metric_value)
            .sum::<f64>()
            / f64::from(len),
    )
}

fn max_metric_value(samples: &[EvidenceSample]) -> Option<f64> {
    let mut max = None;
    for value in samples.iter().map(|sample| sample.metric_value) {
        if !value.is_finite() {
            return None;
        }
        max = Some(match max {
            Some(existing) if existing >= value => existing,
            _ => value,
        });
    }
    max
}

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
    fn perf_gate_driver_env_modes_default_to_fixed() {
        assert_eq!(
            PerfGateMode::from_env_value(None).unwrap(),
            PerfGateMode::Fixed
        );
        assert_eq!(
            PerfGateBandMode::from_env_value(None).unwrap(),
            PerfGateBandMode::Fixed
        );
        assert!(matches!(
            PerfGateMode::from_env_value(Some("bogus")),
            Err(GateDecision::LowConfidence { .. })
        ));
        assert!(matches!(
            PerfGateBandMode::from_env_value(Some("bogus")),
            Err(GateDecision::LowConfidence { .. })
        ));
    }

    #[test]
    fn perf_gate_driver_fixed_mode_matches_legacy_mean_gate() {
        let samples = vec![
            EvidenceSample::new("robot.p95", 10.0, "ms", 1, 1),
            EvidenceSample::new("robot.p95", 11.0, "ms", 1, 2),
            EvidenceSample::new("robot.p95", 12.0, "ms", 1, 3),
        ];
        let config = PerfGateDriverConfig {
            max_samples: samples.len(),
            ..PerfGateDriverConfig::fixed(10.0)
        };
        let legacy = sprt::evaluate_samples(&samples, &config.fixed_config());
        let report = evaluate_perf_gate_driver(&samples, &[], &config);

        assert_eq!(report.mode, PerfGateMode::Fixed);
        assert_eq!(report.bands, PerfGateBandMode::Fixed);
        assert_eq!(report.mean.to_bits(), legacy.mean.to_bits());
        assert_eq!(report.decision, legacy.decision);
        assert_eq!(
            report.keep_candidate,
            matches!(legacy.decision, GateDecision::Accept { .. })
        );
    }

    #[test]
    fn perf_gate_driver_wald_sprt_oc_curve_stays_within_alpha_beta() {
        let trials = 256_u64;
        let mut config = PerfGateDriverConfig::fixed(10.0);
        config.mode = PerfGateMode::Sprt;
        config.relative_threshold = 0.20;
        config.sigma = 1.0;
        config.alpha = 0.05;
        config.beta = 0.05;
        config.min_samples = 4;
        config.max_samples = 200;

        let mut false_rejects = 0_u64;
        let mut false_accepts = 0_u64;
        for trial in 0..trials {
            let mut h0_seed = 0xC0FF_EE00_u64 ^ trial;
            let h0 = gaussian_samples(
                "robot.p95",
                10.0,
                config.sigma,
                config.max_samples,
                &mut h0_seed,
            );
            let h0_report = evaluate_perf_gate_driver(&h0, &[], &config);
            if matches!(h0_report.decision, GateDecision::Reject { .. }) {
                false_rejects += 1;
            }

            let mut h1_seed = 0xBADB_EE00_u64 ^ trial;
            let h1 = gaussian_samples(
                "robot.p95",
                config.baseline * (1.0 + config.relative_threshold),
                config.sigma,
                config.max_samples,
                &mut h1_seed,
            );
            let h1_report = evaluate_perf_gate_driver(&h1, &[], &config);
            if matches!(h1_report.decision, GateDecision::Accept { .. }) {
                false_accepts += 1;
            }
        }

        let false_reject_rate = (false_rejects as f64) / (trials as f64);
        let false_accept_rate = (false_accepts as f64) / (trials as f64);
        assert!(
            false_reject_rate <= config.alpha,
            "false-reject rate {false_reject_rate} exceeded alpha {}",
            config.alpha
        );
        assert!(
            false_accept_rate <= config.beta,
            "false-accept rate {false_accept_rate} exceeded beta {}",
            config.beta
        );
    }

    #[test]
    fn perf_gate_driver_conformal_rejects_flat_median_inflated_p999() {
        let baseline = repeated_samples("robot.p999", 100.0, 64);
        let mut candidate = repeated_samples("robot.p999", 100.0, 999);
        candidate.push(EvidenceSample::new("robot.p999", 10_000.0, "ms", 1, 1_000));

        let fixed_config = PerfGateDriverConfig {
            max_samples: candidate.len(),
            ..PerfGateDriverConfig::fixed(100.0)
        };
        let fixed_report = evaluate_perf_gate_driver(&candidate, &baseline, &fixed_config);
        assert!(
            matches!(fixed_report.decision, GateDecision::Accept { .. }),
            "legacy mean gate should accept the flat-median sample set; got {:?}",
            fixed_report.decision
        );
        assert!(fixed_report.mean <= fixed_report.legacy_upper_bound);

        let mut conformal_config = fixed_config.clone();
        conformal_config.bands = PerfGateBandMode::Conformal;
        conformal_config.conformal = conformal::SplitConformalConfig {
            alpha: 0.05,
            calibration_fraction: 0.5,
            min_calibration_samples: 20,
        };
        let conformal_report = evaluate_perf_gate_driver(&candidate, &baseline, &conformal_config);
        assert!(
            matches!(conformal_report.decision, GateDecision::Reject { .. }),
            "clamped conformal band should reject inflated p999; got {:?}",
            conformal_report.decision
        );
        assert!(!conformal_report.keep_candidate);
        let band = conformal_report
            .band
            .expect("conformal band should be reported");
        assert!(band.upper <= conformal_report.legacy_upper_bound);
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

    fn repeated_samples(claim_id: &str, value: f64, count: u64) -> Vec<EvidenceSample> {
        (0..count)
            .map(|i| EvidenceSample::new(claim_id, value, "ms", 1, i + 1))
            .collect()
    }

    fn gaussian_samples(
        claim_id: &str,
        mean: f64,
        sigma: f64,
        count: usize,
        seed: &mut u64,
    ) -> Vec<EvidenceSample> {
        (0..count)
            .map(|i| {
                EvidenceSample::new(
                    claim_id,
                    gaussian(seed).mul_add(sigma, mean),
                    "ms",
                    1,
                    u64::try_from(i + 1).unwrap(),
                )
            })
            .collect()
    }

    fn gaussian(seed: &mut u64) -> f64 {
        let u1 = xorshift_uniform(seed).max(1e-12);
        let u2 = xorshift_uniform(seed);
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    fn xorshift_uniform(seed: &mut u64) -> f64 {
        let mut state = *seed;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *seed = state;
        (state as f64) / (u64::MAX as f64)
    }
}
