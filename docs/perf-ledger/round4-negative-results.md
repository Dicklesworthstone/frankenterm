# Round-4 Negative-Evidence Ledger

> The Alien Optimization Gauntlet (v0.7.0 campaign). This file is **load-bearing**: every optimization
> candidate that is *rejected* or *reverted* gets an entry here, closed with a grep-able **retry-condition
> predicate**. The next agent who greps for the touched symbol finds exactly what evidence would unblock a
> retry. Negative evidence is first-class — a tried-and-reverted idea recorded here is a *win*, not a failure.

Companion: kept changes → [`round4-keep-ledger.md`](round4-keep-ledger.md). Campaign record →
[`../../tests/artifacts/perf/v070-round4-campaign.md`](../../tests/artifacts/perf/v070-round4-campaign.md).
Discipline source: `running-the-gauntlet-on-your-rust-port` (KEEP-GATE-RULES.md, RETRY-CONDITION-VOCABULARY.md).

---

## The 10 keep-gate rules (a kept change must satisfy ALL)

1. **Profile-first** — measured hotspot evidence (≥0.1% self-time) BEFORE touching source.
2. **Both gates, same run window** — focused bench + broad bench from the same SHA snapshot, same
   `CARGO_TARGET_DIR`, same RCH worker, timestamps <60s apart.
3. **`release-perf` profile, never `--release`** — size-opt (`opt-level="z"`) invalidates perf claims.
4. **Feature-default-mode proof file** — record the gate's default state so a silent flip can't regress.
5. **Symmetric retry shells** — A/B arms wrapped in identical framework cost.
6. **Identical config** — both arms get byte-identical tuning/PRAGMA/env.
7. **Behavior byte-identical** — golden/property/oracle proves observable behavior unchanged.
8. **cv_pct ≤ 5** — any cell claimed as a win has coefficient of variation ≤ 5%; else it's noise.
9. **Attribution ≥ 0.1% self-time** — the win names a specific frame; <0.1% is the micro-lever trap.
10. **Pass-over-pass ratchet** — no regression beyond: primary −3% / geomean −5% / per-category −10% /
    p90 −15% / throughput −5%.

A breach of any one → reject → write an entry below.

## The 8 retry-condition forms (every entry closes with exactly one; NO anti-vocabulary)

1. **Profile attribution above noise** — "Retry only if a profiler attributes a clearly-above-noise share to `<frame>` on `<wider workload>`."
2. **Architectural defer** — "Reconsider only inside the broader `<X>` redesign."
3. **Gate-driven** — "Worth reconsidering when `<specific gate>` moves."
4. **Standalone retirement** — "Not worth retrying as a standalone patch."
5. **Evidence-pipeline mandate** — "Do not retry from a cold read; use `<specific pipeline>` instead."
6. **Structural not numerical** — "Retry condition not applicable — the gain is structural, not numerical."
7. **Workload-property threshold** — "Retry only if `<workload>` exhibits `<property>` below `<threshold>`."
8. **Blocked-by dependency** — "Blocked until `<dep>` lands; track as `<bead_id>`."

**Forbidden (entry is INVALID if it contains):** "later", "in the future", "TODO", "FIXME", "future work",
"we should revisit", "tracked elsewhere" (without a bead id), "might be worth trying", "worth exploring",
"interesting direction", "someone should look at this", "if it seems important".

---

## Rejected entry template (copy per rejection)

```markdown
### <YYYY-MM-DD> | <bead_id / scratch-branch> | <Title>

**Status:** rejected (within-noise | focused-improved-broad-worsened | cold-start-outlier | no-bounded-micro-lever | correctness-abandoned | architectural-change-dressed-as-micro | flake)

**Gate:** <feature/env/config flag> (default off)

**Profile attribution:** "<X>% <Frame>" — flamegraph: <path>

**Measurement (focused):** <bench> <metric> = <before> → <after> (<delta%>); cv_pct=<X>; noise band ±<N>%

**Measurement (broad):** primary_score <before> → <after> (<delta%>)

**Behavior-preservation:** <pass | fail; fail = correctness-abandoned, not perf-rejected>

**A/B verdict:** SPRT=<accept|reject|low_confidence> samples=<n>; conformal=<within|exceeds> band

**Retry-condition predicate:** <one of the 8 verbatim forms above>

**Rollback:** `git revert <sha>` | flag stays default-off | scratch branch <name>

**Sibling references:** <bead ids>
```

---

## Entries

### 2026-06-19 | M6 (stretch) | Persistent COW scrollback grid — DEFERRED (not attempted)

**Status:** deferred-stretch (not attempted) — the boldest remaining idea; deliberately not started to land a clean v0.7.0 convergence rather than open a large high-risk moonshot late in the campaign.

**Gate (intended):** feature `persistent-scrollback` (default off)

**Scope:** `im`-style path-copying rope for the hot scrollback tier giving O(1) immutable snapshots for lock-free search-while-streaming. Touches `frankenterm/term/` + scrollback (core) — large surface, 2-4x memory overhead, collides with Q1/M4 (scrollback) and M1 (term).

**Why deferred:** round-4 already kept 16 ideas (incl. 2 stretch) + RS-erasure + min-plus; M6 is the highest-effort/highest-risk remaining and would delay the release + risk the clean convergence. No measured bottleneck currently attributes contention to scrollback read-vs-write locking.

**Retry-condition predicate (Form 1):** retry only if a profiler attributes a clearly-above-noise share to scrollback read/render lock contention (or clone cost) on a concurrent search-while-streaming workload at high pane count. Until then it is speculative.

**Baseline comparator:** `VecDeque` hot tier + lock/clone for concurrent reads.

**Rollback:** N/A (never landed).

---

_(reverts / within-noise rejections land below as A/B quantification runs on a quiet host)_
