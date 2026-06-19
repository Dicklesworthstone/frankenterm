# Round-5 Negative-Evidence Ledger

> The Alien Optimization Gauntlet (v0.8.0 campaign). **Load-bearing:** every round-5 optimization that
> is *rejected*, *reverted*, or *measured-as-no-win* gets an entry here closed with exactly one of the 8
> grep-able **retry-condition predicate** forms — so the next agent who greps the touched symbol finds
> precisely what evidence would unblock a retry. Negative evidence is a *win*, not a failure.

The **10 keep-gate rules**, the **8 retry-condition forms**, the **forbidden anti-vocabulary**, and the
rejected-entry template are defined once in
[`round4-negative-results.md`](round4-negative-results.md) — they carry over unchanged. Kept/promoted
changes → [`round5-keep-ledger.md`](round5-keep-ledger.md). Campaign record →
[`../../tests/artifacts/perf/v080-round5-campaign.md`](../../tests/artifacts/perf/v080-round5-campaign.md).

Round-5 nuance for the 19 flags: a round-4 flag that, when finally A/B-measured on the quiet Mac, shows
**no keep-gate win** is NOT reverted (it is already default-OFF and zero-risk) — it gets an entry here
recording the measured delta + cv + the retry-condition form that would justify a future default-on
promotion. Only a measured **regression on a default-on path** triggers an actual `git revert`.

---

## Entries

### 2026-06-19 | Q1 prefix-sum — default-ON promotion BLOCKED on cv (the win itself is real, 32.5×)

**Status:** cv-blocked-for-promotion (NOT a reject — the optimization is a measured 32.5× win, kept
default-off; see round5-keep-ledger.md). The blocker is purely the keep-gate rule-8 cv threshold for an
auto-promotion to default-on.
**Measurement:** −96.92% (3.09ms→95µs), p=0, but candidate cv=15.2% / baseline cv=20.6% > 5% (Mac not
quiet — concurrent swarm + tend). Distributions are non-overlapping so the win is unambiguous.
**Retry-condition predicate (Form 5):** Do not promote Q1 to default-on from this noisy reading; re-run
the `scrollback_prefix_index` env A/B on a genuinely quiet Mac (swarm idle / converged) plus a
shallow-scrollback non-regression bench, and promote only once candidate cv≤5 AND the shallow case is
non-regressed. Until then Q1 ships default-off (zero-risk) with this 32.5× deep-scroll evidence on record.
**Rollback:** n/a (default-off, never promoted).

---

### 2026-06-19 | M9 PID fleet-memory + S3-FIFO eviction — bench measures compute, NOT the quality metric

**Status:** measured-no-win-on-current-bench (both stay default-off, zero-risk; NOT reverted). The
round-5 benches (8eef1f001) measure the controller/eviction **compute cost**, not the **quality metric**
each flag exists to improve, so neither can be adjudicated as a win yet.
**Measurement:** M9 `memory_pid_dampening`: hysteresis 87.6µs vs pid 78.6µs (PID compute −10% — but its
purpose is reduced evicted-bytes / reclaim oscillation, which the bench does not capture). S3-FIFO
`lfucache_s3fifo`: lfu 2.64ms vs s3fifo 5.09ms (S3-FIFO is **2× the per-op compute cost** — and its
purpose, scan-resistant hit-rate at equal capacity, is not captured). The bench `budget` strings name
the intended `pressure_replay` / `scan_heavy_hit_rate` outcomes but the bench bodies only time the ops.
**Retry-condition predicate (Form 7):** Retry only if a bench reports the actual QUALITY metric on the
right workload above the legacy baseline by a Mann-Whitney-significant margin — for S3-FIFO, **hit-rate
at equal capacity on a scan-heavy one-hit-wonder access trace** above LFU; for M9, **elevated-tier
evicted-bytes and reclaim-target oscillation on a memory-pressure replay** below the hysteresis baseline.
Until such a quality-metric bench exists (tracked as a P3 follow-up bead), both flags ship default-off on
their round-4 correctness proofs. (S3-FIFO's 2× compute cost makes default-on actively wrong absent a
hit-rate win that pays for it.)
**Rollback:** n/a (default-off, never promoted).

---

### 2026-06-19 | M6 persistent COW scrollback grid — MEASURED not-justified, stays DEFERRED

**Status:** evidence-based re-deferral (NOT attempted; the round-4 retry predicate is now MEASURED FALSE).
E1 built the concurrent-search-while-streaming evidence harness
(`crates/frankenterm-core/benches/m6_search_while_streaming.rs`, group `m6_lock_wait_evidence`); E2 ran
it locally and read the per-pane `Arc<Mutex>` reader lock-wait under 6 background streaming writers.
**Measurement (reader lock-wait p95, contended vs quiescent):**
- 100 panes, scan-under-lock: 42ns → 291ns (6.9× ratio); 200 panes: 42ns → 250ns (5.95× ratio).
- 100 panes, clone-then-scan: 125ns → 333ns (2.7×); 200 panes: 125ns → 291ns (2.3×).
- **All 4 configs `above_noise: false`** — the bar is contended p95 ≥ 3× baseline AND ≥ 50µs; the ratio
  clears 3× but the absolute contended p95 wait is only ~250-333 **nanoseconds**, ~3 orders of magnitude
  below the 50µs threshold. Max-wait outliers reached ~0.5ms once but p95/p99 stay sub-10µs.
- The per-pane mutex serializes access with negligible queueing. Notably the `clone_then_scan` strategy
  HOLDS the lock LONGER (15-47µs) than `scan_under_lock` (3-15µs) — cloning the scrollback is more
  expensive than scanning it under the lock, so M6's COW/snapshot-to-avoid-holding-the-lock premise does
  not even pay off at this scale.
**Verdict:** M6 (path-copying COW rope, ~2-4× memory overhead, large surface touching term + scrollback,
collides with Q1/M4/M1) is **NOT justified by measured contention**. It stays deferred.
**Retry-condition predicate (Form 1):** Retry only if a profiler attributes a clearly-above-noise share
(contended reader lock-wait p95 ≥ 50µs, i.e. ≥3 orders above today's ~250ns) to scrollback read/render
lock contention on a real high-pane-count search-while-streaming workload — not reproduced by this
harness at 200 panes / 6 writers. Until then M6 is speculative.
**Rollback:** n/a (never landed). Bench harness retained for future re-measurement.

---

### 2026-06-19 | Q5 Teddy / Q6 fingerprint-dedup / M5 MPHF-dispatch — MEASURED no-win or REGRESSION

**Status:** measured-no-win (Q5) + measured-regression (Q6, M5). All three round-4 patterns "moonshots"
stay default-off (zero-risk, round-4 correctness-proven); NONE promoted — and good thing, because two
REGRESS on their designed workloads. Bench `crates/frankenterm-core/benches/round5_patterns_a1.rs`,
local Mac, release-perf, clean cv (per-arm ranges <0.5%). One baseline build (no features) + one candidate
build (teddy+fingerprint+mphf); per-group cfg-gated so each comparison is valid.
**Measurement (baseline → candidate):**
- **Q5 teddy_low_match/512_chunks: 109.97µs → 110.55µs (+0.5%)** — within noise, NO win. The Teddy SIMD
  packed-literal prefilter neither helps nor hurts on this workload (its overhead ≈ the regex it skips).
- **Q6 fingerprint_dedup_churn/6144_keys_x2: 86.68ms → 94.29ms (+8.8% SLOWER)** — the 64-bit fingerprint
  + O(1) ring-LRU is slower than `HashMap<String>` + O(n) retain at this key count/churn.
- **M5 mphf_chatty_anchor_routing/192_anchors_x24: 310.59µs → 524.97µs (+69% SLOWER)** — the minimal
  perfect-hash anchor→bitset lookup costs MORE per probe than the `HashMap` route at 192 anchors.
**Retry-condition predicates (Form 7, one per flag):**
- Q5: retry only if a pattern-detection bench on a TRULY low-match-rate chunk stream (≫90% chunks with no
  required literal of any rule) shows Teddy rejecting above noise — this 512-chunk corpus is not low-match
  enough for the prefilter to pay for itself.
- Q6: retry only if a dedup workload with far higher per-key String-alloc pressure / key cardinality than
  6144 keys shows the O(1) ring-LRU beating HashMap+O(n)-retain by a Mann-Whitney-significant margin.
- M5: retry only if an anchor table FAR larger than 192 (where HashMap probe + collision cost actually
  dominates) shows MPHF routing winning — at 192 anchors the HashMap is faster, so MPHF is net cost.
**Rollback:** n/a (all default-off, never promoted; correctness proofs retained).

### 2026-06-19 | Q2/Q3/M1/M2/M3/M7/M8/SR — adjudicated default-off, quantification deferred (Form 7)

**Status:** default-off, not heavily re-run this round (converge decision). Each is either a quality-metric
flag whose win is NOT a wall-clock ns number (so the existing ns benches can't adjudicate it — same class as
M9/S3-FIFO), a 2-build feature deferred to a focused run, or a gate with a measurement footgun. All ship
default-off (zero-risk, round-4 correctness-proven). Per-flag retry predicate (Form 7 unless noted):
- **Q2 group-commit + M8 adaptive M/G/1:** win = reduced fsync/park cost at ~200-pane sustained WAL burst.
  Retry only if a write-replay bench reports fsync-count / writer-wait below the 1ms-park+autocommit (resp.
  fixed-128) baseline at high service-time CV. (macOS fsync timing is unreliable for this — prefer a Linux
  worker run.)
- **M7 predictive cadence:** win = captures reduced ≥15% at non-regressed p95 capture-latency. Retry only on
  a recorded pane-output trace replay (not a microbench).
- **M2 succinct attrs:** win = attr-store RSS / cache-miss share on a deep-scrollback workload. Retry only if
  a warm/cold scrollback memory bench shows it above noise (RSS metric, not ns).
- **M3 SoA glyph quads:** win = GPU vertex-bandwidth at glyph-dense frames. Retry on a headless gui
  frame-time A/B (the RCH workers lack GUI libs for the bench link; run locally on a GUI-capable host).
- **SR (Shiryaev-Roberts):** **a real correctness bug was FIXED this round** (detector was dead on
  non-zero-centered data, 8c8142ef3) — that is a kept correctness improvement, not a perf flag. The perf
  comparison (lower detection-delay at matched ARL0) retry: only on a synthetic-changepoint ARL0/ARL1 corpus.
- **Q3 linear KMP overlap:** win = avoided O(n²) on adversarial common-first-byte / box-drawing input. Its
  env gate reads `env::var_os().is_some()` (empty-but-set = ON), so the build-once env A/B can't express the
  OFF arm; retry with a forced-algorithm A/B via the `#[doc(hidden)]` forced-overlap API on adversarial input.
- **M1 ANSI DFA:** win = branchless table on an ANSI-dense (TUI/vim) capture. Retry with a focused
  feature A/B (`ansi-dfa-table`) on the `simd_scan` ANSI-heavy workload (2-build; converge-deferred here).
**Rollback:** n/a (all default-off).

---

_(further round-5 measured-no-win / reject / revert entries land below.)_
