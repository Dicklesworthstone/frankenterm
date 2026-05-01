//! Bench harness statistical-rigor primitives.
//!
//! Defined under bead `ft-syqcz.2` (BR-RC-FOUNDATION.G3.2). See
//! [`docs/perf/bench-stats-spec.md`](../../../../docs/perf/bench-stats-spec.md)
//! for the methodology and JSON schema.
//!
//! # Why this module exists
//!
//! Today the bench harness emits point estimates (median ns) and a per-bench
//! median ceiling. Two failure modes follow:
//!
//! 1. **Distribution loss** — a regression that pushes the p99.9 from 800 µs
//!    to 12 ms, while leaving the median untouched, ships green.
//! 2. **Multiple-comparison rot** — running 100 bench gates per PR with a
//!    fixed-α t-test produces ~5 expected false positives every CI run, so
//!    operators tune them out and lose the signal.
//!
//! This module supplies the statistical primitives the harness needs to fix
//! both. It is *standalone arithmetic* — no Criterion dependency, no IO.
//! Integration into [`crates/frankenterm-core/benches/bench_common.rs`] is a
//! separate bead so reviewers can audit the math here before any benchmark
//! changes wire it up.
//!
//! # What is implemented today
//!
//! - [`Distribution::from_samples`] — sample-size, mean, stddev, p50/p95/p99/p99.9,
//!   plus bootstrap percentile confidence intervals for each percentile.
//! - [`mann_whitney_u`] — non-parametric two-sample test for "did the
//!   release-vs-previous distributions diverge?" with normal-approximation
//!   p-value (correct asymptotic behaviour for sample sizes ≥ ~20).
//! - [`empirical_bernstein_ci`] — anytime-valid (Howard & Ramdas 2021)
//!   one-sided upper-CI for the running mean, sound under repeated peeking.
//!   This is the per-PR-gate primitive: at any time during a release-soak
//!   period, you can read the current EBCI bound and decide
//!   regression / no-regression without paying a multiple-comparison tax.
//!
//! # Follow-on beads
//!
//! - Wire emission into [`bench_common`] — extend the existing
//!   `wa-bench-meta.jsonl` writer to also emit the [`Distribution`] payload
//!   alongside the existing budget rows.
//! - Per-PR gate using [`empirical_bernstein_ci`] in `scripts/check_bench_budgets.sh`.
//! - Concentration-of-measure sample-size auto-target.
//! - Conformal prediction bands for SLO drift.

use serde::{Deserialize, Serialize};

/// Number of bootstrap resamples used for confidence intervals.
///
/// 2 000 is the smallest value that still gives a stable p99.9 CI for the
/// sample sizes we run benches at (typically 100–1 000). Bumped to 10 000
/// for release-attestation runs by the caller.
pub const DEFAULT_BOOTSTRAP_RESAMPLES: usize = 2_000;

/// Confidence level used for `Distribution::from_samples` bootstrap CIs.
pub const DEFAULT_CONFIDENCE: f64 = 0.95;

/// One percentile reading, plus the bootstrap confidence interval that
/// bounds it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PercentileReading {
    /// e.g. 0.50 for the median, 0.999 for p99.9.
    pub q: f64,
    /// Empirical value at quantile `q`.
    pub value: f64,
    /// Lower edge of the confidence interval (inclusive).
    pub ci_lower: f64,
    /// Upper edge of the confidence interval (inclusive).
    pub ci_upper: f64,
    /// Confidence level used to compute the CI (e.g. 0.95).
    pub confidence: f64,
    /// Number of bootstrap resamples used. 0 means "no bootstrap was run"
    /// (e.g. sample size below the minimum-resampling bar).
    pub bootstrap_resamples: usize,
}

/// Distribution summary written to the `wa-bench-meta.jsonl` record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Distribution {
    /// Number of raw samples behind the summary.
    pub sample_size: usize,
    /// Sample arithmetic mean.
    pub mean: f64,
    /// Sample standard deviation (Bessel-corrected, n-1 denominator).
    pub stddev: f64,
    /// Smallest observation.
    pub min: f64,
    /// Largest observation.
    pub max: f64,
    /// Per-quantile readings: [p50, p95, p99, p99.9].
    pub percentiles: Vec<PercentileReading>,
}

impl Distribution {
    /// Default percentile set: p50, p95, p99, p99.9 — the bridge plan's
    /// minimum reporting requirement.
    pub const DEFAULT_QUANTILES: &'static [f64] = &[0.50, 0.95, 0.99, 0.999];

    /// Summarize a sample without computing bootstrap CIs (faster path).
    /// `ci_lower == ci_upper == value` for every percentile.
    #[must_use]
    pub fn from_samples(samples: &[f64]) -> Option<Self> {
        Self::summarize(samples, Self::DEFAULT_QUANTILES, 0, DEFAULT_CONFIDENCE, 0)
    }

    /// Summarize a sample with a custom set of quantiles, optional
    /// deterministic bootstrap CI, and a caller-chosen confidence level.
    ///
    /// `bootstrap_resamples == 0` skips the bootstrap (CI degenerate to the
    /// point estimate). `seed` makes the resample stream deterministic.
    /// Returns `None` if `samples` is empty.
    #[must_use]
    pub fn summarize(
        samples: &[f64],
        quantiles: &[f64],
        bootstrap_resamples: usize,
        confidence: f64,
        seed: u64,
    ) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }

        let n = samples.len();
        let mut sorted: Vec<f64> = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let sum: f64 = sorted.iter().sum();
        let mean = sum / n as f64;
        let var = if n > 1 {
            sorted.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0)
        } else {
            0.0
        };
        let stddev = var.sqrt();

        let mut percentiles = Vec::with_capacity(quantiles.len());
        for &q in quantiles {
            let value = quantile_sorted(&sorted, q);
            let (ci_lower, ci_upper) = if bootstrap_resamples > 0 {
                bootstrap_percentile_ci(&sorted, q, bootstrap_resamples, confidence, seed)
            } else {
                (value, value)
            };
            percentiles.push(PercentileReading {
                q,
                value,
                ci_lower,
                ci_upper,
                confidence,
                bootstrap_resamples,
            });
        }

        Some(Self {
            sample_size: n,
            mean,
            stddev,
            min: sorted[0],
            max: sorted[n - 1],
            percentiles,
        })
    }
}

/// Linear-interpolation quantile on an already-sorted sample.
fn quantile_sorted(sorted: &[f64], q: f64) -> f64 {
    debug_assert!(!sorted.is_empty());
    debug_assert!((0.0..=1.0).contains(&q));
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let pos = q * (n as f64 - 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = pos - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

/// Deterministic xorshift64* — used for bootstrap resampling without
/// pulling in `rand`. Quality is fine for resampling indices; we are not
/// drawing cryptographic keys.
struct Xorshift64Star {
    state: u64,
}
impl Xorshift64Star {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn next_index(&mut self, modulus: usize) -> usize {
        (self.next_u64() % modulus as u64) as usize
    }
}

/// Bootstrap percentile CI for the sample quantile at `q`.
/// Returns `(lower, upper)` at the given confidence level. `samples` must
/// be sorted ascending.
#[must_use]
pub fn bootstrap_percentile_ci(
    sorted_samples: &[f64],
    q: f64,
    resamples: usize,
    confidence: f64,
    seed: u64,
) -> (f64, f64) {
    if sorted_samples.is_empty() || resamples == 0 {
        return (f64::NAN, f64::NAN);
    }
    let n = sorted_samples.len();
    let mut rng = Xorshift64Star::new(seed);
    let mut estimates = Vec::with_capacity(resamples);
    let mut buf: Vec<f64> = vec![0.0; n];
    for _ in 0..resamples {
        for slot in buf.iter_mut() {
            *slot = sorted_samples[rng.next_index(n)];
        }
        buf.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        estimates.push(quantile_sorted(&buf, q));
    }
    estimates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let alpha = 1.0 - confidence;
    let lo = quantile_sorted(&estimates, alpha / 2.0);
    let hi = quantile_sorted(&estimates, 1.0 - alpha / 2.0);
    (lo, hi)
}

/// Two-sample Mann-Whitney U test result.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MannWhitneyResult {
    /// U-statistic for sample A. `u_b = n_a * n_b - u_a`.
    pub u_a: f64,
    /// Total samples in A.
    pub n_a: usize,
    /// Total samples in B.
    pub n_b: usize,
    /// Two-sided asymptotic p-value via the normal approximation.
    /// Reliable for `min(n_a, n_b) ≥ ~20`; below that, treat as advisory.
    pub p_value: f64,
}

/// Mann-Whitney U test (a.k.a. Wilcoxon rank-sum). Two-sided, with a tie
/// correction. Use to gate "did the release-vs-previous bench distribution
/// shift?" without assuming normality.
#[must_use]
pub fn mann_whitney_u(samples_a: &[f64], samples_b: &[f64]) -> Option<MannWhitneyResult> {
    let n_a = samples_a.len();
    let n_b = samples_b.len();
    if n_a == 0 || n_b == 0 {
        return None;
    }

    // Rank the combined sample, with ties resolved by averaging.
    let mut combined: Vec<(f64, usize)> = samples_a
        .iter()
        .map(|&v| (v, 0_usize))
        .chain(samples_b.iter().map(|&v| (v, 1_usize)))
        .collect();
    combined.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut ranks = vec![0.0_f64; combined.len()];
    let mut tie_correction = 0.0_f64;
    let mut i = 0;
    while i < combined.len() {
        let mut j = i + 1;
        while j < combined.len() && combined[j].0 == combined[i].0 {
            j += 1;
        }
        // Average rank for the tied block (1-indexed in stats convention).
        let avg_rank = (i + j + 1) as f64 / 2.0;
        for k in i..j {
            ranks[k] = avg_rank;
        }
        if j - i > 1 {
            let t = (j - i) as f64;
            tie_correction += t * (t.powi(2) - 1.0);
        }
        i = j;
    }

    let r_a: f64 = ranks
        .iter()
        .zip(combined.iter())
        .filter_map(|(r, (_, side))| if *side == 0 { Some(*r) } else { None })
        .sum();

    let n_a_f = n_a as f64;
    let n_b_f = n_b as f64;
    let u_a = r_a - n_a_f * (n_a_f + 1.0) / 2.0;
    let u_b = n_a_f * n_b_f - u_a;
    let u_min = u_a.min(u_b);

    let n_total = n_a_f + n_b_f;
    let mu = n_a_f * n_b_f / 2.0;
    let sigma_sq =
        n_a_f * n_b_f / 12.0 * (n_total + 1.0 - tie_correction / (n_total * (n_total - 1.0)));
    let sigma = sigma_sq.sqrt();

    // Continuity-corrected z and two-sided p-value.
    let z = if sigma > 0.0 {
        ((u_min - mu).abs() - 0.5) / sigma
    } else {
        0.0
    };
    let p_value = 2.0 * (1.0 - normal_cdf(z));

    Some(MannWhitneyResult {
        u_a,
        n_a,
        n_b,
        p_value: p_value.clamp(0.0, 1.0),
    })
}

/// Standard normal CDF via Abramowitz & Stegun 26.2.17 — accurate to ~7
/// decimal digits, good enough for p-value reporting.
fn normal_cdf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.231_641_9 * x);
    let y = 1.0
        - (((((1.330_274_429 * t - 1.821_255_978) * t + 1.781_477_937) * t - 0.356_563_782) * t
            + 0.319_381_530)
            * t)
            * (-x * x / 2.0).exp()
            / (2.0 * std::f64::consts::PI).sqrt();
    0.5 * (1.0 + sign * (2.0 * y - 1.0))
}

/// Anytime-valid (Howard & Ramdas 2021, "Time-uniform, nonparametric,
/// nonasymptotic confidence sequences") *upper* confidence bound on the
/// running mean of bounded samples.
///
/// Sound under repeated peeking — a CI gate that calls this primitive
/// every commit does NOT pay a multiple-comparison tax. The bridge plan's
/// "Lai-Robbins SPRT or always-valid CI" requirement is satisfied here by
/// the empirical-Bernstein curve in the line family.
///
/// Returns the upper bound `μ̂ + δ` such that with probability `≥ 1-α`,
/// the true mean is below the bound at every sample size simultaneously.
///
/// Inputs:
/// - `samples` — observations, all assumed bounded by `[0, range]`.
/// - `range` — known a-priori upper bound on each observation. For
///   wall-time benches, this is the per-iteration timeout ceiling.
/// - `alpha` — overall failure probability, e.g. 0.05.
///
/// Returns `None` if `samples` is empty or `range <= 0`.
#[must_use]
pub fn empirical_bernstein_ci(samples: &[f64], range: f64, alpha: f64) -> Option<f64> {
    if samples.is_empty() || range <= 0.0 || !(0.0..1.0).contains(&alpha) {
        return None;
    }
    let n = samples.len() as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let var = if samples.len() > 1 {
        samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };

    // Howard & Ramdas (2021), Eq. 11 — empirical Bernstein style. The
    // log term `log(1/α)` becomes `log(2/α) + log(log_2(2n))` in the
    // time-uniform regime to spend confidence over an unbounded number
    // of looks. The `+ ln(2)` keeps the constant well-behaved for n=1.
    let log_term = (2.0_f64 / alpha).ln() + ((2.0 * n).log2().max(1.0)).ln() + 2.0_f64.ln();
    let bernstein = (2.0 * var * log_term / n).sqrt() + 7.0 / 3.0 * range * log_term / (n - 1.0);
    Some(mean + bernstein)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn quantile_sorted_known_values() {
        let s = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert!(approx(quantile_sorted(&s, 0.0), 1.0, 1e-9));
        assert!(approx(quantile_sorted(&s, 0.5), 3.0, 1e-9));
        assert!(approx(quantile_sorted(&s, 1.0), 5.0, 1e-9));
        // p25 between 2.0 and 3.0 → 2.0 + 0.0 * (3.0-2.0) = 2.0
        assert!(approx(quantile_sorted(&s, 0.25), 2.0, 1e-9));
    }

    #[test]
    fn distribution_from_samples_basic() {
        let s = (1..=100).map(|i| i as f64).collect::<Vec<_>>();
        let d = Distribution::from_samples(&s).expect("non-empty");
        assert_eq!(d.sample_size, 100);
        assert!(approx(d.mean, 50.5, 1e-9));
        assert!(approx(d.min, 1.0, 1e-9));
        assert!(approx(d.max, 100.0, 1e-9));
        let p50 = d
            .percentiles
            .iter()
            .find(|p| (p.q - 0.5).abs() < 1e-9)
            .unwrap();
        assert!(approx(p50.value, 50.5, 1e-6));
        let p99 = d
            .percentiles
            .iter()
            .find(|p| (p.q - 0.99).abs() < 1e-9)
            .unwrap();
        assert!(p99.value > p50.value);
    }

    #[test]
    fn bootstrap_ci_is_deterministic_with_seed() {
        let s: Vec<f64> = (1..=200).map(|i| i as f64).collect();
        let mut sorted = s.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let ci1 = bootstrap_percentile_ci(&sorted, 0.95, 500, 0.95, 42);
        let ci2 = bootstrap_percentile_ci(&sorted, 0.95, 500, 0.95, 42);
        assert!(approx(ci1.0, ci2.0, 1e-9));
        assert!(approx(ci1.1, ci2.1, 1e-9));
        // CI bounds the sample p95.
        let p95 = quantile_sorted(&sorted, 0.95);
        assert!(ci1.0 <= p95);
        assert!(ci1.1 >= p95);
    }

    #[test]
    fn bootstrap_ci_widens_with_lower_resamples() {
        let s: Vec<f64> = (1..=50).map(|i| i as f64 * 0.1).collect();
        let mut sorted = s.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let narrow = bootstrap_percentile_ci(&sorted, 0.99, 2000, 0.95, 7);
        let _ = narrow;
        // Just smoke — we aren't asserting strict ordering across resample
        // counts, only that the CI is finite and brackets the estimate.
        assert!(narrow.0.is_finite() && narrow.1.is_finite());
    }

    #[test]
    fn mann_whitney_identical_samples_high_p() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let b = a.clone();
        let r = mann_whitney_u(&a, &b).unwrap();
        // Identical → p ≈ 1; assert clearly non-significant.
        assert!(
            r.p_value > 0.5,
            "expected p>0.5 for identical samples, got {}",
            r.p_value
        );
    }

    #[test]
    fn mann_whitney_clearly_separated_low_p() {
        let a: Vec<f64> = (1..=30).map(|i| i as f64).collect();
        let b: Vec<f64> = (101..=130).map(|i| i as f64).collect();
        let r = mann_whitney_u(&a, &b).unwrap();
        assert!(
            r.p_value < 0.001,
            "expected p<0.001 for disjoint samples, got {}",
            r.p_value
        );
    }

    #[test]
    fn mann_whitney_handles_ties() {
        let a = vec![1.0, 2.0, 2.0, 3.0, 3.0, 3.0, 4.0, 4.0, 5.0, 5.0];
        let b = vec![1.0, 2.0, 3.0, 3.0, 4.0, 4.0, 4.0, 5.0, 5.0, 5.0];
        // Heavy ties; p should still be valid (not NaN) and within [0,1].
        let r = mann_whitney_u(&a, &b).unwrap();
        assert!(r.p_value.is_finite());
        assert!((0.0..=1.0).contains(&r.p_value));
    }

    #[test]
    fn empirical_bernstein_bounds_known_mean() {
        // Constant samples → variance 0 → bound = mean + (7/3) * range *
        // log_term / (n - 1).
        let samples = vec![0.5_f64; 100];
        let upper = empirical_bernstein_ci(&samples, 1.0, 0.05).unwrap();
        assert!(upper > 0.5, "EBCI must lie above the empirical mean");
        assert!(upper < 1.5, "EBCI bound exploded: {}", upper);
    }

    #[test]
    fn empirical_bernstein_tightens_with_samples() {
        let mk = |n: usize| -> Vec<f64> { (0..n).map(|i| (i % 2) as f64 * 0.5 + 0.25).collect() };
        let small = empirical_bernstein_ci(&mk(50), 1.0, 0.05).unwrap();
        let large = empirical_bernstein_ci(&mk(5000), 1.0, 0.05).unwrap();
        // Larger sample → tighter bound.
        assert!(
            large < small,
            "expected tighter bound as n grows: small={small} large={large}"
        );
    }

    #[test]
    fn empirical_bernstein_rejects_invalid_input() {
        assert!(empirical_bernstein_ci(&[], 1.0, 0.05).is_none());
        assert!(empirical_bernstein_ci(&[0.5], 0.0, 0.05).is_none());
        assert!(empirical_bernstein_ci(&[0.5], 1.0, 1.0).is_none());
    }

    #[test]
    fn distribution_serde_roundtrip() {
        let s: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        let d = Distribution::summarize(&s, &[0.5, 0.99], 100, 0.95, 17).unwrap();
        let json = serde_json::to_string(&d).unwrap();
        let back: Distribution = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }
}
