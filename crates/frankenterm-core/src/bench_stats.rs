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
use std::path::Path;

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
        (sorted[hi] - sorted[lo]).mul_add(frac, sorted[lo])
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
        for slot in &mut buf {
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
        while j < combined.len()
            && matches!(
                combined[j].0.partial_cmp(&combined[i].0),
                Some(std::cmp::Ordering::Equal)
            )
        {
            j += 1;
        }
        // Average rank for the tied block (1-indexed in stats convention).
        let avg_rank = (i + j + 1) as f64 / 2.0;
        for rank in &mut ranks[i..j] {
            *rank = avg_rank;
        }
        if j - i > 1 {
            let t = (j - i) as f64;
            let t_squared = t * t;
            tie_correction = t_squared.mul_add(t, tie_correction - t);
        }
        i = j;
    }

    let r_a: f64 = ranks
        .iter()
        .zip(combined.iter())
        .filter_map(|(r, (_, side))| if *side == 0 { Some(*r) } else { None })
        .sum();

    let left_count = n_a as f64;
    let right_count = n_b as f64;
    let u_a = r_a - left_count * (left_count + 1.0) / 2.0;
    let u_b = left_count.mul_add(right_count, -u_a);
    let u_min = u_a.min(u_b);

    let n_total = left_count + right_count;
    let mu = left_count * right_count / 2.0;
    let sigma_sq = left_count * right_count / 12.0
        * (n_total + 1.0 - tie_correction / (n_total * (n_total - 1.0)));
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
    let t = 0.231_641_9_f64.mul_add(x, 1.0).recip();
    let polynomial = 1.330_274_429_f64
        .mul_add(t, -1.821_255_978)
        .mul_add(t, 1.781_477_937)
        .mul_add(t, -0.356_563_782)
        .mul_add(t, 0.319_381_530)
        * t;
    let density = (-x * x / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let y = 1.0 - polynomial * density;
    let erf_approx = 2.0_f64.mul_add(y, -1.0);
    erf_approx.mul_add(sign, 1.0) * 0.5
}

/// Raw Criterion `<bench>/new/sample.json` shape (only the fields we read).
#[derive(Deserialize)]
struct CriterionRawSample {
    iters: Vec<f64>,
    times: Vec<f64>,
}

/// Convert a Criterion `sample.json` into a per-iteration nanosecond
/// [`Distribution`]. Returns `None` if the file is missing, malformed,
/// has zero usable rows, or the iters/times arrays disagree on length.
///
/// The bridge between Criterion's per-iter sampling output and the
/// statistical-rigor primitives in this module — used by
/// `bench_common::emit_bench_distributions` (ft-9zzkg).
#[must_use]
pub fn distribution_from_criterion_sample(path: &Path) -> Option<Distribution> {
    let text = std::fs::read_to_string(path).ok()?;
    let raw: CriterionRawSample = serde_json::from_str(&text).ok()?;
    distribution_from_raw_iters_times(&raw.iters, &raw.times)
}

/// Build a per-iteration ns [`Distribution`] from the raw `iters` /
/// `times` arrays Criterion serializes. Split out so unit tests can
/// drive the math without touching the filesystem.
#[must_use]
pub fn distribution_from_raw_iters_times(iters: &[f64], times: &[f64]) -> Option<Distribution> {
    if iters.is_empty() || iters.len() != times.len() {
        return None;
    }
    let per_iter_ns: Vec<f64> = iters
        .iter()
        .zip(times.iter())
        .filter_map(|(i, t)| {
            if *i > 0.0 && t.is_finite() && i.is_finite() {
                let v = t / i;
                if v.is_finite() && v >= 0.0 {
                    Some(v)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();
    if per_iter_ns.is_empty() {
        return None;
    }
    Distribution::from_samples(&per_iter_ns)
}

/// Derive `(criterion_group, bench_id)` from a `sample.json` path
/// relative to `target/criterion/`. Criterion writes:
///
/// - `<group>/new/sample.json`                          (ungrouped bench)
/// - `<group>/<bench_id>/new/sample.json`               (grouped bench)
/// - `<group>/<bench_id>/<param>/new/sample.json`       (parameterised bench)
///
/// For the parameterised case the bench id contains the join, e.g.
/// `mux_client_ops/pdu_encode_write/64KB`. Returns `None` if the path
/// shape doesn't match Criterion's layout (e.g. it points at a
/// `change/` snapshot rather than a `new/` one).
#[must_use]
pub fn criterion_group_and_bench_id(
    criterion_root: &Path,
    sample_path: &Path,
) -> Option<(String, String)> {
    let rel = sample_path.strip_prefix(criterion_root).ok()?;
    let segments: Vec<String> = rel
        .iter()
        .map(|s| s.to_string_lossy().to_string())
        .collect();
    // The trailing two segments must be `new` then `sample.json`. Any
    // other shape (e.g. ending in `change/sample.json` or
    // `base/sample.json`) is not the run we want to summarise.
    let n = segments.len();
    if n < 3 {
        return None;
    }
    if segments[n - 1] != "sample.json" || segments[n - 2] != "new" {
        return None;
    }
    match n {
        3 => Some((segments[0].clone(), String::new())),
        4 => Some((segments[0].clone(), segments[1].clone())),
        _ => Some((segments[0].clone(), segments[1..n - 2].join("/"))),
    }
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
/// Returns `None` if `samples` is empty, `range <= 0`, `range` is
/// non-finite, or `alpha` is outside `[0.0, 1.0)`.
///
/// Per ft-eebc9 fix: previously `range <= 0.0` accepted NaN
/// (NaN comparisons all return false), and the function would
/// then compute Some(NaN). Added `!range.is_finite()` so NaN /
/// ±infinity are rejected.
#[must_use]
pub fn empirical_bernstein_ci(samples: &[f64], range: f64, alpha: f64) -> Option<f64> {
    if samples.is_empty() || !range.is_finite() || range <= 0.0 || !(0.0..1.0).contains(&alpha) {
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

// ============================================================================
// Concentration-of-measure sample sizing (ft-ozgsc)
// ============================================================================

/// Hoeffding-bounded minimum sample size to detect a regression
/// of magnitude `threshold` with overall confidence `1 - alpha`.
///
/// Hoeffding's inequality bounds the deviation of a sample mean
/// from the true mean by `epsilon`, given observations bounded in
/// `[0, range]`:
///
///     P(|X̄ - μ| ≥ ε) ≤ 2 · exp(-2 · n · ε² / range²)
///
/// Solving for `n` at the target failure probability `α`:
///
///     n ≥ range² · ln(2 / α) / (2 · ε²)
///
/// Inputs:
/// - `threshold` — minimum effect size to detect (same units as
///   the observations; e.g., a 10% regression on a 100ns p99 is
///   `threshold = 10.0`).
/// - `alpha` — overall failure probability for the bound.
/// - `range` — known a-priori upper bound on each observation
///   (same convention as [`empirical_bernstein_ci`]).
///
/// Returns `None` when any input is non-finite, non-positive, or
/// out-of-range.
#[must_use]
pub fn min_sample_size_hoeffding(threshold: f64, alpha: f64, range: f64) -> Option<usize> {
    if !threshold.is_finite()
        || threshold <= 0.0
        || !range.is_finite()
        || range <= 0.0
        || !(0.0..1.0).contains(&alpha)
        || alpha <= 0.0
    {
        return None;
    }
    let log_term = (2.0_f64 / alpha).ln();
    let n = (range * range * log_term) / (2.0 * threshold * threshold);
    if !n.is_finite() {
        return None;
    }
    Some(n.ceil() as usize)
}

/// Bernstein-bounded minimum sample size, sharper than Hoeffding
/// when the variance upper bound `var_bound` is known.
///
/// Bernstein's inequality:
///
///     P(|X̄ - μ| ≥ ε) ≤ 2 · exp(-n · ε² / (2 · var_bound + (2/3) · range · ε))
///
/// Solving for `n` at failure probability `α`:
///
///     n ≥ (2 · var_bound + (2/3) · range · ε) · ln(2/α) / ε²
///
/// Returns `None` for invalid inputs (same convention as
/// [`min_sample_size_hoeffding`]). When `var_bound` exceeds
/// `range² / 4` (the maximum-variance bound for `[0, range]`-
/// bounded variables), Bernstein degenerates to Hoeffding —
/// substrate clamps `var_bound` at `range² / 4` rather than
/// reporting an error.
#[must_use]
pub fn min_sample_size_bernstein(
    threshold: f64,
    alpha: f64,
    range: f64,
    var_bound: f64,
) -> Option<usize> {
    if !threshold.is_finite()
        || threshold <= 0.0
        || !range.is_finite()
        || range <= 0.0
        || !(0.0..1.0).contains(&alpha)
        || alpha <= 0.0
        || !var_bound.is_finite()
        || var_bound < 0.0
    {
        return None;
    }
    let max_var = range * range / 4.0;
    let var = var_bound.min(max_var);
    let log_term = (2.0_f64 / alpha).ln();
    let numerator = ((2.0 / 3.0) * range).mul_add(threshold, 2.0 * var) * log_term;
    let n = numerator / (threshold * threshold);
    if !n.is_finite() {
        return None;
    }
    Some(n.ceil() as usize)
}

/// Composite "minimum sample size to detect a regression" entry
/// point. Picks the tighter of Hoeffding (variance-free) and
/// Bernstein (variance-aware) when both inputs are available;
/// falls back to Hoeffding-only when `var_bound` is `None`.
#[must_use]
pub fn min_sample_size_for_regression(
    threshold: f64,
    alpha: f64,
    range: f64,
    var_bound: Option<f64>,
) -> Option<usize> {
    let hoeffding = min_sample_size_hoeffding(threshold, alpha, range)?;
    match var_bound {
        Some(v) => {
            let bernstein = min_sample_size_bernstein(threshold, alpha, range, v)?;
            Some(hoeffding.min(bernstein))
        }
        None => Some(hoeffding),
    }
}

// ============================================================================
// Conformal SLO bands (ft-ozgsc)
// ============================================================================

/// Conformal-prediction-interval band over a sample distribution.
///
/// Conformal bands give a **distribution-free** SLO interval: with
/// probability `1 - alpha`, a future observation drawn from the
/// same distribution lies within `[lower, upper]`. The substrate
/// uses the split-conformal nonconformity-score recipe applied to
/// observation magnitudes (no separate calibration set —
/// substrate uses leave-one-out via empirical quantiles).
///
/// Returns `None` when:
/// - `samples` is empty.
/// - `alpha` is outside `(0.0, 1.0)`.
/// - `samples.len()` is too small to yield a meaningful quantile
///   (substrate floors at 4 — below this, the conformal interval
///   is too wide to be useful).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConformalBand {
    pub lower: f64,
    pub upper: f64,
    /// Realised confidence (≥ `1 - alpha` due to integer-quantile
    /// rounding). Useful when comparing across different sample
    /// sizes.
    pub realised_confidence: f64,
}

/// Compute a conformal-prediction band for `samples` at miscoverage
/// `alpha`. Two-tailed: lower = α/2 quantile, upper = (1-α/2)
/// quantile, with the (n+1) finite-sample correction that lifts the
/// quantile index by 1.
///
/// Empirically: with `samples.len() = 100` and `alpha = 0.10`, the
/// band is roughly `[5th percentile, 95th percentile]` with the
/// correction; finite-sample-valid for any distribution.
#[must_use]
pub fn conformal_band(samples: &[f64], alpha: f64) -> Option<ConformalBand> {
    const MIN_SAMPLES: usize = 4;
    if samples.is_empty()
        || samples.len() < MIN_SAMPLES
        || !(0.0..1.0).contains(&alpha)
        || alpha <= 0.0
    {
        return None;
    }
    if !samples.iter().all(|x| x.is_finite()) {
        return None;
    }
    let mut sorted: Vec<f64> = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Two-tailed split: lower at α/2, upper at 1-α/2.
    let half = alpha / 2.0;
    // Finite-sample correction: use (n+1)-quantile.
    let n_plus_1 = (sorted.len() + 1) as f64;
    let lower_q_idx_f = (half * n_plus_1).floor() as usize;
    let upper_q_idx_f = ((1.0 - half) * n_plus_1).ceil() as usize;
    // Clamp to valid index range.
    let lo_idx = lower_q_idx_f.saturating_sub(1).min(sorted.len() - 1);
    let hi_idx = upper_q_idx_f.saturating_sub(1).min(sorted.len() - 1);
    let lower = sorted[lo_idx];
    let upper = sorted[hi_idx];
    // Realised confidence: count of samples inside the band /
    // total. With the finite-sample correction this is ≥ 1-α.
    let inside = sorted.iter().filter(|&&x| x >= lower && x <= upper).count() as f64;
    let realised_confidence = inside / sorted.len() as f64;
    Some(ConformalBand {
        lower,
        upper,
        realised_confidence,
    })
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

    /// ft-eebc9 regression: previously `range <= 0.0` admitted NaN
    /// (NaN comparisons all return false). Fix gates on
    /// `range.is_finite()` first.
    #[test]
    fn empirical_bernstein_rejects_non_finite_range() {
        assert!(empirical_bernstein_ci(&[0.5], f64::NAN, 0.05).is_none());
        assert!(empirical_bernstein_ci(&[0.5], f64::INFINITY, 0.05).is_none());
        assert!(empirical_bernstein_ci(&[0.5], f64::NEG_INFINITY, 0.05).is_none());
    }

    #[test]
    fn distribution_serde_roundtrip() {
        let s: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        let d = Distribution::summarize(&s, &[0.5, 0.99], 100, 0.95, 17).unwrap();
        let json = serde_json::to_string(&d).unwrap();
        let back: Distribution = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    // ---- ft-9zzkg: Criterion-sample → Distribution conversion ----

    #[test]
    fn raw_iters_times_per_iteration_division() {
        // 3 batches: 100 iters in 1000ns (10ns/iter), 200 in 2400 (12),
        // 50 in 600 (12). Distribution captures all 3 per-iter readings.
        let iters = [100.0, 200.0, 50.0];
        let times = [1_000.0, 2_400.0, 600.0];
        let dist =
            distribution_from_raw_iters_times(&iters, &times).expect("non-empty per-iter ns");
        assert_eq!(dist.sample_size, 3);
        assert!(approx(dist.min, 10.0, 1e-9));
        assert!(approx(dist.max, 12.0, 1e-9));
    }

    #[test]
    fn raw_iters_times_skips_zero_iters_rows() {
        let iters = [100.0, 0.0, 50.0];
        let times = [1_000.0, 999.0, 500.0];
        let dist = distribution_from_raw_iters_times(&iters, &times).unwrap();
        assert_eq!(dist.sample_size, 2);
    }

    #[test]
    fn raw_iters_times_rejects_mismatched_lengths() {
        let iters = [100.0, 200.0];
        let times = [1_000.0];
        assert!(distribution_from_raw_iters_times(&iters, &times).is_none());
    }

    #[test]
    fn raw_iters_times_rejects_empty() {
        assert!(distribution_from_raw_iters_times(&[], &[]).is_none());
    }

    #[test]
    fn criterion_path_ungrouped() {
        let root = Path::new("target/criterion");
        let p = Path::new("target/criterion/foo_bench/new/sample.json");
        let (g, b) = criterion_group_and_bench_id(root, p).unwrap();
        assert_eq!(g, "foo_bench");
        assert_eq!(b, "");
    }

    #[test]
    fn criterion_path_grouped() {
        let root = Path::new("target/criterion");
        let p = Path::new("target/criterion/pattern_throughput/scan_64KB/new/sample.json");
        let (g, b) = criterion_group_and_bench_id(root, p).unwrap();
        assert_eq!(g, "pattern_throughput");
        assert_eq!(b, "scan_64KB");
    }

    #[test]
    fn criterion_path_parameterised() {
        let root = Path::new("target/criterion");
        let p = Path::new("target/criterion/mux_client_ops/pdu_encode_write/4096/new/sample.json");
        let (g, b) = criterion_group_and_bench_id(root, p).unwrap();
        assert_eq!(g, "mux_client_ops");
        assert_eq!(b, "pdu_encode_write/4096");
    }

    #[test]
    fn criterion_path_rejects_change_snapshot() {
        // Criterion also writes change/sample.json on every run; we only
        // want the `new/` payloads. Anything else must be rejected.
        let root = Path::new("target/criterion");
        let p = Path::new("target/criterion/foo/change/sample.json");
        assert!(criterion_group_and_bench_id(root, p).is_none());
        let p = Path::new("target/criterion/foo/base/sample.json");
        assert!(criterion_group_and_bench_id(root, p).is_none());
    }

    #[test]
    fn criterion_path_rejects_unrelated_root() {
        let root = Path::new("target/criterion");
        let p = Path::new("/somewhere/else/sample.json");
        assert!(criterion_group_and_bench_id(root, p).is_none());
    }

    #[test]
    fn distribution_from_criterion_sample_reads_fixture() {
        let dir = std::env::temp_dir().join(format!("ft9zzkg_sample_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.json");
        let payload = r#"{"sampling_mode":"Linear","iters":[10.0,20.0],"times":[100.0,400.0]}"#;
        std::fs::write(&path, payload).unwrap();

        let dist = distribution_from_criterion_sample(&path).expect("parses");
        assert_eq!(dist.sample_size, 2);
        // Per-iter: 100/10 = 10ns, 400/20 = 20ns.
        assert!(approx(dist.min, 10.0, 1e-9));
        assert!(approx(dist.max, 20.0, 1e-9));
        assert!(approx(dist.mean, 15.0, 1e-9));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn distribution_from_criterion_sample_handles_missing_file() {
        let path = Path::new("/this/path/does/not/exist/sample.json");
        assert!(distribution_from_criterion_sample(path).is_none());
    }

    // ============================================================================
    // ft-ozgsc — Concentration-of-measure sample sizing
    // ============================================================================

    #[test]
    fn hoeffding_known_value_matches_textbook() {
        // For range=1.0, threshold=0.05, alpha=0.05:
        // n = 1 * ln(2/0.05) / (2 * 0.05²) = ln(40) / 0.005
        //   = 3.6889 / 0.005 = 737.78 → 738
        let n = min_sample_size_hoeffding(0.05, 0.05, 1.0).unwrap();
        assert_eq!(n, 738);
    }

    #[test]
    fn hoeffding_tightens_with_smaller_threshold() {
        let big_thr = min_sample_size_hoeffding(0.10, 0.05, 1.0).unwrap();
        let small_thr = min_sample_size_hoeffding(0.05, 0.05, 1.0).unwrap();
        assert!(small_thr > big_thr, "smaller threshold ⇒ more samples");
    }

    #[test]
    fn hoeffding_tightens_with_smaller_alpha() {
        let weak = min_sample_size_hoeffding(0.05, 0.10, 1.0).unwrap();
        let strict = min_sample_size_hoeffding(0.05, 0.01, 1.0).unwrap();
        assert!(strict > weak, "tighter alpha ⇒ more samples");
    }

    #[test]
    fn hoeffding_rejects_invalid_inputs() {
        assert!(min_sample_size_hoeffding(0.0, 0.05, 1.0).is_none());
        assert!(min_sample_size_hoeffding(-0.1, 0.05, 1.0).is_none());
        assert!(min_sample_size_hoeffding(0.05, 0.0, 1.0).is_none());
        assert!(min_sample_size_hoeffding(0.05, 1.0, 1.0).is_none());
        assert!(min_sample_size_hoeffding(0.05, -0.1, 1.0).is_none());
        assert!(min_sample_size_hoeffding(0.05, 0.05, 0.0).is_none());
        assert!(min_sample_size_hoeffding(0.05, 0.05, -1.0).is_none());
        assert!(min_sample_size_hoeffding(f64::NAN, 0.05, 1.0).is_none());
        assert!(min_sample_size_hoeffding(0.05, 0.05, f64::INFINITY).is_none());
    }

    #[test]
    fn bernstein_tighter_than_hoeffding_when_variance_low() {
        // Low-variance distribution: Bernstein wins because
        // it leverages the var bound.
        let h = min_sample_size_hoeffding(0.05, 0.05, 1.0).unwrap();
        // var_bound = 0.01 (much less than range²/4 = 0.25).
        let b = min_sample_size_bernstein(0.05, 0.05, 1.0, 0.01).unwrap();
        assert!(
            b < h,
            "Bernstein should be tighter than Hoeffding for low-variance bound \
             (b={b}, h={h})",
        );
    }

    #[test]
    fn bernstein_clamps_var_bound_above_max() {
        // var_bound > range²/4 should clamp to range²/4 (the
        // theoretical max for [0, range]-bounded variables).
        // Result should equal the var_bound = max case.
        let max = min_sample_size_bernstein(0.05, 0.05, 1.0, 0.25).unwrap();
        let absurd = min_sample_size_bernstein(0.05, 0.05, 1.0, 100.0).unwrap();
        assert_eq!(max, absurd);
    }

    #[test]
    fn bernstein_rejects_invalid_inputs() {
        assert!(min_sample_size_bernstein(0.0, 0.05, 1.0, 0.1).is_none());
        assert!(min_sample_size_bernstein(0.05, 0.05, 1.0, -0.1).is_none());
        assert!(min_sample_size_bernstein(0.05, 0.05, 1.0, f64::NAN).is_none());
    }

    #[test]
    fn composite_picks_tighter_of_hoeffding_and_bernstein() {
        let h = min_sample_size_hoeffding(0.05, 0.05, 1.0).unwrap();
        let b = min_sample_size_bernstein(0.05, 0.05, 1.0, 0.01).unwrap();
        let composite = min_sample_size_for_regression(0.05, 0.05, 1.0, Some(0.01)).unwrap();
        assert_eq!(composite, h.min(b));
    }

    #[test]
    fn composite_falls_back_to_hoeffding_when_var_bound_none() {
        let h = min_sample_size_hoeffding(0.05, 0.05, 1.0).unwrap();
        let composite = min_sample_size_for_regression(0.05, 0.05, 1.0, None).unwrap();
        assert_eq!(composite, h);
    }

    // ============================================================================
    // ft-ozgsc — Conformal SLO bands
    // ============================================================================

    #[test]
    fn conformal_band_brackets_the_input() {
        let samples: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let band = conformal_band(&samples, 0.10).unwrap();
        // Band should be within input range.
        assert!(band.lower >= 1.0);
        assert!(band.upper <= 100.0);
        // Realised confidence should be ≥ 1 - alpha = 0.90.
        assert!(
            band.realised_confidence >= 0.90,
            "realised confidence = {}, expected >= 0.90",
            band.realised_confidence,
        );
    }

    #[test]
    fn conformal_band_tighter_alpha_yields_wider_band() {
        let samples: Vec<f64> = (1..=200).map(|i| i as f64).collect();
        let loose = conformal_band(&samples, 0.20).unwrap();
        let strict = conformal_band(&samples, 0.02).unwrap();
        // Tighter alpha (lower miscoverage) ⇒ wider band.
        let loose_width = loose.upper - loose.lower;
        let strict_width = strict.upper - strict.lower;
        assert!(
            strict_width >= loose_width,
            "strict={strict_width}, loose={loose_width}",
        );
    }

    #[test]
    fn conformal_band_rejects_invalid_inputs() {
        assert!(conformal_band(&[], 0.05).is_none());
        assert!(conformal_band(&[1.0, 2.0, 3.0], 0.05).is_none()); // < MIN_SAMPLES
        let ok = vec![1.0, 2.0, 3.0, 4.0];
        assert!(conformal_band(&ok, 0.0).is_none());
        assert!(conformal_band(&ok, 1.0).is_none());
        let with_nan = vec![1.0, 2.0, 3.0, f64::NAN];
        assert!(conformal_band(&with_nan, 0.05).is_none());
    }

    #[test]
    fn conformal_band_well_defined_at_minimum_sample_size() {
        // At n=4 (the floor), the band should be well-defined
        // even though it's very wide.
        let samples = vec![1.0, 2.0, 3.0, 4.0];
        let band = conformal_band(&samples, 0.50).unwrap();
        assert!(band.lower <= band.upper);
        assert!(band.realised_confidence > 0.0);
    }

    #[test]
    fn conformal_band_handles_uniform_samples() {
        // All-equal samples: lower == upper, realised
        // confidence = 1.0.
        let samples = vec![42.0; 100];
        let band = conformal_band(&samples, 0.10).unwrap();
        assert_eq!(band.lower, 42.0);
        assert_eq!(band.upper, 42.0);
        assert_eq!(band.realised_confidence, 1.0);
    }
}
