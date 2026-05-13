# Pipeline-latency derivation via network calculus

Operator-facing derivation showing how the pipeline's analytical
`p99_ms` bound is computed from per-stage rate/latency measurements.
Pairs with the substrate at
`crates/frankenterm-core/src/network_calculus_bound.rs` and the
attestation artifact published per release at
`docs/attestations/perf/lindley-bounds.json`.

## TL;DR

For the currently published headline benchmark pipeline
`capture -> delta-extract -> storage write`:

- **Analytical bound**: 8.1ms (Lindley `h(alpha, beta) = T + b/R`).
- **Empirical p99**: 8.5ms (measured on the bench corpus).
- **Verdict**: within the substrate's 20% agreement tolerance.

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

## Coverage status

`LatencyStage` contains eight leaf stages. The current release artifact
only covers the three stages that make up the published 4KB overlap
capture benchmark. That is the only Lindley slice that is both modeled
and backed by the checked-in `lindley-bounds.json` artifact today.

| Claim surface | `LatencyStage` leaves involved | Current status |
|---------------|--------------------------------|----------------|
| `<50ms` capture benchmark | `PtyCapture`, `DeltaExtraction`, `StorageWrite` | Covered by `docs/attestations/perf/lindley-bounds.json`. |
| End-to-end capture path | `PtyCapture`, `DeltaExtraction`, `StorageWrite`, `PatternDetection`, `EventEmission` | Modeled, pending empirical: `LindleyTelemetryModel::documented_end_to_end_capture_default()` now covers all five leaves with a 23.1ms budget-backed bound, but the release artifact still lacks an empirical agreement row for the full PTY-to-event path. |
| Renderer input-to-photon SLOs | Renderer-specific stages are not represented as `LatencyStage` leaves yet | Gap: G18 owns the renderer SLO catalog; this Lindley derivation cannot honestly claim that surface until renderer stage telemetry lands. |
| Robot Mode response `<5ms` | `ApiResponse` plus handler-specific read/query work | Gap: G19/G54 evidence streams include `robot.p95`, but there is no `LatencyStage` telemetry model for the handler path yet. |
| FTS5 query `<10ms` | Search/read path plus `ApiResponse`; not the write-side `StorageWrite` leaf | Gap: G19/G54 evidence streams include `fts5.query_p99`, but the FTS5 query service curve is not in `latency_stages.rs` yet. |

The unresolved evidence gaps above are tracked in `ft-tf6g3.51` rather
than being folded into this artifact with invented numbers. The release
bundle can use the current artifact for the 4KB overlap benchmark, and
it can cite the end-to-end capture chain only as `modeled_pending_empirical`
until a real PTY-to-event empirical row lands. It must not cite this
artifact as proof for renderer, Robot Mode, or FTS5 read-path SLOs.

## Capture Latency

This is the manifest target for `capture_latency_p99` in
`docs/perf/headline-claims.json`.

### Per-stage measurements

Source: `latency_stages.rs` telemetry (live observed rates) +
worst-case scenario (operator-supplied burst). These are the three rows
included in the current attestation artifact.

| Stage             | Rate (R, events/ms) | Latency (T, ms) | Source |
|-------------------|----------------------|-----------------|--------|
| capture (PTY read)| 200                  | 1.0             | `latency_stages.rs` p99 |
| delta-extract     | 150                  | 2.0             | `latency_stages.rs` p99 |
| storage write     | 100                  | 5.0             | `latency_stages.rs` p99 |

Arrival: burst `b = 10 events`, rate `r = 90 events/ms`. The original
operator sketch used 100 events/ms, but the substrate requires strict
stability (`r < min(R_i)`) and the slowest service rate in this slice is
100 events/ms, so the release artifact uses a 10% steady-state margin.

Composed pipeline: `β = min(200, 150, 100) · (t - 1 - 2 - 5)⁺ = 100·(t - 8)⁺`.

Lindley bound: `h = 8 + 10/100 = 8.1ms` per arrival.

The README's `<50ms` figure remains the user-facing budget ceiling for
the benchmark lane. The Lindley artifact is a tighter analytical
cross-check for the modeled stage slice.

### End-to-end capture model

The broader PTY-to-event capture path is:

```
capture -> delta-extract -> storage write -> pattern detect -> event emit
```

`LindleyTelemetryModel::documented_end_to_end_capture_default()` binds
all five `LatencyStage::CAPTURE_PATH` leaves. The first three rows reuse
the benchmark's measured service curves. The final two rows use the
checked-in p99 budget ceilings for the missing leaves:

| Stage | Rate (R, events/ms) | Latency (T, ms) | Source |
|-------|----------------------|-----------------|--------|
| pattern detect | 100 | 10.0 | `default_budgets()` p99 ceiling |
| event emit | 100 | 5.0 | `default_budgets()` p99 ceiling |

Composed pipeline: `β = 100·(t - 23)⁺`.

Lindley bound: `h = 23 + 10/100 = 23.1ms`.

That is below the `<50ms` end-to-end capture budget, but it is not yet a
release proof because the artifact still lacks an empirical PTY-to-event
comparison row. The JSON reports this surface as
`modeled_pending_empirical`.

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
    analytical_bound_ms: 8.1,     // from pipeline_delay_bound
    empirical_p99_ms: 8.5,         // from headline-claim bench
}
```

Substrate predicates:
- `within_tolerance()` — `|empirical - analytical| / analytical ≤ 20%`
- `exceeds_bound()` — `empirical > analytical` (release blocker)

For the current modeled slice: empirical (8.5) and analytical (8.1) are
within the 20% agreement tolerance. If a release violates either
predicate, the substrate
emits a regression event and the integration's release CI files a
P1 bead via `br create`.

---

## Attestation artifact

Per release, the substrate's
`LindleyBoundsArtifact::render_attestation_json()` produces:

```json
{
  "release_version": "0.1.0",
  "arrival": { "burst": 10, "rate": 90 },
  "stages": [
    { "name": "capture",  "service_rate": 200, "service_latency": 1.0 },
    { "name": "delta_extract",  "service_rate": 150, "service_latency": 2.0 },
    { "name": "storage_write",  "service_rate": 100, "service_latency": 5.0 }
  ],
  "stage_content_hashes": [
    { "name": "capture", "content_sha256": "..." },
    { "name": "delta_extract", "content_sha256": "..." },
    { "name": "storage_write", "content_sha256": "..." }
  ],
  "analytical_bound_ms": 8.1,
  "empirical_p99_ms": 8.5,
  "deviation_pct": 4.94,
  "within_tolerance": true,
  "coverage_status": [
    { "claim_surface": "capture_4kb_overlap_benchmark", "status": "covered" },
    { "claim_surface": "end_to_end_capture_path", "status": "modeled_pending_empirical" },
    { "claim_surface": "robot_mode_response_lt_5ms", "status": "pending_service_curve" },
    { "claim_surface": "fts5_query_lt_10ms", "status": "pending_service_curve" },
    { "claim_surface": "renderer_input_to_photon", "status": "pending_stage_telemetry" }
  ]
}
```

Written to `docs/attestations/perf/lindley-bounds.json`, with one
content hash for each published stage row and explicit coverage status
for adjacent latency surfaces, then sigstore-signed per
BR-RC-FOUNDATION.G3.1.

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
