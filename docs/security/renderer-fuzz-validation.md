# Renderer Fuzz Lane Validation

**Bead:** [BR-TERM-EMULATOR-UPLIFT.1.6.cont] / `ft-n0hpo`
**Parent:** `ft-mpc9b.1.6` (foundation shipped at `1f2a44dd3`).
**SLO:** RQ-S4 in
[`docs/perf/resize-quality-slo.md`](../perf/resize-quality-slo.md)
— 24h adversarial fuzz, **0 critical artifacts**.
**Status:** Foundation slice shipped — failure-artifact
contract + GitHub Actions workflow + scenario manifest + audit
doc + path consolidation. The GitHub Actions workflow now uses
the standard `ubuntu-24.04` hosted runner with Mesa llvmpipe
setup, and it fails fast with a configuration error until the
production harness CLI wiring lands.

## Headline rule

> Every visual regression in the renderer must be reproducible
> from a single `u64` seed, classified as **critical** or
> **minor**, and emit a self-describing failure-artifact tree
> the next-day triager can land on without re-executing the
> prefix.

## Critical-vs-minor taxonomy

The fuzz lane classifies every observed violation. Three
critical classes (from
[`fuzz/README.md`](../../tests/renderer_golden/fuzz/README.md)):

| Slug | Trigger | RQ-S4 impact |
|---|---|---|
| `blank_frame` | Entire frame is blank when the previous frame was non-blank | Hard fail |
| `stale_full_frame` | Frame is byte-identical to a frame ≥ 200 events earlier (missed Present) | Hard fail |
| `tear_band` | Pristine area (no dirty mark) shows pixel divergence ΔL∞ ≥ 32 | Hard fail |

Two minor classes:

| Slug | Trigger | Budget |
|---|---|---|
| `ssim_below_threshold` | SSIM < 0.99 | 0.1% of resize-class events over 24h |
| `excessive_pixel_change` | Changed-pixel fraction > 0.001 | Same |

The contract layer at
`crates/frankenterm-core/src/gpu_regression_fuzz_report.rs`
encodes these as `ViolationKind` variants with `is_critical()`
predicate; the harness binary classifies frames at runtime
and writes `violations.jsonl` rows the GHA workflow tallies.

## Run artifact layout

```text
runs/<run_id>/
├── meta.json              # RunMeta — seed, started_at, host,
│                          #   harness_version, events_processed,
│                          #   critical_count
├── violations.jsonl       # one ViolationRecord per critical/minor
└── violations/
    └── <event_idx zero-padded to 8 digits>/
        ├── before.png     # last good frame
        ├── after.png      # offending frame
        ├── diff.png       # pixel diff visualization (red = changed)
        ├── log.jsonl      # structured-log slice covering the event
        └── reproducer.sh  # cargo test ... --fuzz-seed=<seed>
                           #   --fuzz-start-at=<event_idx>
```

`run_id` is a 16-hex-char FNV-1a hash of `(seed, started_at_ms,
host)` — deterministic per inputs so a triager can recompute it
from `meta.json` alone. Path helpers live at
`gpu_regression_fuzz_report::RunLayout`.

## Scenario manifest

The bead's "18-scenario plan" (the bead text says 18 but
enumerates 7+12=19; the manifest matches the enumeration)
is encoded as `scenario_manifest()` in the contract module:

| Status | Count |
|---|---:|
| Shipped (golden + CI lane) | 5 |
| Partial (related fixture exists, additive needed) | 2 |
| Gap (12-scenario action #3) | 11 |
| Blocked on sub-bead | 1 (`screen-reader-active` — needs a11y harness) |
| **Total** | **19** |

Source of truth:
[`tests/renderer_golden/SCENARIOS.md`](../../tests/renderer_golden/SCENARIOS.md).
The on-disk fixtures live at `tests/golden/gpu/` (path
consolidation resolved — the bead's reference to
`tests/renderer_golden/scenarios/` is retired).

## CI cadence

`.github/workflows/renderer-fuzz.yml`:

- **Trigger:** nightly at 03:00 UTC + manual `workflow_dispatch`.
- **Runner strategy:** standard GitHub-hosted `ubuntu-24.04`
  with Mesa llvmpipe (`FT_GPU_HARNESS_FORCE_SOFTWARE=1`).
  The retired custom GPU label is not used because this repository
  has no provisioned runner for it, and queue-only jobs are not
  renderer proof.
- **Readiness gate:** before any Cargo build, the workflow checks
  that `crates/frankenterm-gui/tests/gpu_regression.rs` accepts
  `--fuzz-seed`, `--fuzz-duration`, `--fuzz-start-at`, and
  `--runs-dir`. If the harness regresses and drops one of those
  flags, the scheduled run exits with a clear "Renderer fuzz
  harness not wired" error before spending Cargo time.
- **Matrix:** 8 fixed seeds + 1 date-derived random
  (`a5a5a5a5`, `deadbeef`, `cafebabe`, `feedface`, `12345678`,
  `87654321`, `0badc0de`, `f00dface`, plus `random` derived
  from `date -u +%s ^ 0xc0ffeebabe`). Manual dispatch can
  set `seed_override` to run exactly one seed for triage.
- **Per-seed budget:** 3h (configurable via
  `workflow_dispatch.duration_secs` input). 9 parallel jobs
  run within a 24h wall-clock window.
- **Pass criterion:** zero critical violations across all 9
  seeds. The workflow fails the run on any critical.
- **Artifacts:** `runs/` tree uploaded per seed (30-day
  retention). Aggregated summary posted to the next-day
  commit status.

## Reproducer ergonomics

Every failure carries enough state for a triager to land
directly on the offending event:

```bash
cargo test \
    --release \
    -p frankenterm-gui \
    --test gpu_regression \
    --features headless-render \
    -- \
    --nocapture \
    --fuzz-seed=<seed-from-meta.json> \
    --fuzz-start-at=<event_idx> \
    --runs-dir=$PWD/runs
```

The CLI flag envelope is `FuzzCliFlags` in the contract
module — `fuzz_mode_active()` is true iff any of `seed`,
`duration_secs`, or `start_at_event_idx` is set.

## Health snapshot

`GpuFuzzHealth` is the `ft doctor` surface:

```text
last_run                  : Option<RunMeta>
critical_24h              : per-kind counter for the rolling 24h window
minor_24h                 : same
rq_s4_ok                  : true iff critical_24h is empty
```

`fold_violation(&mut health, &record)` updates the snapshot;
the doctor wires it to a WARN-level message when
`rq_s4_ok == false`.

## Bead acceptance status

| Item | Status |
|---|---|
| Failure-artifact contract (`runs/<run_id>/` layout, classification) | ✓ `gpu_regression_fuzz_report` module + 17 lib tests |
| Scenario manifest with status (shipped/partial/gap/blocked) | ✓ `scenario_manifest()` + `coverage_snapshot()` |
| 12 missing scenario fixtures | ⏳ separate scenario-corpus follow-on; fuzz CLI wiring does not generate golden fixtures |
| Harness CLI flag wiring (`--fuzz-seed`, `--fuzz-duration`, etc.) | ✓ harness binary parses `FuzzCliFlags`, dispatches to `FuzzStream`, and writes `runs/<run_id>/` artifacts |
| GitHub Actions workflow | ✓ `.github/workflows/renderer-fuzz.yml` uses `ubuntu-24.04` llvmpipe preflight before the matrix |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |
| Path consolidation (renderer_golden/scenarios → golden/gpu) | ✓ SCENARIOS.md updated |
| `dead_code` allow removed in `gpu_regression_fuzz.rs` | ✓ removed after `gpu_regression.rs` wired the caller |

## Cross-references

- **Generator (parent bead):**
  `crates/frankenterm-gui/src/gpu_regression_fuzz.rs` —
  `FuzzSeed` / `FuzzStream` / `FuzzInputEvent` / `FuzzConfig`.
- **Comparator:**
  `crates/frankenterm-gui/src/gpu_regression.rs` —
  `compare_images` (SSIM + ΔL∞ + changed-pixel-fraction).
- **Failure-artifact contract:**
  `crates/frankenterm-core/src/gpu_regression_fuzz_report.rs`.
- **Scenario catalog:**
  `tests/renderer_golden/SCENARIOS.md`.
- **Fuzz-lane spec:**
  `tests/renderer_golden/fuzz/README.md`.
- **GHA workflow:** `.github/workflows/renderer-fuzz.yml`.
- **SLO:** `docs/perf/resize-quality-slo.md` (RQ-S4).
- **Attestation cross-link:** `BR-RC-FOUNDATION.G3.1`
  (`ft-syqcz.1`).
- **Sibling foundation fixtures** (same `*Health` /
  contract-layer pattern this session):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `wayland_frame_pacing`, `bidi_correctness`,
  `tx_killswitch_model`, `passive_watch_invariant`,
  `wire_dedup_model`, `redactor_coverage_matrix`,
  `tui_parity_oracle`, `robot_checkpoint_state_machine`,
  `robot_work_state_machine`, `robot_fleet_state_machine`,
  `wayland_compositor_matrix`, `audit_erasure_spec`,
  `robot_context_state_machine`.
