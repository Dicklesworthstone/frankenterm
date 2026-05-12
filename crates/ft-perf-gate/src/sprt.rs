//! Sequential performance regression gate scaffolding.
//!
//! ```
//! use ft_perf_gate::{sprt, EvidenceSample, VecEvidenceStream};
//!
//! let samples = vec![
//!     EvidenceSample::new("robot.p95", 10.0, "ms", 1, 1),
//!     EvidenceSample::new("robot.p95", 11.0, "ms", 1, 2),
//! ];
//! let mut stream = VecEvidenceStream::new(samples);
//! let report = sprt::evaluate_sprt(&mut stream, &sprt::SprtConfig::new(10.0)).unwrap();
//! assert!(report.decision.is_terminal());
//! ```

use crate::{EvidenceSample, EvidenceStream, GateDecision};
use serde::{Deserialize, Serialize};

/// Minimal SPRT configuration shared by downstream statistical gates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SprtConfig {
    /// Baseline value from the prior accepted release.
    pub baseline: f64,
    /// Relative regression threshold, where `0.10` means 10%.
    pub relative_threshold: f64,
    /// Minimum sample count before a terminal decision is allowed.
    pub min_samples: usize,
    /// Confidence value carried into terminal placeholder decisions.
    pub confidence: Option<f64>,
}

impl SprtConfig {
    /// Create a config with FrankenTerm's default 10% regression floor.
    #[must_use]
    pub fn new(baseline: f64) -> Self {
        Self {
            baseline,
            relative_threshold: 0.10,
            min_samples: 2,
            confidence: Some(0.95),
        }
    }
}

/// Summary of the samples consumed by the placeholder gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SprtReport {
    /// Claim identifier being evaluated.
    pub claim_id: String,
    /// Number of samples consumed.
    pub sample_count: usize,
    /// Arithmetic mean over the consumed samples.
    pub mean: f64,
    /// Final gate decision.
    pub decision: GateDecision,
}

/// Evaluate a stream with the current scaffold SPRT policy.
pub fn evaluate_sprt<S: EvidenceStream>(
    stream: &mut S,
    config: &SprtConfig,
) -> Result<SprtReport, S::Error> {
    let samples = stream.collect_limited(config.min_samples.max(1_024))?;
    Ok(evaluate_samples(&samples, config))
}

/// Evaluate already-collected evidence samples.
#[must_use]
pub fn evaluate_samples(samples: &[EvidenceSample], config: &SprtConfig) -> SprtReport {
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |sample| sample.claim_id.clone());

    let decision = if samples.len() < config.min_samples {
        GateDecision::Continue {
            reason: "not enough evidence for sequential decision".to_string(),
            needed_samples: u64::try_from(config.min_samples.saturating_sub(samples.len())).ok(),
        }
    } else if config.baseline <= 0.0 || !config.baseline.is_finite() {
        GateDecision::LowConfidence {
            reason: "baseline must be finite and positive".to_string(),
            confidence: None,
        }
    } else {
        let mean = mean_value(samples).unwrap_or(f64::NAN);
        if !mean.is_finite() {
            GateDecision::LowConfidence {
                reason: "sample mean is not finite".to_string(),
                confidence: None,
            }
        } else if mean > config.baseline * (1.0 + config.relative_threshold) {
            GateDecision::Reject {
                reason: "mean exceeds baseline regression threshold".to_string(),
                confidence: config.confidence,
            }
        } else {
            GateDecision::Accept {
                reason: "mean remains within regression threshold".to_string(),
                confidence: config.confidence,
            }
        }
    };

    SprtReport {
        claim_id,
        sample_count: samples.len(),
        mean: mean_value(samples).unwrap_or(f64::NAN),
        decision,
    }
}

fn mean_value(samples: &[EvidenceSample]) -> Option<f64> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continues_until_minimum_sample_count() {
        let samples = [EvidenceSample::new("robot.p95", 10.0, "ms", 1, 1)];
        let report = evaluate_samples(&samples, &SprtConfig::new(10.0));
        assert!(matches!(report.decision, GateDecision::Continue { .. }));
    }

    #[test]
    fn rejects_clear_regression() {
        let samples = [
            EvidenceSample::new("robot.p95", 12.0, "ms", 1, 1),
            EvidenceSample::new("robot.p95", 13.0, "ms", 1, 2),
        ];
        let report = evaluate_samples(&samples, &SprtConfig::new(10.0));
        assert!(matches!(report.decision, GateDecision::Reject { .. }));
    }
}
