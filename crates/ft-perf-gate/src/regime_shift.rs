//! Regime-shift detector scaffolding for performance evidence streams.
//!
//! ```
//! use ft_perf_gate::regime_shift::{self, RegimeShiftConfig};
//! use ft_perf_gate::{EvidenceSample, GateDecision};
//!
//! let samples = [
//!     EvidenceSample::new("robot.p95", 10.0, "ms", 1, 1),
//!     EvidenceSample::new("robot.p95", 10.0, "ms", 1, 2),
//!     EvidenceSample::new("robot.p95", 20.0, "ms", 1, 3),
//!     EvidenceSample::new("robot.p95", 20.0, "ms", 1, 4),
//! ];
//! let report = regime_shift::detect_from_samples(&samples, &RegimeShiftConfig::default());
//! assert!(matches!(report.decision, GateDecision::RegimeShift { .. }));
//! ```

use crate::{EvidenceSample, EvidenceStream, GateDecision};
use serde::{Deserialize, Serialize};

/// Configuration for the placeholder divergence detector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeShiftConfig {
    /// Minimum samples in each side of the split window.
    pub min_window: usize,
    /// Relative mean divergence that triggers a regime-shift decision.
    pub divergence_threshold: f64,
}

impl Default for RegimeShiftConfig {
    fn default() -> Self {
        Self {
            min_window: 2,
            divergence_threshold: 0.25,
        }
    }
}

/// Summary emitted by the regime-shift scaffold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeShiftReport {
    /// Claim identifier being evaluated.
    pub claim_id: String,
    /// Relative divergence between the two windows.
    pub divergence: f64,
    /// Final gate decision.
    pub decision: GateDecision,
}

/// Detect a regime shift from a stream.
pub fn detect_regime_shift<S: EvidenceStream>(
    stream: &mut S,
    config: &RegimeShiftConfig,
) -> Result<RegimeShiftReport, S::Error> {
    let samples = stream.collect_limited(config.min_window.saturating_mul(2).max(1_024))?;
    Ok(detect_from_samples(&samples, config))
}

/// Detect a regime shift from collected evidence.
#[must_use]
pub fn detect_from_samples(
    samples: &[EvidenceSample],
    config: &RegimeShiftConfig,
) -> RegimeShiftReport {
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |sample| sample.claim_id.clone());
    if samples.len() < config.min_window.saturating_mul(2) {
        return RegimeShiftReport {
            claim_id,
            divergence: 0.0,
            decision: GateDecision::Continue {
                reason: "not enough samples for split-window comparison".to_string(),
                needed_samples: u64::try_from(
                    config
                        .min_window
                        .saturating_mul(2)
                        .saturating_sub(samples.len()),
                )
                .ok(),
            },
        };
    }

    let split = samples.len() / 2;
    let Some(left_mean) = mean_value(&samples[..split]) else {
        return low_confidence_report(claim_id, "left window mean is not finite");
    };
    let Some(right_mean) = mean_value(&samples[split..]) else {
        return low_confidence_report(claim_id, "right window mean is not finite");
    };
    let denominator = left_mean.abs().max(f64::EPSILON);
    let divergence = (right_mean - left_mean).abs() / denominator;
    let decision = if divergence >= config.divergence_threshold {
        GateDecision::RegimeShift {
            reason: "split-window mean divergence exceeded threshold".to_string(),
            divergence,
        }
    } else {
        GateDecision::Accept {
            reason: "no split-window regime shift detected".to_string(),
            confidence: None,
        }
    };

    RegimeShiftReport {
        claim_id,
        divergence,
        decision,
    }
}

fn low_confidence_report(claim_id: String, reason: &str) -> RegimeShiftReport {
    RegimeShiftReport {
        claim_id,
        divergence: f64::NAN,
        decision: GateDecision::LowConfidence {
            reason: reason.to_string(),
            confidence: None,
        },
    }
}

fn mean_value(samples: &[EvidenceSample]) -> Option<f64> {
    let len = u32::try_from(samples.len()).ok()?;
    if len == 0 {
        return None;
    }
    let mean = samples
        .iter()
        .map(|sample| sample.metric_value)
        .sum::<f64>()
        / f64::from(len);
    mean.is_finite().then_some(mean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_large_window_shift() {
        let samples = [
            EvidenceSample::new("robot.p95", 10.0, "ms", 1, 1),
            EvidenceSample::new("robot.p95", 10.0, "ms", 1, 2),
            EvidenceSample::new("robot.p95", 20.0, "ms", 1, 3),
            EvidenceSample::new("robot.p95", 20.0, "ms", 1, 4),
        ];
        let report = detect_from_samples(&samples, &RegimeShiftConfig::default());
        assert!(matches!(report.decision, GateDecision::RegimeShift { .. }));
    }
}
