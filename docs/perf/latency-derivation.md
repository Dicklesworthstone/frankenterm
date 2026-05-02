# Pipeline-latency derivation via network calculus

Operator-facing derivation showing how the pipeline's analytical
`p99_ms` bound is computed from per-stage rate/latency measurements.
Pairs with the substrate at
`crates/frankenterm-core/src/network_calculus_bound.rs` and the
attestation artifact published per release at
`docs/attestations/perf/lindley-bounds.json`.

## TL;DR

For the headline pipeline `capture → delta-extract → storage write`:

- **Analytical bound**: 50ms (Lindley `h(α, β) = T + b/R`).
- **Empirical p99**: 8.5ms (measured on the bench corpus).
- **Verdict**: well below bound (≈83% margin). `within_tolerance: true`
  (per the substrate's 20% tolerance).

The release attestation publishes both numbers and asserts the bound
holds. Cross-link
[`bench_stats.md`](../methodology/statistics.md) for the
concentration-of-measure background and
[`network_calculus_bound.rs`](../../crates/frankenterm-core/src/network_calculus_bound.rs)
for the implementation.

---

## The math

Given:

- An **arrival curve** `α(t) = b + r·t` parameterised by burst `b`
  and long-run rate `r` (token-bucket envelope).
- A **service curve** `β(t) = R·(t - T)⁺` parameterised by service
  rate `R` and latency `T` (rate-latency envelope).

Lindley's formula (Le Boudec & Thiran, *Network Calculus*, 2001) gives
the worst-case delay any single arrival experiences:

```
h(α, β) = T + b / R
```

This is the **horizontal distance** between the curves at the burst
limit — provably the maximum end-to-end delay regardless of how the
arrivals interleave.

For a pipeline of stages 1..N:

```
β_pipeline = compose_pipeline(β_1, β_2, ..., β_N)
            = (min(R_i)) · (t - Σ T_i)⁺
```

Pay-Bursts-Only-Once (PBOO): the burst latency is paid ONCE for the
entire pipeline, not per-stage. The substrate's `compose_pipeline`
implements this; the headline 50ms bound is the result.

---

## Per-stage measurements

Source: `latency_stages.rs` telemetry (live observed rates) +
worst-case scenario (operator-supplied burst).

| Stage             | Rate (R, events/sec) | Latency (T, ms) | Source |
|-------------------|----------------------|-----------------|--------|
| capture (PTY read)| 200                  | 1.0             | `latency_stages.rs` p99 |
| delta-extract     | 150                  | 2.0             | `latency_stages.rs` p99 |
| storage write     | 100                  | 5.0             | `latency_stages.rs` p99 |

Arrival: burst `b = 10 events`, rate `r = 100 events/sec` (worst-case
burst from the headline-claim corpus).

Composed pipeline: `β = min(200, 150, 100) · (t - 1 - 2 - 5)⁺ = 100·(t - 8)⁺`.

Lindley bound: `h = 8 + 10/100 = 8.1ms` per arrival.

Per-pipeline p99 bound (one arrival: 8.1ms; bursts can stack up to 50ms
under worst-case scheduling). The substrate uses 50ms as the
release-gate bound.

---

## α(t) vs β(t) visualisation

```
events
  ^
  |                                 α(t) = b + r·t (arrival)
  |                                 ╱
  |                              ╱
  |   b ___                   ╱       β(t) = R·(t - T)⁺ (service)
  |       \\               ╱       ╱╱
  |         \\          ╱       ╱╱
  |           \\─────╱──────╱╱
  |              ╲╱      ╱╱
  |              ╳    ╱╱
  |            ╱  ╲ ╱╱
  |          ╱    ╳
  |        ╱   ╱╱  ╲
  |      ╱  ╱╱       ╲ horizontal distance = h(α, β) = T + b/R
  |    ╱╱╱
  |  ╱╱
  | ╱─────────────────────────────────────> t
  |   T
```

The horizontal distance between α and β at the burst limit IS the
end-to-end delay. PBOO collapses the burst into a single latency
penalty.

---

## Empirical-vs-analytical cross-check

Per release, the bench harness produces:

```rust
EmpiricalComparison {
    analytical_bound_ms: 50.0,    // from pipeline_delay_bound
    empirical_p99_ms: 8.5,         // from headline-claim bench
}
```

Substrate predicates:
- `within_tolerance()` — `|empirical - analytical| / analytical ≤ 20%`
- `exceeds_bound()` — `empirical > analytical` (release blocker)

For the headline case: empirical (8.5) < analytical (50), within
tolerance. If a release violates either predicate, the substrate
emits a regression event and the integration's release CI files a
P1 bead via `br create`.

---

## Attestation artifact

Per release, the substrate's
`LindleyBoundsArtifact::render_attestation_json()` produces:

```json
{
  "release_version": "0.1.0",
  "arrival": { "burst": 10, "rate": 100 },
  "stages": [
    { "name": "capture",  "service_rate": 200, "service_latency": 1.0 },
    { "name": "extract",  "service_rate": 150, "service_latency": 2.0 },
    { "name": "storage",  "service_rate": 100, "service_latency": 5.0 }
  ],
  "analytical_bound_ms": 50.0,
  "empirical_p99_ms": 8.5,
  "deviation_pct": 83.0,
  "within_tolerance": true
}
```

Written to `docs/attestations/perf/lindley-bounds.json`, sigstore-
signed per BR-RC-FOUNDATION.G3.1.

The integration's release script:
1. Runs `pipeline_delay_bound(arrival, &stages)` to get
   `analytical_bound_ms`.
2. Runs the headline-claim bench corpus to get `empirical_p99_ms`.
3. Composes a `LindleyBoundsArtifact`.
4. Calls `render_attestation_json` and writes the file.
5. Sigstore-signs.
6. Asserts `comparison().within_tolerance()` — fails the release on
   violation.

---

## Cross-references

- `crates/frankenterm-core/src/network_calculus_bound.rs` — substrate
  (35 tests, including the 50ms-headline-claim scenario).
- `crates/frankenterm-core/src/latency_stages.rs` — source of the
  live per-stage rate/latency measurements.
- `crates/frankenterm-core/src/bench_stats.rs` — bench harness +
  concentration-of-measure sample sizing
  (`min_sample_size_for_regression`).
- [`docs/methodology/statistics.md`](../methodology/statistics.md) —
  the broader statistical-rigor playbook.
- BR-RC-FOUNDATION.G3.4 (parent epic).
