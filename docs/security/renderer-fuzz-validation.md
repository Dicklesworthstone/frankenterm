# Renderer Fuzz Lane Validation

**Bead:** [BR-TERM-EMULATOR-UPLIFT.1.6.cont] / `ft-n0hpo`
**Parent:** `ft-mpc9b.1.6` (foundation shipped at `1f2a44dd3`).
**SLO:** RQ-S4 in
[`docs/perf/resize-quality-slo.md`](../perf/resize-quality-slo.md)
— 24h adversarial fuzz, **0 critical artifacts**.
**Status:** Failure-artifact contract, scenario manifest, and headless
fuzz CLI are implemented. `crates/frankenterm-gui/tests/gpu_regression.rs`
parses `--fuzz-seed`, `--fuzz-duration`, `--fuzz-start-at`, and `--runs-dir`
and runs the fuzz stream with `headless-render`. This is source availability,
not retained 24-hour native-renderer qualification. FrankenTerm uses RCH
for development Cargo proof and DSR exclusively for release qualification.

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
and writes `violations.jsonl`, run metadata, and a summary. Qualification
must retain and validate the actual duration, event count, seed, adapter,
source identity, and violation counts.

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

The bead's 18-scenario plan is encoded as `scenario_manifest()` in
the contract module:

| Status | Count |
|---|---:|
| Manifest rows labeled shipped (not a native qualification count) | 4 |
| Partial (related fixture exists, additive needed) | 2 |
| Gap (`ft-ruona` non-a11y fixture work) | 11 |
| Headless-shipped (a11y event-stream + native comparator contract) | 1 (`screen-reader-active` — `ft-0q5zm` / `ft-5pk4h`) |
| **Total** | **18** |

Historical scenario catalog:
[`tests/renderer_golden/SCENARIOS.md`](../../tests/renderer_golden/SCENARIOS.md).
The on-disk fixtures live at `tests/golden/gpu/` (path
consolidation resolved — the bead's reference to
`tests/renderer_golden/scenarios/` is retired).
The current native coverage and acceptance authority is
[`renderer-scenario-contract.md`](../design/renderer-scenario-contract.md),
which separately records fixture completeness and native capture gaps.

## Qualification plan

No scheduled native fuzz cadence or completed RQ-S4 run is established by
this document. Release orchestration belongs to DSR; development Cargo
commands require strict remote RCH. A software-adapter run qualifies only
the recorded headless path, not the native GUI or display stack.

The proposed seed set remains eight fixed seeds (`a5a5a5a5`, `deadbeef`,
`cafebabe`, `feedface`, `12345678`, `87654321`, `0badc0de`, `f00dface`)
plus a recorded random seed. Retain each complete `runs/` tree and fail
qualification on any critical violation. Nine three-hour seed runs do
not establish a continuous 24-hour native run; the artifact must prove
the duration and execution scope required by RQ-S4.

## Reproducer ergonomics

Every failure carries enough state for a triager to land
directly on the offending event:

```bash
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 \
  rch --no-self-healing exec -- \
  env CARGO_TARGET_DIR=/tmp/ft-renderer-fuzz-repro \
  cargo test --locked \
    --profile release-interactive \
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
| Failure-artifact contract (`runs/<run_id>/` layout, classification) | ✓ `gpu_regression_fuzz_report` module + lib tests |
| Scenario manifest with status (shipped/partial/gap/blocked/headless-shipped) | ✓ `scenario_manifest()` + `coverage_snapshot()` |
| Non-a11y missing/additive scenario fixtures | ⏳ `ft-ruona`; fuzz CLI wiring does not generate golden fixtures |
| `screen-reader-active` a11y comparator | ✓ `ft-0q5zm` ships the headless a11y event-stream contract (`ScreenReaderSession` / `screen_reader_active_golden` / `screen_reader_active_violations`, built on `a11y_tree`); `ft-5pk4h` adds the native per-platform comparator result contract (`compare_native_screen_reader_events`) with explicit pass/fail/skipped recorder state |
| Harness CLI flag wiring (`--fuzz-seed`, `--fuzz-duration`, etc.) | ✓ harness binary parses `FuzzCliFlags`, dispatches to `FuzzStream`, and writes `runs/<run_id>/` artifacts |
| DSR native fuzz qualification | Pending retained RQ-S4 evidence; CLI wiring alone does not close this gate |
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
- **Release orchestration:** DSR only, per `AGENTS.md` Rule 0.1.
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
