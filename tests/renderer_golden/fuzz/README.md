# Adversarial Fuzz Lane (ft-mpc9b.1.6)

**Bead:** `ft-mpc9b.1.6`
**SLO:** RQ-S4 in [`docs/perf/resize-quality-slo.md`](../../../docs/perf/resize-quality-slo.md)
— 24h adversarial fuzz, **0 critical artifacts**.
**Generator:** [`crates/frankenterm-gui/src/gpu_regression_fuzz.rs`](../../../crates/frankenterm-gui/src/gpu_regression_fuzz.rs)

This lane catches visual regressions the deterministic scenario
suite misses by hammering the headless renderer for 24h with a
seed-driven random sequence of resize / scroll / write / selection
/ focus events, comparing each captured frame against an analytic
reference.

## Reproducibility contract

Every failure is reproducible from a single `u64`. The lane records
two layers of state:

- the **seed** the run was started from
- the **rng_state** at the offending event index

The seed alone is enough to replay the entire run; the rng_state
lets a triager land directly on the offending event without
re-executing the prefix.

```
runs/<run_id>/
├── meta.json              # seed, started_at, finished_at, host, harness_version
├── violations.jsonl       # one line per critical artifact observed
└── violations/
    └── <event_idx>/
        ├── before.png     # last good frame
        ├── after.png      # offending frame
        ├── diff.png       # pixel diff visualization (red = changed)
        ├── log.jsonl      # structured-log slice covering the event
        └── reproducer.sh  # `cargo test ... --fuzz-seed=<seed> --fuzz-start-at=<event_idx>`
```

## Event distribution (current)

The generator's event mix is calibrated for the 200-pane fleet
target. Approximate weights (see `gpu_regression_fuzz.rs`):

| Variant | Weight |
|---|---:|
| `Write` (printable ASCII burst) | 55 % |
| `Resize` | 15 % |
| `Scroll` | 10 % |
| `SelectStart` | 8 % |
| `SelectExtend` | 4 % |
| `EscapeBurst` (CSI/SGR primitives) | 3 % |
| `SelectEnd` | 2 % |
| `FocusToggle` | 2 % |
| `Clear` | 1 % |

These are tuneable via `FuzzConfig`. The continuation bead may
adjust once the integration ramps and the parser baseline is
captured.

## Critical vs minor artifact classification

A frame is **critical** if any of the following holds:

- entire frame is blank when the previous frame was non-blank
- frame is byte-identical to a frame ≥ 200 events earlier
  (stale full-frame from missed Present)
- a "tear band" is detected: a pristine area (no dirty mark) shows
  pixel divergence ≥ ΔL∞ 32

A frame is **minor** if it fails the comparator's standard
thresholds (SSIM < 0.99 or changed-pixel-fraction > 0.001) but
does not match any critical class. The 24h budget for minor
artifacts is 0.1% of resize-class events (per the bead).

## Running locally

The integration bead wires this. The intended ergonomics:

```bash
# 60-second smoke run with a fixed seed
cargo test -p frankenterm-gui --features headless-render --test gpu_regression -- \
    --fuzz-seed=0xCAFE_F00D \
    --fuzz-duration=60 \
    --runs-dir=tests/renderer_golden/fuzz/runs

# Reproduce a recorded violation
cargo test -p frankenterm-gui --features headless-render --test gpu_regression -- \
    --fuzz-seed=$(jq -r .seed runs/<id>/meta.json) \
    --fuzz-start-at=$(jq -r .event_idx runs/<id>/violations.jsonl | head -1) \
    --fuzz-duration=10
```

Until the wiring lands the generator is exercised by its inline
unit tests (`cargo test -p frankenterm-gui --lib gpu_regression_fuzz`).
The scheduled GitHub Actions lane runs on standard `ubuntu-24.04`
with Mesa llvmpipe and fails fast in a preflight until the harness
accepts these `--fuzz-*` flags; it no longer queues on an
unprovisioned GPU runner label.

## What is deferred

See the **continuation** entry in
[`../SCENARIOS.md`](../SCENARIOS.md). The summary: harness binary
needs `--fuzz-*` flags, failure-artifact emitter needs to write
the layout above, and the GitHub Actions nightly schedule needs
to land. The seed generator itself is shippable today.
