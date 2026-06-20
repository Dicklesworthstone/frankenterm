# Round-6 Keep / Promotion Ledger

> The Alien Optimization Gauntlet (v0.9.0 campaign). Round-6 jobs: (1) **quantify** the 6 round-5 new
> default-OFF ideas (D1/D2/EV1-EV4) A/B-unmeasured at ship; (2) **promote** the proven algorithmic wins
> (Q1 32×, adaptive-M4 19×) into a recommended default set; (3) land **new BIG&BOLD profiled
> algorithmic/bandwidth ideas** (the one class that won). Discipline + 10 keep-gate rules + 8 retry forms:
> [`round4-negative-results.md`](round4-negative-results.md). Rejects/no-wins →
> [`round6-negative-results.md`](round6-negative-results.md). Campaign record →
> [`../../tests/artifacts/perf/v090-round6-campaign.md`](../../tests/artifacts/perf/v090-round6-campaign.md).

**Bench host:** local Apple-Silicon Mac under swarm load (operator choice). Certify ≥2× non-overlapping
wins only. Correctness proofs stay **RCH-remote / fail-closed**. Keep entry template:
[`round4-keep-ledger.md`](round4-keep-ledger.md).

## Carryover quantification status — the 6 round-5 ideas + 2 promotion candidates

| Idea | Gate | Path | A/B plan | Verdict |
|---|---|---|---|---|
| Q1 prefix-sum (PROMOTE) | config `scrollback.prefix_index` | scrollback_tiers.rs:1069 | deep-scroll (≥2× holds) + NEW shallow non-regression | _pending — promote default-on if shallow clean_ |
| M4 CDC dedup (ADAPTIVE) | config `scrollback.cdc_dedup` | scrollback_tiers.rs:423 | 19× holds + cheap redundancy probe auto-enable | _pending — adaptive, not static default-on_ |
| D1 parser printable-run batch | escape-parser gate | performer.rs / escape-parser | term/parser throughput A/B (TUI-dense) | _pending_ |
| D2 CSI/OSC dispatch table | setter/feature gate | performer.rs | CSI-heavy A/B | _pending_ |
| EV1 bulk-ASCII row writer | env `FT_MOONSHOT_TERM_BULK_ASCII_ROW_WRITE` | performer.rs:220 | pure-ASCII row-fill A/B | _pending (.18 proof-pending)_ |
| EV3 blocked/rank-select pages | env `FT_MOONSHOT_SCROLLBACK_BLOCKED_PAGE_INDEX` | scrollback_tiers.rs:236 | NEW single-line-from-cold vs full-page bench | _pending (.21 proof-pending)_ |
| EV4 set-based FTS batcher | env `FT_MOONSHOT_FTS_INSERT_SELECT_BATCH` | storage.rs:18840 | NEW deferred-FTS-sync throughput bench | _pending (.22 proof-pending)_ |

## Promotions / keeps (filled as A/B runs land)

_(KEEP-and-promote-to-default-on entries land here with the full same-run-window proof card. Flags that
fail to show a certifiable large-effect win stay shipped-but-default-off with a refreshed retry predicate
in round6-negative-results.md — zero-risk, no revert.)_
