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
| .13 clustered-ASCII | 4.43× dense-ASCII render | env `FT_MOONSHOT_TERM_ASCII_CLUSTER_RUN_APPEND`, `screen.rs` | mixed-content non-regression + byte-equiv (green) | _pending_ |
| D1 printable-run batch | 1.47× | escape-parser/`performer.rs` | mixed-content non-regression + byte-equiv | _pending_ |
| EV1 bulk-ASCII row writer | 1.16× | env `FT_MOONSHOT_TERM_BULK_ASCII_ROW_WRITE`, `performer.rs` | mixed-content non-regression + byte-equiv | _pending_ |
| adaptive-M4 CDC (RSS) | 19× dedup ratio on redundant content | env `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP=adaptive`, `scrollback_tiers.rs:423` | deterministic fleet-resident-bytes RSS win + cheap probe | _pending_ |

---

_(KEEP-and-promote entries land above this line with a full same-run-window proof card. Flags that fail
to show a certifiable win stay shipped-but-default-off with a refreshed retry predicate in
round7-negative-results.md — zero-risk, no revert.)_
