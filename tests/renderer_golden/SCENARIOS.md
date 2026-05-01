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
| 1 | `steady-typing` | — | gap | ft-mpc9b.1.6.cont (TODO) | RQ-S8 (frame skip steady state) |
| 2 | `vim-edit` | — | gap | ft-mpc9b.1.6.cont | RQ-S6 (heavy-burst input latency) |
| 3 | `htop-top` | — | gap | ft-mpc9b.1.6.cont | RQ-S5 (idle GPU), RQ-S8 |
| 4 | `neofetch-banner` | — | gap | ft-mpc9b.1.6.cont | RQ-S11 (snap-back SSIM) |
| 5 | `resize-step` | `multipane-resize-static-snapshot` (close) | partial | ft-mpc9b.1.6.cont | RQ-S1 (resize FPS) |
| 6 | `resize-burst` | — | gap | ft-mpc9b.1.6.cont | RQ-S1, RQ-S10 (atlas rebuild count) |
| 7 | `scroll-stress` | `stress/` | shipped (verify naming) | — | RQ-S6 |
| 8 | `selection-drag` | `selection-{char,line,word}` (close, no drag) | partial | ft-mpc9b.1.6.cont | — |
| 9 | `scrollback-search` | — | gap | ft-mpc9b.1.6.cont | — |
| 10 | `multi-pane-split` | `multipane-{2split-h,2split-v,grid-4,deep-nested,floating-overlay}` | shipped | — | RQ-S12 (floating-pane overhead) |
| 11 | `dpi-change` | — | gap | ft-mpc9b.1.6.cont | RQ-S10 |
| 12 | `font-change` | — | gap | ft-mpc9b.1.6.cont | RQ-S10 |
| 13 | `alt-screen` | — | gap | ft-mpc9b.1.6.cont | — |
| 14 | `mouse-tracking` | — | gap | ft-mpc9b.1.6.cont | — |
| 15 | `wide-gamut` | — | gap | ft-mpc9b.1.6.cont | — |
| 16 | `rtl-script` | `text-rtl-arabic-hebrew` | shipped | — | — |
| 17 | `cjk-mixed` | `text-cjk-mixed` | shipped | — | — |
| 18 | `screen-reader-active` | — | gap; needs A11y harness | ft-mpc9b.1.6.cont | — |

Status legend:

- **shipped** — fixture exists, golden captured, runs in CI today
- **partial** — a closely related fixture exists but does not exercise
  the exact dirty-event path the bead calls out; needs an additive
  fixture or an extension to the existing one
- **gap** — no matching fixture; the continuation bead delivers it

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
  `tests/golden/gpu/` inventory; the **gap** rows above flow into the
  continuation bead with explicit owners
- RQ-S4 (24h fuzz, 0 critical artifacts) in the SLO catalog now has a
  concrete seed-generator owner — the comparator already exists
  (`compare_images`), so the integration bead is just plumbing

## What is deferred (continuation, see follow-up bead)

- Concrete `tests/golden/gpu/<scenario>/` fixtures for every **gap**
  row above (12 scenarios). Each fixture needs an `input.json`,
  `meta.json`, `expected.json`, and a captured `golden.png`.
- `--fuzz <seed> --duration <secs>` mode in the harness binary that
  drives `FuzzStream` against the headless renderer.
- `runs/<run_id>/violations.jsonl` failure-artifact emitter: on SSIM
  drop or pixel diff in a pristine area, write the seed, event index,
  before/after PNGs, and structured-log slice.
- GitHub Actions nightly 24h-budget workflow with seed sweep.
- Per-release attestation entry at
  `docs/attestations/render-parity-<version>.json` (depends on
  `BR-RC-FOUNDATION.G3.1` attestation graph schema).
- A11y harness for `screen-reader-active` (scenario 18) — needs the
  platform accessibility tree comparator, which is its own epic.
- Path consolidation: bead specifies `tests/renderer_golden/scenarios/`
  but existing fixtures live at `tests/golden/gpu/`. The continuation
  bead picks one (current preference: keep `tests/golden/gpu/`
  because the harness binary already uses it) and updates this
  catalog + the bead to match.
