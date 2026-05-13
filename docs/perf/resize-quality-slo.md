# Resize Quality SLO — Renderer-Overhaul Targets

**Bead:** `ft-mpc9b.7`
**Parent epic:** `ft-mpc9b` — *BR-TERM-EMULATOR-UPLIFT — close the wezterm-vs-ghostty/rio gap*
**Machine-readable mirror:** [`docs/perf/resize-quality-slo.json`](./resize-quality-slo.json)
**Complementary doc:** [`docs/resize-performance-slos.md`](../resize-performance-slos.md) — covers the *upstream-of-render* scheduler/reflow stage budgets (`wa-1u90p` track). This doc covers the *render-output* quality contract.
**Status:** v0.1 — numeric targets frozen; bench wiring is per-SLO and tracked in `blocked_by` rows.

---

## Why this doc exists

The `ft-mpc9b` epic spreads its acceptance numbers across many sub-beads (atlas
stability, conditional redraw, compositor, Metal-direct, etc.). Anyone who
asks *"did the renderer overhaul actually deliver?"* needs one place to read
the answer. This doc is that place.

It is also the schema producers feed into:

- README "Performance Targets" links (BR-RC-DOCTRINE.G6 auto-stamp)
- the headline-claim manifest (BR-RC-FOUNDATION.G3.3)
- the competitor matrix (BR-RC-FOUNDATION.G3.5)
- per-release attestation bundles (BR-RC-FOUNDATION.G3.1, deferred — see below)
- per-PR regression gate (BR-RC-FOUNDATION.G3.2, deferred — see below)

## Scope

**In scope** — renderer-overhaul SLOs delivered by sub-epics 1-6 of
`ft-mpc9b`:

- GPU/atlas/render-path quality targets
- Per-frame render quality (frame skip, idle GPU, parity SSIM)
- End-to-end input-to-photon latency (after ft-mpc9b.5.1 lands)
- Atlas stability under resize/DPI change (ft-mpc9b.1.1)
- Compositor / floating-pane overhead (ft-mpc9b.4.1)

**Out of scope** — covered by other docs:

- Upstream-of-render scheduler/reflow stage budgets → see
  `docs/resize-performance-slos.md` (M1-M4: interaction latency, stage
  budgets, artifact incidence, crash budgets).
- Non-renderer correctness (workflows, policy, search, MCP).

## Status legend

| Status | Meaning |
|---|---|
| `frozen` | Numeric target is committed to in the epic. Bench may still be pending; the *number* itself does not move except by a new ADR. |
| `dependency_bound` | Target is reserved for refresh once measurement plumbing lands. The number is operator-provisional until then. |
| `bench_pending` | Source bench file does not yet exist; will land alongside the sub-bead that delivers the implementation. |
| `substrate_wired` | Source bench/test file exists and emits machine-readable measured/degraded evidence; production SLO proof still requires a retained target-run artifact. |

## SLO catalog

Every SLO has a stable id (`RQ-S*`), a target, the scenario it is measured
under, the bench/test that produces the metric, and the bead that owns
delivery of that bench. The `id` is the canonical reference used by the
machine-readable JSON, the README, and the attestation bundle.

| ID | Title | Target | Source bench / test | Owner bead | Status |
|---|---|---|---|---|---|
| RQ-S1 | Resize FPS | ≥60 sustained on 200-pane fleet, 5s gesture (p99 frame ≤16.6ms) | `crates/frankenterm-core/benches/resize_storm.rs` | ft-mpc9b.5.1 | bench_pending |
| RQ-S2 | Input-to-photon (macOS) | p95 < 16ms | `crates/frankenterm-gui/benches/renderer_slo/input_to_photon.rs` | ft-tf6g3.3.2 | substrate_wired |
| RQ-S3 | Input-to-photon (Wayland) | p95 < 20ms | `crates/frankenterm-gui/benches/renderer_slo/input_to_photon.rs` | ft-tf6g3.3.2 | substrate_wired |
| RQ-S4 | Visual artifacts (24h fuzz) | 0 critical from random resize+scroll+content | `tests/renderer_golden/fuzz` | ft-mpc9b.1.6 | bench_pending |
| RQ-S5 | Idle GPU usage | 0% sustained when no semantic change > 500ms | `crates/frankenterm-core/benches/idle_gpu.rs` | ft-mpc9b.5.1 | bench_pending |
| RQ-S6 | Heavy-burst input latency | p95 < 50ms with 1MB/s output across 50 panes | `crates/frankenterm-core/benches/heavy_burst.rs` | ft-mpc9b.5.1 | bench_pending |
| RQ-S7 | Battery drain (24h idle, M2) | ≤5% on a healthy battery | manual lab — `docs/perf/lab/battery_drain_24h.sh` | ft-mpc9b.5.1 | bench_pending |
| RQ-S8 | Frame skip rate (steady state) | ≥99% frames skipped on idle | `crates/frankenterm-core/benches/steady_state.rs` | ft-mpc9b.5.1 | bench_pending |
| RQ-S9 | Reflow latency | p95 < 5ms for 1000-line scrollback, 80→200 cols | `crates/frankenterm-core/benches/reflow.rs` | ft-mpc9b.1.2 | dependency_bound (wa-1u90p.1.3) |
| RQ-S10 | Atlas rebuild count | 0 on pure window-size resize | `crates/frankenterm-core/benches/atlas_stability.rs` | ft-mpc9b.1.1 | bench_pending |
| RQ-S11 | Snap-back delta (Draft → Standard) | SSIM ≥ 0.999 | `tests/renderer_golden/scenarios` | ft-mpc9b.2 | bench_pending |
| RQ-S12 | Floating-pane overhead | < 0.5ms additional per pane vs tiled | `crates/frankenterm-core/benches/compositor_layers.rs` | ft-mpc9b.4.1 | bench_pending |

The full machine-readable catalog (with scenarios, structured-log paths, and
`blocked_by` arrays) lives in `docs/perf/resize-quality-slo.json`. That file
is the single source of truth; this table is generated from it. If the two
disagree, the JSON wins.

## Structured-log contract

Every SLO bench writes JSON-line records that the attestation pipeline
ingests without further parsing. The path is per-SLO and listed in
`structured_log` in the JSON catalog.

**Per-iteration record** (one line per Criterion sample):

```json
{ "ts_ns": 1714560000000000000, "iteration": 1234, "value": 12_500_000, "unit": "ns" }
```

**Summary record** (one line, written at end of run):

```json
{ "ts_ns": 1714560360000000000, "p50": 9_800_000, "p95": 13_200_000, "p99": 17_400_000, "p999": 28_100_000, "sample_size": 10000, "ci_low": 9_500_000, "ci_high": 10_100_000 }
```

All values are integer nanoseconds unless `unit` says otherwise.

The input-to-photon substrate emits one `ft.perf.evidence-sample.v1` row to
`target/criterion/slo-input_to_photon_<platform>.jsonl` before Criterion starts
sampling. A missing GPU or photon detector is represented as an explicit
degraded state in the renderer SLO evidence, not as a passing measurement.
The operator surfaces for this substrate are `ft doctor --json`
`.renderer_slos.input_to_photon` and the read-only MCP resource
`wa://perf/renderer-slo/input_to_photon`.

## CI gate (deferred)

A per-PR regression gate is part of the bead's acceptance, but is **not**
shippable today: the gate needs the statistical-rigor primitive from
`BR-RC-FOUNDATION.G3.2` (Mann-Whitney U / Lai-Robbins SPRT), and at least
one SLO bench has to land first to validate the wiring. The design is
captured in the JSON's `ci_gate` block; the implementation lands once the
upstream blockers clear.

## Attestation publishing (deferred)

Per release we want a signed `perf/resize-quality-slo.<version>.json`
artifact in the attestation bundle. That depends on
`BR-RC-FOUNDATION.G3.1` (attestation graph schema + sigstore signing).
The design is captured in the JSON's `attestation_publishing` block.

## How to add or change an SLO

1. Edit `docs/perf/resize-quality-slo.json` (the source of truth).
2. Regenerate the table in this doc by hand (the doc is short enough that a
   regeneration script is not yet warranted; revisit if the catalog grows
   beyond ~20 rows).
3. Bump the entry in the `history` array of the JSON.
4. If the *number* changed (not just the bench wiring), open an ADR; the
   epic owners (currently `jemanuel`) sign off.

## Cross-references

- `docs/resize-performance-slos.md` — upstream-of-render budgets (wa-1u90p)
- `tests/renderer_golden/` — reference frames + SSIM harness (ft-mpc9b.1.6, ft-ombfl)
- `crates/frankenterm-core/benches/bench_common.rs` — Criterion budget /
  manifest helpers; new SLO benches use this for budget metadata.
- `BR-RC-FOUNDATION.G3.1` — attestation graph schema (deferred dep)
- `BR-RC-FOUNDATION.G3.2` — statistical rigor primitive (deferred dep)
- `BR-RC-FOUNDATION.G3.3` — headline-claim manifest (consumes RQ-S* numbers)
- `BR-RC-FOUNDATION.G3.5` — competitor matrix (consumes RQ-S* numbers)
- `BR-RC-DOCTRINE.G6` — README auto-stamp linking to this doc
