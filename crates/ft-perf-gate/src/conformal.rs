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
    let Some(lower) = values.first().copied() else {
        return Err(GateDecision::LowConfidence {
            reason: "calibration samples must not be empty".to_string(),
            confidence: None,
        });
    };
    let Some(upper) = values.last().copied() else {
        return Err(GateDecision::LowConfidence {
            reason: "calibration samples must not be empty".to_string(),
            confidence: None,
        });
    };

    Ok(ConformalBand {
        claim_id,
        lower,
        upper,
        calibration_samples: values.len(),
        alpha: config.alpha,
    })
}

// =============================================================================
// Split-conformal prediction band (G26 / ft-tf6g3.11)
// =============================================================================
//
// Reference: Angelopoulos & Bates 2022, "A Gentle Introduction to Conformal
// Prediction and Distribution-Free Uncertainty Quantification"
// (https://arxiv.org/abs/2107.07511). The split-conformal procedure used here:
//
//   1. Split N samples into calibration (~half) and test windows. The
//      Angelopoulos-Bates tutorial uses ANY consistent point-estimator as
//      the "predictor"; we use the calibration set's mean since the
//      claim is a single scalar metric.
//   2. Score each calibration point with its nonconformity score
//      s_i = |x_i - mean_calibration|. Other choices are valid (absolute
//      residual, sign-adjusted residual); absolute deviation is the
//      simplest sound choice for a symmetric two-sided band.
//   3. The band's half-radius is the ceiling((1-alpha)(n+1)/n)-th order
//      statistic of the calibration scores. This is the finite-sample
//      conformal quantile.
//   4. The published band is [mean_cal - radius, mean_cal + radius] and
//      its marginal coverage is at least (1 - alpha) for any
//      exchangeable test point drawn from the same distribution.

/// Configuration for split-conformal prediction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplitConformalConfig {
    /// Target miscoverage rate. Conventional: 0.05 -> 95% coverage.
    pub alpha: f64,
    /// Fraction of samples reserved for calibration. Conventional: 0.5.
    pub calibration_fraction: f64,
    /// Minimum calibration samples required to publish a band.
    pub min_calibration_samples: usize,
}

impl Default for SplitConformalConfig {
    fn default() -> Self {
        Self {
            alpha: 0.05,
            calibration_fraction: 0.5,
            // For alpha=0.05 the finite-sample quantile requires
            // n_cal >= ceil((1 - alpha)/alpha) = 19 to admit a valid
            // (1 - alpha)(n+1)-th order statistic. Default to 20 to clear
            // the boundary with one sample of headroom; callers using
            // smaller alpha can override.
            min_calibration_samples: 20,
        }
    }
}

/// Fit a finite-sample split-conformal band from collected evidence.
///
/// Returns `Ok(band)` when calibration succeeds with the configured
/// minimum sample count; returns `Err(GateDecision)` when the
/// pre-calibration check fails. The band's `lower` / `upper` fields
/// then represent the conformal prediction interval and `alpha`
/// records the target miscoverage rate.
pub fn fit_split_conformal_band(
    samples: &[EvidenceSample],
    cfg: &SplitConformalConfig,
) -> Result<ConformalBand, GateDecision> {
    // Note: `(0.0..1.0)` is half-open (start-inclusive, end-exclusive) in
    // Rust, so it accepts 0.0. We want alpha and calibration_fraction
    // STRICTLY in (0, 1) — an alpha of 0 is degenerate (asks for 100%
    // coverage which forces an infinite band) and would also cause
    // division-by-zero in the under-coverage diagnostic below. Add the
    // explicit > 0 guards.
    if !(0.0..1.0).contains(&cfg.alpha)
        || cfg.alpha <= 0.0
        || !(0.0..1.0).contains(&cfg.calibration_fraction)
        || cfg.calibration_fraction <= 0.0
    {
        return Err(GateDecision::LowConfidence {
            reason: "split-conformal config malformed (alpha or calibration_fraction out of (0,1))"
                .to_string(),
            confidence: None,
        });
    }
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |s| s.claim_id.clone());

    let values: Vec<f64> = samples.iter().map(|s| s.metric_value).collect();
    if values.iter().any(|v| !v.is_finite()) {
        return Err(GateDecision::LowConfidence {
            reason: "calibration samples must be finite".to_string(),
            confidence: None,
        });
    }

    // Calibration / test split — deterministic so reruns produce the same
    // band. We take the first calibration_fraction of the input in arrival
    // order; callers wanting random splits should shuffle the input
    // upstream (the G54 fixture corpus is already deterministic).
    let n_cal = ((values.len() as f64) * cfg.calibration_fraction).floor() as usize;
    if n_cal < cfg.min_calibration_samples {
        return Err(GateDecision::Continue {
            reason: "not enough calibration samples for split-conformal".to_string(),
            needed_samples: u64::try_from(cfg.min_calibration_samples.saturating_sub(n_cal)).ok(),
        });
    }
    let cal: &[f64] = &values[..n_cal];

    let mean: f64 = cal.iter().sum::<f64>() / (n_cal as f64);
    let mut scores: Vec<f64> = cal.iter().map(|v| (v - mean).abs()).collect();
    scores.sort_by(f64::total_cmp);

    // Conformal quantile: the ceil((1 - alpha) * (n + 1))-th order statistic
    // (1-based) is the standard finite-sample quantile that guarantees
    // marginal coverage >= (1 - alpha). If that index exceeds n, the
    // finite-sample procedure cannot publish a guaranteed (1 - alpha) band
    // — the bound would be +infinity. Return Continue requesting enough
    // additional samples to make the index valid (rather than silently
    // returning a max-score-based band that technically under-covers).
    let n = n_cal;
    let q_level = ((1.0 - cfg.alpha) * ((n + 1) as f64)).ceil() as usize;
    if q_level > n {
        // Need n_cal >= ceil((1-alpha)(n+1))  =>  n_cal >= ceil((1-alpha)/alpha)
        // in the worst case. Compute the smallest n_cal that admits a valid
        // quantile and request the difference.
        let required = ((1.0 - cfg.alpha) / cfg.alpha).ceil() as usize;
        let needed_calibration_extra = required.saturating_sub(n_cal);
        // Each extra calibration sample requires roughly 1/calibration_fraction
        // total extra samples; round up to be safe.
        let needed_total =
            ((needed_calibration_extra as f64) / cfg.calibration_fraction).ceil() as usize;
        return Err(GateDecision::Continue {
            reason: format!(
                "split-conformal: n_cal={n_cal} too small for alpha={}; need n_cal >= {required} for a valid (1-alpha) quantile",
                cfg.alpha
            ),
            needed_samples: u64::try_from(needed_total.max(1)).ok(),
        });
    }
    let q_idx = q_level - 1; // 1-based -> 0-based
    let radius = scores[q_idx];

    Ok(ConformalBand {
        claim_id,
        lower: mean - radius,
        upper: mean + radius,
        calibration_samples: n_cal,
        alpha: cfg.alpha,
    })
}

/// Audit the marginal coverage of a fitted band over a held-out test set.
///
/// Returns the empirical coverage rate (fraction of test samples whose
/// `metric_value` lies inside `[band.lower, band.upper]`). The split-conformal
/// theory guarantees `coverage >= 1 - alpha` in expectation over the random
/// calibration / test split, with finite-sample deviation bounded by the
/// number of calibration samples.
#[must_use]
pub fn audit_coverage(band: &ConformalBand, test: &[EvidenceSample]) -> f64 {
    // Filter non-finite samples out of BOTH the numerator and denominator.
    // The previous form kept non-finite samples in `test.len()` while
    // excluding them from `inside`, biasing the coverage rate downward
    // whenever the test set contained any NaN / Inf observations.
    let finite: Vec<&EvidenceSample> = test.iter().filter(|s| s.metric_value.is_finite()).collect();
    if finite.is_empty() {
        return 0.0;
    }
    let inside = finite
        .iter()
        .filter(|s| (band.lower..=band.upper).contains(&s.metric_value))
        .count();
    (inside as f64) / (finite.len() as f64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::suboptimal_flops)]
    use super::*;

    #[test]
    fn fits_conservative_band() -> Result<(), String> {
        let samples = [
            EvidenceSample::new("robot.p95", 3.0, "ms", 1, 1),
            EvidenceSample::new("robot.p95", 4.0, "ms", 1, 2),
            EvidenceSample::new("robot.p95", 5.0, "ms", 1, 3),
        ];
        let band = match fit_band_from_samples(&samples, &ConformalConfig::default()) {
            Ok(band) => band,
            Err(decision) => return Err(format!("unexpected decision: {decision:?}")),
        };
        assert!((band.lower - 3.0).abs() < f64::EPSILON);
        assert!((band.upper - 5.0).abs() < f64::EPSILON);
        assert!(matches!(band.decide(4.5), GateDecision::Accept { .. }));
        assert!(matches!(band.decide(8.0), GateDecision::Reject { .. }));
        Ok(())
    }

    #[test]
    fn split_conformal_fits_a_band_with_reasonable_radius() -> Result<(), String> {
        // 64 synthesized samples around mean 10 with stddev 0.5. With the
        // default calibration_fraction=0.5, this gives 32 calibration
        // samples — well above the alpha=0.05 floor of 19.
        let mut seed: u64 = 0xABCDE;
        let samples: Vec<EvidenceSample> = (0..64)
            .map(|i| {
                let v = 10.0 + gauss_test(&mut seed) * 0.5;
                EvidenceSample::new("robot.p95", v, "ms", 1, i)
            })
            .collect();
        let band = fit_split_conformal_band(&samples, &SplitConformalConfig::default())
            .map_err(|d| format!("unexpected decision: {d:?}"))?;
        assert_eq!(band.calibration_samples, 32);
        assert!(band.upper > band.lower, "upper > lower");
        // With sigma=0.5 and ~32 calibration samples the radius should be small.
        assert!(
            band.upper - band.lower < 4.0,
            "band too wide: {} to {}",
            band.lower,
            band.upper
        );
        Ok(())
    }

    #[test]
    fn split_conformal_returns_continue_when_n_too_small_for_alpha() {
        // For alpha=0.05 the quantile requires n_cal >= 19; 16 samples
        // / fraction=0.5 = 8 calibration samples → not enough; the
        // detector must return Continue rather than silently fall back
        // to a max-score band that under-covers.
        let mut seed: u64 = 0xBEEF1;
        let samples: Vec<EvidenceSample> = (0..16)
            .map(|i| {
                let v = 10.0 + gauss_test(&mut seed) * 0.5;
                EvidenceSample::new("robot.p95", v, "ms", 1, i)
            })
            .collect();
        let cfg = SplitConformalConfig {
            alpha: 0.05,
            calibration_fraction: 0.5,
            min_calibration_samples: 8, // intentionally below 19
        };
        match fit_split_conformal_band(&samples, &cfg) {
            Err(GateDecision::Continue { needed_samples, .. }) => {
                assert!(needed_samples.is_some(), "should report needed_samples");
            }
            other => panic!("expected Continue for n_cal=8 with alpha=0.05; got {other:?}"),
        }
    }

    #[test]
    fn split_conformal_continues_below_min_calibration() {
        let samples: Vec<EvidenceSample> = (0..4)
            .map(|i| EvidenceSample::new("robot.p95", 10.0 + (i as f64) * 0.1, "ms", 1, i))
            .collect();
        match fit_split_conformal_band(&samples, &SplitConformalConfig::default()) {
            Err(GateDecision::Continue { .. }) => {}
            other => panic!("expected Continue for 4-sample input; got {other:?}"),
        }
    }

    #[test]
    fn split_conformal_audit_coverage_satisfies_target() {
        // Calibrate on the first half, audit on the second half. Marginal
        // coverage should be at or above (1 - alpha) modulo finite-sample noise.
        let mut seed: u64 = 0x42;
        let samples: Vec<EvidenceSample> = (0..200)
            .map(|i| {
                let v = 10.0 + gauss_test(&mut seed) * 0.5;
                EvidenceSample::new("robot.p95", v, "ms", 1, i)
            })
            .collect();
        let band = fit_split_conformal_band(&samples, &SplitConformalConfig::default())
            .expect("band fits cleanly");
        let test_set: &[EvidenceSample] = &samples[100..];
        let coverage = audit_coverage(&band, test_set);
        // With alpha=0.05 the theoretical coverage is >= 0.95; allow 0.85 as a
        // generous finite-sample lower bound for n=100 calibration samples.
        assert!(coverage >= 0.85, "coverage {coverage} below 0.85 floor");
    }

    #[test]
    fn split_conformal_rejects_malformed_alpha() {
        let samples: Vec<EvidenceSample> = (0..40)
            .map(|i| EvidenceSample::new("robot.p95", 10.0, "ms", 1, i))
            .collect();
        let cfg = SplitConformalConfig {
            alpha: 1.5,
            ..Default::default()
        };
        match fit_split_conformal_band(&samples, &cfg) {
            Err(GateDecision::LowConfidence { .. }) => {}
            other => panic!("expected LowConfidence for alpha=1.5; got {other:?}"),
        }
    }

    #[test]
    fn split_conformal_rejects_zero_alpha() {
        // alpha=0 is degenerate (asks for 100% coverage which forces an
        // infinite band) and would cause division-by-zero in the
        // under-coverage diagnostic. The half-open Range<f64>::contains
        // accepts 0.0; the explicit > 0 guard is what rejects it.
        let samples: Vec<EvidenceSample> = (0..40)
            .map(|i| EvidenceSample::new("robot.p95", 10.0, "ms", 1, i))
            .collect();
        let cfg = SplitConformalConfig {
            alpha: 0.0,
            ..Default::default()
        };
        match fit_split_conformal_band(&samples, &cfg) {
            Err(GateDecision::LowConfidence { .. }) => {}
            other => panic!("expected LowConfidence for alpha=0.0; got {other:?}"),
        }
    }

    #[test]
    fn audit_coverage_excludes_nan_from_denominator() {
        // 8 finite samples (4 inside band, 4 outside) + 2 NaN samples.
        // Coverage should be 4/8 = 0.5, NOT 4/10 = 0.4 (which is what the
        // old buggy code returned because NaN samples were excluded from
        // the numerator but kept in the denominator).
        let band = ConformalBand {
            claim_id: "test".to_string(),
            lower: 0.0,
            upper: 10.0,
            calibration_samples: 8,
            alpha: 0.05,
        };
        let mut samples: Vec<EvidenceSample> = (0..4)
            .map(|i| EvidenceSample::new("test", 5.0, "ms", 1, i))
            .collect(); // 4 inside
        samples.extend((4..8).map(|i| EvidenceSample::new("test", 100.0, "ms", 1, i))); // 4 outside
        samples.push(EvidenceSample::new("test", f64::NAN, "ms", 1, 100));
        samples.push(EvidenceSample::new("test", f64::INFINITY, "ms", 1, 101));
        let coverage = audit_coverage(&band, &samples);
        assert!(
            (coverage - 0.5).abs() < 1e-9,
            "expected 0.5 (4 inside / 8 finite); got {coverage}"
        );
    }

    fn gauss_test(seed: &mut u64) -> f64 {
        let u1 = xorshift_uniform(seed);
        let u2 = xorshift_uniform(seed);
        let u1 = u1.max(1e-12);
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    fn xorshift_uniform(seed: &mut u64) -> f64 {
        let mut s = *seed;
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *seed = s;
        (s as f64) / (u64::MAX as f64)
    }
}
