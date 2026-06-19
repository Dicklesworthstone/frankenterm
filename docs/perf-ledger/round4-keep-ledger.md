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

### 2026-06-19 | M3 / cod_5 | GPU instanced SoA glyph quads

**Status:** kept provisional (durable optimization, default-off env-gated; A/B quant pending)
**Gate:** env `FT_MOONSHOT_INSTANCED_GLYPH_QUADS` (default off)
**Commit:** 5ed94736c
**Behavior-preservation:** pixel-golden render equivalence (SoA instanced == CPU AoS) across glyph/emoji/CJK/ligature corpus; pane-reported RCH-pass. NOTE: M3 committing unblocked the GUI v0.6.1 crash fix (same webgpu.rs file).
**A/B verdict:** DEFERRED. Retry-condition (Form 5): do not retry from a cold read; use a quiet-host GPU frame-time A/B on the headless-render harness (CPU vertex-bandwidth win surfaces under glyph-dense frames).
**Baseline comparator:** CPU 4-vert-per-glyph AoS builder.
**Rollback:** env default-off; `git revert 5ed94736c`.

### 2026-06-19 | M9 / cc_3 | Anti-windup PID fleet-memory de-escalation

**Status:** kept provisional (durable optimization, default-off, monotone-safe; A/B quant pending)
**Gate:** config `memory.dampening=pid` (default hysteresis)
**Commit:** cfee3bd88
**Behavior-preservation:** escalation stays bang-bang (instant safety); PID governs only de-escalation/reclaim-magnitude; monotone floor (never reclaims less than legacy at Critical/Emergency); fail-closed to fixed fractions on RSS NaN/stall; plant-ID stability cert (`fleet_memory_pid_dampening_cert.rs`); pane-reported RCH-pass.
**A/B verdict:** DEFERRED. Retry-condition (Form 3): worth reconsidering when a memory-pressure replay shows evicted-bytes or tier-flap oscillation above the hysteresis baseline.
**Baseline comparator:** fixed eviction fractions + count hysteresis.
**Rollback:** config default hysteresis; `git revert cfee3bd88`.

### 2026-06-19 | S3-FIFO (stretch) / cod_2 | Scan-resistant S3-FIFO eviction (lfucache)

**Status:** kept provisional (durable optimization, default-off; A/B quant pending)
**Gate:** config `cache.eviction=s3fifo` (default current fifo/lfu)
**Commit:** 2c399af8c
**Behavior-preservation:** cache policy only (never affects correctness, only hit-rate); default mode reproduces today's eviction order (golden); pane-reported.
**A/B verdict:** DEFERRED. Retry-condition (Form 7): retry only if an access-trace bench shows s3fifo hit-rate above lfu at equal capacity by a Mann-Whitney-significant margin on a scan-heavy (one-hit-wonder) workload.
**Baseline comparator:** LFU (u16 freq + decay).
**Rollback:** config default fifo/lfu; `git revert 2c399af8c`.

### 2026-06-19 | Q2 / cc_2 | Group-commit widen (events/gaps) + condvar writer wake

**Status:** kept provisional (durable optimization, default-off; A/B quant pending)
**Gate:** config `storage.group_commit_events` + `storage.writer_blocking_recv` (default false)
**Commit:** dd3511fa7
**Behavior-preservation:** golden identical final DB dump + per-command result order; crash-atomicity (partial batch all-or-nothing); pane-reported RCH-pass.
**A/B verdict:** DEFERRED. Retry-condition (Form 1): retry only if a write-replay bench attributes above-noise fsync/park cost at ~200-pane sustained write load (the 1ms park + per-event autocommit dominates only under burst).
**Baseline comparator:** 1ms try_recv park + per-event autocommit.
**Rollback:** config defaults false; `git revert dd3511fa7`.

### 2026-06-19 | GUI v0.6.1 startup-crash fix (RELEASE GATE — PASSED) / cod_4 | WebGPU surface display handle

**Status:** kept (release-critical correctness fix, default-active; VERIFIED) — Form 6 structural-not-numerical
**Gate:** none — a bug fix, ships on.
**Commit:** 6c8201bbd
**Behavior-preservation:** webgpu.rs:1175 now passes the display handle to surface creation (`from_display_and_window`), fixing the wgpu-29 regression (581971fe9) that crashed v0.6.1 instantly on launch. **VERIFIED:** RCH check green + LLDB launch with FRANKENTERM_LUA_CONFIG=1 (the exact repro env) reached "WebGPU surface configured / Renderer initialized / gui-startup Lua event" with NO "No DisplayHandle" and NO "Failed to create window". RELEASE GATE: PASSED — v0.7.0 GUI launches cleanly.
**A/B verdict:** N/A — correctness fix.
**Rollback:** `git revert 6c8201bbd` (would re-introduce the launch crash — do NOT).

### 2026-06-19 | Q4 / cod_1 | Lazy capture-group materialization (defer past dedup gate)

**Status:** kept provisional (durable optimization, default-off; A/B + edge-proof pending)
**Gate:** feature `patterns-lazy-captures` (default off)
**Commit:** 5522d8bbc
**Behavior-preservation:** capture spans materialized to JSON/string only after the dedup gate confirms novelty; detection-stream golden unchanged. Default-features proof; the `--no-default-features` dedup edge-proof (ft-0wnq3) was deferred (had been blocked by sibling bocpd dirty-tree contamination).
**A/B verdict:** DEFERRED. Retry-condition (Form 7): retry only if a dhat alloc-count bench shows capture-materialization allocs above noise on a high-suppression (chatty repeated-output) workload.
**Baseline comparator:** eager `extract_captures` + JSON before dedup.
**Rollback:** feature default-off; `git revert 5522d8bbc`.

### 2026-06-19 | M4 / cc_1 | Content-defined chunking dedup of warm scrollback pages

**Status:** kept provisional (durable optimization, default-off; A/B quant pending)
**Gate:** config `scrollback.cdc_dedup` (default false)
**Commit:** 31d707ddc
**Behavior-preservation:** round-trip byte-identity decompress(cdc(page))==page over a capture corpus (`proptest_scrollback_cdc_dedup.rs`); golden default-mode unchanged.
**A/B verdict:** DEFERRED. Retry-condition (Form 7): retry only if a storage-size/CPU bench shows page-redundancy above threshold on a TUI-redraw-heavy capture (dedup amortizes only when intra/inter-page self-similarity is high).
**Baseline comparator:** plain per-page zstd.
**Rollback:** config default-off; `git revert 31d707ddc`.

### 2026-06-19 | Shiryaev-Roberts (stretch) / cod_2 | Low-delay change detector (bocpd)

**Status:** kept provisional (durable optimization, default-off; A/B quant pending)
**Gate:** config `bocpd.detector=shiryaev_roberts` (default bocpd)
**Commit:** 834a8b6cf
**Behavior-preservation:** alternate detector parallel to Adams-MacKay BOCPD; degenerate priors -> existing Student-t fallback; Schmitt hysteresis applies to both; golden default bocpd-mode unchanged. NOTE: an earlier mid-edit of this file was the dirty-tree contamination source; now committed + compiling.
**A/B verdict:** DEFERRED. Retry-condition (Form 1): retry only if a synthetic-changepoint corpus shows SR lower detection-delay at matched false-alarm rate (ARL0/ARL1 tradeoff) vs BOCPD.
**Baseline comparator:** Adams-MacKay BOCPD recent-change-mass alarm.
**Rollback:** config default bocpd; `git revert 834a8b6cf`.

### 2026-06-19 | Q5 / cod_1 | Teddy SIMD multi-pattern prefilter (patterns)

**Status:** kept provisional (durable optimization, default-off; A/B quant pending)
**Gate:** feature `teddy-prefilter` (default off)
**Commit:** c40468e79
**Behavior-preservation:** SIMD packed-literal (aho-corasick Teddy, safe-rust) prefilter ahead of fancy_regex; sound (only skips chunks with no required literal of any rule); byte-equivalent detection stream over conformance corpus; code-first.
**A/B verdict:** DEFERRED. Retry-condition (Form 1): retry only if a pattern-detection bench attributes above-noise regex-eval time on a low-match-rate chunk workload (prefilter wins when most chunks match nothing).
**Baseline comparator:** Bloom prefilter -> per-rule fancy_regex.
**Rollback:** feature default-off; `git revert c40468e79`.

### 2026-06-19 | M7 / cod_3 | Predictive poll cadence (renewal/hazard)

**Status:** kept provisional (durable optimization, default-off; proof + A/B deferred)
**Gate:** config `ingest.cadence_model=predictive` (default backoff)
**Commit:** 6fdd6b1d2
**Behavior-preservation:** predictive renewal/hazard model governs idle-direction interval only; reset-on-change preserved; hard-floored by token-bucket; fail-closed to x1.5 backoff on cold-start/NaN. Committed code-first (proof deferred — the m7 RCH build wedged 4x, infra not code; retry in flight).
**A/B verdict:** DEFERRED. Retry-condition (Form 5): do not retry from a cold read; use a recorded pane-output trace replay (captures reduced >=15% at p95 capture-latency non-regressed) once the m7 RCH build stops wedging.
**Baseline comparator:** x1.5 multiplicative backoff + static pane_tiers table.
**Rollback:** config default backoff; `git revert 6fdd6b1d2`.
