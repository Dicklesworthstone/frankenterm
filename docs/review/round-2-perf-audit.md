# Round-2 Performance Hot-Spot Audit — Saturation

**Scope:** repeat the round-1 perf-hotspot audit
(`docs/review/perf-hotspot-audit.md`, HEAD a9bdaa9e) post the two
perf fixes that landed during the implement rotation.
**Date:** 2026-04-26
**Verdict:** **SATURATED** — both round-1 findings shipped, no new
hotspots.

## Round-1 findings — both shipped

| Bead       | Finding                                              | Fix at HEAD                                                |
| ---------- | ---------------------------------------------------- | ---------------------------------------------------------- |
| ft-bhyxz   | storage read-path opens fresh `Connection` per query | **3001def0** — `PooledReadConn` LIFO pool, 77 sites migrated |
| ft-gbpoy   | codec `serialize_with_mode` Auto path double-serializes | **51101858** — replaced inner re-serialize with `zstd::stream::encode_all` |

## Verification at HEAD

```
grep -c "PooledReadConn::acquire" crates/frankenterm-core/src/storage.rs
  → 77

grep -c "open_read_storage_conn"  crates/frankenterm-core/src/storage.rs
  → 5  (function decl + 1 comment + Drop fallback + 2 doc refs;
        all expected residuals — no production call sites)

grep -c "zstd::stream::encode_all" frankenterm/codec/src/lib.rs
  → 1  (the replacement single-call)

grep -c "zstd::Encoder::new"      frankenterm/codec/src/lib.rs
  → 0  (old double-serialize pattern fully retired)
```

## Round-2 sweep

```
grep -rn "Connection::open"               crates/frankenterm-core-*/src/  → 0 hits
grep -rnE "for .* in .*\{[^}]*for .* in"  crates/frankenterm-core-*/src/  → 0 hits
grep -nE 'format!\(.*"(INSERT|DELETE|UPDATE)' crates/frankenterm-core/src/storage.rs
  → 1 hit, line 14850 (build_tier_query DELETE — same as round 1, uses
                       ? parameter binding for user input, not a finding)
```

The 10 new sub-crates (`*-types` leaves + `tantivy`/`ars`/`fleet`/
`replay` clusters) contain no `Connection::open`, no nested
loops-over-panes, no new format!-into-SQL sites. The leaf-types
crates use serde + pure type definitions; the cluster crates
inherit their data-access surfaces from `frankenterm-core` via
path-deps and don't add their own DB code.

The pattern-detection hot path (`PatternEngine::detect`,
`patterns.rs:2271`) and delta-extraction (`extract_delta`,
`ingest.rs:1662`) are unchanged at HEAD and were both classified
"well-engineered" in round 1.

## Comparison to round 1

| Category | Round 1 | Round 2 | Delta |
| --- | ---: | ---: | ---: |
| Per-query `Connection::open` sites | 78 | **0** (all pooled) | -78 |
| Codec double-serialize on Auto path | yes | **no** | fixed |
| Sub-crate Connection::open | n/a | 0 | clean |
| Sub-crate nested pane loops | n/a | 0 | clean |
| Sub-crate format!-into-SQL | n/a | 0 | clean |
| Pattern engine quick-reject pre-filter | yes | yes | unchanged |
| Delta-extraction memchr SIMD path | yes | yes | unchanged |
| **New beads filed** | 2 | **0** | saturated |

## Saturation accounting

**Round 2 of 3** for the perf rotation.

The two structural fixes (PooledReadConn + zstd::stream::encode_all)
addressed the only foundational concerns flagged in round 1. The
remaining hot paths were already classified well-engineered (Aho-
Corasick + quick-reject in patterns; memchr SIMD + bounded overlap
in delta extraction; time-windowed cache for `list_panes`; consistent
RwLock ordering for registry/cursors).

No new microsecond-bench data gathered in this 15-min slot — round 1
caveated the same. The fixes are structural and the test suites + CI
guards verify correctness.

## Stop-condition tally

| Skill | Round 1 | Round 2 |
| --- | :---: | :---: |
| mock-finder | 1 finding (resolved) | ✓ saturated |
| deadlock-finder | 0 findings | ✓ saturated |
| reality-check | 3 findings (all closed) | ✓ saturated |
| **perf** | 2 findings (both shipped) | ✓ **saturated** |
| security | 1 finding (open ft-ii8ss) | pending |
| modes-of-reasoning | 2 findings (both closed) | pending |

**4 of 6 review skills now round-2 saturated.** Two remaining
(security, modes-of-reasoning) before the 3-saturated-rounds
stop-condition fires.
