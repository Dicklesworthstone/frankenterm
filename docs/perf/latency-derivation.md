# Pipeline-latency derivation via network calculus

Operator-facing derivation showing how the pipeline's model delay
bound is computed from supplied per-stage rate/latency parameters.
Pairs with the substrate at
`crates/frankenterm-core/src/network_calculus_bound.rs` and the
attestation artifact published per release at
`docs/attestations/perf/lindley-bounds.json`.

## TL;DR

For the historical model of the headline benchmark pipeline
`capture -> delta-extract -> storage write`:

- **Analytical bound**: 8.067ms (Lindley `h(alpha, beta) = T + b/R`).
- **Historical empirical reference**: 8.5ms. This is not a measurement of
  the current source or release candidate.
- **Verdict**: the empirical value exceeds the analytical bound by about
  5.37%. It meets a 20% model-agreement tolerance but does **not** satisfy
  the asserted upper bound.

The checked-in artifact publishes both numbers and `within_tolerance=true`.
That field cannot certify the bound. These are retained historical model
inputs, not fresh measurements of this source revision. Cross-link
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
limit, provided arrivals actually obey the envelope, each stage supplies
the stated service curve, and the stability condition holds. Estimated
p99 stage latencies or a statement-count speedup are not deterministic
service guarantees. Without those assumptions this is a model prediction.

For a pipeline of stages 1..N:

```
β_pipeline = compose_pipeline(β_1, β_2, ..., β_N)
            = (min(R_i)) · (t - Σ T_i)⁺
```

Pay-Bursts-Only-Once (PBOO): the burst latency is paid ONCE for the
entire pipeline, not per-stage. The substrate's `compose_pipeline`
implements this composition; it does not itself prove the headline 50ms SLO.

---

## Coverage status

`LatencyStage` contains eight leaf stages. The retained historical artifact
models the three stages of the 4KB overlap capture benchmark. Its manifest
slot is deferred to `ft-7h5da.10.2`; the file is not bundled as current
release proof. A diagnostic calculation or passing algebra test cannot
replace measured service curves and a held-out capture trace.

| Claim surface | `LatencyStage` leaves involved | Current status |
|---------------|--------------------------------|----------------|
| `<50ms` capture benchmark | `PtyCapture`, `DeltaExtraction`, `StorageWrite` | Historical artifact models this slice; its 8.5ms empirical value exceeds its 8.067ms bound. No bound-validity claim follows. |
| End-to-end capture path | `PtyCapture`, `DeltaExtraction`, `StorageWrite`, `PatternDetection`, `EventEmission` | Modeled, pending empirical: `LindleyTelemetryModel::documented_end_to_end_capture_default()` now covers all five leaves with a 23.1ms budget-backed bound, but the release artifact still lacks an empirical agreement row for the full PTY-to-event path. |
| Renderer input-to-photon SLOs | Renderer-specific classified-input proxy stages in `crates/frankenterm-gui/src/renderer_slo.rs` | Proxy substrate wired, physical claim unproven: v2 emits content-free classified-input stage telemetry and `tests/input_to_photon_bound.rs` checks internal Lindley agreement, but it omits native input, mux/PTY, production-window presentation, scan-out, and photons. The release artifact must keep the physical claim non-covered until correlated live-path evidence is retained. |
| Robot Mode response `<5ms` | `ApiResponse` plus handler-specific read/query work | Gap: G19/G54 evidence streams include `robot.p95`, but there is no `LatencyStage` telemetry model for the handler path yet. |
| FTS5 query `<10ms` | Search/read path plus `ApiResponse`; not the write-side `StorageWrite` leaf | Gap: G19/G54 evidence streams include `fts5.query_p99`, but the FTS5 query service curve is not in `latency_stages.rs` yet. |

The unresolved evidence gaps above are tracked in `ft-tf6g3.51` rather
than being folded into this artifact with invented numbers. The release
bundle currently defers this slot. The retained file is historical model
evidence for the 4KB overlap benchmark, and the end-to-end capture chain is
only `modeled_pending_empirical`
until a real PTY-to-event empirical row lands. It can cite renderer
input-to-photon only as `proxy_only_stage_telemetry_physical_path_unproven`.
It must not cite this artifact as proof for renderer, Robot Mode, or FTS5
read-path SLOs.

## Capture Latency

This is the manifest target for `capture_latency_p99` in
`docs/perf/headline-claims.json`.

### Historical model inputs

Source: documentation-derived defaults in `latency_stages/lindley.rs`,
including a modeled statement-count improvement and a carried-forward
latency. These are not live observed rates. They are the three rows in
the historical artifact, not current-release measurements.

| Stage             | Rate (R, events/ms) | Latency (T, ms) | Source |
|-------------------|----------------------|-----------------|--------|
| capture (PTY read)| 200                  | 1.0             | Historical documented input |
| delta-extract     | 150                  | 2.0             | Historical documented input |
| storage write     | 150                  | 5.0             | W9.1 statement-count extrapolation; latency carried forward |

Arrival: burst `b = 10 events`, rate `r = 90 events/ms`. The original
operator sketch used 100 events/ms, but the substrate requires strict
stability (`r < min(R_i)`) and the slowest service rate in this slice is
150 events/ms, so the release artifact uses a 40% steady-state margin.

W9.1 changed the storage-write hot path from one sequence lookup plus
one insert per segment to one sequence lookup per same-pane group plus
one insert per segment, all inside one explicit WAL transaction. For
the attestation burst `N = 10`, the conservative modeled statement
count improves from `2N = 20` to `N + 1 = 11` before counting avoided
per-row transaction setup. Applying that `20/11` factor to the old
100 events/ms storage row yields about 181 events/ms. The checked-in
service curve caps storage at 150 events/ms, matching the
DeltaExtraction bottleneck, and keeps the old 5.0ms p99 until a
retained batch benchmark publishes a new empirical latency row.

Composed pipeline: `β = min(200, 150, 150) · (t - 1 - 2 - 5)⁺ = 150·(t - 8)⁺`.

Lindley bound: `h = 8 + 10/150 = 8.0666666667ms` per arrival.

The README's `<50ms` figure is a budget ceiling for the benchmark lane.
The historical Lindley calculation does not establish that the current
implementation meets it.

### End-to-end capture model

The broader PTY-to-event capture path is:

```
capture -> delta-extract -> storage write -> pattern detect -> event emit
```

`LindleyTelemetryModel::documented_end_to_end_capture_default()` binds
all five `LatencyStage::CAPTURE_PATH` leaves. The first three rows reuse
the historical model inputs above. The final two rows use the
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

The horizontal distance bounds the delay under the stated envelopes;
it is not the observed delay of every event. PBOO collapses the burst into a single latency
penalty.

---

## Empirical-vs-analytical cross-check

The historical example comparison is:

```rust
EmpiricalComparison {
    analytical_bound_ms: 8.0666666667, // from pipeline_delay_bound
    empirical_p99_ms: 8.5,         // historical reference, not a fresh observation
}
```

Substrate predicates:
- `within_tolerance()` — `|empirical - analytical| / analytical ≤ 20%`
- `exceeds_bound()` — `empirical > analytical` (release blocker)

For the current modeled slice, `within_tolerance()` is true and
`exceeds_bound()` is also true. The former measures model agreement;
the latter invalidates the proposed upper-bound claim. No production
DSR caller that automatically files a regression bead is established by
these library predicates. A release gate must validate finite inputs
and reject an exceeded bound before making that claim.

---

## Attestation artifact

The legacy artifact had this shape. It is shown for interpreting old
evidence, not as output from the current measured producer:

```json
{
  "release_version": "0.0.0-substrate",
  "arrival": { "burst": 10, "rate": 90 },
  "stages": [
    { "name": "capture",  "service_rate": 200, "service_latency": 1.0 },
    { "name": "delta_extract",  "service_rate": 150, "service_latency": 2.0 },
    { "name": "storage_write",  "service_rate": 150, "service_latency": 5.0 }
  ],
  "stage_content_hashes": [
    { "name": "capture", "content_sha256": "..." },
    { "name": "delta_extract", "content_sha256": "..." },
    { "name": "storage_write", "content_sha256": "..." }
  ],
  "analytical_bound_ms": 8.0666666667,
  "empirical_p99_ms": 8.5,
  "deviation_pct": 5.37,
  "within_tolerance": true,
  "coverage_status": [
    { "claim_surface": "capture_4kb_overlap_benchmark", "status": "covered" },
    { "claim_surface": "end_to_end_capture_path", "status": "modeled_pending_empirical" },
    { "claim_surface": "robot_mode_response_lt_5ms", "status": "pending_service_curve" },
    { "claim_surface": "fts5_query_lt_10ms", "status": "pending_service_curve" },
    { "claim_surface": "renderer_input_to_photon", "status": "stage_telemetry_substrate_wired_pending_lab_run" }
  ]
}
```

The example reproduces the legacy artifact's fields; `covered` and
`within_tolerance` are not proof of the exceeded bound. The artifact is
retained at `docs/attestations/perf/lindley-bounds.json`, with one
content hash for each published stage row and explicit coverage status
for adjacent latency surfaces. Signing and trusted release validation
require the actual DSR bundle path and `ft-xxfwy.15`/`.49`; this document
does not establish that the checked-in file is signed.

Required release closeout:
1. Runs `pipeline_delay_bound(arrival, &stages)` to get
   `analytical_bound_ms`.
2. Retains current-source capture measurements and an independent held-out
   trace to get `empirical_p99_ms`; historical defaults are prohibited.
3. Composes a `LindleyBoundsArtifact`.
4. Calls `render_attestation_json` and writes the file.
5. Signs through the DSR release attestation path and verifies with the
   operator-owned release policy.
6. Validates finite/nonnegative measurements and admitted envelope
   assumptions, then requires `!comparison().exceeds_bound()` for any
   upper-bound claim. Record tolerance agreement separately.

---

## Current-source measurement procedure

The opt-in `lindley_bounds_build --measure-live` producer reads an explicitly
owned mux pane through `WeztermClient::get_text_with_cx`, extracts deltas with
`PaneCursor::capture_snapshot` and a 4096-byte overlap window, and submits
ten concurrent `StorageHandle::append_segment_with_cx` requests per burst.
The storage duration includes waiting for the other captures in the burst.
It exercises the production grouped writer; it does not observe or certify
the number of requests in each physical SQLite transaction.

The producer freezes its model after 1,000 calibration requests, then records
1,000 held-out requests. Service rates are minimum observed burst throughput;
stage latencies are calibration maximums, despite the legacy telemetry field
being named `p99_latency_ms`. The held-out p99 is computed independently.
Arrival-envelope, per-stage min-plus service, maximum-delay and 20% agreement
checks are separate. An exceeded bound or failed agreement remains a failed
measurement; do not refit the model using the held-out trace to make it pass.
This is a finite-workload model check, not a deterministic future guarantee.
It excludes production watch scheduling, pattern/event dispatch, native
renderer latency and power-loss durability.

Use three binaries built from the same retained candidate source with
`release-interactive`: `ft`, `frankenterm-mux-server`, and the
`lindley_bounds_build` example with feature `vendored`. The release parent
orchestrates their RCH/DSR builds; a Cargo test executable is not the example
producer. Do not use a system-installed mux as candidate proof.

Create a new, private evidence directory with a short socket path, isolated
HOME/XDG directories, and the following native `frankenterm.toml` (substitute
the absolute owned socket path):

```toml
initial_cols = 80
initial_rows = 24
scrollback_lines = 64

[[unix_domains]]
name = "lindley-owned"
socket_path = "/absolute/private-run/mux.sock"
no_serve_automatically = true
```

The 64-line scrollback plus 24 visible rows retains a full frame while keeping
the common snapshot overlap below 4KiB. Larger scrollback changes that workload
and can correctly fail the overlap check. The initial producer fills the
screen before publishing its ready marker so trailing blank screen rows do
not masquerade as an append.

Start only the owned server, in the foreground under the release driver's
bounded lifecycle supervision. Pass the example path as a positional shell
argument; do not interpolate it into shell code:

```bash
"$MUX_BIN" --config-file "$RUN/frankenterm.toml" --daemonize=false \
  --cwd "$RUN" -- /bin/sh -c 'stty -echo; exec "$1" --pane-producer' \
  lindley-owned "$LINDLEY_BIN"
```

The driver supplies `WEZTERM_UNIX_SOCKET`, `FRANKENTERM_UNIX_SOCKET` and
`FRANKENTERM_CONFIG_FILE` for this private server, and isolated configuration
directories. Retain its PID and logs. Require the socket lease file to name
that PID before connecting. Create a private `ft.toml` with
`[vendored] mux_socket_path = "/absolute/private-run/mux.sock"`; run candidate
`ft -c "$RUN/ft.toml" list --json` under a 30-second process timeout and require
exactly one pane. Use only that returned pane ID. The producer independently
requires its ready marker before sending input.

Run the already-built example through the external watchdog wrapper, using
an empty artifact directory and a database path that does not exist:

```bash
FT_LINDLEY_MUX_SOCKET="$RUN/mux.sock" \
FT_LINDLEY_PANE_ID="$OWNED_PANE_ID" \
FT_LINDLEY_DB_PATH="$RUN/capture.sqlite3" \
FT_RELEASE_VERSION="$RELEASE_VERSION" \
FT_LINDLEY_SOURCE_SHA="$SOURCE_SHA" \
FT_LINDLEY_ARRIVAL_RATE_EVENTS_PER_MS=0.1 \
FT_WEZTERM_CLI="$FT_BIN" \
FT_LINDLEY_BOUNDS_ARTIFACT_DIR="$RUN/measurement" \
bash scripts/lindley-bounds-build.sh --measure-live-executable "$LINDLEY_BIN"
```

Declare the arrival rate before calibration. The default external watchdog is
2,400 seconds, with bounded TERM/KILL settlement; it also covers synchronous
initialization and output stalls. The wrapper retains regular-file stdout,
stderr, an executable-hash/process receipt, per-burst traces and final JSON.
Exit 1 means an observed model check failed; exit 2 means invalid inputs,
execution/integrity failure or watchdog expiry. Preserve the database and
logs on every outcome, then stop and settle only the owned server/processes.

Before promoting any result, retain and verify the exact commit/tree,
profile, target, executable hashes, native host, socket/pane ownership,
wall-clock interval, filesystem/mount placement, configuration, complete raw
traces and corpus checks. A tmpfs run is not disk-durability evidence. The
producer's supplied source SHA and profile claims remain unverified until
joined to external build receipts. Its `release_ready` stays false; the
producing bead's attestation checklist and manifest wiring are a separate
reviewed closeout. No current measurement is recorded in this document yet.

## Cross-references

- `crates/frankenterm-core/src/network_calculus_bound.rs` — substrate
  (35 tests, including the 50ms-headline-claim scenario).
- `crates/frankenterm-core/src/latency_stages/lindley.rs` — telemetry schema
  and historical documentation-derived defaults.
- `crates/frankenterm-core/examples/lindley_bounds_build.rs` — diagnostic
  calculation and opt-in dedicated-mux measurement producer.
- `crates/frankenterm-core/src/bench_stats.rs` — bench harness +
  concentration-of-measure sample sizing
  (`min_sample_size_for_regression`).
- [`docs/methodology/statistics.md`](../methodology/statistics.md) —
  the broader statistical-rigor playbook.
- BR-RC-FOUNDATION.G3.4 (parent epic).
