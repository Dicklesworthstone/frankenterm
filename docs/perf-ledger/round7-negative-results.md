# Round-7 Negative-Evidence Ledger

> The Alien Optimization Gauntlet (v0.10.0 campaign). **Load-bearing:** every round-7 optimization that
> is *rejected*, *reverted*, *measured-no-win*, or *refuted-on-liveness* gets an entry here closed with
> exactly one of the 8 grep-able **retry-condition predicate** forms. Negative evidence is a *win*.

The **10 keep-gate rules**, the **8 retry-condition forms**, the **forbidden anti-vocabulary**, and the
rejected-entry template are defined once in [`round4-negative-results.md`](round4-negative-results.md) —
they carry over unchanged. Kept/promoted → [`round7-keep-ledger.md`](round7-keep-ledger.md). Campaign
record → [`../../tests/artifacts/perf/v100-round7-campaign.md`](../../tests/artifacts/perf/v100-round7-campaign.md).

## PRE-REJECTED / already-resolved (round 4/5/6 evidence — do NOT re-propose without NEW evidence)

Grep round{4,5,6}-negative-results.md before any pattern touches these.
- **Redactor structural single-pass** — ALREADY SHIPPED. `redact()` (`redactor.rs:690`) early-returns via
  a combined `SECRET_PATTERN_SET: LazyLock<RegexSet>`; the 22% self-time is the irreducible cost of an
  already-optimal scan (round6-profile-targets.md:83-87). Do NOT re-open redaction.
- **Custom replacements of stdlib HashMap/Vec** (M5 MPHF, Q6 fingerprint, Q5 Teddy) — all lost at real size.
- **Per-op micro-opts of sub-µs paths** (redaction lookback, LRU, FNV, RRF) — confirmed already-optimal.
- **Serial replacements of vectorized code** (M1 ANSI-DFA lost to SWAR) — exhausted.
- **Controller/policy swaps whose "win" is a quality metric** (M9 PID tie, S3-FIFO conditional) — adjudicable
  only via the deterministic harness; not blanket default-on.
- **COW-to-dodge-a-lock** (M6) — sub-µs contention, killed.
- **GUI vertex-bandwidth** (M3 SoA glyph quads) — cost is Metal readback, not bandwidth.
- **Built-but-unwired surfaces** (distributed `DistributedHttpClient` test-only; web `/stream/events`
  publisher-less per ft-zeo5o) — NOT valid perf targets until wired.

---

## Entries

_(round-7 measured-no-win / reject / revert / liveness-refute entries land below, one per the
rejected-entry template, each closed with exactly one of the 8 retry-condition forms.)_
