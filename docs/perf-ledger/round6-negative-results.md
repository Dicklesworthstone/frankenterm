# Round-6 Negative-Evidence Ledger

> The Alien Optimization Gauntlet (v0.9.0 campaign). **Load-bearing:** every round-6 optimization that is
> *rejected*, *reverted*, or *measured-as-no-win* gets an entry here closed with exactly one of the 8
> grep-able **retry-condition predicate** forms. Negative evidence is a *win*, not a failure.

The **10 keep-gate rules**, the **8 retry-condition forms**, the **forbidden anti-vocabulary**, and the
rejected-entry template are defined once in [`round4-negative-results.md`](round4-negative-results.md) —
they carry over unchanged. Kept/promoted changes → [`round6-keep-ledger.md`](round6-keep-ledger.md).
Campaign record → [`../../tests/artifacts/perf/v090-round6-campaign.md`](../../tests/artifacts/perf/v090-round6-campaign.md).

## Round-6 bench-host caveat (operator-confirmed)

Benches run on the **local Mac under swarm load** → cv ~15-20%. Therefore round-6 **only certifies
large-effect, non-overlapping wins (≥2×)**. A small-effect or quality-metric (evicted-bytes / hit-rate /
RSS / alloc-count) candidate that cannot be adjudicated under this cv is NOT a reject — it stays
default-off with a **Form-7** predicate naming the quiet-host / quality-metric bench that would unblock it.
Only a measured **regression on a default-on path** triggers a `git revert`.

## PRE-REJECTED classes (round-5 evidence — do NOT re-propose without NEW evidence)

These lost at real sizes in round 4/5; grep round4+round5-negative-results.md before any pattern touches them.
- **Custom replacements of stdlib HashMap/Vec** — perfect-hash (M5 MPHF +69% slower @192 anchors),
  fingerprint dedup (Q6 +8.8% slower @6144 keys), SIMD packed-literal prefilters (Q5 Teddy +0.5% noise).
- **Prefilters/caches whose overhead ≈ savings** (Q5 class).
- **Controller/policy swaps whose "win" is a quality metric, not wall-clock** (M9 PID compute-only,
  S3-FIFO 2× compute). Adjudicable only with the `.20` quality harness on a quiet host.
- **COW/snapshot scrollback to dodge a lock** (M6) — measured sub-µs lock-wait (250ns @200 panes,
  3 orders below the 50µs bar); clone costs MORE than scan-under-lock. KILLED.
- **Micro-opts of already-sub-µs paths** (redaction lookback, LRU token mgmt, FNV embedding, RRF fusion —
  all confirmed already-optimal by the round-6 hot-path investigation).

---

## Entries

_(round-6 measured-no-win / reject / revert entries land below, one per the rejected-entry template.)_
