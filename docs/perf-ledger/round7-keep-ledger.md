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
| EV4 set-based FTS batcher | p95 6.0×, mean 9.12× (default-on candidate, "no common-case downside") | env `FT_MOONSHOT_FTS_INSERT_SELECT_BATCH`, `storage.rs` | small-batch non-regression + byte-equiv (green) | _pending_ |
| .13 clustered-ASCII | 4.43× dense-ASCII render | env `FT_MOONSHOT_TERM_ASCII_CLUSTER_RUN_APPEND`, `screen.rs` | mixed-content non-regression + byte-equiv (green) | promoted by `ft-97g96` |
| D1 printable-run batch | 1.47× | escape-parser/`performer.rs` | mixed-content non-regression + byte-equiv | promoted by `ft-97g96` |
| EV1 bulk-ASCII row writer | 1.16× | env `FT_MOONSHOT_TERM_BULK_ASCII_ROW_WRITE`, `performer.rs` | mixed-content non-regression + byte-equiv | promoted by `ft-97g96` |
| adaptive-M4 CDC (RSS) | 19× dedup ratio on redundant content | env `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP=adaptive`, `scrollback_tiers.rs:423` | deterministic fleet-resident-bytes RSS win + cheap probe | _pending_ |

---

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

_(KEEP-and-promote entries land above this line with a full same-run-window proof card. Flags that fail
to show a certifiable win stay shipped-but-default-off with a refreshed retry predicate in
round7-negative-results.md — zero-risk, no revert.)_
