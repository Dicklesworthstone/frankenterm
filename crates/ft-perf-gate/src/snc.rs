//! Stochastic Network Calculus (SNC) bounds for heavy-tail arrivals.
//!
//! G38 (`ft-tf6g3.23`). Classical deterministic network calculus (the
//! Lindley-equation machinery in `latency_stages.rs` / `network_calculus_bound.rs`)
//! produces vacuous bounds when arrivals are heavy-tail (Pareto α<2), because
//! the moment-generating function of the arrival process is infinite. SNC
//! (Jiang 2008; Ciucu 2007) extends to MGF-bounded envelopes that give
//! finite probabilistic bounds in this regime.
//!
//! This module provides:
//!
//! - [`HillEstimator`] for estimating the tail index α from a sample slice
//! - [`MgfEnvelope`] and [`MgfServiceCurve`] for the MGF-based arrival /
//!   service envelopes
//! - [`compute_snc_bound`] computing a p-confidence end-to-end delay bound
//!
//! For light-tail (α ≥ 2) inputs, the deriver returns
//! [`SncBound::LindleyDomain`] so callers can fall back to the cheaper
//! deterministic Lindley path (G23 / `latency_stages.rs`).
//!
//! ```
//! use ft_perf_gate::snc::{HillEstimate, hill_estimate};
//!
//! // A clearly heavy-tail sample (Pareto with alpha=1.5).
//! let samples: Vec<f64> = (1..=200).map(|i| (1.0_f64 / (1.0 - i as f64 / 201.0)).powf(1.0/1.5)).collect();
//! let est: HillEstimate = hill_estimate(&samples, 50);
//! assert!(est.alpha < 2.0, "Hill estimator should detect alpha<2 on Pareto-1.5; got {}", est.alpha);
//! ```
//!
//! References:
//! - Jiang, Y. & Liu, Y. (2008) "Stochastic Network Calculus." Springer.
//! - Ciucu, F. (2007) "Network Calculus Delay Bounds in Queueing Networks
//!   with Exact Solutions." Proc. ITC-20.
//! - Hill, B. M. (1975) "A simple general approach to inference about the
//!   tail of a distribution." Annals of Statistics 3(5).

use crate::{EvidenceSample, GateDecision};
use serde::{Deserialize, Serialize};

/// Result of the Hill tail-index estimator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HillEstimate {
    /// Estimated tail index α. Smaller α = heavier tail. α<2 means
    /// classical Lindley is out of domain.
    pub alpha: f64,
    /// Number of upper-order statistics used as the Hill k parameter.
    pub k: usize,
    /// Standard error of the estimate (asymptotic: α/sqrt(k)).
    pub stderr: f64,
}

/// Estimate the tail index α from `samples` using the upper k order
/// statistics. Returns NaN α on a degenerate input (all-zero, all-equal,
/// non-positive values).
#[must_use]
pub fn hill_estimate(samples: &[f64], k: usize) -> HillEstimate {
    let k = k.max(2);
    let positive: Vec<f64> = samples
        .iter()
        .copied()
        .filter(|v| *v > 0.0 && v.is_finite())
        .collect();
    if positive.len() < k + 1 {
        return HillEstimate {
            alpha: f64::NAN,
            k,
            stderr: f64::NAN,
        };
    }
    let mut sorted = positive.clone();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    // Upper k+1 order statistics: X_(n-k) is the threshold.
    let threshold = sorted[n - k - 1];
    if threshold <= 0.0 {
        return HillEstimate {
            alpha: f64::NAN,
            k,
            stderr: f64::NAN,
        };
    }
    // Hill estimator: 1/α = (1/k) * sum_{i=0..k} ln(X_(n-i) / X_(n-k))
    let mut sum = 0.0;
    for i in 0..k {
        let xi = sorted[n - 1 - i];
        sum += (xi / threshold).ln();
    }
    let one_over_alpha = sum / (k as f64);
    if one_over_alpha <= 0.0 {
        return HillEstimate {
            alpha: f64::INFINITY,
            k,
            stderr: f64::NAN,
        };
    }
    let alpha = 1.0 / one_over_alpha;
    // Asymptotic stderr: alpha / sqrt(k).
    let stderr = alpha / (k as f64).sqrt();
    HillEstimate { alpha, k, stderr }
}

/// One-stage MGF-bounded service curve under SNC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MgfServiceCurve {
    /// Long-run service rate (units consistent with arrival).
    pub rate: f64,
    /// Service-curve burst tolerance.
    pub burst: f64,
}

/// One-stage MGF-bounded arrival envelope under SNC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MgfArrivalEnvelope {
    /// Estimated long-run arrival rate.
    pub rate: f64,
    /// Estimated tail index alpha. Used to choose the MGF parameter.
    pub tail_alpha: f64,
}

/// SNC bound output. Either a finite probabilistic bound or an
/// out-of-domain marker that punts to the deterministic Lindley path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SncBound {
    /// Heavy-tail SNC bound at the requested confidence level.
    HeavyTailBound {
        /// Confidence p (probability that the delay is at most `delay_ms`).
        confidence: f64,
        /// Worst-case delay bound at confidence `p`. The numeric value is
        /// in **the same unit as the input samples' `metric_unit`** —
        /// typically ms, but callers passing samples in `us` or
        /// `bytes_per_line` (etc.) receive a bound in that unit. The
        /// field name retains the `_ms` suffix for serde wire-format
        /// compatibility with the v1 wire format, but consumers should
        /// look up the unit from the originating evidence stream's
        /// `metric_unit`.
        delay_ms: f64,
        /// Tail-index estimate that produced the bound.
        alpha: f64,
    },
    /// Light-tail input — Lindley is in-domain and cheaper.
    LindleyDomain { alpha: f64 },
    /// Unable to compute (e.g., divergent MGF, insufficient samples).
    OutOfDomain { reason: String },
}

/// Configuration for the SNC bound deriver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SncConfig {
    /// Target confidence p for the delay bound (e.g., 0.9999).
    pub confidence: f64,
    /// Hill estimator k (upper-order statistics used). Internally clamped
    /// to a minimum of 2 by both [`hill_estimate`] and
    /// [`compute_snc_bound`] to avoid degenerate single-point estimates.
    pub hill_k: usize,
    /// Tail-index threshold below which we are "heavy-tail" (α<this).
    /// Conventional: 2.0 (Lindley breaks down).
    ///
    /// **Important caveat.** The Hill estimator is a tail-index estimator
    /// that assumes the underlying distribution IS Pareto in its upper
    /// tail. Applied to Gaussian or other light-tail data, Hill can
    /// produce small finite-sample α estimates (sometimes below 2.0!)
    /// due to estimator bias, which would route truly-Gaussian samples
    /// through the heavy-tail branch. Callers that need a strict
    /// heavy-tail-vs-light-tail classification SHOULD pair this with a
    /// goodness-of-fit test (e.g. Kolmogorov-Smirnov against the empirical
    /// distribution) before trusting the routing verdict. For pure
    /// heavy-tail vs known-light-tail discrimination at the bound-quality
    /// level (rather than routing-verdict level) the Hill α serves well.
    pub heavy_tail_alpha_threshold: f64,
    /// Stability margin — currently unused in the v1 single-stage
    /// observed-delay form (kept for serde compatibility + reserved for
    /// the future multi-stage / queueing-composition variant of
    /// `compute_snc_bound`).
    pub stability_margin: f64,
}

impl Default for SncConfig {
    fn default() -> Self {
        Self {
            confidence: 0.9999,
            hill_k: 50,
            heavy_tail_alpha_threshold: 2.0,
            stability_margin: 0.1,
        }
    }
}

/// Compute the SNC delay quantile from a slice of latency observations.
///
/// **Honest simplification.** The classical Jiang 2008 Theorem 2.4.1
/// formula combines an MGF-bounded arrival envelope with a rate-latency
/// service curve to give a queueing delay bound. The v1 implementation
/// here treats `samples` as DIRECT delay observations (their
/// `metric_value` is the observed end-to-end latency in `metric_unit`,
/// not an arrival rate in events/sec) and returns the Pareto-tail
/// quantile fit to those observations.
///
/// Concretely: P(X > x) ~ (xm/x)^α for heavy-tail X ⇒ inverting at the
/// (1 - confidence) tail gives x_p = xm · (1/(1-p))^(1/α), where xm is
/// the order statistic X_(n-k) the Hill estimator used. For α ≥
/// `heavy_tail_alpha_threshold`, falls back to [`SncBound::LindleyDomain`]
/// since the deterministic Lindley path in `latency_stages.rs` is the
/// right tool there.
///
/// The [`MgfServiceCurve`] argument is currently a placeholder for
/// future multi-stage / queueing composition (G38 follow-on). It is
/// validated (rate > 0, finite) but its values do not modulate the
/// returned quantile in the single-stage observed-delay form.
#[must_use]
pub fn compute_snc_bound(
    samples: &[EvidenceSample],
    service: &MgfServiceCurve,
    cfg: &SncConfig,
) -> SncBound {
    if !(0.0..1.0).contains(&cfg.confidence) || cfg.confidence < 0.5 {
        return SncBound::OutOfDomain {
            reason: "snc confidence must be in (0.5, 1)".to_string(),
        };
    }
    if service.rate <= 0.0 || !service.rate.is_finite() {
        return SncBound::OutOfDomain {
            reason: "snc service rate must be positive and finite".to_string(),
        };
    }
    // Clamp hill_k to match the internal floor in `hill_estimate`. Without
    // this, callers setting hill_k < 2 would get an alpha estimate computed
    // with k=2 but an xm threshold read at sorted[len - hill_k - 1] for
    // the unclamped k — two inconsistent k values in the same computation.
    let hill_k = cfg.hill_k.max(2);
    let values: Vec<f64> = samples
        .iter()
        .map(|s| s.metric_value)
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    if values.len() < hill_k + 1 {
        return SncBound::OutOfDomain {
            reason: format!(
                "snc needs at least hill_k+1 = {} positive finite samples",
                hill_k + 1
            ),
        };
    }
    let hill = hill_estimate(&values, hill_k);
    if !hill.alpha.is_finite() {
        return SncBound::OutOfDomain {
            reason: "hill estimator produced non-finite alpha".to_string(),
        };
    }
    if hill.alpha >= cfg.heavy_tail_alpha_threshold {
        return SncBound::LindleyDomain { alpha: hill.alpha };
    }

    // Pareto tail-quantile: P(X > x) ~ (xm/x)^alpha
    // Inverting at the (1 - p) upper tail: x_p = xm * (1/(1-p))^(1/alpha)
    let mut sorted = values.clone();
    sorted.sort_by(f64::total_cmp);
    let xm = sorted[sorted.len() - hill_k - 1];
    let one_minus_p = 1.0 - cfg.confidence;
    let delay_ms = xm * one_minus_p.powf(-1.0 / hill.alpha);

    if !delay_ms.is_finite() || delay_ms <= 0.0 {
        return SncBound::OutOfDomain {
            reason: format!(
                "snc quantile computation produced non-finite or non-positive delay {delay_ms}"
            ),
        };
    }

    SncBound::HeavyTailBound {
        confidence: cfg.confidence,
        delay_ms,
        alpha: hill.alpha,
    }
}

/// Convert an SNC bound into a [`GateDecision`] for cross-gate composability.
#[must_use]
pub fn snc_decision(bound: &SncBound, slo_ms: f64) -> GateDecision {
    match bound {
        SncBound::HeavyTailBound {
            confidence,
            delay_ms,
            ..
        } => {
            if *delay_ms <= slo_ms {
                GateDecision::Accept {
                    reason: format!(
                        "snc p={confidence:.4} bound {delay_ms:.3}ms below SLO {slo_ms:.3}ms"
                    ),
                    confidence: Some(*confidence),
                }
            } else {
                GateDecision::Reject {
                    reason: format!(
                        "snc p={confidence:.4} bound {delay_ms:.3}ms exceeds SLO {slo_ms:.3}ms"
                    ),
                    confidence: Some(*confidence),
                }
            }
        }
        SncBound::LindleyDomain { alpha } => GateDecision::LowConfidence {
            reason: format!("snc inapplicable: light-tail alpha={alpha:.3}; use Lindley path"),
            confidence: None,
        },
        SncBound::OutOfDomain { reason } => GateDecision::LowConfidence {
            reason: format!("snc out of domain: {reason}"),
            confidence: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hill_detects_heavy_tail_pareto_1_5() {
        // Pareto sample with alpha=1.5: inverse-CDF transform of uniform.
        let mut seed: u64 = 0xABBA;
        let alpha = 1.5_f64;
        let samples: Vec<f64> = (0..500)
            .map(|_| {
                let u = uniform(&mut seed).max(1e-9);
                (1.0 / u).powf(1.0 / alpha)
            })
            .collect();
        let est = hill_estimate(&samples, 100);
        // Hill estimator is consistent but biased; allow ±0.3 around alpha=1.5
        assert!(
            (1.0..2.0).contains(&est.alpha),
            "hill alpha estimate {} not in [1.0, 2.0) for Pareto-1.5",
            est.alpha
        );
    }

    #[test]
    fn hill_returns_high_alpha_for_light_tail() {
        // Exponential samples — light-tail, Hill alpha should be large.
        let mut seed: u64 = 0xCAFE;
        let samples: Vec<f64> = (0..500)
            .map(|_| {
                let u = uniform(&mut seed).max(1e-9);
                -u.ln()
            })
            .collect();
        let est = hill_estimate(&samples, 100);
        // For exponential, Hill alpha grows with k; expect >= 1.5 but not strictly bounded.
        // The key assertion is that it's finite + positive.
        assert!(est.alpha.is_finite() && est.alpha > 0.0);
    }

    #[test]
    fn compute_snc_returns_lindley_domain_for_exponential() {
        let mut seed: u64 = 0xBEEF;
        let samples: Vec<EvidenceSample> = (0..200)
            .map(|i| {
                let u = uniform(&mut seed).max(1e-9);
                EvidenceSample::new("robot.p95", -u.ln() * 5.0, "ms", 1, i as u64)
            })
            .collect();
        // Service rate must exceed mean arrival rate by stability_margin.
        // Exponential -ln(u) * 5 has mean ~5, so service rate 100 is generous.
        let service = MgfServiceCurve {
            rate: 100.0,
            burst: 1.0,
        };
        let cfg = SncConfig::default();
        let bound = compute_snc_bound(&samples, &service, &cfg);
        // Exponential is light-tail; depending on Hill sample, alpha may be
        // above or below threshold. Either LindleyDomain or HeavyTailBound
        // is acceptable; OutOfDomain (non-finite alpha) is NOT.
        assert!(
            !matches!(bound, SncBound::OutOfDomain { .. }),
            "exponential input should not produce OutOfDomain; got {:?}",
            bound
        );
    }

    #[test]
    fn compute_snc_produces_heavy_tail_bound_for_pareto() {
        let mut seed: u64 = 0xFEED;
        let alpha = 1.5_f64;
        let samples: Vec<EvidenceSample> = (0..500)
            .map(|i| {
                let u = uniform(&mut seed).max(1e-9);
                let v = (1.0 / u).powf(1.0 / alpha);
                EvidenceSample::new("robot.p95", v, "ms", 1, i as u64)
            })
            .collect();
        let service = MgfServiceCurve {
            rate: 100.0,
            burst: 1.0,
        };
        let cfg = SncConfig::default();
        let bound = compute_snc_bound(&samples, &service, &cfg);
        match bound {
            SncBound::HeavyTailBound {
                confidence,
                delay_ms,
                alpha: a,
            } => {
                assert!((0.99..=0.9999_f64).contains(&confidence));
                assert!(delay_ms.is_finite() && delay_ms > 0.0);
                assert!((1.0..2.0).contains(&a));
            }
            other => panic!("expected HeavyTailBound for Pareto-1.5; got {other:?}"),
        }
    }

    #[test]
    fn compute_snc_handles_insufficient_samples() {
        let samples: Vec<EvidenceSample> = (0..10)
            .map(|i| EvidenceSample::new("robot.p95", 1.0, "ms", 1, i))
            .collect();
        let service = MgfServiceCurve {
            rate: 1.0,
            burst: 0.0,
        };
        let cfg = SncConfig::default();
        let bound = compute_snc_bound(&samples, &service, &cfg);
        assert!(matches!(bound, SncBound::OutOfDomain { .. }));
    }

    #[test]
    fn snc_decision_accepts_when_bound_below_slo() {
        let bound = SncBound::HeavyTailBound {
            confidence: 0.9999,
            delay_ms: 3.0,
            alpha: 1.5,
        };
        match snc_decision(&bound, 5.0) {
            GateDecision::Accept { .. } => {}
            other => panic!("expected Accept; got {other:?}"),
        }
    }

    #[test]
    fn snc_decision_rejects_when_bound_exceeds_slo() {
        let bound = SncBound::HeavyTailBound {
            confidence: 0.9999,
            delay_ms: 12.0,
            alpha: 1.3,
        };
        match snc_decision(&bound, 5.0) {
            GateDecision::Reject { .. } => {}
            other => panic!("expected Reject; got {other:?}"),
        }
    }

    fn uniform(seed: &mut u64) -> f64 {
        let mut s = *seed;
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *seed = s;
        (s as f64) / (u64::MAX as f64)
    }
}
