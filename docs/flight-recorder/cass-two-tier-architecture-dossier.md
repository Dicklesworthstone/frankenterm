# Cass Two-Tier Architecture Dossier (`ft-oegrb.1.2`)

Date: 2026-02-12  
Author: `OrangeBarn`  
Source repo studied: `/Users/jemanuel/projects/coding_agent_session_search`

## Scope
Deep source study of cass focusing on:
- ingest + normalization
- raw store vs search layer boundaries
- lexical/semantic/hybrid retrieval
- what should be copied, adapted, or avoided for FrankenTerm flight-recorder search

## Executive Summary
- cass is effectively a two-tier system:
  - canonical persistence in SQLite (normalized conversations/messages plus FTS mirror)
  - search acceleration in Tantivy + optional semantic vector indexes (CVVI exact, HNSW approximate)
- cass query execution is mode-based: `lexical`, `semantic`, `hybrid` with RRF fusion, plus lexical wildcard fallback when sparse.
- For FrankenTerm, the architecture is highly reusable, but ingestion assumptions must change from file/session scan to live pane-event ingestion.

## Evidence Map (cass internals)
### 1) Normalization contract at connector boundary
- Normalized entities are explicit and stable:
  - `NormalizedConversation`: `src/connectors/mod.rs:595`
  - `NormalizedMessage`: `src/connectors/mod.rs:608`
  - connector contract (`detect` + `scan`): `src/connectors/mod.rs:627`

### 2) Canonical storage tier (SQLite + migration discipline)
- Schema/migrations are versioned and transactional:
  - base tables (`agents`, `workspaces`, `conversations`, `messages`): `src/storage/sqlite.rs:413`
  - FTS virtual table: `src/storage/sqlite.rs:495`
  - migration transaction logic: `src/storage/sqlite.rs:3899`
- Safe open path with rebuild signaling:
  - `open_or_rebuild`: `src/storage/sqlite.rs:1044`
  - indexer recovery wrapper: `src/indexer/mod.rs:1210`

### 3) Lexical search tier (Tantivy projection)
- Tantivy schema is separately versioned by schema hash:
  - `SCHEMA_HASH`: `src/search/tantivy.rs:52`
  - open/rebuild behavior: `src/search/tantivy.rs:85`
  - tokenizer registration: `src/search/tantivy.rs:475`
  - schema fields (agent/workspace/content/provenance/prefix fields): `src/search/tantivy.rs:386`
- Projection writes happen on ingest:
  - per-message add path: `src/search/tantivy.rs:267`

### 4) Semantic search tier (exact + approximate)
- CVVI binary vector index is explicit and checksummed:
  - format contract: `src/search/vector_index.rs:1`
  - header magic/version: `src/search/vector_index.rs:47`
  - mmap/preconvert load path: `src/search/vector_index.rs:663`
  - exact top-k and collapsed-by-message search: `src/search/vector_index.rs:800`
- ANN (HNSW) layer is optional and tied to CVVI/embedder identity:
  - module purpose and tradeoffs: `src/search/ann_index.rs:1`
  - build from vector index: `src/search/ann_index.rs:118`
  - search stats (`ef`, estimated recall): `src/search/ann_index.rs:75`

### 5) Query orchestration (mode switch + fusion + fallback)
- `SearchMode`: lexical/semantic/hybrid: `src/search/query.rs:163`
- lexical path with Tantivy primary + SQLite fallback: `src/search/query.rs:2277`
- semantic path with optional ANN: `src/search/query.rs:2479`
- hybrid path with RRF: `src/search/query.rs:2833`
- RRF implementation: `src/search/query.rs:1025`
- wildcard fallback for sparse lexical results: `src/search/query.rs:2740`

### 6) Optional progressive "two-tier semantic quality" module
- Separate fast-embedder + quality-daemon refinement exists:
  - architecture: `src/search/two_tier_search.rs:1`
  - phased search (`Initial` then `Refined`): `src/search/two_tier_search.rs:490`
- This is orthogonal to the main lexical/semantic/hybrid path (not the only "two-tier" in cass).

## cass Two-Tier Architecture (as implemented)
### Tier A: Canonical truth (SQLite)
- SQLite is the source of truth for normalized conversation/message data.
- Inserts and append behavior are transaction-bound and idempotency-aware (`external_id` + conversation append).
- FTS5 table is maintained as a SQLite-local text search mirror.

### Tier B: Search acceleration projections
- Tantivy provides primary lexical retrieval for speed and richer query semantics.
- Semantic retrieval is split into:
  - exact CVVI vector scan (with filtering/collapse)
  - optional HNSW ANN for faster approximate candidate generation
- Query layer chooses mode and fuses results when hybrid.

## Ingest-to-Query Dataflow (cass)
1. Connectors scan sources and emit normalized conversations/messages.  
2. Indexer persists batch to SQLite in one transaction (`insert_conversations_batched`).  
3. Newly inserted messages are projected into Tantivy docs (`add_messages`).  
4. Optional semantic indexing loads all messages for embedding and writes CVVI (+ optional HNSW).  
5. Search request executes by mode:
- lexical: Tantivy first, optional SQLite fallback
- semantic: embed query -> CVVI exact or HNSW candidates + rescoring
- hybrid: lexical + semantic candidate sets fused via RRF

## FrankenTerm Mapping (direct concept map)
| cass concept | cass implementation | FrankenTerm target | recommendation |
|---|---|---|---|
| Normalized conversation/message model | `src/connectors/mod.rs` structs | Recorder event/session normalized envelope | Adapt |
| Canonical relational store + migrations | `src/storage/sqlite.rs` | Recorder archive DB (pane events, snapshots, rules/events) | Copy pattern, adapt schema |
| Search projection index | `src/search/tantivy.rs` | Recorder text index over pane output + metadata | Adapt |
| Search mode orchestration | `src/search/query.rs` | `ft robot search`/future semantic search modes | Copy/adapt |
| Semantic filtering maps | `SemanticFilterMaps` in `vector_index.rs` | Fast filter translation from IDs/paths/sources/panes | Adapt |
| CVVI binary vector format | `src/search/vector_index.rs` | Recorder semantic index artifact | Adapt (format optional) |
| HNSW ANN side index | `src/search/ann_index.rs` | Optional low-latency semantic mode | Adapt |
| Progressive fast->quality rerank | `src/search/two_tier_search.rs` | Potential UI/refinement path | Defer/experimental |

## Copy / Adapt / Avoid Decisions
### Copy (high confidence)
| Item | Why | Compatibility (1-5) |
|---|---|---|
| `SearchMode` split (lexical/semantic/hybrid) | Clean API contract and future-proofs capabilities | 5 |
| RRF fusion pattern | Deterministic, simple, robust to score scale mismatch | 5 |
| Schema-hash guard for index projection rebuild | Practical corruption/mismatch recovery | 5 |
| Explicit filter mapping layer (`SearchFilters` -> typed filter) | Prevents query/storage coupling leakage | 5 |

### Adapt (recommended with changes)
| Item | Required adaptation | Compatibility (1-5) |
|---|---|---|
| SQLite canonical schema | Replace conversation/session assumptions with pane event model | 4 |
| Tantivy document schema | Re-key around pane/event IDs, spans, snapshot IDs, rule IDs | 4 |
| Prefix cache + wildcard fallback | Tune for terminal-token style queries and shorter snippets | 4 |
| CVVI vector index path | Keep concept, but align row identity to recorder events/chunks | 3 |
| HNSW ANN integration | Keep optional and gated by build lifecycle + model governance | 3 |

### Avoid (or defer)
| Item | Why avoid now | Compatibility (1-5) |
|---|---|---|
| Blind destructive index dir wipe on mismatch | Too risky for always-on recorder; prefer generation swap/atomic pointer | 2 |
| File-scan-centric incremental assumptions | FrankenTerm is live-stream first, not transcript-file first | 1 |
| Immediate adoption of `two_tier_search` fast/quality daemon flow | Useful later, but not core blocker for first recorder search version | 2 |

## Assumptions in cass that do not hold for FrankenTerm live mux data
1. Source granularity is transcript files and append-only conversations.
2. Rebuild windows can tolerate temporary empty search index.
3. Embedding pass can be done as an offline batch over "all messages".
4. Session identity is durable per connector artifact (`external_id`), not per live pane lifecycle.
5. Write/read contention profile is lower than live pane stream ingestion.

## Implications for FrankenTerm design
1. Ingest must be event-driven first, scanner second (for backfill/recovery only).
2. Search projections need generation-aware hot-swap, not destructive rebuild in place.
3. Semantic indexing should support incremental chunk queues instead of full re-embed sweeps.
4. Identity model should key by pane/workspace/timestamped event lineage rather than file path.
5. Query filters should include pane/workspace/source/rule-event/snapshot dimensions from day one.

## Candidate reusable modules/patterns (scored)
| Candidate | Reuse type | Score (1-5) | Notes |
|---|---|---|---|
| `src/search/query.rs` mode switch + RRF core | direct logic port | 5 | strongest near-term value |
| `src/search/tantivy.rs` schema-hash + tokenizer wiring | structural template | 4 | adapt fields heavily |
| `src/search/vector_index.rs` filterable exact vector search | partial port | 3 | keep API, adjust row model |
| `src/search/ann_index.rs` stats-rich ANN wrapper | partial port | 3 | preserve optional path |
| `src/storage/sqlite.rs` migration/rebuild signaling | structural template | 4 | adapt for non-destructive rebuild policy |
| `src/indexer/mod.rs` batch ingest + projection split | architectural pattern | 4 | convert scanner assumptions to stream events |
| `src/search/two_tier_search.rs` phased refinement UX | defer | 2 | useful after baseline semantic search is stable |

## Recommended adaptation sequence for FrankenTerm
1. Establish recorder-native canonical schema and event identity model.  
2. Implement lexical projection with schema hash and safe generation rollovers.  
3. Reuse query mode API now (`lexical` first, keep `semantic`/`hybrid` flags).  
4. Add semantic exact search with strict filter mapping and collapsed message/event semantics.  
5. Add ANN only after baseline relevance + observability are stable.  
6. Revisit fast/quality progressive rerank only if latency budget and UX justify it.

## Bottom Line
cass provides a strong template for split canonical-store + projection-search architecture.  
For FrankenTerm, the reusable core is the query-mode orchestration, fusion, and projection discipline; the main required rewrite is the ingest and identity model to support live terminal event streams safely.
