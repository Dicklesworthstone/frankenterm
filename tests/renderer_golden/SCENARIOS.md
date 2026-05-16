# Renderer-Overhaul Scenario Catalog (ft-mpc9b.1.6)

**Bead:** `ft-mpc9b.1.6` — Visual regression harness for renderer changes
**Cross-link beads:** `ft-ombfl` (existing harness epic), `ft-35yac.1.2` (parity test)
**SLO catalog:** [`docs/perf/resize-quality-slo.md`](../../docs/perf/resize-quality-slo.md) — RQ-S4 (24h fuzz), RQ-S11 (snap-back SSIM)

This document is the single reconciled source for the 18-scenario plan
called out in `ft-mpc9b.1.6`. It maps each plan scenario onto the
**existing** harness fixtures shipped under `ft-ombfl` (which live at
[`tests/golden/gpu/`](../golden/gpu/), not `tests/renderer_golden/scenarios/`)
and identifies the gaps that follow-up beads close.

The harness binary that drives these fixtures lives at
`crates/frankenterm-gui/tests/gpu_regression.rs`; the comparator
(`compare_images`, SSIM + ΔL∞ + changed-pixel-fraction) lives at
`crates/frankenterm-gui/src/gpu_regression.rs`. Any new scenario added
under this catalog must conform to the same fixture format
(`input.json`, `meta.json`, `expected.json`, `golden.png`).

## Scenario index

| # | Scenario id | Existing fixture | Status | Owner bead | SLO mapping |
|---|---|---|---|---|---|
| 1 | `steady-typing` | — | gap | ft-ruona | RQ-S8 (frame skip steady state) |
| 2 | `vim-edit` | — | gap | ft-ruona | RQ-S6 (heavy-burst input latency) |
| 3 | `htop-top` | — | gap | ft-ruona | RQ-S5 (idle GPU), RQ-S8 |
| 4 | `neofetch-banner` | — | gap | ft-ruona | RQ-S11 (snap-back SSIM) |
| 5 | `resize-step` | `multipane-resize-static-snapshot` (close) | partial | ft-ruona | RQ-S1 (resize FPS) |
| 6 | `resize-burst` | — | gap | ft-ruona | RQ-S1, RQ-S10 (atlas rebuild count) |
| 7 | `scroll-stress` | `stress/` | shipped (verify naming) | — | RQ-S6 |
| 8 | `selection-drag` | `selection-{char,line,word}` (close, no drag) | partial | ft-ruona | — |
| 9 | `scrollback-search` | — | gap | ft-ruona | — |
| 10 | `multi-pane-split` | `multipane-{2split-h,2split-v,grid-4,deep-nested,floating-overlay}` | shipped | — | RQ-S12 (floating-pane overhead) |
| 11 | `dpi-change` | — | gap | ft-ruona | RQ-S10 |
| 12 | `font-change` | — | gap | ft-ruona | RQ-S10 |
| 13 | `alt-screen` | — | gap | ft-ruona | — |
| 14 | `mouse-tracking` | — | gap | ft-ruona | — |
| 15 | `wide-gamut` | — | gap | ft-ruona | — |
| 16 | `rtl-script` | `text-rtl-arabic-hebrew` | shipped | — | — |
| 17 | `cjk-mixed` | `text-cjk-mixed` | shipped | — | — |
| 18 | `screen-reader-active` | — | blocked on a11y comparator | ft-0q5zm | — |

Status legend:

- **shipped** — fixture exists, golden captured, runs in CI today
- **partial** — a closely related fixture exists but does not exercise
  the exact dirty-event path the bead calls out; needs an additive
  fixture or an extension to the existing one
- **gap** — no matching fixture; `ft-ruona` delivers the non-a11y fixture
- **blocked** — no matching fixture; a separate harness/comparator bead is
  needed before the fixture can be generated

## Existing fixtures not in the bead's 18

The harness already ships fixtures the renderer-overhaul plan does
not enumerate. They stay in CI and are not "extra work" — they cover
ground the bead doesn't, and the comparator infra is shared.

| Existing fixture | Coverage |
|---|---|
| `cursor-{beam,block,underline}-{steady,blink}` | Cursor shape + blink phases (RQ-S2/RQ-S3 input-to-photon) |
| `overlay-ime-composition` | IME composition state |
| `overlay-visual-mode` | Visual-mode overlay rendering |
| `text-basic-paragraph` | Baseline text rendering |
| `text-box-drawing` | Unicode box characters |
| `text-combining-marks` | Combining-mark stacking |
| `text-emoji-fallback` | Emoji + fallback font selection |
| `_smoketest` | Static-PNG roundtrip (scaffold check, no GPU) |

## What this bead ships in the foundation slice

Code:

- [`crates/frankenterm-gui/src/gpu_regression_fuzz.rs`](../../crates/frankenterm-gui/src/gpu_regression_fuzz.rs) —
  deterministic seed-based input-event generator (`FuzzSeed`,
  `FuzzInputEvent`, `FuzzStream`, `FuzzConfig`) for the 24h adversarial
  fuzz lane. Same seed → same stream forever, so any failure is
  reproducible from the seed alone.

Docs:

- this catalog (`tests/renderer_golden/SCENARIOS.md`)
- the fuzz lane spec ([`fuzz/README.md`](fuzz/README.md))

Cross-references:

- the bead's 18-scenario plan is now reconciled with the existing
  `tests/golden/gpu/` inventory; the non-a11y **gap** and **partial**
  rows above flow into `ft-ruona`, and `screen-reader-active` flows
  into `ft-0q5zm`
- RQ-S4 (24h fuzz, 0 critical artifacts) in the SLO catalog now has a
  concrete seed-generator owner — the comparator already exists
  (`compare_images`), so the integration bead is just plumbing

## What the continuation bead (`ft-n0hpo`) ships

**Path consolidation (resolved):** Fixtures live at
`tests/golden/gpu/`. The bead text's `tests/renderer_golden/scenarios/`
reference is retired in favor of the on-disk path. This catalog and
the contract module agree on `tests/golden/gpu/`.

**Foundation slice — shipped at this bead:**

- [`crates/frankenterm-core/src/gpu_regression_fuzz_report.rs`](../../crates/frankenterm-core/src/gpu_regression_fuzz_report.rs) —
  failure-artifact emitter contract: `RunId`, `RunMeta`,
  `ViolationKind` (with the 3 critical classes from
  `fuzz/README.md`), `ViolationRecord`, `RunLayout` (filesystem
  path helpers), `FuzzCliFlags` (typed CLI flag envelope),
  `ScenarioRecord` + `scenario_manifest()` (the 18-row catalog
  encoded in Rust, 1:1 with the table above), `coverage_snapshot()`,
  `GpuFuzzHealth` (ft doctor surface). Unit tests cover the contract.
- [`.github/workflows/renderer-fuzz.yml`](../../.github/workflows/renderer-fuzz.yml) —
  Nightly 24h workflow: 8 fixed seeds (`a5a5a5a5`, `deadbeef`,
  `cafebabe`, `feedface`, `12345678`, `87654321`, `0badc0de`,
  `f00dface`) + 1 date-derived random. 3h budget per seed × 9
  seeds = 27h total compute, 24h wall (matrix runs in parallel
  on standard `ubuntu-24.04` with Mesa llvmpipe). The workflow
  runs a standalone preflight before the matrix. Once the preflight
  passes, the matrix aggregates `violations.jsonl` across runs,
  posts the next-day commit-status check, and **fails on any critical
  violation** (RQ-S4: zero criticals).
- [`docs/security/renderer-fuzz-validation.md`](../../docs/security/renderer-fuzz-validation.md) —
  audit doc with the failure-artifact taxonomy, run-layout
  reference, GHA workflow description, RQ-S4 trace, and bead
  acceptance status.

**Remaining follow-on (production proof):**

- Full 24h renderer-fuzz proof on the scheduled lane. The harness
  binary at `crates/frankenterm-gui/tests/gpu_regression.rs` now
  parses argv into `FuzzCliFlags`, dispatches to `FuzzStream` on
  `fuzz_mode_active()`, emits deterministic duplicate-render frames,
  and writes `runs/<run_id>/` artifacts. RQ-S4 is not proven until
  the full seed matrix completes with zero critical violations.
- Concrete `tests/golden/gpu/<scenario>/` fixtures for every non-a11y
  **gap** row (11 scenarios) and additive coverage for the two
  **partial** rows. Each fixture needs `input.json`, `meta.json`,
  `expected.json`, and a captured `golden.png` from the headless
  renderer. This corpus work is tracked by `ft-ruona`.
- A11y harness for `screen-reader-active` (scenario 18) — tracked by
  `ft-0q5zm` because it needs the platform accessibility tree
  comparator before a renderer golden can be generated.
- Per-release attestation entry at
  `docs/attestations/render-parity-<version>.json` (depends on
  `BR-RC-FOUNDATION.G3.1` / `ft-syqcz.1` attestation graph schema).
