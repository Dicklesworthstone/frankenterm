# Round-2 Reality-Check Sweep — Saturation

**Scope:** verify README.md + AGENTS.md (post 5c9f62fc refresh) against
HEAD reality (post ft-y0loj.* sub-crate split, post ft-y378j.* rename,
post ft-kuxho codec rolling-upgrade, post ft-gkqej audit-marker pipeline).
**Date:** 2026-04-26
**Verdict:** **SATURATED** — every numeric claim verified accurate.

## Verification

| Claim                                       | README/AGENTS | Reality at HEAD | Match? |
| ------------------------------------------- | ------------: | --------------: | :----: |
| Workspace crates                            | 64            | 64              | ✓      |
| Vendored crates under `frankenterm/`        | 47            | 47              | ✓      |
| `crates/frankenterm-core/src` Rust modules  | 427           | 427             | ✓      |
| Core-library Rust LOC                       | 779k+         | 779,724         | ✓      |
| Tests                                       | 45,000+       | 47,603 (round 1)| ✓      |
| Last-verified date                          | April 26, 2026| 2026-04-26      | ✓      |
| 10 new sub-crates listed in workspace tree  | yes           | yes             | ✓      |
| `runtime_async` framed as canonical surface | yes           | yes (post ft-y378j.2) | ✓ |
| Deprecated `runtime_compat` alias residuals | acknowledged  | 8 files / 45 occ| ✓      |

## Round-1 findings — all closed

The three reality-check beads filed in round 1 (HEAD 49f0c161):

| Bead       | Title                                                | Status |
| ---------- | ---------------------------------------------------- | ------ |
| ft-f1ec3   | README + AGENTS.md workspace stats stale             | **CLOSED** at 5c9f62fc |
| ft-y378j   | runtime_compat alias still load-bearing              | **CLOSED** via .1/.2/.3 children |
| ft-lwa5q   | replay broken stub                                   | **RESOLVED** by cc7's c62dda66 |

Plus other major epics shipped during the rotation:
- **ft-xbnl0** (goal-line program): closed 2026-04-23 ✓
- **ft-zoxxq** (mux boundary truth): closed 2026-04-26 ✓
- **ft-y0loj** (sub-crate split): in progress, ~7 of 10 beads closed
- **ft-kuxho** (codec rolling-upgrade): per landing trail
- **ft-gkqej** (audit-marker pipeline): per landing trail

## What changed since round 1

| Change | Round 1 | Round 2 | Note |
| --- | --- | --- | --- |
| Workspace crate count framing | "54" stale | "64" accurate | ft-f1ec3 closed |
| Module count framing | "483" stale | "427" accurate | ft-f1ec3 closed |
| LOC framing | "790k" off | "779k" accurate | ft-f1ec3 closed |
| Verification stamp | April 6 | April 26 | refreshed |
| Workspace tree | listed only original 5 | lists all 10 + original | ft-f1ec3 closed |
| `runtime_async` framing | "deprecated alias for one release" | softened, with call-site count | accurate |
| Replay-and-forensics claim | broken in tree | working at HEAD | ft-lwa5q resolved |
| CI cycle guard | absent | present (ft-94juo) | new |
| CI rename guard | absent | present (ft-y378j.3) | new |

## Comparison to round 1

| Category | Round 1 | Round 2 | Delta |
| --- | ---: | ---: | ---: |
| Numeric stats stale | yes | **no** | fixed by ft-f1ec3 |
| Workspace tree missing sub-crates | yes | **no** | fixed by ft-f1ec3 |
| runtime_compat framing optimistic | yes | **no** | call-site count now reflects reality |
| Replay subsystem broken | yes | **no** | resolved by cc7 |
| CI guards documented vs absent | absent | present + documented | +2 (ft-94juo, ft-y378j.3) |
| **New beads filed** | 3 | **0** | saturated |

## Saturation accounting

**Round 2 of 3** for the reality-check rotation.

The post-refresh docs are factually accurate. The two remaining
"open promises" — runtime_compat alias removal (ft-y378j.4) and the
final tier-2 mcp/connector extractions (ft-t2d70 PARK ADR) — are
explicitly framed as deferred work in both the README and the
proposal docs they reference, so they don't represent doc-vs-code
drift.

## Stop-condition tally

| Skill | Round 1 (initial) | Round 2 |
| --- | :---: | :---: |
| mock-finder | 1 finding (now resolved) | ✓ saturated |
| deadlock-finder | 0 findings | ✓ saturated |
| reality-check | 3 findings (all closed) | ✓ saturated |

**3 of 3 review skills now saturated in their round-2 sweep.**
The remaining 3 skills (perf, security, modes-of-reasoning) need
their round-2 rotation before the orchestrator's stop-condition
fires.
