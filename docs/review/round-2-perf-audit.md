# Round-2 Performance Hot-Spot Audit — Historical Structural Follow-up

**Scope:** repeat the round-1 perf-hotspot audit
(`docs/review/perf-hotspot-audit.md`, HEAD a9bdaa9e) post the two
perf fixes that landed during the implement rotation.
**Original date:** 2026-04-26<br>
**Truth-status refresh:** 2026-08-06<br>
**Current verdict:** **NOT SATURATED.** The two narrow round-1 structural
findings shipped. The original source-scan rotation found no additional item in
its bounded scope, but it did not profile the live mux/GUI critical path or
qualify any native target class.

## Round-1 findings — both shipped

| Bead       | Finding                                              | Historical fix evidence                                    |
| ---------- | ---------------------------------------------------- | ---------------------------------------------------------- |
| ft-bhyxz   | storage read-path opens fresh `Connection` per query | **3001def0** — `PooledReadConn` LIFO pool, 77 sites migrated |
| ft-gbpoy   | codec `serialize_with_mode` Auto path double-serializes | **51101858** — replaced inner re-serialize with `zstd::stream::encode_all` |

A 2026-08 source-only spot check still found the `PooledReadConn` acquisition
surface and direct `encode_all` compression path. No compile, benchmark, or
runtime result was produced by that spot check.

## Historical source verification (captured 2026-04-26)

The transcript below describes the source tree at the original review point.
It was not rerun as a compile, benchmark, or runtime proof during the 2026-08
truth refresh:

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

The original scan sampled the then-current extracted sub-crates and reported no
textual matches for its three patterns. Those grep patterns cannot prove the
absence of database-open paths, nested algorithmic work, contention, or dynamic
SQL, and the workspace has evolved since then.

At the original review point, the scan saw no relevant change in the inspected
pattern-detection and delta-extraction bodies. The old “well-engineered” label
was a source-review judgment, not a profile or a statement about current line
locations. In particular, it reviewed the core `PatternEngine::detect` body,
while the production runtime calls `detect_with_context`, whose tail copy,
overlap filtering, agent sharding, and dedupe work were not covered. The scan
therefore provides no live-path latency or allocation evidence for large
ongoing sessions.

## Historical comparison to round 1

| Category | Round 1 | Round 2 | Delta |
| --- | ---: | ---: | ---: |
| Per-query `Connection::open` textual sites in the scanned path | 78 | **0** in the historical scan | -78 scanned hits |
| Codec double-serialize on Auto path | yes | **no** | fixed |
| Sub-crate `Connection::open` pattern | n/a | 0 | no textual hit in sampled paths |
| Sub-crate nested-loop regex | n/a | 0 | no textual hit; not an algorithmic proof |
| Sub-crate format!-into-SQL regex | n/a | 0 | no textual hit; not a dynamic-SQL proof |
| Pattern engine quick-reject pre-filter | yes | yes | unchanged |
| Delta-extraction memchr SIMD path | yes | yes | unchanged |
| **New beads filed by this narrow scan** | 2 | **0** | no additional static finding in that pass |

## Retired saturation accounting

The original workflow called this **round 2 of 3** for a review rotation. That
label is retained only as historical process context; it is not a product or
performance finish line.

The two structural fixes (PooledReadConn + zstd::stream::encode_all)
addressed the two concerns flagged in round 1. Other inspected paths were
classified separately (Aho-Corasick + quick-
reject in patterns; memchr SIMD + bounded overlap in delta extraction;
consistent RwLock ordering for registry/cursors). The historical
time-windowed `list_panes` cache was later removed: full `PaneInfo`
contains volatile focus/cursor/cwd/title/viewport/zoom state, and a TTL
cache could return older metadata across clients, transports, or concurrent
mutations. A future optimization must coalesce one authoritative in-flight
revision rather than reuse a completed result by elapsed time.

No microbenchmark or live-system data was gathered in this slot. Structural
tests may verify intended code behavior, but they do not verify latency,
throughput, frame pacing, visual quality, target-machine scaling, or long-run
resource behavior.

## Historical stop-condition tally

| Skill | Round 1 | Round 2 |
| --- | :---: | :---: |
| mock-finder | 1 finding (resolved) | ✓ saturated |
| deadlock-finder | 0 findings | ✓ saturated |
| reality-check | 3 findings (all closed) | ✓ saturated |
| **perf** | 2 findings (both shipped) | narrow static rotation used the label “saturated” |
| security | 1 finding (open ft-ii8ss) | pending |
| modes-of-reasoning | 2 findings (both closed) | pending |

This tally governed the old review workflow only. It must not close the active
performance campaign or support a release claim.

## Negative-evidence ledger

| Claim that remains unproven | Why this round cannot prove it | Required evidence |
|---|---|---|
| Low keypress-to-present latency over the user's LAN path | No live input, transport, PTY, renderer, GPU, or display timestamps | Retained end-to-end percentile traces on the declared Mac/LAN/remote-machine topology |
| Fast and attractive resize/zoom | No native GUI exercise, frame pacing, reflow validation, or visual-differential artifact | Coupled timing and appearance evidence under interactive resize and multiple zoom transitions |
| M4/M5 optimization or Threadripper PRO 5995WX scaling | No end-to-end interactive qualification run on the named Apple generations or the 64-core/128-thread `trj` host, and no qualifying CPU/NUMA/affinity provenance | Separate non-skipped artifacts for each Apple generation and the declared AMD host |
| Large ongoing-session responsiveness | No 4h/24h/72h soak, tail-latency series, post-parse RSS, or recovery exercise | Bounded aged-session workloads with memory attribution and responsiveness percentiles |
| Safe full-`PaneInfo` TTL reuse | The elapsed-time cache was removed because volatile mux metadata lacks revision authority | At most revision-bound, cross-client/cross-transport in-flight singleflight with correctness proof |

The current campaign therefore remains open even though the two historical
source-level fixes remain valuable.
