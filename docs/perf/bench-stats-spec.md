# Bench Harness Statistical Methodology

**Bead:** [`ft-syqcz.2`](#) (BR-RC-FOUNDATION.G3.2) ·
**Module:** `frankenterm_core::bench_stats` ·
**Schema:** [`bench-stats-schema.json`](bench-stats-schema.json)

## Why this exists

Today the bench harness emits **point estimates** — per-bench median in
nanoseconds, plus a coarse 10x ceiling enforced by
[`scripts/check_bench_budgets.sh`](../../scripts/check_bench_budgets.sh).
Two failure modes:

1. **Distribution loss.** A regression that pushes p99.9 from 800 µs to
   12 ms with the median untouched ships green. The headline-claim
   *"sub-50 ms capture latency"* is meaningless without a tail
   distribution.
2. **Multiple-comparison rot.** Running ~100 bench gates per PR with a
   fixed-α t-test gives ~5 expected false positives every CI run.
   Operators tune them out, the signal dies.

The bridge plan ([`docs/reality-check-bridge-plan.md`](../reality-check-bridge-plan.md)
G3) demands distributions + sequential-test-corrected gating. This doc
defines the methodology.

## What the harness emits

Each bench run appends one JSON-line record to
`target/criterion/wa-bench-meta.jsonl`. Schema:
[`bench-stats-schema.json`](bench-stats-schema.json). Required fields:

```json
{
  "schema_version": "1.0.0",
  "bench": "scan_pipeline/4kb_overlap",
  "generated_at_ms": 1745000000000,
  "wa_version": "0.1.0",
  "wa_commit": "34b447b",
  "environment": { "os": "macos", "arch": "aarch64", "cpu": "Apple M1 Pro", ... },
  "distribution": {
    "sample_size": 200,
    "mean": 41123.4,
    "stddev": 2880.1,
    "min": 38120.0,
    "max": 51900.0,
    "percentiles": [
      { "q": 0.5,   "value": 40880.0, "ci_lower": 40110.0, "ci_upper": 41540.0, "confidence": 0.95, "bootstrap_resamples": 2000 },
      { "q": 0.95,  "value": 47200.0, "ci_lower": 46010.0, "ci_upper": 48400.0, "confidence": 0.95, "bootstrap_resamples": 2000 },
      { "q": 0.99,  "value": 49880.0, "ci_lower": 48200.0, "ci_upper": 51140.0, "confidence": 0.95, "bootstrap_resamples": 2000 },
      { "q": 0.999, "value": 51400.0, "ci_lower": 49800.0, "ci_upper": 51900.0, "confidence": 0.95, "bootstrap_resamples": 2000 }
    ]
  },
  "comparison": {
    "baseline_commit": "1b00be7",
    "test": "ebci-upper-bound",
    "verdict": "pass",
    "ebci_upper_bound": 42100.0,
    "ebci_alpha": 0.05,
    "regression_threshold_relative": 0.10
  }
}
```

## Statistical primitives

All implemented in `crates/frankenterm-core/src/bench_stats.rs` with
unit tests proving each behaves correctly on a known case.

### 1. Distributions (`Distribution::from_samples`, `Distribution::summarize`)

- Linear-interpolation quantiles on the sorted sample.
- p50 / p95 / p99 / p99.9 reported by default — these are the bridge
  plan's minimum reporting set.
- Optional bootstrap percentile CI per quantile. Non-parametric, no
  distributional assumption. Deterministic when seeded (`xorshift64*`
  RNG; quality is fine for resampling indices, not for crypto).

### 2. Mann–Whitney U test (`mann_whitney_u`)

- Two-sample non-parametric test for "did the distributions diverge?"
- Tie correction via averaged ranks.
- Asymptotic two-sided p-value via the normal approximation, with
  continuity correction. Reliable for `min(n_a, n_b) ≥ ~20`; below
  that, treat as advisory.
- Unit tests cover identical samples (p ≈ 1), disjoint samples
  (p < 0.001), and heavy ties.

Why MWU and not Welch's t? The bench distributions are heavy-tailed
(syscall jitter, GC pauses, thermal effects) and not normal.
Distribution-free tests do not assume normality.

### 3. Empirical-Bernstein confidence sequence (`empirical_bernstein_ci`)

- Maurer & Pontil (2009), Theorem 4, supplies the fixed-sample bound.
  At prefix length `n >= 2`, allocate `delta_n = alpha / (n*(n-1))`.
  These budgets sum to `alpha`, so a union bound covers every prefix
  of one fixed i.i.d. stream. This replaces an unsupported handwritten
  log-log term; it is a conservative construction, not Howard's stitched
  boundary. See [the primary derivation](https://arxiv.org/pdf/0907.3740).
- **Repeated observations and multiple benchmarks are different.**
  For 100 streams and family error budget 0.05, an equal allocation uses
  `alpha_i=0.0005`; unequal prespecified allocations may also sum to 0.05.
  Reusing 0.05 in every stream does not solve the multiple-comparison
  problem. Changing code versions or populations does not extend one
  stationary stream; it requires a separate experimental/error budget.
- Inputs: bounded samples in `[0, range]`, overall failure probability
  `α` in `(0,1)`. Returns the upper bound, capped by the known support,
  such that `Pr(true mean <= upper at every n >= 2) >= 1-alpha` under
  those sampling assumptions. Out-of-support/non-finite values are refused.
- Unit tests cover known-mean bounding, tightening with sample size,
  and invalid-input rejection.

The bridge plan asked for *Lai-Robbins SPRT or always-valid CI*. EBCI
is the always-valid-CI side of the dichotomy and is the easier
primitive to wire into the existing bench-record pipeline. SPRT
remains a future option if we ever need a *sequential test* (decision
boundary) rather than a *running confidence interval*.

## Per-PR gate workflow

```text
PR opens
  ├─ run all bench groups → emit Distribution per bench
  ├─ for each headline-claim bench:
  │    fetch baseline distribution from main HEAD
  │    allocate this stream's alpha from the family budget
  │    bound the candidate mean; account separately for baseline uncertainty
  │    compare mean with a prespecified mean budget in the same units
  │    upper <= budget: qualify; lower > budget: regression; else inconclusive
  └─ comparison verdict written into wa-bench-meta.jsonl
```

This is a proposed DSR/RCH gate workflow. The script that would drive it lives separately
([`scripts/check_bench_stats.sh`](../../scripts/check_bench_stats.sh) —
not yet shipped, tracked as a follow-on bead).

## What is NOT yet implemented (follow-ons)

The methodology in this doc is shipped; the integration work is in
follow-on beads:

| Action | Status | Bead |
|--------|--------|------|
| 1. JSONL distribution emission per bench | data types ✅, harness wiring follow-on | TBD |
| 2. Mann-Whitney comparison per bench | math ✅, integration follow-on | TBD |
| 3. EBCI gate for per-PR regressions | math ✅, scripts/check_bench_stats.sh follow-on | TBD |
| 4. Concentration-of-measure sample-size auto-target | follow-on | TBD |
| 5. Conformal prediction interval for SLO bands | follow-on | TBD |
| 6. Methodology doc | ✅ this file | — |

## Implementation invariants

1. **No `criterion` dependency in `bench_stats`.** The math is pure
   arithmetic over `&[f64]`. This keeps the module testable without
   the heavyweight bench machinery and lets external tools (Python
   notebooks, R scripts) re-derive the numbers.
2. **Deterministic seeds.** Bootstrap CIs accept a `seed` and produce
   bit-identical output for re-runs. Release attestations need to be
   reproducible.
3. **Bounded sample assumption for EBCI.** Every bench MUST declare
   its `range` (a-priori upper bound on per-iteration time).
   A multiple of a percentile target is not a proven upper bound.
   Record every timeout/support violation and refuse the bound; dropping
   slow observations or clipping them changes the estimand. Wall-clock
   benches without a justified bound are not eligible for this gate.
4. **Match the statistic.** A mean confidence bound does not certify p99
   or p99.9. Signed candidate-minus-baseline deltas need a declared shift
   and support, and uncertainty in an estimated baseline must be included.
   Failure to certify with an upper bound alone is inconclusive, not proof
   of regression. Sparse tail samples remain sparse after bootstrapping.

## See also

- [`docs/reality-check-bridge-plan.md`](../reality-check-bridge-plan.md) §G3
- [`docs/perf/bench-stats-schema.json`](bench-stats-schema.json) — JSON Schema
- `crates/frankenterm-core/src/bench_stats.rs` — implementation
- `crates/frankenterm-core/benches/bench_common.rs` — existing harness this work plugs into
- `scripts/check_bench_budgets.sh` — current point-estimate gate (will be superseded by the EBCI gate)
- Howard, S. R., Ramdas, A., McAuliffe, J., & Sekhon, J. (2021).
  *Time-uniform, nonparametric, nonasymptotic confidence sequences.*
  Annals of Statistics 49(2), 1055–1080.
