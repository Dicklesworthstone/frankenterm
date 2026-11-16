# Cross-Project Extraction Matrix (`ft-oegrb.1.4`)

Date: 2026-02-12  
Author: `WildSpring`  
Inputs:
- `docs/flight-recorder/frankensqlite-append-log-dossier.md`
- `docs/flight-recorder/cass-two-tier-architecture-dossier.md`
- `docs/flight-recorder/xf-hybrid-retrieval-dossier.md`
- `docs/flight-recorder/tantivy-schema-v1.md`

## Goal
Define a single synthesis plan for FrankenTerm flight-recorder architecture by explicitly choosing what to copy, adapt, or avoid from:
1. FrankenSQLite (`/Users/jemanuel/projects/frankensqlite`)
2. cass (`/Users/jemanuel/projects/coding_agent_session_search`)
3. xf (`/Users/jemanuel/projects/xf`)

This document is meant to be implementation-driving and self-contained for downstream beads.

## Executive Synthesis
- Use **FrankenSQLite** as the primary source for durability mechanics:
  - append-only segment discipline
  - torn-tail recovery
  - bounded backpressure and async repair split
- Use **cass** as the primary source for search architecture shape:
  - canonical store vs projection split
  - lexical/semantic/hybrid mode contracts
  - RRF fusion model and filter-mapping discipline
- Use **xf** as the primary source for production operations:
  - embedding daemon/job lifecycle
  - retry/backoff/overload controls
  - vector cache/staleness checks and deterministic artifact handling

## Extraction Matrix

| Decision axis | FrankenSQLite | cass | xf | Synthesis default for FrankenTerm | Bead impact |
|---|---|---|---|---|---|
| Canonical data plane | Strong append-log + integrity semantics | SQLite canonical model, conversation-centric | SQLite canonical + FTS + projection tables | Recorder-native append log as source-of-truth + canonical metadata tables | `.2`, `.3` |
| Append format + crash behavior | Excellent (segment headers, checksums, torn-tail prefix recovery) | Not primary focus | Not primary focus | Adopt FrankenSQLite-style append semantics for recorder segments | `.3.2`, `.3.4`, `.7.3` |
| Backpressure posture | Strong (`Busy`/drop-first bounded admission) | Moderate | Strong operational overload controls | Explicit bounded ingest queue with configurable overflow policy | `.2.6`, `.3.2`, `.3.6` |
| Repair strategy | Async repair pipeline + structured fallback | N/A | N/A for raw durability | Phase 1 checksum + deterministic fallback; phase 2 optional sidecar/repair | `.3.4`, `.7.2`, `.8.4` |
| Lexical indexing | N/A | Tantivy projection + schema-hash rebuild behavior | Tantivy schema/query + operational health checks | Tantivy projection from canonical offsets with non-destructive rebuild generation; contract anchored at `docs/flight-recorder/tantivy-schema-v1.md` | `.4.1`..`.4.5` |
| Semantic indexing | RaptorQ/object durability concepts | CVVI + optional ANN (HNSW) roadmap | Exact vector artifacts + two-tier fast/quality flow | Start exact semantic retrieval + hybrid fusion; ANN deferred until scale justifies | `.5.1`..`.5.4` |
| Hybrid ranking | N/A | RRF orchestration | Deterministic RRF + two-tier blend knobs | Standardize deterministic RRF core; optional two-tier mode as gated extension | `.5.4`, `.6.4` |
| Query mode contract | N/A | lexical/semantic/hybrid | lexical/semantic/hybrid/two-tier | Public API exposes lexical/semantic/hybrid first; two-tier behind feature gate | `.6.1`..`.6.4` |
| Identity/filter model | Page/frame/object oriented | conversation/message/source filters | tweet/dm-like typed IDs | Recorder-native typed identity: workspace/session/pane/event/span/rule/snapshot | `.2.1`, `.4.1`, `.5.2` |
| Background embedding ops | N/A | Partial/offline-centric | Strong daemon + job table + retries + overload cap | Adopt xf-style background embedding job system | `.5.6`, `.6.3`, `.8.4` |
| Rebuild/recovery operations | Strong explicit recovery routing | Some rebuild signaling | Strong operational checks/caching | One-way canonical->projection rebuild pipeline with full observability | `.4.4`, `.7.4`, `.8.2` |
| Governance/privacy | Not recorder-specific | Not recorder-specific | Some operational controls | Define explicit recorder privacy/redaction/access tiers before broad rollout | `.2.5`, `.8.3`, `.6.5` |

## Copy / Adapt / Avoid (Combined)

### Copy now
- FrankenSQLite:
  - append-only segment + checksum + torn-tail valid-prefix strategy
  - bounded admission/overflow semantics
  - hot-path durability vs async repair separation
- cass:
  - lexical/semantic/hybrid query contract
  - RRF fusion baseline
  - canonical-store vs projection-store boundary
- xf:
  - embedding daemon/job control model
  - overload caps and retry/backoff behavior
  - vector cache staleness checks

### Adapt before use
- FrankenSQLite WAL/page vocabulary -> recorder event vocabulary.
- cass conversation/message identity -> pane/session/event/span identity.
- xf doc-type assumptions -> recorder-native typed identity taxonomy.
- cass/xf rebuild behavior -> generation swap with immutable canonical log retention.

### Avoid initially
- Full FrankenSQLite storage stack transplant.
- Immediate ANN/HNSW complexity before exact semantic baseline quality is proven.
- Destructive in-place rebuild flows for active recorder systems.
- Making semantic mode mandatory before lexical stability and privacy controls are validated.

## Proposed Architecture Decisions (ready for ADR drafting in `ft-oegrb.1.5`)

### AD-01 Canonical source of truth
- Recorder append log is canonical.
- Indexes are derived projections and fully rebuildable.

### AD-02 Ingest backpressure contract
- Recorder ingest is bounded and explicit.
- Overflow policy is configurable and observable; no hidden unbounded queue.

### AD-03 Query mode contract
- API supports `lexical`, `semantic`, `hybrid` from first release.
- `hybrid` uses deterministic RRF core.
- `two-tier` optional path is feature-gated.

### AD-04 Recovery philosophy
- Phase 1: checksum-verified append + replay/checkpoint determinism.
- Phase 2: optional sidecar/repair coding if reliability goals require it.

### AD-05 Operational model
- Embedding generation runs as background jobs with cancellation/progress and overload protections.
- Query path remains available in lexical mode when semantic path degrades.

## Sequencing Recommendations
1. Finalize recorder event identity schema (`ft-oegrb.2.1`).
2. Build canonical append path + deterministic replay/checkpoint (`ft-oegrb.3.2`, `ft-oegrb.3.4`).
3. Ship lexical index and query first (`ft-oegrb.4.1`..`.4.5`) starting from `docs/flight-recorder/tantivy-schema-v1.md`.
4. Add semantic exact retrieval + hybrid fusion (`ft-oegrb.5.1`..`.5.4`).
5. Layer operational daemonization, budgets, and policy gates (`ft-oegrb.5.6`, `.6.5`, `.8.3`).
6. Prove with load/chaos/recovery suites before broad rollout (`ft-oegrb.7.*`, `.8.1`).

## Open Design Questions for ADR
1. Should recorder overflow default to `Busy` fail-fast or sampling/degrade mode?
2. What minimum retention guarantees are required before enabling semantic backfill?
3. Is sidecar repair coding a hard requirement for v1, or a gated v1.1+ capability?
4. What rollout SLOs (ingest latency, query p95, recovery time) must be met for default-on?

## Bottom Line
The synthesis is clear:
- **FrankenSQLite durability patterns** + **cass search architecture** + **xf operational hardening**.
- This combination gives FrankenTerm a practical path to high-throughput, replay-trustworthy recording with hybrid retrieval and production-grade operational controls.
