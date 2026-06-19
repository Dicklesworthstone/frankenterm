# Round-4 Keep Ledger

> The Alien Optimization Gauntlet (v0.7.0 campaign). Every *kept* optimization gets an entry here with its
> same-run-window A/B proof, behavior-preservation proof, and rollback recipe. Rejected/reverted candidates →
> [`round4-negative-results.md`](round4-negative-results.md). Keep-gate rules + retry vocabulary documented there.

Campaign record: [`../../tests/artifacts/perf/v070-round4-campaign.md`](../../tests/artifacts/perf/v070-round4-campaign.md).

---

## Keep entry template (copy per kept change)

```markdown
### <YYYY-MM-DD> | <bead_id> | <Title>

**Status:** kept (durable optimization | durable infra | structural)

**Gate:** <feature/env/config flag> + default state (off until proven; promotion note if flipped on)

**Profile attribution:** "Closed <X>% <Frame> self-time" — flamegraph: <path>

**Measurement (focused):** <bench> <metric> = <before> → <after> (<delta%>, <speedup>); cv_pct=<X> (≤5)

**Measurement (broad):** primary_score <before> → <after> (<delta%>); per-category deltas: <...>
  - Same run window: git=<sha>, target=<dir>, worker=<rch host>, ts=<ISO-8601>

**Behavior-preservation:** "<test summary>; byte-identical golden/property/oracle between baseline and candidate."

**A/B verdict:** SPRT=accept samples=<n>; conformal=within band (all of p50/p95/p99/p999)

**Pattern applied:** <succinct/RLE | branchless DFA | seqlock prefix-sum | group-commit | SIMD prefilter | ...>

**Rollback:** `git revert <sha>` | flag default-off | env safety valve <VAR>
```

---

## Round-3 backfill (quantify the 8 shipped-but-unmeasured moonshots from v0.6.1)

The v0.6.1 campaign kept 8 moonshots correctness-proven but mostly UNMEASURED. Phase 0 re-benches each on a
clean host through the new bench-AB harness and records the quantified delta below (or demotes/reverts if a
clean A/B shows no real win). One clean number existed at ship: SWAR ft-p8vls −2.5% p50 ASCII.

| Moonshot | Bead | Gate | Quantified delta | Verdict |
|---|---|---|---|---|
| SWAR VTE printable scan | ft-p8vls | `bench-scalar-vte-scan` A/B | _pending Phase 0 re-bench_ | _pending_ |
| Reflow chunks + Arc SharedLines | ft-osyaf | default-active | _pending_ | _pending_ |
| Wrap-point cache | ft-3vdce | default-active | _pending_ | _pending_ |
| SoA glyph quads | ft-3r0yk | `FT_MOONSHOT_INSTANCED_GLYPH_QUADS` | _pending_ | _pending_ |
| Glyph-run interning | ft-egok5 | default-on (`FT_DISABLE_GLYPH_RUN_INTERNING`) | _pending_ | _pending_ |
| CDC dedup (codec) | ft-6c1t0 | opt-in | _pending_ | _pending_ |
| Disruptor SPSC ring | ft-87qfi | `disruptor-pane-io` | _pending_ | _pending_ |
| Succinct RLE cell attrs | ft-dkfiy | `succinct_attrs` | _pending_ (+ add byte-equiv test) | _pending_ |

---

## Entries

> Provisional-keep policy (noisy shared RCH workers, ~11min builds): correctness is rigorously
> RCH-proven per idea and every change is **default-off**, so enabling is safe and shipping is zero-risk.
> A/B *quantification* (for any default-on promotion) is tracked here with a grep-able retry predicate and
> run in consolidated batches; an idea is only reverted if A/B shows a regression on a default-on path.

### 2026-06-19 | Q3 / cc_3 | Linear KMP overlap match in ingest.extract_delta

**Status:** kept provisional (durable optimization, default-off; A/B quant pending)
**Gate:** config `ingest.delta_linear_overlap` (default false)
**Commit:** 2bebc40d0
**Behavior-preservation:** KMP single-pass overlap == quadratic-overlap `DeltaResult` (incl reason strings) equivalence proptest over random + box-drawing + emoji-boundary fuzz; pane-reported RCH-pass.
**A/B verdict:** DEFERRED. Retry-condition (Form 5): do not retry from a cold read; use a quiet-host env-toggle `delta_extraction` bench (build-once back-to-back, common-first-byte / box-drawing workload where the O(n^2) candidate loop dominates) instead.
**Baseline comparator:** nested memchr + per-candidate slice compare (O(n^2) worst case).
**Rollback:** flag default-off; `git revert 2bebc40d0`.

### 2026-06-19 | ft-perf-gate driver / cod_3 | Round-4 perf-gate driver (SPRT + conformal)

**Status:** kept (durable infra) — Form 6 structural-not-numerical
**Gate:** `FT_PERF_GATE_MODE={fixed|sprt|anytime}` (default fixed = bit-identical) + `FT_PERF_GATE_BANDS={fixed|conformal}` (default fixed)
**Commit:** a819e28b0
**Behavior-preservation:** default `fixed` bit-identical to legacy gate; conformal clamped `min(band, baseline*1.10)` (monotone-tighter, only ever adds rejections); OC-curve (false-reject<=alpha, false-accept<=beta) + flat-median/inflated-p999 unit tests; pane-reported RCH-pass.
**A/B verdict:** N/A — harness infra, not a runtime perf change.
**Rollback:** default `fixed` mode; `git revert a819e28b0`.

### 2026-06-19 | M2 / cod_5 | Succinct RLE cell attrs + ft-dkfiy byte-equiv gap closed

**Status:** kept provisional (durable optimization, default-off; A/B quant pending)
**Gate:** feature `succinct_attrs` (default off)
**Commit:** 2e2f729dc (termwiz scrollback equivalence gate) atop the round-3 succinct scaffold
**Behavior-preservation:** per-column `attr(col)` == AoS byte-equivalence — the previously-MISSING default-vs-succinct test is now added, closing the ft-dkfiy gap (termwiz no longer runs fewer tests under the feature); RCH remote hz2 PASS.
**A/B verdict:** DEFERRED. Retry-condition (Form 7): retry only if a warm/cold scrollback memory bench shows attr-store RSS or cache-miss share above noise on a deep-scrollback workload.
**Baseline comparator:** AoS `Vec<CellAttributes>` per cell.
**Rollback:** feature default-off; `git revert 2e2f729dc`.

### 2026-06-19 | M1 / cod_4 | Branchless ANSI DFA table (build.rs-generated)

**Status:** kept provisional (durable optimization, default-off; A/B quant pending)
**Gate:** feature `ansi-dfa-table` (default off)
**Commit:** 6d2d02f7e
**Behavior-preservation:** build.rs-generated flat transition+action table is provably equal to the existing `ansi_state_step` FSM — exhaustive (state,byte) equivalence test + chunk fuzz byte-equal counts; pane-reported RCH-pass ("ansi-dfa-table ansi_dfa passed").
**A/B verdict:** DEFERRED. Retry-condition (Form 1): retry only if a profiler attributes a clearly-above-noise share to the scalar `ansi_state_step` loop on an ANSI-dense workload (TUI/vim capture); current scan benches may not isolate the branch-mispredict cost.
**Baseline comparator:** per-byte scalar match-based `ansi_state_step` FSM.
**Rollback:** feature default-off; `git revert 6d2d02f7e`.

### 2026-06-19 | Q1 / cc_1 | Seqlock warm-tier prefix-sum (scrollback locate O(pages)->O(log))

**Status:** kept provisional (durable optimization, default-off; A/B quant pending)
**Gate:** config `scrollback.prefix_index` (default false)
**Commit:** 6710e80f9
**Behavior-preservation:** indexed == linear `ScrollbackLocationHint` equivalence proptest over random push/evict + 10k offsets (`proptest_scrollback_prefix_index.rs`); pane-reported RCH-pass.
**A/B verdict:** DEFERRED. Retry-condition (Form 1): retry only if a profiler attributes a clearly-above-noise share to `locate_offset`/`tier_for_offset` on a deep-scrollback interactive-scroll workload (the O(pages) re-sum dominates only with hundreds of warm pages).
**Baseline comparator:** per-call `warm.iter().sum()` + reverse linear page walk.
**Rollback:** flag default-off; `git revert 6710e80f9`.
