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

_(round-5 measured-no-win / reject / revert entries land below as A/B runs complete on the quiet host.)_
