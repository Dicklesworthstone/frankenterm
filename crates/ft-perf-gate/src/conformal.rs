//! Split-conformal SLO band scaffolding.
//!
//! ```
//! use ft_perf_gate::conformal::{self, ConformalConfig};
//! use ft_perf_gate::{EvidenceSample, GateDecision};
//!
//! let samples = [
//!     EvidenceSample::new("robot.p95", 3.0, "ms", 1, 1),
//!     EvidenceSample::new("robot.p95", 4.0, "ms", 1, 2),
//!     EvidenceSample::new("robot.p95", 5.0, "ms", 1, 3),
//! ];
//! let band = conformal::fit_band_from_samples(&samples, &ConformalConfig::default()).unwrap();
//! assert!(matches!(band.decide(4.5), GateDecision::Accept { .. }));
//! ```

use crate::{EvidenceSample, EvidenceStream, GateDecision};
use serde::{Deserialize, Serialize};

/// Configuration for conservative conformal band fitting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformalConfig {
    /// Target miscoverage rate.
    pub alpha: f64,
    /// Minimum calibration samples required to publish a band.
    pub min_calibration_samples: usize,
}

impl Default for ConformalConfig {
    fn default() -> Self {
        Self {
            alpha: 0.05,
            min_calibration_samples: 3,
        }
    }
}

/// A conservative calibration band over one metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformalBand {
    /// Claim identifier being bounded.
    pub claim_id: String,
    /// Inclusive lower bound.
    pub lower: f64,
    /// Inclusive upper bound.
    pub upper: f64,
    /// Number of calibration samples used.
    pub calibration_samples: usize,
    /// Target miscoverage rate.
    pub alpha: f64,
}

impl ConformalBand {
    /// Decide whether a value lies inside the published band.
    #[must_use]
    pub fn decide(&self, value: f64) -> GateDecision {
        if !value.is_finite() {
            return GateDecision::LowConfidence {
                reason: "candidate value is not finite".to_string(),
                confidence: None,
            };
        }
        if (self.lower..=self.upper).contains(&value) {
            GateDecision::Accept {
                reason: "value lies inside conformal band".to_string(),
                confidence: Some(1.0 - self.alpha),
            }
        } else {
            GateDecision::Reject {
                reason: "value lies outside conformal band".to_string(),
                confidence: Some(1.0 - self.alpha),
            }
        }
    }
}

/// Fit a conservative band from a stream.
pub fn fit_band<S: EvidenceStream>(
    stream: &mut S,
    config: &ConformalConfig,
) -> Result<Result<ConformalBand, GateDecision>, S::Error> {
    let samples = stream.collect_limited(config.min_calibration_samples.max(1_024))?;
    Ok(fit_band_from_samples(&samples, config))
}

/// Fit a conservative band from collected evidence.
pub fn fit_band_from_samples(
    samples: &[EvidenceSample],
    config: &ConformalConfig,
) -> Result<ConformalBand, GateDecision> {
    if samples.len() < config.min_calibration_samples {
        return Err(GateDecision::Continue {
            reason: "not enough calibration samples".to_string(),
            needed_samples: u64::try_from(
                config.min_calibration_samples.saturating_sub(samples.len()),
            )
            .ok(),
        });
    }

    let mut values: Vec<f64> = samples.iter().map(|sample| sample.metric_value).collect();
    if values.iter().any(|value| !value.is_finite()) {
        return Err(GateDecision::LowConfidence {
            reason: "calibration samples must be finite".to_string(),
            confidence: None,
        });
    }
    values.sort_by(f64::total_cmp);
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |sample| sample.claim_id.clone());

    Ok(ConformalBand {
        claim_id,
        lower: values[0],
        upper: values[values.len() - 1],
        calibration_samples: values.len(),
        alpha: config.alpha,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_conservative_band() {
        let samples = [
            EvidenceSample::new("robot.p95", 3.0, "ms", 1, 1),
            EvidenceSample::new("robot.p95", 4.0, "ms", 1, 2),
            EvidenceSample::new("robot.p95", 5.0, "ms", 1, 3),
        ];
        let band = fit_band_from_samples(&samples, &ConformalConfig::default()).unwrap();
        assert!((band.lower - 3.0).abs() < f64::EPSILON);
        assert!((band.upper - 5.0).abs() < f64::EPSILON);
        assert!(matches!(band.decide(4.5), GateDecision::Accept { .. }));
        assert!(matches!(band.decide(8.0), GateDecision::Reject { .. }));
    }
}
