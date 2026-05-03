# Swarm Capacity Baseline Plan

Date: 2026-05-03
Bead: `ft-onheq.1`
Status: baseline taxonomy published; high-scale runs blocked on this host

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

The recommended next implementation bead is `ft-onheq.2`, because several
certificate fields above are still only partially exposed by current telemetry.
