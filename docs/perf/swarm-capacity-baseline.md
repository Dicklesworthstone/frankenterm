# Swarm Capacity Baseline Plan

Date: 2026-05-03
Beads: `ft-onheq.1`, `ft-onheq.11`
Status: baseline taxonomy and candidate ranking published; high-scale runs blocked on this host

## Purpose

This plan defines the evidence required before implementing the `ft-onheq`
capacity-certificate and admission-control work. It is measurement-first:
future controller, threshold, or optimization changes must point to one of the
workload classes below and a reproducible artifact bundle before claiming a
performance win.

This builds on existing repo surfaces rather than adding a parallel benchmark
framework:

- `docs/backpressure-policy.md` describes the capture/write/event queue pipeline.
- `docs/ft-xbnl0-5-3-blessed-tuning-playbook.md` records the existing
  1/50/100/200 pane evidence base and tuning profiles.
- `docs/test-logging-contract.md` defines `manifest.json`, redaction, and
  artifact requirements.
- `crates/frankenterm-core/src/capacity_governor.rs` contains the current
  rch-aware heavy-workload governor.
- `crates/frankenterm-core/src/runtime_telemetry.rs`,
  `crates/frankenterm-core/src/continuous_backpressure.rs`, and
  `crates/frankenterm-core/src/test_artifacts.rs` provide reusable telemetry
  and artifact primitives.

## Current Host Snapshot

This host is not a valid 64+ core / 256 GiB validation machine. It can produce
taxonomy and command-shape evidence, but high-scale claims must come from a
target-class host or retained artifacts.

Command:

```bash
df -h . /tmp && sysctl -n hw.ncpu hw.memsize 2>/dev/null || true \
  && git rev-parse --short HEAD \
  && rustc -vV | sed -n '1,8p'
```

Observed before cleanup on 2026-05-02:

```text
/dev/disk3s5   1.8Ti   1.8Ti   159Mi   100%   /System/Volumes/Data
/dev/disk3s5   1.8Ti   1.8Ti   159Mi   100%   /System/Volumes/Data
14
68719476736
1bc1cdf6d
rustc 1.97.0-nightly (67bcaa9c4 2026-05-01)
host: aarch64-apple-darwin
```

After explicit cleanup of the failed target directory created by the
`ft-pe0i2` verification attempt, the volume still had only about 249 MiB free.
Do not run cargo, Criterion, or soak baselines on this host while it remains in
that state. A real baseline run should require at least 20 GiB free and a
target-class host for 64-core / 256 GiB claims.

## Artifact Contract

Every baseline run must produce an artifact directory under:

```text
tests/e2e/artifacts/ft-onheq/capacity-baseline/<YYYYMMDDTHHMMSSZ>/
```

Required files:

- `manifest.json` following `docs/test-logging-contract.md`.
- `commands.txt` with exact commands in execution order.
- `env.json` with git SHA, rustc version, target triple, OS, CPU count,
  memory bytes, disk free bytes, feature flags, and `CARGO_TARGET_DIR`.
- `summary.json` with workload class, pane scale, duration, status, and
  required metrics.
- redaction verification for logs or excerpts that may contain pane content.
- failure context for blocked or skipped runs. Missing data is never a pass.

## Workload Classes

| Class | Existing anchors | Required metrics | Baseline command |
| --- | --- | --- | --- |
| Idle observation | `watch_daemon_one_iteration_e2e.rs`, `observation_loop.rs`, `ft status --health` | pane count, capture queue depth, write queue depth, ingest lag, event lag, CPU/memory, p50/p95/p99 loop latency | `cargo test -p frankenterm-core --test watch_daemon_one_iteration_e2e -- --nocapture` |
| Heavy capture | `delta_extraction.rs`, `tailer.rs`, `fixtures/perf/agent-output-corpus/` | bytes/sec, lines/sec, delta extraction p50/p95/p99, capture queue depth, dropped/gap segments | `cargo bench -p frankenterm-core --bench delta_extraction` |
| Workflow storm | `proptest_workflows_*`, `golden_workflow_execution.rs`, `scripts/e2e_plan_workflow.sh` | workflow queue depth, lock wait, step latency, abort/cancel count, policy decisions, p95/p99 step time | `cargo test -p frankenterm-core --test golden_workflow_execution -- --nocapture` |
| Policy-denial storm | `policy.rs`, policy-denial audit docs/tests | deny/require-approval rate, audit write latency, redaction count, write queue impact | `cargo test -p frankenterm-core --test proptest_command_guard_telemetry -- --nocapture` |
| Snapshot/restore | `snapshot_e2e.rs`, `snapshot_engine.rs`, `scripts/e2e_session_persistence.sh` | snapshot duration, restore duration, bytes written, checkpoint count, p99 replay/read latency | `cargo test -p frankenterm-core --test snapshot_e2e -- --nocapture` |
| Search-heavy | `fts_query.rs`, `tantivy_search.rs`, `frankensearch_bench.rs`, `e2e_search_perf.sh` | query throughput, p50/p95/p99 latency, index refresh lag, memory, storage read-pool wait | `cargo bench -p frankenterm-core --bench fts_query` |
| MCP/robot burst | `mcp_response.rs`, `framework_throughput.rs`, `robot_family_contract.rs` | request rate, p50/p95/p99 response, policy-gate latency, error count, queue depth | `cargo bench -p frankenterm-core --bench mcp_response` |
| Storage write saturation | `wal_throughput.rs`, `storage_regression.rs`, `wal_checkpoint_sustained_write.rs`, `scripts/e2e_storage_stress.sh` | writer queue depth, write throughput, checkpoint time, WAL size, read-pool wait, p99 write latency | `cargo bench -p frankenterm-core --bench wal_throughput` |
| Backpressure escalation | `docs/backpressure-policy.md`, `continuous_backpressure.rs`, `scripts/e2e_backpressure.sh` | tier transitions, hysteresis duration, queue ratios, shed/defer/gap counts, recovery time | `cargo test -p frankenterm-core --test proptest_continuous_backpressure -- --nocapture` |
| Capacity governor | `capacity_governor.rs`, `capacity_governor_integration.rs`, `proptest_capacity_governor.rs` | allow/throttle/offload/block counts, pressure signals, decision reason, rch availability | `cargo test -p frankenterm-core --test capacity_governor_integration -- --nocapture` |

The first complete capacity certificate should combine at least idle
observation, backpressure escalation, storage write saturation, and capacity
governor data. Target-class validation should also include 50, 100, and 200
pane classes from the retained soak-matrix pattern.

## Normalized Capacity Metrics

Later `ft-onheq.3` work needs these fields per stage:

```json
{
  "stage": "storage.write_queue",
  "workload_class": "storage_write_saturation",
  "pane_scale": 200,
  "arrival_rate_per_s": 0.0,
  "service_rate_per_s": 0.0,
  "service_time_ms": {"p50": 0.0, "p95": 0.0, "p99": 0.0},
  "queue_depth": {"mean": 0.0, "p95": 0.0, "p99": 0.0, "capacity": 10000},
  "utilization": 0.0,
  "errors": 0,
  "cancellations": 0,
  "backpressure_tier": "green|yellow|red|black|unknown",
  "assumption_flags": [
    "insufficient_samples",
    "heavy_tailed_service",
    "bursty_arrivals",
    "disk_pressure",
    "model_residual_gt_20pct"
  ]
}
```

Use explicit `unknown` values when a metric is not instrumented yet. Do not
substitute zero for missing latency, queue, or rate data.

## Command Prefix

Use `rch` for cargo work and isolate target output. On a target-class host with
sufficient disk:

```bash
export CARGO_TARGET_DIR=/tmp/ft-onheq-capacity-target
export FT_CAPACITY_ARTIFACT_ROOT=tests/e2e/artifacts/ft-onheq/capacity-baseline/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$FT_CAPACITY_ARTIFACT_ROOT"
rch exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" bash -lc '<command>'
```

If free disk is below 20 GiB, record a blocked summary instead of starting a
build or soak run:

```json
{
  "status": "blocked",
  "reason_code": "blocked_disk_pressure",
  "minimum_free_bytes": 21474836480
}
```

## Baseline Run Matrix

Smallest local smoke once disk is healthy:

```bash
export CARGO_TARGET_DIR=/tmp/ft-codex-ft5bdc5-target
rch exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
  cargo test -p frankenterm-core --test capacity_governor_integration -- --nocapture
```

Target-class smoke:

```bash
export CARGO_TARGET_DIR=/tmp/ft-onheq-capacity-target
rch exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
  cargo test -p frankenterm-core --test capacity_governor_integration -- --nocapture
rch exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
  cargo test -p frankenterm-core --test proptest_continuous_backpressure -- --nocapture
rch exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
  cargo test -p frankenterm-core --test golden_workflow_execution -- --nocapture
```

Target-class benchmark lane:

```bash
export CARGO_TARGET_DIR=/tmp/ft-onheq-capacity-target
rch exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
  cargo bench -p frankenterm-core --bench delta_extraction
rch exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
  cargo bench -p frankenterm-core --bench wal_throughput
rch exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
  cargo bench -p frankenterm-core --bench mcp_response
```

Target-class soak lane:

```bash
bash scripts/e2e_swarm_stress.sh
bash scripts/e2e_backpressure.sh
bash scripts/e2e_storage_stress.sh
bash scripts/check_ft_xbnl0_4_6_release_gates.sh
```

Keep the full artifact bundle. The retained `ft-xbnl0.4.5` soak matrix is
historical input, not a substitute for `ft-onheq` certificate input because it
does not yet provide all normalized arrival/service/queue fields above.

## Queueing and Control Assumptions

Start with conservative checks only:

- Little's Law consistency per stage: `L ~= lambda * W`.
- utilization should stay below 0.8 for latency-sensitive stages unless an
  explicit burst/soak profile justifies otherwise.
- high service-time variance downgrades the certificate to `unknown` until
  measured p99 data is available.
- controller decisions fail closed on stale baselines, missing samples, disk
  pressure, or model residuals above tolerance.

## Candidate Ranking Refresh

`ft-onheq.11` refreshed the alien-graveyard and Rust ecosystem scan before the
first implementation bead. The canonical paths named by the skill are not
mounted at `/data/projects/alien_cs_graveyard`, but the same repository is
available locally at `/Users/jemanuel/projects/alien_cs_graveyard`:

```bash
rg -n 'queue|tail|latency|control|backpressure|admission|scheduler|sketch|histogram|evidence ledger|capacity' \
  /Users/jemanuel/projects/alien_cs_graveyard/alien_cs_graveyard.md \
  /Users/jemanuel/projects/alien_cs_graveyard/high_level_summary_of_frankensuite_planned_and_implemented_features_and_concepts.md
```

Relevant source anchors:

- `alien_cs_graveyard.md` requires symptom-first work, baseline capture, p99/p999
  tail decomposition, evidence ledgers for runtime decisions, fallback triggers
  for adaptive controllers, and timescale-separation checks when multiple
  controllers share telemetry.
- `high_level_summary_of_frankensuite_planned_and_implemented_features_and_concepts.md`
  names FrankenTerm queueing/network/retry observables specifically: probe RTT
  tails, reconnect retries, workflow queue lag, event queue tails, and expected
  false-positive/false-negative loss for suspicion and retry policy.
- Both sources steer this epic toward concrete artifacts: queue/service
  telemetry, capacity certificates, conformal/tail monitors, evidence ledgers,
  replay tests, and conservative control decisions. They do not justify a
  lock-free or adaptive rewrite before the measurement substrate exists.

Rust ecosystem scan, 2026-05-03:

- Existing workspace dependency `metrics = 0.23.1` is already used in mux, codec,
  GUI, window, and font code. Reuse the facade for stage counters and coarse
  histograms; do not add a parallel metrics facade.
- Existing workspace dependency `hdrhistogram = 7.1` and current crates.io
  `hdrhistogram = 7.5.4` are MIT/Apache-2.0. Use only if `ft-onheq.2` needs an
  in-process fixed histogram beyond the existing telemetry structs.
- Existing workspace dependency `governor = 0.5.1` is MIT and crates.io latest is
  `0.10.4`. Do not update or depend on the newer API in this epic; the first
  controller should be deterministic dry-run logic over measured queues, not an
  external rate-limit rewrite.
- `sketches-ddsketch = 0.3.1` is present transitively through Tantivy and crates.io
  latest is `0.4.0` under Apache-2.0. Treat it as a candidate for tail summaries
  only after a direct dependency promotion bead records the maintenance and API
  decision.
- `prometheus-client = 0.24.1` is Apache-2.0 OR MIT and suitable for future
  export, but `ft-onheq` should first expose typed robot/doctor/certificate
  data without adding a new exporter dependency.
- `quanta = 0.12.6` is MIT and high-speed timing oriented. It remains a profiling
  candidate only; the first implementation should use existing monotonic timing
  surfaces unless timestamp overhead is measured as a top-five hotspot.

EV matrix:

| Candidate | Impact | Confidence | Reuse | Effort | Adoption friction | EV | Decision |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Queue and service-time observables (`ft-onheq.2`) | 5 | 5 | 5 | 2 | 1 | 62.5 | Ship first; blocks every model. |
| Queueing capacity certificates (`ft-onheq.3`) | 5 | 4 | 4 | 3 | 1 | 26.7 | Ship after observables and this refresh. |
| Conformal tail-risk monitors (`ft-onheq.4`) | 5 | 4 | 3 | 4 | 1 | 15.0 | Ship before enabled control; fail unknown. |
| Evidence ledger and replay bundles (`ft-onheq.7`, `ft-onheq.9`) | 4 | 4 | 4 | 3 | 1 | 21.3 | Required for audit/repro before actuation. |
| Conservative dry-run controller (`ft-onheq.5`) | 5 | 4 | 3 | 4 | 2 | 7.5 | Ship after monitor, fairness, and ledger. |
| Priority-aware fairness (`ft-onheq.6`) | 4 | 4 | 3 | 3 | 2 | 8.0 | Needed before controller decisions affect classes. |
| Robot/doctor capacity surfaces (`ft-onheq.8`) | 4 | 4 | 5 | 3 | 1 | 26.7 | Ship once certificates/decisions exist. |
| Capacity regression gates (`ft-onheq.10`) | 4 | 4 | 5 | 3 | 1 | 26.7 | Ship after first trace-driven artifacts. |
| Regret-bounded adaptive tuning (`ft-onheq.12`) | 3 | 3 | 2 | 5 | 3 | 1.2 | Defer; below EV gate until static controller works. |
| Lock-free queues / seqlocks / RCU | 4 | 2 | 2 | 5 | 4 | 0.8 | Reject for now; needs measured contention hotspot. |
| S3-FIFO/cache admission | 3 | 2 | 2 | 5 | 4 | 0.6 | Reject for this epic; no cache-thrash evidence. |

Selected implementation queue:

1. `ft-onheq.2`: instrument arrivals, completions, backlog, service time,
   cancellations/timeouts, and error class across the hot stages named above.
2. `ft-onheq.3`: compile stage capacity certificates from those normalized
   fields with explicit `unknown` and stale-baseline states.
3. `ft-onheq.4`: add conformal/tail-risk monitors so heavy-tail or
   insufficient-sample states do not produce false green certificates.
4. `ft-onheq.7` and `ft-onheq.6`: persist decisions and define fairness before
   the controller can influence work classes.
5. `ft-onheq.5`: implement dry-run conservative control first; enabled mode
   stays opt-in and waits for replay coverage.
6. `ft-onheq.8`, `ft-onheq.9`, `ft-onheq.10`: expose, rehearse, and gate the
   system with reproducible artifacts.
7. `ft-onheq.12`: revisit adaptive tuning only after measured static control
   leaves a meaningful gap.

Fallback and rollback triggers:

- Any missing queue/service field produces `unknown`, not `0`.
- Any stale baseline, disk pressure, insufficient samples, high model residual,
  or p99 regression budget breach disables enabled control and keeps dry-run
  evidence only.
- Any direct dependency promotion requires a separate dependency/update bead
  with license, unsafe-surface, determinism, API, and maintenance notes.
- If two controllers share telemetry, require replay evidence and a written
  timescale-separation statement before enabling both.
- Every implementation commit remains revertable as a single lever; the rollback
  plan is `git revert <commit>` plus config defaulting the controller disabled.

The recommended next implementation bead is `ft-onheq.2`, because several
certificate fields above are still only partially exposed by current telemetry.
