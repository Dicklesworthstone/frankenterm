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
| Q1 prefix-sum (PROMOTE) | config `scrollback.prefix_index` | scrollback_tiers.rs:1069 | deep-scroll (≥2× holds) + NEW shallow non-regression | **PROMOTED default-on** — deep 20.18×, shallow +0.72% non-reg (card below) |
| M4 CDC dedup (ADAPTIVE) | config `scrollback.cdc_dedup` | scrollback_tiers.rs:423 | 19× holds + cheap redundancy probe auto-enable | _pending — adaptive, not static default-on_ |
| D1 parser printable-run batch | escape-parser gate | performer.rs / escape-parser | term/parser throughput A/B (TUI-dense) | _pending_ |
| D2 CSI/OSC dispatch table | setter/feature gate | performer.rs | CSI-heavy A/B | _pending_ |
| EV1 bulk-ASCII row writer | env `FT_MOONSHOT_TERM_BULK_ASCII_ROW_WRITE` | performer.rs:220 | pure-ASCII row-fill A/B | _pending (.18 proof-pending)_ |
| EV3 blocked/rank-select pages | env `FT_MOONSHOT_SCROLLBACK_BLOCKED_PAGE_INDEX` | scrollback_tiers.rs:236 | NEW single-line-from-cold vs full-page bench | _pending (.21 proof-pending)_ |
| EV4 set-based FTS batcher | env `FT_MOONSHOT_FTS_INSERT_SELECT_BATCH` | storage.rs:18840 | NEW deferred-FTS-sync throughput bench | _pending (.22 proof-pending)_ |

## Promotions / keeps (filled as A/B runs land)

### 2026-06-20 | ft-p4vzl.5 | Q1 seqlock scrollback prefix-sum — PROMOTED default-on

**Status:** PROMOTED to default-on (durable algorithmic win — complexity-class O(pages)→O(log)).

**Gate:** config `scrollback.prefix_index` — default flipped false→true (env `FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX`
retained as an override). Code flip owned by cod_1 (scrollback_tiers.rs).

**Measurement (focused, same-run-window, local Mac under swarm load, build-once env A/B):**
- `scrollback_prefix_index/deep_scroll_locate_offset`: 1 616 198 ns → 80 095 ns = **−95.04%, 20.18×**;
  candidate cv 1.43% / baseline cv 2.73% (both ≤5 → cv-gate now satisfied, unlike round-5's 15-20%);
  p_value_mw=0. KEEP.
- `scrollback_prefix_index/shallow_hot_locate_offset` (common case, hot-tier-only, warm/cold=0 asserted):
  10 384 ns → 10 459 ns = **+0.72% (0.993×)**; cv 2.70%; far inside the −3% primary ratchet → **NON-REGRESSED**.

**Behavior-preservation:** indexed == linear `ScrollbackLocationHint` equivalence proptest (round-5
`proptest_scrollback_prefix_index.rs`); observable behavior identical — only internal maintenance differs.

**A/B verdict:** deep KEEP (20.18× ≥2×, non-overlapping, cv≤5); shallow non-regression (+0.72% < 3% ratchet).
Promotion gate satisfied. The 20× (vs round-5's 32.5×) reflects host/tree variance; both unambiguously ≥2×.

**Pattern applied:** seqlock prefix-sum (incremental cumulative line-count + binary-search locate).

**Rollback:** set config default back to false / `git revert` the default-flip commit; env override remains.

**B0 realism note (round6-profile-targets.md):** `locate_offset` is only **0.006%** of realistic fleet
self-time — the realistic deep-scroll cost is the zstd page **decode** (`warm_line`, 5.18%), not the
locate. Q1's 20-32× is a **tail-latency win at extreme scroll depth**, not a fleet-CPU win. Kept default-on
because it is byte-equivalent and the shallow common-case cost is negligible (+0.72% of a 0.006% frame),
but its realistic ceiling is small. The higher-EV deep-scroll levers target `warm_line` decode:
**EV3** (blocked single-line decode) and **M4** (CDC shared-chunk reconstruction).

### 2026-06-20 | ft-p4vzl.8 / lw0s7.22 | EV4 set-based FTS catch-up batcher — KEEP (large-effect; default-on candidate)

**Status:** KEEP (durable throughput/bandwidth win, default-off). Certifiable large-effect despite high cv.

**Gate:** env `FT_MOONSHOT_FTS_INSERT_SELECT_BATCH`. cod_3 added an env-gated same-ID bench arm
(dc01bd950) for a cleaner low-cv re-run.

**Measurement (local Mac, `deferred_fts_sync`, INSERT…SELECT batch vs per-segment insert):** throughput
180.6 Kelem/s ON; **speedup mean 9.12×, p50 14.12×, p95 6.01×.** Candidate cv 66.14% (FTS insert
cold/warm variance) so the driver auto-REJECTed on cv rule 8 — **but the win is ≥6× at every percentile
(p95 6.01×), non-overlapping, far beyond the 2× certifiable bar.** This is the large-effect case the cv
rule does not veto (same class as round-5 Q1/M4).

**Behavior-preservation:** byte-equiv FTS index content (set-based INSERT…SELECT == per-segment inserts);
cod_3 oracle + RCH-green `insert_select_batch` lib test on hz1.

**A/B verdict:** KEEP (≥6× at p95). **Default-on candidate** — the deferred-FTS catch-up is a background
sync with no common-case downside (same operation, batched). Promote pending a small-batch non-regression
check + a lower-cv re-run on dc01bd950's clean bench.

**Pattern applied:** set-based bulk SQL (INSERT…SELECT) replacing per-row roundtrips.

**Rollback:** env default-off; `git revert`.

---

_(Further KEEP-and-promote entries land above this line with a full same-run-window proof card. Flags that
fail to show a certifiable large-effect win stay shipped-but-default-off with a refreshed retry predicate
in round6-negative-results.md — zero-risk, no revert.)_
