# v0.10.0 — Round-7 Alien Optimization Gauntlet Campaign Record

> NTM 8-pane swarm, autonomous orchestrator. Resumes the radical-innovation perf campaign from v0.9.0
> (`055bca9b0`). Epic `ft-yjihu`. Ledgers: `docs/perf-ledger/round7-{keep-ledger,negative-results,profile-targets}.md`.

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
| ft-uvjfr | cod_3 | EV4 set-based FTS → default-on | promotion | **HELD** — code uncommitted (gate flip + `round7_fts_promote.rs` in working tree only) |
| ft-97g96 | cod_2 | .13/D1/EV1 term-render → default-on | promotion | **CERTIFIED** (`5c2d995eb`) |
| ft-ykde4 | cod_1 | EV3 liveness + adaptive-M4 RSS | memory axis | EV3 **refuted-on-liveness** (`cc03d97f8`); adaptive-M4 RSS pending |
| ft-6aban | cc_2 | deterministic fleet-resident-bytes RSS harness | infra | **landed** (`21a1d0b6f`) |
| ft-mcz7t | cod_4 | startup-WAL / EventBus / BOCPD profile sweep | new-axis | in_progress |
| ft-8cpho | cod_5 | scan_pipeline removal + ft-ui1xn A/B | hygiene | ft-ui1xn A/B **blocked** RCH-E410 (`df794dca3`) |
| ft-rof2k | orch | cut v0.10.0 | release | pending |

cc_1 = ledger steward; cc_3 = byte-equivalence / correctness reviewer.

## Scorecard

_Adjudicated by cc_1 (ledger steward) against the 10 keep-gate rules + 8 retry forms (round4-negative-results.md)._
_Tend #1 — 4 sibling commits: `5c2d995eb` term-render, `cc03d97f8` EV3, `df794dca3` scan-pipeline, `21a1d0b6f` RSS harness._

### Certified promotions → default-on

| Idea | Bead / commit | Win (round-6 dense A/B) | Mixed-content non-regression | Byte-equiv | Own default-true gate | Verdict |
|---|---|---|---|---|---|---|
| Dense-ASCII term stack: .13 cluster-run + D1 printable-run + EV1 bulk-row | ft-97g96 / `5c2d995eb` | .13 **4.43×**, D1 **1.47×**, EV1 **1.16×** | `terminal/csi_osc_heavy` +1.15% (cand CV 1.38%, base CV 2.88%, p=3e-4) — inside −3% primary / −10% per-category ratchet | RCH-green `vmi1152480`: `parser_print_batching_` lib (4 passed) + `parser_print_batching_equivalence` (6 passed) | `ascii_cluster_run_append_enabled` · `default_print_batching` · `bulk_ascii_row_write_enabled` — each its own default-true fn + own per-flag falsey env; set-wide `FT_MOONSHOT_RECOMMENDED` off-switch; shared `storage_env_flag_enabled` untouched | **CERTIFIED ✅** |

### Held (cannot certify from an uncommitted tree)

| Idea | Bead | Why held | Unblock |
|---|---|---|---|
| EV4 set-based FTS INSERT…SELECT | ft-uvjfr / cod_3 | default-on flip + `round7_fts_promote.rs` oracle live only in cod_3's working tree; HEAD `storage.rs` bench-only (`3bf7b0630`); card in no commit | cod_3 commits the gate flip + oracle, then RCH-green byte-equiv. (Same unlanded test is RCH-E410-blocking ft-ui1xn.) |

### Refuted / blocked (negative evidence = a win)

| Idea | Bead / commit | Status | Retry form |
|---|---|---|---|
| EV3 blocked/rank-select single-line scrollback decode | ft-ykde4 / `cc03d97f8` | **refuted-on-liveness** — no non-test prod caller of `warm_line`/`cold_line`/`decode_page_line`; prod page reads go `warm_page_lines`→`decode_page` | **Form 1** (profile attribution above noise on a prod deep-scroll / cold-readback workload) |
| ft-ui1xn quick_reject Bloom vs AC-direct A/B | ft-8cpho / `df794dca3` | **blocked (RCH-E410)** — Cargo dep-closure can't find uncommitted `round7_fts_promote.rs`; no timing verdict; `quick_reject` stays default-on | **Form 8** (blocked until ft-uvjfr lands/removes the test entry; track ft-uvjfr) |

### Infra landed

| Item | Bead / commit | Note |
|---|---|---|
| Deterministic fleet-resident-bytes RSS harness | ft-6aban / `21a1d0b6f` | `crates/frankenterm-core/tests/round7_rss_harness.rs` (496 lines) — substrate for the adaptive-M4 CDC RSS adjudication (verdict still _pending_ on cod_1) |

### CRITICAL gate-guard audit — PASS
Code-level verification of `5c2d995eb`: every promoted term-render flag reads its OWN default-true gate fn
(`ascii_cluster_run_append_enabled` / `bulk_ascii_row_write_enabled` via own `LazyLock`; `default_print_batching`
via own fn) with its OWN per-flag falsey env override; **none** delegates to the shared `storage_env_flag_enabled`
(still `.unwrap_or(false)`, serving only the 3 group-commit flags). The round-7 `FT_MOONSHOT_RECOMMENDED` is a
NEW set-wide *disable* hatch scoped to the 3 recommended flags — not a default-flipped shared *enable* helper.
EV4's working-tree gate (`fts_insert_select_batch_enabled_from_env`) is also a dedicated fn — will re-verify on commit.

### Open
- **adaptive-M4 CDC (RSS)** — RSS harness landed (ft-6aban); awaiting cod_1 deterministic resident-bytes win + cheap redundancy probe.
- **EV4** — held; flip to CERTIFIED once cod_3's code + RCH-green proof land (also unblocks ft-ui1xn).
- **new-axis (cod_4, ft-mcz7t)** — startup-WAL profile pending the ≥0.5% attribution + verified-prod-caller gate.
- **v0.10.0 cut (ft-rof2k)** — pending convergence.
