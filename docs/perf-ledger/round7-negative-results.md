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

### 2026-06-21 | ft-ykde4 | EV3 blocked/rank-select single-line scrollback decode

**Status:** rejected (refuted-on-liveness)

**Gate:** env `FT_MOONSHOT_SCROLLBACK_BLOCKED_PAGE_INDEX` (default off)

**Profile attribution:** Round-6 B0 attributed `scrollback.warm_line` at 5.18% on the synthetic
deep-scroll decode harness, but round-7 caller tracing found no non-test production caller of the
single-line decode API.

**Measurement (focused):** Not run for promotion: `scrollback_ev3_cold_line` exercises the
single-line `cold_line` bench surface, but that surface is bench/test-only in the production
checkout.

**Measurement (broad):** Not applicable; no production request path reaches the EV3 single-line
decode lever.

**Behavior-preservation:** pass — the EV3 byte-equivalence/proptest substrate remains shipped
behind its default-off gate; this entry only rejects promoting or further optimizing the dead
single-line path.

**Liveness evidence:** `rg -n "warm_line\(|cold_line\(|decode_page_line\(" crates/frankenterm-core/src
crates/frankenterm-core/benches crates/frankenterm-core/tests tests frankenterm -g '*.rs'`
finds only the API definitions in `scrollback_tiers.rs`, in-file `#[cfg(test)]` uses after
`scrollback_tiers.rs:1897`, the `scrollback_ev3_cold_line` bench, the
`round6_profile_realistic_workloads` test harness, and `fleet_memory_controller.rs:2001` inside
that file's `#[cfg(test)] mod tests` starting at `fleet_memory_controller.rs:1499`. No non-test
production caller invokes `warm_line`, `cold_line`, or `decode_page_line`.

**A/B verdict:** reject — liveness gate failed before A/B. Production scrollback reads that ask for
whole pages still go through `warm_page_lines` -> `decode_page` (`scrollback_tiers.rs:1004-1008`),
so EV3's target-block decode does not participate.

**Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share to
`TieredScrollback::warm_line` or `TieredScrollback::cold_line` on a non-test production deep-scroll
or cold-history readback workload.

**Rollback:** flag stays default-off

**Sibling references:** ft-ykde4; adaptive-M4 RSS adjudication remains pending on ft-6aban
(`tests/round7_rss_harness.rs`).
