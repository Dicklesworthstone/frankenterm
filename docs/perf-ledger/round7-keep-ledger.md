# Round-7 Keep / Promotion Ledger

> The Alien Optimization Gauntlet (v0.10.0 campaign). Round-7 jobs: (1) **CASH IN** the round-6
> certified-but-default-off wins → default-on (EV4 set-based FTS 6-14×; .13 clustered-ASCII 4.43× +
> D1 1.47× + EV1 1.16× term-render stack); (2) adjudicate the proof-deferred **adaptive-M4 CDC** as
> an **RSS** win via a deterministic fleet-resident-bytes harness; (3) profile-first new-axis mining
> (startup-WAL the only plausible new CPU win). Discipline + 10 keep-gate rules + 8 retry forms:
> [`round4-negative-results.md`](round4-negative-results.md). Rejects/no-wins →
> [`round7-negative-results.md`](round7-negative-results.md). Campaign record →
> [`../../tests/artifacts/perf/v100-round7-campaign.md`](../../tests/artifacts/perf/v100-round7-campaign.md).

**Bench host:** local Apple-Silicon Mac, swarm idled for bench windows (operator choice). Certify ≥2×
non-overlapping wins; use **deterministic harnesses** (A5 quality + new round-7 RSS harness) for
non-regression / RSS metrics that don't need a quiet host. Correctness proofs stay **RCH-remote /
fail-closed**. Keep entry template: [`round4-keep-ledger.md`](round4-keep-ledger.md).

**CRITICAL promotion guard:** each promoted flag gets its OWN default-`true` gate fn (mirror Q1's
`prefix_index_enabled_from_env` `.unwrap_or(true)`). NEVER flip a shared env-helper default
(`storage_env_flag_enabled`) — it would over-promote every flag that delegates to it.

## Promotion targets (filled as proofs land)

| Idea | Round-6 evidence | Gate | Promotion proof needed | Status |
|---|---|---|---|---|
| EV4 set-based FTS batcher | p95 6.0×, mean 9.12× (default-on candidate, "no common-case downside") | env `FT_MOONSHOT_FTS_INSERT_SELECT_BATCH`, `storage.rs` | small-batch non-regression + byte-equiv (green) | **CERTIFIED** (`ft-uvjfr` / `8318c5514`) |
| .13 clustered-ASCII | 4.43× dense-ASCII render | env `FT_MOONSHOT_TERM_ASCII_CLUSTER_RUN_APPEND`, `screen.rs` | mixed-content non-regression + byte-equiv (green) | promoted by `ft-97g96` |
| D1 printable-run batch | 1.47× | escape-parser/`performer.rs` | mixed-content non-regression + byte-equiv | promoted by `ft-97g96` |
| EV1 bulk-ASCII row writer | 1.16× | env `FT_MOONSHOT_TERM_BULK_ASCII_ROW_WRITE`, `performer.rs` | mixed-content non-regression + byte-equiv | promoted by `ft-97g96` |
| adaptive-M4 CDC (RSS) | 19× dedup ratio on redundant content | env `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP`, `scrollback_tiers.rs` | deterministic fleet-resident-bytes RSS win + cheap probe | **CERTIFIED** (`ft-ykde4`) |

---

### 2026-06-21 | ft-ykde4 / cod_1 | adaptive-M4 CDC RSS promotion — CERTIFIED

**Status:** kept and promoted — adaptive CDC is default-on via the cheap redundancy probe.

**Gate:** `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP` remains the safety valve. Unset defaults to the promoted adaptive probe; `adaptive`/`auto`/`probe` select it explicitly; `0`/`false`/`off` disables CDC and preserves the legacy standalone-zstd representation; `1`/`true`/`yes`/`on` forces always-on CDC for diagnostics.

**Profile attribution:** warm-tier resident bytes for redundant terminal redraws. The adaptive probe samples early pages, allocates the CDC store only when the redundancy ratio clears `CDC_ADAPTIVE_RATIO_THRESHOLD_X100=150`, and otherwise stays on standalone zstd.

**Measurement (deterministic RSS harness):** RCH-remote `vmi1227854`, `cargo test -p frankenterm-core --test round7_rss_harness -- --nocapture`. Redundant redraw fleet: off `27,869,200 B`, adaptive `5,533,600 B`, delta `-80.14%` (`WIN`), saving `22,335,600 B`; adaptive engaged `200/200` panes and deduped to `13` chunks. Low-redundancy fleet: off `65,616,200 B`, adaptive `65,616,200 B`, delta `+0.00%` (`TIE`), adaptive engaged `0/200` panes. Always-on CDC showed the expected low-redundancy regression: `72,869,600 B`, `+11.05%`.

**Behavior-preservation:** RCH-remote `vmi1227854`, `cargo test -p frankenterm-core --test proptest_scrollback_cdc_dedup`, `4 passed`. The reconstruction proof covers byte-identical warm-page decode, repeated-content savings, eviction/refcount accounting, and default construction remaining storage-lazy before the adaptive probe engages.

**A/B verdict:** promote; adaptive captures the large redundant-redraw RSS win while matching baseline exactly where always-on CDC regresses.

**Pattern applied:** eager per-page standalone compression -> adaptive redundancy probe + content-addressed chunk interning only for redundant warm tiers.

**Rollback:** set `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP=0`/`false`/`off`, or `git revert <ft-ykde4 commit>`.

### 2026-06-21 | ft-97g96 / cod_2 | Dense-ASCII term-render recommended-set promotion

**Status:** kept and promoted (term-render dense-ASCII stack, default-on after round-7 keep gate)

**Gate:** `FT_MOONSHOT_RECOMMENDED=0` disables the promoted recommended set. Each member keeps a dedicated default-on gate with its own falsey escape hatch: `ascii_cluster_run_append_enabled()` reads `FT_MOONSHOT_TERM_ASCII_CLUSTER_RUN_APPEND`; `default_print_batching()` reads `FT_MOONSHOT_PARSER_PRINT_BATCHING`; `Performer::bulk_ascii_row_write_enabled()` reads `FT_MOONSHOT_TERM_BULK_ASCII_ROW_WRITE`. `FT_MOONSHOT_PARSER_TABLE_DISPATCH` / D2 remains opt-in and was not promoted.

**Profile attribution:** dense printable rows spend time materializing one width-1 ASCII cell at a time through parser print actions, terminal row writes, and clustered-line append. The promoted stack batches the printable run, fills contiguous normal-attribute rows in bulk where safe, and appends clustered ASCII runs only when the dense-run predicate matches.

**Measurement (dense evidence from round-6):** `.13` clustered-ASCII run append `4.43x`; D1 printable-run batching `1.47x`; EV1 bulk-ASCII row writer `1.16x`.

**Measurement (mixed-content non-regression):** `scripts/round4-bench-ab.sh --local --package frankenterm-term --bench term_parser_ab --group term_parser_ab --id terminal/csi_osc_heavy --gate env:FT_MOONSHOT_RECOMMENDED=1/0`; baseline `805813.3 ns`, candidate `815113.7 ns`, delta `+1.15%` (`0.989x`), candidate CV `1.38%`, baseline CV `2.88%`, `p=0.0003028`. This is inside the +/-10% mixed-content non-regression band; the candidate path remains structurally gated on dense ASCII runs, so CSI/OSC and mixed content fall through to the scalar behavior.

**Behavior-preservation:** RCH-remote term oracle on `vmi1152480` passed `cargo test -p frankenterm-term parser_print_batching_ --lib` (`4 passed`, including parser terminal effects, EV1 row writer, .13 clustered-run append, and chunked effects). RCH-remote parser oracle on `vmi1152480` passed `cargo test -p frankenterm-escape-parser --test parser_print_batching_equivalence` (`6 passed`, including chunk-boundary and whole-buffer equivalence).

**A/B verdict:** promote the recommended set; dense evidence clears the keep gate and mixed content stays within the non-regression guard.

**Pattern applied:** scalar per-byte terminal materialization -> predicate-gated printable-run batching + row/run bulk materialization.

**Rollback:** set `FT_MOONSHOT_RECOMMENDED=0` for the set, set an individual promoted flag to `0`/`false`/`off` for one member, or `git revert <ft-97g96 commit>`.

### 2026-06-21 | ft-uvjfr / cod_3 | EV4 set-based deferred-FTS INSERT-SELECT promotion — CERTIFIED

**Status:** kept and promoted — **CERTIFIED default-on** (durable storage throughput optimization). Code
landed in `8318c5514` (storage.rs gate flip + `round7_fts_promote.rs` oracle + Cargo.toml test entry).

**Gate-guard (CRITICAL) — code-verified at HEAD `8318c5514`:** the gate is the dedicated
`fts_insert_select_batch_enabled_from_env()` with its OWN `.unwrap_or(true)` (default-on when unset); it does
**NOT** route through the shared `storage_env_flag_enabled()`, which remains `.unwrap_or(false)` (serving only
the 3 group-commit flags). `env_value_is_truthy` matches only `1|true|yes|on`, so
`FT_MOONSHOT_FTS_INSERT_SELECT_BATCH=0`/`false`/`off` (or empty) returns false → **disables** the batcher. PASS.

**Gate:** env `FT_MOONSHOT_FTS_INSERT_SELECT_BATCH` remains the safety valve; unset defaults on through dedicated `fts_insert_select_batch_enabled_from_env()` and `0`/`false`/`off` disables. Shared `storage_env_flag_enabled()` remains default-off for other storage moonshots.

**Profile attribution:** deferred FTS sync insert path; set-based `INSERT INTO output_segments_fts(rowid, content) SELECT id, content ... ORDER BY seq LIMIT N` removes per-segment Rust/SQLite round trips.

**Measurement (focused):** `frankenterm-core::deferred_fts_sync::deferred_fts_sync/env_gate/4096` mean `206.812 ms` -> `22.675 ms` (`-89.04%`, `9.121x`); p50 `209.376 ms` -> `14.827 ms` (`14.121x`); p95 `311.041 ms` -> `51.761 ms` (`6.009x`). Candidate CV was noisy in the local wrapper (`66.14%`), but the same-run local A/B clears the 2x promotion target and EV4 correctness was RCH-green.

**Measurement (broad):** N/A for promotion; this is a default flip of the already-kept EV4 storage path.

**Behavior-preservation:** byte-equivalent FTS index content, two-source proof. (1) Round-6 RCH-green:
the env-gated `insert_select_batch` lib test (dc01bd950 bench-arm era, hz1) proved set-based
`INSERT…SELECT` == per-segment inserts. (2) Committed oracle `round7_fts_promote.rs` (`8318c5514`,
`round7_fts_promote_default_on_matches_disabled_per_row_oracle_small_batch`): unset-default EV4 vs
`FT_MOONSHOT_FTS_INSERT_SELECT_BATCH=0` per-row oracle on small batches — sync shape structurally identical
(`segments_indexed=6`, `panes_processed=2`, second sync no-op `segments_indexed=0`), and FTS search
projections byte-equivalent across all/pane/zone/pane+zone+time filters (4 searches).

**A/B verdict:** promote; throughput win exceeds 2× (round-6 p95 6.0× / mean 9.12×) and the per-flag safety valve remains available.

**Pattern applied:** N+1 insert loop -> set-based SQLite batch insert.

**Rollback:** `git revert 8318c5514` or set `FT_MOONSHOT_FTS_INSERT_SELECT_BATCH=0`/`false`/`off`.

_(KEEP-and-promote entries land above this line with a full same-run-window proof card. Flags that fail
to show a certifiable win stay shipped-but-default-off with a refreshed retry predicate in
round7-negative-results.md — zero-risk, no revert.)_
