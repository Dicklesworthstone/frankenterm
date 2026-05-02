# Statistical methods for FrankenTerm

Statistical tools the reality-check bridge plan introduces. This is the
"why" companion to `bench_stats.rs` (the "how"). When in doubt:
consult this doc, then map to a `bench_stats::*` function.

Index from `AGENTS.md` § Testing.

---

## Sequential testing vs fixed-α gating

**Fixed-α gating** (the naïve approach): "Run N samples, compute the
test statistic, reject the null at α=0.05." Problem: if the bench
harness peeks at intermediate state (sneaks a "is the bench done?"
check at every iteration), the realised α inflates.

**Sequential testing**: "At every observation, compute a confidence
sequence (CS) that's valid AT ANY STOPPING TIME." The CS expands or
contracts as samples arrive, but it always covers the true mean with
≥ 1-α coverage regardless of how the harness stops.

**When to use sequential testing**:
- Adaptive bench harnesses that auto-target a precision threshold
  ("run until the CS half-width is below 5%").
- CI gates that want to early-stop on clear-pass / clear-fail.

**Implementation in this repo**:
- `bench_stats::empirical_bernstein_ci(samples, range, alpha)` ships
  the time-uniform Bernstein bound (Howard & Ramdas 2021). Safe to
  call at any stopping time.
- See `crates/frankenterm-core/src/bench_stats.rs:464` —
  `empirical_bernstein_ci`'s docstring + worked example.

---

## Concentration-of-measure bounds for sample sizing

**Question**: "How many samples does the bench need to *reliably*
detect a 10% regression?"

**Hoeffding's inequality**: for observations bounded in `[0, range]`:

> P(|X̄ - μ| ≥ ε) ≤ 2 · exp(-2nε² / range²)

Solving for n: `n ≥ range² · ln(2/α) / (2 · ε²)`.

**Bernstein's inequality** (sharper when variance bound is known):

> P(|X̄ - μ| ≥ ε) ≤ 2 · exp(-nε² / (2·var + (2/3)·range·ε))

**When to use which**:
- No prior on variance: Hoeffding (variance-free, conservative).
- Have a variance bound (e.g., from prior bench runs): Bernstein
  (typically 5-20× tighter when var is small).

**Implementation in this repo**:
- `bench_stats::min_sample_size_hoeffding(threshold, alpha, range)`
- `bench_stats::min_sample_size_bernstein(threshold, alpha, range,
  var_bound)`
- `bench_stats::min_sample_size_for_regression(threshold, alpha,
  range, var_bound: Option<f64>)` — composite picks tighter when
  both available; falls back to Hoeffding when `var_bound` is
  `None`.

**Worked example**: detect a 10% regression on a 100ns p99 SLO at
α=0.05, observations bounded in [0, 1ms]:
- threshold = 10ns
- range = 1_000_000ns (1ms)
- α = 0.05
- Without variance prior:
  `min_sample_size_hoeffding(10.0, 0.05, 1_000_000.0)` ≈ 1.84e10.
  Hoeffding says you'd need 18 billion samples to *guarantee*
  detection of a 10ns shift on a 1ms-bounded observation. That's
  unreasonable, so:
- With variance prior (e.g., var ≈ 100ns² from prior runs):
  `min_sample_size_bernstein(10.0, 0.05, 1_000_000.0, 100.0)` ≈
  1.6e6. Bernstein leverages the var bound → 4 orders of
  magnitude tighter.

This is why the bench harness must track per-bench variance budgets:
without them, Hoeffding-only sample sizing is impractical for
tight-SLO benches.

---

## Conformal prediction for SLO bands

**Question**: "Given the historical distribution of bench observations,
what range should the SLO band cover so that 95% of future
observations fall inside, **without** assuming a parametric
distribution?"

**Conformal prediction** (Vovk, Gammerman, Shafer 2005) gives
distribution-free prediction intervals. For a target miscoverage α:

1. Sort historical observations.
2. Quantile-pick lower and upper bounds with the (n+1) finite-sample
   correction.
3. With probability ≥ 1-α, a future observation lies within the
   resulting band — regardless of underlying distribution.

**When to use**:
- SLO bands that adapt to drift (regime shift between releases).
- Per-bench "did this run anomaly out?" alerting.

**When NOT to use**:
- Hand-tuned thresholds where you have a domain-specific reason to
  pick a fixed cutoff (e.g., 100ns for p99 lock-wait).

**Implementation in this repo**:
- `bench_stats::ConformalBand { lower, upper, realised_confidence }`.
- `bench_stats::conformal_band(samples, alpha)` — two-tailed split
  conformal with (n+1) correction. Floors at 4 samples; rejects
  non-finite inputs.
- See `crates/frankenterm-core/src/bench_stats.rs` for the
  implementation + 5 regression tests.

**Operator value**: bench harness reports per-bench distribution
"the band shifted from [50ns, 150ns] to [80ns, 250ns] across the
last 30 builds — investigate". Hand-tuned thresholds can't surface
this without manual recalibration; conformal bands track drift
automatically.

---

## Mann-Whitney U / KS — distribution comparison

**Question**: "Is distribution A statistically different from
distribution B?" (Non-parametric: no normality assumption.)

**Mann-Whitney U test**: rank-based two-sample test. Tests "are
samples from A drawn from the same distribution as samples from B?"
Robust to outliers, assumes only that the two samples are i.i.d.

**Kolmogorov-Smirnov test**: maximum CDF gap. Sensitive to
distribution-shape differences (location AND shape).

**When to use Mann-Whitney**:
- Comparing latency distributions across builds.
- A/B-style "did this change make things faster / slower?"
  questions.

**When to use KS**:
- "Is the distribution shape different?" (e.g., did a previously
  unimodal distribution become bimodal?)

**Implementation in this repo**:
- `bench_stats::mann_whitney_u(samples_a, samples_b) ->
  Option<MannWhitneyResult>` — the U statistic + two-tailed p-value.
- See `crates/frankenterm-core/src/bench_stats.rs:265`.

**Worked example**: Bench A (current) measures p99 = 95ns,
Bench B (post-change) measures p99 = 110ns. Is the difference real,
or noise?
- `mann_whitney_u(&samples_a, &samples_b)` returns p-value.
- p < 0.05 → statistically significant difference.
- Combine with Bernstein CI on the *magnitude* of the shift to
  decide if the shift is *meaningful*.

---

## Picking the right test

| Question                                          | Tool                              |
|---------------------------------------------------|-----------------------------------|
| Tighten α at any stopping time?                   | Bernstein CS                      |
| How many samples to detect a 10% regression?      | Hoeffding / Bernstein sample-size |
| Drift-adaptive SLO band?                          | Conformal band                    |
| A vs B: same distribution?                        | Mann-Whitney U                    |
| A vs B: same SHAPE?                               | KS test                           |

When in doubt: start with Mann-Whitney (most robust), pair with
Bernstein CI on the difference for magnitude. Reach for conformal
when threshold drift becomes a problem.
