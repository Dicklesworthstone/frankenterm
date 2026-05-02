# Competitor Resize Bench

`ft-t101b` publishes a per-release competitor resize snapshot that can be
replayed from raw per-terminal JSON. The policy matches
`frankenterm_core::competitor_delta`: compare `ft` against WezTerm, Ghostty,
and Rio on six metrics; `delta_pct <= -10.0` is a regression; the second
consecutive regressed release emits a P1 `br create` command.

## Workload

- Terminals: `ft`, WezTerm, Ghostty, Rio.
- Panes per terminal: 50.
- Corpus: `/usr/share/dict/words`.
- Gesture: 5 second resize storm.
- Metrics: `fps_p50`, `fps_p95`, `fps_p99`, `frame_time_p95_ms`,
  `gpu_memory_peak_mb`, `cpu_peak_pct`.

## Hardware Baselines

Snapshots must use one of these labels:

- `m2-macbook-pro-16gb`
- `framework-laptop-13-i7`
- `threadripper-rtx-4070`
- `github-actions-runner`

The output path is:

```text
docs/perf/competitor-resize-<version>-<hardware-baseline>.json
```

The regression state file is append-only JSONL:

```text
docs/perf/regression-state.jsonl
```

## Local Runs

Deterministic smoke:

```bash
bash scripts/competitor-bench.sh --simulate --release-version local-smoke --baseline github-actions-runner
```

Aggregate live raw data:

```bash
bash scripts/competitor-bench.sh \
  --input-dir /path/to/raw-json \
  --release-version 0.1.0 \
  --baseline m2-macbook-pro-16gb
```

Operator-gated live plan:

```bash
bash scripts/competitor-bench.sh --live --release-version 0.1.0 --baseline m2-macbook-pro-16gb
```

`--file-p1` executes the generated `br create --type=bug --priority=1`
commands for newly consecutive regressions. Without `--file-p1`, the commands
are recorded in the snapshot under `p1_regressions[].br_command`.

## Raw JSON

Each raw file is named `<competitor>.json` and uses:

```json
{
  "schema_version": "ft.competitor.resize.raw.v1",
  "competitor": "ft",
  "release_version": "0.1.0",
  "hardware_baseline": "github-actions-runner",
  "runner_sku": "ubuntu-24.04",
  "workload": {
    "terminal_count": 4,
    "panes_per_terminal": 50,
    "duration_seconds": 5,
    "corpus": "/usr/share/dict/words",
    "resize_gesture": "5s resize storm"
  },
  "metrics": {
    "fps_p50": 96.0,
    "fps_p95": 88.0,
    "fps_p99": 82.0,
    "frame_time_p95_ms": 13.2,
    "gpu_memory_peak_mb": 620.0,
    "cpu_peak_pct": 118.0
  }
}
```

The aggregate schema is `docs/perf/competitor-resize-schema.json`.
