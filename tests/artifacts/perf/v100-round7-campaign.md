# v0.10.0 — Round-7 Alien Optimization Gauntlet Campaign Record — FINAL

> **v0.10.0 SHIPPED** — 3 platforms + checksums:
> https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.10.0 (tag `5baf072f9`).
> NTM 8-pane swarm, autonomous orchestrator. Resumed the radical-innovation perf campaign from v0.9.0
> (`055bca9b0`). Epic `ft-yjihu`. Ledgers: `docs/perf-ledger/round7-{keep-ledger,negative-results,profile-targets}.md`.
> **This is the final round-7 scorecard.**

## Charter (operator-locked)

- **Scope:** full BIG&BOLD mining + cash-in promotions.
- **Cadence:** autonomous to release.
- **End state:** cut v0.10.0.
- **Bench host:** local Mac (swarm idled for ≥2× certs) + deterministic harnesses (A5 quality + new RSS).

## Up-front corrections (foreclosed the kickoff's two headline structural targets)

1. **Redactor single-pass RegexSet — already shipped** (`redactor.rs:690` `SECRET_PATTERN_SET`). 22% is the
   irreducible cost of an already-optimal scan. Dropped (round6-profile-targets.md:83-87 says do-not-reopen).
2. **Scrollback `warm_line` (EV3 target) — liveness-suspect** (only test/bench callers; prod uses full-page
   `decode_page`). adaptive-M4 CDC (live full-page path + RSS angle) is the higher-EV scrollback lever.
3. **The well is drying** — highest-confidence value is cashing in certified wins + adjudicating
   adaptive-M4 as an RSS win; new-axis mining expected to yield mostly negative-evidence (the moat).

## Workstreams (epic ft-yjihu)

| Bead | Pane | Workstream | Class | Status |
|---|---|---|---|---|
| ft-uvjfr | cod_3 | EV4 set-based FTS → default-on | promotion | **CERTIFIED** (`8318c5514`) |
| ft-97g96 | cod_2 | .13/D1/EV1 term-render → default-on | promotion | **CERTIFIED** (`5c2d995eb`) |
| ft-ykde4 | cod_1 | EV3 liveness + adaptive-M4 RSS | memory axis | EV3 **refuted-on-liveness** (`cc03d97f8`); adaptive-M4 **CERTIFIED** (`557982cb7`, ships next release) |
| ft-6aban | cc_2 | deterministic fleet-resident-bytes RSS harness | infra | **landed** (`21a1d0b6f`) |
| ft-mcz7t | cod_4 | startup-WAL / EventBus / BOCPD profile sweep | new-axis | WAL-recovery **deferred to round-8** (`ft-yjihu.1`) |
| ft-8cpho | cod_5 | scan_pipeline removal + ft-ui1xn A/B | hygiene | scan_pipeline deletion **deferred (operator)**; ft-ui1xn A/B Form-8 (dep landed) |
| ft-rof2k | orch | cut v0.10.0 | release | **SHIPPED** (`v0.10.0` / `5baf072f9`, 3 platforms + checksums) |

cc_1 = ledger steward; cc_3 = byte-equivalence / correctness reviewer.

## Scorecard

_Adjudicated by cc_1 (ledger steward) against the 10 keep-gate rules + 8 retry forms (round4-negative-results.md)._
_Tend #1 — 4 commits: `5c2d995eb` term-render, `cc03d97f8` EV3, `df794dca3` scan-pipeline, `21a1d0b6f` RSS harness._
_Tend #2 — EV4 committed (`8318c5514`): HELD→CERTIFIED._
_Tend #3 (FINAL) — adaptive-M4 committed (`557982cb7`): CERTIFIED. **3 promotions certified** (term-render, EV4, adaptive-M4). v0.10.0 SHIPPED._

### Certified promotions → default-on

| Idea | Bead / commit | Win (round-6 A/B) | Non-regression | Byte-equiv | Own default-true gate | Verdict |
|---|---|---|---|---|---|---|
| Dense-ASCII term stack: .13 cluster-run + D1 printable-run + EV1 bulk-row | ft-97g96 / `5c2d995eb` | .13 **4.43×**, D1 **1.47×**, EV1 **1.16×** | `terminal/csi_osc_heavy` +1.15% (cand CV 1.38%, base CV 2.88%, p=3e-4) — inside −3% primary / −10% per-category ratchet | RCH-green `vmi1152480`: `parser_print_batching_` lib (4 passed) + `parser_print_batching_equivalence` (6 passed) | `ascii_cluster_run_append_enabled` · `default_print_batching` · `bulk_ascii_row_write_enabled` — each its own default-true fn + own per-flag falsey env; set-wide `FT_MOONSHOT_RECOMMENDED` off-switch; shared `storage_env_flag_enabled` untouched | **CERTIFIED ✅** |
| EV4 set-based FTS INSERT…SELECT batcher | ft-uvjfr / `8318c5514` | p95 **6.0×** / mean **9.12×** / p50 14.12× (`deferred_fts_sync`, cand CV 66% — large-effect, ≥6× every percentile) | N/A (background catch-up sync, same op batched) | round-6 RCH-green `insert_select_batch` (hz1) **+** committed oracle `round7_fts_promote.rs` (byte-equiv across all/pane/zone/time projections, sync-shape parity) | dedicated `fts_insert_select_batch_enabled_from_env` w/ own `.unwrap_or(true)`; `=0/false/off` disables (`env_value_is_truthy`); shared `storage_env_flag_enabled` untouched (`.unwrap_or(false)`) | **CERTIFIED ✅** |
| adaptive-M4 CDC scrollback dedup (RSS) ⚠ **ships next release** | ft-ykde4 / `557982cb7` | **−80.14% fleet RSS** on redundant scrollback (27.87 MB → 5.53 MB @200 panes; probe engaged 200/200, deduped to 13 chunks) — deterministic RSS harness `vmi1227854` | low-redundancy **+0.00% TIE** (probe declined 200/200); always-on CDC would regress **+11.05%** — the adaptive probe is exactly what avoids that | RCH-green `vmi1227854`: `proptest_scrollback_cdc_dedup` (4 passed) — byte-identical warm-page decode + refcount/eviction accounting | dedicated `cdc_dedup_mode_from_env` w/ own `.unwrap_or(CdcDedupMode::Adaptive)`; `=0/false/off`→`Off`; not a shared helper | **CERTIFIED ✅** |

> **adaptive-M4 release timing:** committed `557982cb7` @ 03:01:42, **~12 min after** the v0.10.0 tag
> `5baf072f9` @ 02:49:23 (`git tag --contains 557982cb7` does not list v0.10.0). It therefore **ships in the
> next release**, not v0.10.0. **v0.10.0 keeps CDC dedup default-off** — accurate, since the default-on flip
> is not in the tagged tree. Term-render + EV4 promotions DID make the v0.10.0 cut.

### Refuted / blocked (negative evidence = a win)

| Idea | Bead / commit | Status | Retry form |
|---|---|---|---|
| EV3 blocked/rank-select single-line scrollback decode | ft-ykde4 / `cc03d97f8` | **refuted-on-liveness** — no non-test prod caller of `warm_line`/`cold_line`/`decode_page_line`; prod page reads go `warm_page_lines`→`decode_page` | **Form 1** (profile attribution above noise on a prod deep-scroll / cold-readback workload) |
| ft-ui1xn quick_reject Bloom vs AC-direct A/B | ft-8cpho / `df794dca3` | **blocked (RCH-E410)** — Cargo dep-closure couldn't find uncommitted `round7_fts_promote.rs`; no timing verdict; `quick_reject` stays default-on. **Form-8 dep now LANDED** (`8318c5514` committed the test) → cod_5 re-run unblocked | **Form 8** (was blocked on ft-uvjfr; dep satisfied — A/B re-runnable) |

### Infra landed

| Item | Bead / commit | Note |
|---|---|---|
| Deterministic fleet-resident-bytes RSS harness | ft-6aban / `21a1d0b6f` | `crates/frankenterm-core/tests/round7_rss_harness.rs` (496 lines) — **delivered** the adaptive-M4 CDC RSS cert (off/adaptive/always-on three-arm verdict on `vmi1227854`) |

### CRITICAL gate-guard audit — PASS (all 3 promotions, code-verified at HEAD)
- **Term-render (`5c2d995eb`):** every promoted flag reads its OWN default-true gate fn
  (`ascii_cluster_run_append_enabled` / `bulk_ascii_row_write_enabled` via own `LazyLock`; `default_print_batching`
  via own fn) with its OWN per-flag falsey env. `FT_MOONSHOT_RECOMMENDED` is a NEW set-wide *disable* hatch
  scoped to the 3 flags — not a default-flipped shared *enable* helper.
- **EV4 (`8318c5514`):** gate is the dedicated `fts_insert_select_batch_enabled_from_env` with its OWN
  `.unwrap_or(true)`; `=0/false/off` disables (`env_value_is_truthy` matches only `1|true|yes|on`).
- **adaptive-M4 (`557982cb7`):** gate is the dedicated `cdc_dedup_mode_from_env` with its OWN
  `.unwrap_or(CdcDedupMode::Adaptive)`; `=0/false/off`→`Off`, `adaptive/auto/probe`→`Adaptive`, truthy→`Always`.
- **Shared `storage_env_flag_enabled` confirmed UNTOUCHED** at HEAD (`.unwrap_or(false)`, serving only the 3
  group-commit flags) — no over-promotion. PASS for all three.

### Final disposition
- **Release:** v0.10.0 SHIPPED (`5baf072f9`, 3 platforms + checksums). Carries term-render + EV4 promotions default-on; CDC dedup default-off (adaptive-M4 landed post-tag).
- **3 promotions CERTIFIED:** dense-ASCII term-render stack (.13/D1/EV1), EV4 set-based FTS, adaptive-M4 CDC RSS (next release).
- **1 idea refuted:** EV3 single-line scrollback decode — refuted-on-liveness (Form 1).
- **Deferred:**
  - WAL-recovery new-axis → **round-8, `ft-yjihu.1`**.
  - `scan_pipeline` deletion → **deferred (operator decision)**; the dead-code A/B `ft-ui1xn` stays Form-8 (dep `round7_fts_promote.rs` now committed → re-runnable, not blocking).
- **Infra retained:** deterministic fleet-RSS harness (`round7_rss_harness.rs`) for future memory-axis adjudication.
