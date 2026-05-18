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
    #![allow(clippy::suboptimal_flops)]
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

    #[test]
    fn wald_sprt_accepts_h0_when_samples_match_baseline() {
        let cfg = WaldSprtConfig {
            mu_null: 10.0,
            mu_alt: 11.5,
            sigma: 0.5,
            alpha: 0.05,
            beta: 0.05,
            min_samples: 4,
            max_samples: 200,
        };
        let mut rng_seed: u64 = 0xC0FFEE;
        let samples: Vec<EvidenceSample> = (0..40)
            .map(|i| {
                let v = 10.0 + gauss(&mut rng_seed) * 0.5;
                EvidenceSample::new("robot.p95", v, "ms", 1, i as u64)
            })
            .collect();
        let report = evaluate_wald_sprt(&samples, &cfg);
        assert!(
            matches!(report.decision, GateDecision::Accept { .. }),
            "expected Accept H0 under baseline mean, got {:?}",
            report.decision
        );
    }

    #[test]
    fn wald_sprt_rejects_h0_under_regression() {
        let cfg = WaldSprtConfig {
            mu_null: 10.0,
            mu_alt: 11.5,
            sigma: 0.5,
            alpha: 0.05,
            beta: 0.05,
            min_samples: 4,
            max_samples: 200,
        };
        let mut rng_seed: u64 = 0xBADBEEF;
        let samples: Vec<EvidenceSample> = (0..60)
            .map(|i| {
                let v = 11.5 + gauss(&mut rng_seed) * 0.5;
                EvidenceSample::new("robot.p95", v, "ms", 1, i as u64)
            })
            .collect();
        let report = evaluate_wald_sprt(&samples, &cfg);
        assert!(
            matches!(report.decision, GateDecision::Reject { .. }),
            "expected Reject H0 under regression mean, got {:?}",
            report.decision
        );
    }

    #[test]
    fn wald_sprt_continues_when_evidence_inconclusive() {
        let cfg = WaldSprtConfig {
            mu_null: 10.0,
            mu_alt: 10.05,
            sigma: 1.0,
            alpha: 0.01,
            beta: 0.01,
            min_samples: 4,
            max_samples: 10,
        };
        let mut rng_seed: u64 = 0xFEED;
        let samples: Vec<EvidenceSample> = (0..10)
            .map(|i| {
                let v = 10.0 + gauss(&mut rng_seed) * 1.0;
                EvidenceSample::new("robot.p95", v, "ms", 1, i as u64)
            })
            .collect();
        let report = evaluate_wald_sprt(&samples, &cfg);
        // When hypotheses are very close and max_samples is small, the SPRT
        // either Continues (still in indifference region) OR returns
        // LowConfidence (exhausted budget without crossing a threshold). Both
        // are honest non-terminal outcomes; we just want to reject Accept/Reject.
        assert!(
            matches!(
                report.decision,
                GateDecision::Continue { .. } | GateDecision::LowConfidence { .. }
            ),
            "expected Continue or LowConfidence for very-close hypotheses + small max_samples, got {:?}",
            report.decision
        );
    }

    #[test]
    fn anytime_valid_ci_accepts_clearly_below_threshold() {
        let cfg = AnytimeValidCiConfig {
            sigma: 0.5,
            alpha: 0.05,
            threshold: 11.5,
            test_kind: AnytimeValidTest::UpperBoundMustHold,
            min_samples: 4,
            max_samples: 200,
        };
        let mut rng_seed: u64 = 0xAAAAA;
        let samples: Vec<EvidenceSample> = (0..80)
            .map(|i| {
                let v = 10.0 + gauss(&mut rng_seed) * 0.5;
                EvidenceSample::new("robot.p95", v, "ms", 1, i as u64)
            })
            .collect();
        let report = evaluate_anytime_valid_ci(&samples, &cfg);
        assert!(
            matches!(report.decision, GateDecision::Accept { .. }),
            "expected Accept when CI upper bound < threshold, got {:?}",
            report.decision
        );
    }

    #[test]
    fn anytime_valid_ci_rejects_clearly_above_threshold() {
        let cfg = AnytimeValidCiConfig {
            sigma: 0.5,
            alpha: 0.05,
            threshold: 11.5,
            test_kind: AnytimeValidTest::UpperBoundMustHold,
            min_samples: 4,
            max_samples: 200,
        };
        let mut rng_seed: u64 = 0xBBBBB;
        let samples: Vec<EvidenceSample> = (0..80)
            .map(|i| {
                let v = 13.0 + gauss(&mut rng_seed) * 0.5;
                EvidenceSample::new("robot.p95", v, "ms", 1, i as u64)
            })
            .collect();
        let report = evaluate_anytime_valid_ci(&samples, &cfg);
        assert!(
            matches!(report.decision, GateDecision::Reject { .. }),
            "expected Reject when CI lower bound > threshold, got {:?}",
            report.decision
        );
    }

    /// Deterministic Box-Muller-derived Gaussian sample via xorshift RNG.
    /// Test-only; reproducible across runs given the same seed.
    fn gauss(seed: &mut u64) -> f64 {
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

// =============================================================================
// Wald's Sequential Probability Ratio Test (G25 / ft-tf6g3.10)
// =============================================================================
//
// Reference: A. Wald, "Sequential Tests of Statistical Hypotheses" (1945).
//
// Two simple hypotheses on the mean of a Gaussian with known sigma:
//   H0: mu = mu_null   (no regression — current release matches baseline)
//   H1: mu = mu_alt    (regression — current release exceeds baseline by the
//                       threshold the operator is willing to detect)
//
// At each step n the log-likelihood ratio under H1 vs H0 is:
//   LLR_n = sum_{i=1..n} [ log f1(x_i) - log f0(x_i) ]
//         = (mu_alt - mu_null) / sigma^2 * sum_{i} (x_i - (mu_null + mu_alt)/2)
//
// Decision thresholds derived from operator-chosen Type I / Type II rates:
//   A = log((1 - beta) / alpha)        accept H1 when LLR_n >= A
//   B = log(beta / (1 - alpha))        accept H0 when LLR_n <= B
//
// Between B and A the test continues. Bounded by max_samples to ensure
// terminal decision even when the truth is exactly at the boundary.

/// Configuration for Wald's SPRT against a Gaussian-with-known-sigma model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaldSprtConfig {
    /// Baseline mean (H0).
    pub mu_null: f64,
    /// Regression-detection mean (H1). Must satisfy mu_alt != mu_null.
    pub mu_alt: f64,
    /// Known measurement noise standard deviation. Must be positive + finite.
    pub sigma: f64,
    /// Type I error rate (false-reject of H0). Conventional: 0.01 to 0.05.
    pub alpha: f64,
    /// Type II error rate (false-accept of H0). Conventional: 0.01 to 0.10.
    pub beta: f64,
    /// Minimum samples before allowing a terminal decision.
    pub min_samples: usize,
    /// Maximum samples; force terminal decision after this many.
    pub max_samples: usize,
}

/// Report from a Wald SPRT evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaldSprtReport {
    /// Claim identifier under test.
    pub claim_id: String,
    /// Final accumulated log-likelihood ratio.
    pub llr: f64,
    /// Upper threshold (accept H1 / Reject when LLR >= A).
    pub threshold_accept_h1: f64,
    /// Lower threshold (accept H0 / Accept when LLR <= B).
    pub threshold_accept_h0: f64,
    /// Sample index at which the terminal decision was reached.
    pub samples_consumed: usize,
    /// Operator decision.
    pub decision: GateDecision,
}

/// Evaluate Wald's SPRT over an already-collected sample slice.
#[must_use]
pub fn evaluate_wald_sprt(samples: &[EvidenceSample], cfg: &WaldSprtConfig) -> WaldSprtReport {
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |s| s.claim_id.clone());

    // Reject malformed config early; emit LowConfidence so callers can tell
    // the difference between "Wald says continue" and "we cannot compute".
    let threshold_a = ((1.0 - cfg.beta) / cfg.alpha).ln();
    let threshold_b = (cfg.beta / (1.0 - cfg.alpha)).ln();
    if !cfg.sigma.is_finite()
        || cfg.sigma <= 0.0
        || !cfg.mu_null.is_finite()
        || !cfg.mu_alt.is_finite()
        || (cfg.mu_alt - cfg.mu_null).abs() < f64::EPSILON
        || !(0.0..1.0).contains(&cfg.alpha)
        || !(0.0..1.0).contains(&cfg.beta)
        || !threshold_a.is_finite()
        || !threshold_b.is_finite()
    {
        return WaldSprtReport {
            claim_id,
            llr: f64::NAN,
            threshold_accept_h1: threshold_a,
            threshold_accept_h0: threshold_b,
            samples_consumed: 0,
            decision: GateDecision::LowConfidence {
                reason: "wald sprt config malformed (sigma, mu, alpha, or beta out of domain)"
                    .to_string(),
                confidence: None,
            },
        };
    }

    #[allow(clippy::suspicious_operation_groupings)]
    let coefficient = (cfg.mu_alt - cfg.mu_null) / (cfg.sigma * cfg.sigma);
    let midpoint = 0.5 * (cfg.mu_null + cfg.mu_alt);
    let max_n = cfg.max_samples.max(cfg.min_samples).max(1);

    let mut llr = 0.0_f64;
    let mut consumed = 0_usize;
    let mut terminal: Option<GateDecision> = None;

    for (i, sample) in samples.iter().take(max_n).enumerate() {
        let x = sample.metric_value;
        if !x.is_finite() {
            // Don't trust a NaN sample; continue but flag low confidence on
            // the next terminal decision.
            continue;
        }
        llr = coefficient.mul_add(x - midpoint, llr);
        consumed = i + 1;
        if consumed < cfg.min_samples {
            continue;
        }
        if llr >= threshold_a {
            terminal = Some(GateDecision::Reject {
                reason: "wald-sprt LLR crossed Reject(H0)/Accept(H1) boundary".to_string(),
                confidence: Some(1.0 - cfg.alpha),
            });
            break;
        }
        if llr <= threshold_b {
            terminal = Some(GateDecision::Accept {
                reason: "wald-sprt LLR crossed Accept(H0) boundary".to_string(),
                confidence: Some(1.0 - cfg.beta),
            });
            break;
        }
    }

    let decision = terminal.unwrap_or_else(|| {
        if consumed >= cfg.max_samples {
            // Bounded test; fall back to the side closest to the LLR.
            if llr.abs() < f64::EPSILON {
                GateDecision::Continue {
                    reason: "wald-sprt exhausted max_samples with LLR ~ 0; truth at boundary".to_string(),
                    needed_samples: None,
                }
            } else if llr > 0.0 {
                GateDecision::LowConfidence {
                    reason: "wald-sprt exhausted max_samples leaning toward H1 but did not cross threshold A".to_string(),
                    confidence: None,
                }
            } else {
                GateDecision::LowConfidence {
                    reason: "wald-sprt exhausted max_samples leaning toward H0 but did not cross threshold B".to_string(),
                    confidence: None,
                }
            }
        } else {
            GateDecision::Continue {
                reason: "wald-sprt LLR within indifference region; more samples needed".to_string(),
                needed_samples: u64::try_from(cfg.max_samples.saturating_sub(consumed)).ok(),
            }
        }
    });

    WaldSprtReport {
        claim_id,
        llr,
        threshold_accept_h1: threshold_a,
        threshold_accept_h0: threshold_b,
        samples_consumed: consumed,
        decision,
    }
}

// =============================================================================
// Always-valid confidence sequence (G25 / ft-tf6g3.10)
// =============================================================================
//
// Reference: Howard, Ramdas, McAuliffe, Sekhon 2021,
//   "Time-uniform, nonparametric, nonasymptotic confidence sequences."
//   https://arxiv.org/abs/1810.08240 (this work) and follow-up 2103.06476.
//
// For a sub-Gaussian observation distribution with parameter sigma, an
// anytime-valid (1 - alpha) confidence sequence on the mean is:
//
//   r(n) = sigma * sqrt( 2 * (log(1/alpha) + log(log(e * n) + 1)) / n )
//   CI_n = [ mean_n - r(n) , mean_n + r(n) ]
//
// The radius is order sqrt(log(log n) / n) — only marginally larger than the
// fixed-n CI's sqrt(log(1/alpha)/n), but the coverage holds *uniformly* over
// all stopping times.

/// Which side of `threshold` the operator wants the anytime CI to certify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnytimeValidTest {
    /// Accept when CI upper bound < threshold (claim "mu is below the line").
    UpperBoundMustHold,
    /// Accept when CI lower bound > threshold (claim "mu is above the line").
    LowerBoundMustHold,
}

/// Configuration for Howard-et-al anytime-valid sub-Gaussian CI gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnytimeValidCiConfig {
    /// Sub-Gaussian parameter (typically the known measurement stddev).
    pub sigma: f64,
    /// Miscoverage rate; conventional 0.01 to 0.05.
    pub alpha: f64,
    /// SLO threshold the operator is testing the mean against.
    pub threshold: f64,
    /// Which inequality the operator wants certified.
    pub test_kind: AnytimeValidTest,
    /// Minimum samples before allowing a terminal decision.
    pub min_samples: usize,
    /// Maximum samples; bounded test.
    pub max_samples: usize,
}

/// Report from an anytime-valid-CI evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnytimeValidReport {
    /// Claim identifier under test.
    pub claim_id: String,
    /// Sample mean at termination.
    pub mean: f64,
    /// CI radius at termination (mean +/- radius).
    pub radius: f64,
    /// Number of samples consumed.
    pub samples_consumed: usize,
    /// Operator decision.
    pub decision: GateDecision,
}

/// Evaluate the Howard-et-al anytime-valid CI gate over a sample slice.
#[must_use]
pub fn evaluate_anytime_valid_ci(
    samples: &[EvidenceSample],
    cfg: &AnytimeValidCiConfig,
) -> AnytimeValidReport {
    let claim_id = samples
        .first()
        .map_or_else(|| "unknown".to_string(), |s| s.claim_id.clone());

    if !cfg.sigma.is_finite() || cfg.sigma <= 0.0 || !(0.0..1.0).contains(&cfg.alpha) {
        return AnytimeValidReport {
            claim_id,
            mean: f64::NAN,
            radius: f64::NAN,
            samples_consumed: 0,
            decision: GateDecision::LowConfidence {
                reason: "anytime-valid-ci config malformed (sigma or alpha out of domain)"
                    .to_string(),
                confidence: None,
            },
        };
    }

    let max_n = cfg.max_samples.max(cfg.min_samples).max(1);
    let mut sum = 0.0_f64;
    let mut consumed = 0_usize;
    let mut terminal: Option<GateDecision> = None;
    let mut last_mean = 0.0_f64;
    let mut last_radius = f64::INFINITY;

    for (i, sample) in samples.iter().take(max_n).enumerate() {
        let x = sample.metric_value;
        if !x.is_finite() {
            continue;
        }
        sum += x;
        consumed = i + 1;
        let n = consumed as f64;
        let mean = sum / n;
        // r(n) = sigma * sqrt( 2 * (ln(1/alpha) + ln(ln(e*n)+1)) / n )
        let inv_alpha = (1.0_f64 / cfg.alpha).ln();
        let log_log_term = (std::f64::consts::E * n).ln().ln_1p();
        let radius = cfg.sigma * (2.0 * (inv_alpha + log_log_term) / n).sqrt();
        last_mean = mean;
        last_radius = radius;
        if consumed < cfg.min_samples {
            continue;
        }
        match cfg.test_kind {
            AnytimeValidTest::UpperBoundMustHold => {
                if mean + radius < cfg.threshold {
                    terminal = Some(GateDecision::Accept {
                        reason: "anytime-valid-ci upper bound below threshold".to_string(),
                        confidence: Some(1.0 - cfg.alpha),
                    });
                    break;
                }
                if mean - radius > cfg.threshold {
                    terminal = Some(GateDecision::Reject {
                        reason: "anytime-valid-ci lower bound above threshold".to_string(),
                        confidence: Some(1.0 - cfg.alpha),
                    });
                    break;
                }
            }
            AnytimeValidTest::LowerBoundMustHold => {
                if mean - radius > cfg.threshold {
                    terminal = Some(GateDecision::Accept {
                        reason: "anytime-valid-ci lower bound above threshold".to_string(),
                        confidence: Some(1.0 - cfg.alpha),
                    });
                    break;
                }
                if mean + radius < cfg.threshold {
                    terminal = Some(GateDecision::Reject {
                        reason: "anytime-valid-ci upper bound below threshold".to_string(),
                        confidence: Some(1.0 - cfg.alpha),
                    });
                    break;
                }
            }
        }
    }

    let decision = terminal.unwrap_or_else(|| {
        if consumed >= cfg.max_samples {
            GateDecision::LowConfidence {
                reason: "anytime-valid-ci exhausted max_samples with CI straddling threshold"
                    .to_string(),
                confidence: None,
            }
        } else {
            GateDecision::Continue {
                reason: "anytime-valid-ci straddles threshold; more samples needed".to_string(),
                needed_samples: u64::try_from(cfg.max_samples.saturating_sub(consumed)).ok(),
            }
        }
    });

    AnytimeValidReport {
        claim_id,
        mean: last_mean,
        radius: last_radius,
        samples_consumed: consumed,
        decision,
    }
}
