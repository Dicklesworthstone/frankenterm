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

### 3. Empirical-Bernstein anytime-valid CI (`empirical_bernstein_ci`)

- Howard & Ramdas (2021), *"Time-uniform, nonparametric, nonasymptotic
  confidence sequences"* — empirical-Bernstein style upper bound on the
  running mean.
- **Sound under repeated peeking.** A CI gate that calls this primitive
  every commit does NOT pay a multiple-comparison tax. This is what
  fixes "5 false positives per CI run."
- Inputs: bounded samples in `[0, range]`, overall failure probability
  `α`. Returns `μ̂ + δ` such that `Pr(true mean ≤ μ̂ + δ at every n) ≥
  1 − α`.
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
  │    EBCI upper-bound on (release - baseline) deltas
  │    if upper_bound > baseline_p50 * (1 + threshold_relative): fail
  └─ comparison verdict written into wa-bench-meta.jsonl
```

The CI script that drives this lives separately
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
   `range = 10 × p99.9_target` is a safe default. Wall-clock benches
   without a documented timeout are not eligible for EBCI gating.

## See also

- [`docs/reality-check-bridge-plan.md`](../reality-check-bridge-plan.md) §G3
- [`docs/perf/bench-stats-schema.json`](bench-stats-schema.json) — JSON Schema
- `crates/frankenterm-core/src/bench_stats.rs` — implementation
- `crates/frankenterm-core/benches/bench_common.rs` — existing harness this work plugs into
- `scripts/check_bench_budgets.sh` — current point-estimate gate (will be superseded by the EBCI gate)
- Howard, S. R., Ramdas, A., McAuliffe, J., & Sekhon, J. (2021).
  *Time-uniform, nonparametric, nonasymptotic confidence sequences.*
  Annals of Statistics 49(2), 1055–1080.
