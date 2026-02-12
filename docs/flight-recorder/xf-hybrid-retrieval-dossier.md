# xf Hybrid Retrieval Architecture Dossier (`ft-oegrb.1.3`)

Date: 2026-02-12  
Author: `SilverHarbor`  
Source repo studied: `/Users/jemanuel/projects/xf`

## Scope
Deep source study of xf focused on:
- high-volume ingest/index pipeline
- lexical/semantic/hybrid retrieval internals
- ranking and query API behavior
- production-operational reliability patterns
- delta vs cass for FrankenTerm flight-recorder planning

## Executive Summary
- xf is a dual-store search system: canonical SQLite tables + Tantivy lexical projection + separate binary vector index artifacts (`vector.idx`, optional fast/quality tier files).
- Query orchestration is explicit mode-based (`lexical`, `semantic`, `hybrid`, `two-tier`) with deterministic RRF fusion and optional fast/quality semantic blending.
- Production operations are first-class: background embedding daemon, job table, cancellation/progress, client retries with auto-spawn/backoff, and in-flight overload guardrails.
- Vector retrieval is exact (SIMD dot-product over normalized vectors), with mmap fast path and in-memory fallback, plus stale-cache checks against DB mtime/count.
- For FrankenTerm, xf contributes strong operational search patterns; cass remains stronger on connector normalization and ANN-ready semantic architecture.

## Evidence Map (xf internals)

### 1) Entry points and configuration layering
- Layered config precedence (defaults -> user config -> env -> CLI): `src/config.rs:1`, `src/config.rs:266`.
- Rich search API surface with explicit mode and two-tier controls:
  - `SearchArgs` and mode flags: `src/cli.rs:204`, `src/cli.rs:257`, `src/cli.rs:289`.
  - `--fast-only` / `--quality-only` / `--blend-factor` imply two-tier mode: `src/cli.rs:291`, `src/cli.rs:298`, `src/cli.rs:305`.
- Index pipeline entry:
  - archive validation, storage/index open, per-type parse/store/index loop: `src/main.rs:830`, `src/main.rs:915`, `src/main.rs:994`.
  - lexical commit + reload then embeddings path selection: `src/main.rs:1093`.
  - daemon background embedding first, synchronous fallback: `src/main.rs:1109`, `src/main.rs:1138`.

### 2) Canonical storage + derived projections
- SQLite performance and consistency pragmas (WAL/NORMAL/FK/cache): `src/storage.rs:128`.
- Schema versioning + migrations:
  - schema version control: `src/storage.rs:18`, `src/storage.rs:171`.
  - model-aware embedding migration: `src/storage.rs:222`.
- Canonical and projection tables:
  - core data tables + standalone FTS tables: `src/storage.rs:275`, `src/storage.rs:363`.
  - embeddings table keyed by (`doc_id`,`doc_type`,`model_id`): `src/storage.rs:385`.
  - background embedding job table with unique (`db_path`,`model_id`): `src/storage.rs:398`.
- Transactional writes and stale-FTS cleanup:
  - tweet path batch-deletes FTS before insert: `src/storage.rs:490`.
  - all primary content writes are transaction-based: `src/storage.rs:485`, `src/storage.rs:548`, `src/storage.rs:593`.
- Repair/maintenance hooks:
  - idempotent DB optimize + WAL checkpoint: `src/storage.rs:945`.
  - full FTS rebuild from canonical tables: `src/storage.rs:955`.

### 3) Lexical retrieval engine (Tantivy)
- Tantivy schema includes:
  - primary text field with positions/frequencies,
  - prefix companion field for prefix matching,
  - typed metadata and timestamp fields: `src/search.rs:213`.
- Query behavior:
  - phrase queries avoid prefix field (no positions); non-phrase queries include both text + prefix: `src/search.rs:540`.
  - optional doc-type filtering via BooleanQuery/TermQuery: `src/search.rs:555`.
  - highlighting and snippet extraction integrated in search path: `src/search.rs:583`.
- Bulk ID materialization optimized for semantic/hybrid backfill:
  - `get_by_ids` builds typed lookup query and preserves input order: `src/search.rs:686`, `src/search.rs:713`.
- Lexical index health checks cover directory/meta/version/segment count/doc count/sample latency/size:
  - `src/search.rs:829`, `src/search.rs:937`, `src/search.rs:986`.

### 4) Semantic retrieval and vector index mechanics
- Vector index format is explicit and validated:
  - magic/version/header layout: `src/vector.rs:16`, `src/vector.rs:35`.
  - deterministic record ordering and atomic temp->rename writes: `src/vector.rs:309`, `src/vector.rs:352`, `src/vector.rs:389`.
- Two-tier vector artifacts are first-class:
  - default, fast, quality filenames: `src/vector.rs:243`, `src/vector.rs:245`, `src/vector.rs:247`.
  - model-filtered write path for named indexes: `src/vector.rs:407`.
- Query execution:
  - mmap-backed top-k scans with allocation-minimizing heap strategy: `src/vector.rs:679`, `src/vector.rs:697`.
  - in-memory fallback and unified interface: `src/vector.rs:806`, `src/vector.rs:839`.
- Process-level vector cache in CLI:
  - once-only load with init lock + stale check by DB mtime/count: `src/main.rs:50`, `src/main.rs:96`, `src/main.rs:241`.
  - fast/quality named index lazy load helpers: `src/main.rs:150`, `src/main.rs:178`.

### 5) Ranking/orchestration behavior
- Search modes and aliases are centralized: `src/hybrid.rs:50`.
- Hybrid RRF details:
  - `RRF_K = 60`, deterministic tie-breaking, candidate amplification (`x3`): `src/hybrid.rs:43`, `src/hybrid.rs:144`, `src/hybrid.rs:217`.
- cmd_search mode paths:
  - lexical progressive fetch for post-filters/sorting: `src/main.rs:1329`.
  - semantic with embedder resolution + hash fallback: `src/main.rs:1357`, `src/main.rs:1384`.
  - hybrid lexical+semantic with RRF and typed backfill: `src/main.rs:1434`, `src/main.rs:1480`.
  - two-tier fast+quality semantic blend then final RRF with lexical: `src/main.rs:1538`, `src/main.rs:1635`, `src/main.rs:1650`.
- Two-tier metrics primitives (latency, rank-change, correlation, skip reason) are defined and loggable:
  - `src/hybrid.rs:325`, `src/hybrid.rs:374`.

### 6) Reliability/operational patterns (daemon stack)
- Protocol:
  - length-prefixed MessagePack envelope with version/request correlation: `src/daemon/protocol.rs:1`, `src/daemon/protocol.rs:10`.
  - explicit error-code taxonomy, including overload/job/db failures: `src/daemon/protocol.rs:234`.
- Daemon core:
  - UDS daemon with owner-only socket perms and idle timeout shutdown: `src/daemon/core.rs:205`, `src/daemon/core.rs:230`, `src/daemon/core.rs:247`.
  - in-flight guard with hard cap (`MAX_IN_FLIGHT=64`) and overload response: `src/daemon/core.rs:29`, `src/daemon/core.rs:337`, `src/daemon/core.rs:367`.
  - model manager + worker initialization on startup: `src/daemon/core.rs:187`.
- Client:
  - auto-spawn if socket missing/stale, exponential backoff connect, request retry loop: `src/daemon/client.rs:133`, `src/daemon/client.rs:166`, `src/daemon/client.rs:227`.
  - request/response timeout + max payload guard: `src/daemon/client.rs:274`, `src/daemon/client.rs:278`.
- Worker/job lifecycle:
  - bounded worker channel (`mpsc::channel(32)`), cancellation flag, chunked progress updates: `src/daemon/worker.rs:76`, `src/daemon/worker.rs:68`, `src/daemon/worker.rs:264`, `src/daemon/worker.rs:448`.
  - content-hash reuse path avoids redundant embedding recomputation: `src/daemon/worker.rs:326`, `src/daemon/worker.rs:367`.
- Model/resource governance:
  - LRU model eviction on capacity pressure: `src/daemon/models.rs:33`, `src/daemon/models.rs:210`.
  - resource policy plumbing (nice/ionice/thread bounds/idle timeout/memory-pressure monitor): `src/daemon/resource.rs:17`, `src/daemon/resource.rs:194`, `src/daemon/resource.rs:308`.

## xf Ingest-to-Query Dataflow
1. `xf index` validates archive and parses selected data types: `src/main.rs:830`, `src/main.rs:994`.  
2. Canonical rows are persisted transactionally in SQLite + FTS tables updated: `src/storage.rs:485`, `src/storage.rs:490`.  
3. Tantivy lexical docs are written and committed: `src/main.rs:916`, `src/main.rs:1093`.  
4. Embeddings are generated (daemon background preferred, sync fallback) and vector files written: `src/main.rs:1109`, `src/daemon/worker.rs:460`.  
5. `xf search` chooses mode and combines lexical/semantic candidates per mode logic: `src/main.rs:1329`, `src/main.rs:1434`, `src/main.rs:1538`.  
6. Results are post-filtered/sorted/paginated and emitted as text/json/toon/csv: `src/main.rs:1708`.

## Portable Patterns for FrankenTerm (from xf)
| Pattern | xf evidence | Portability | Notes |
|---|---|---|---|
| Background embedding as resumable job system | `src/storage.rs:398`, `src/daemon/core.rs:540`, `src/daemon/worker.rs:176` | High | Great fit for recorder semantic backfill and live-progress UX |
| Cache + staleness metadata for vector index | `src/main.rs:50`, `src/main.rs:241` | High | Useful for long-running `ft` daemon processes |
| Deterministic fusion pipeline (RRF + explicit tie-breakers) | `src/hybrid.rs:144`, `src/hybrid.rs:190` | High | Directly portable to `ft robot search` ranking composition |
| Typed doc identity in vector artifacts | `src/vector.rs:64`, `src/vector.rs:264` | Medium | Adapt type model to pane/span/rule/snapshot dimensions |
| Mmap-first, DB-fallback semantic index load | `src/vector.rs:839` | High | Good safety/perf tradeoff for rolling deploys |
| Overload + timeout guardrails in daemon RPC | `src/daemon/core.rs:367`, `src/daemon/client.rs:274` | High | Critical for multi-agent environments |
| Resource governance (LRU + nice/ionice/threads) | `src/daemon/models.rs:210`, `src/daemon/resource.rs:194` | Medium-High | Operationally valuable; thread/priority knobs map cleanly |

## Non-Portable Assumptions / Adaptations Required
| xf assumption | Evidence | Why non-portable for FrankenTerm | Adaptation |
|---|---|---|---|
| Fixed X-specific doc types (`tweet/like/dm/grok`) | `src/vector.rs:66` | Recorder needs pane/event/snapshot/rule dimensions | Generalize typed IDs and filter maps |
| Archive-file ingest model (`data/` directory) | `src/main.rs:856` | Recorder is live stream first | Add event-ingest pipeline and backfill scanner as secondary |
| Force rebuild path deletes DB/index directly | `src/main.rs:892` | Risky for always-on recorder | Prefer generation swap + atomic pointer cutover |
| Semantic default can degrade to hash similarity | `src/main.rs:1384`, `src/daemon/worker.rs:164` | Hash fallback may mismatch expected semantic quality | Enforce explicit model governance per mode |
| Daemon is Unix-socket centric | `src/daemon/core.rs:1` | Cross-platform/service integration constraints | Abstract transport for future remote mode |
| Job-status API without DB scope returns empty | `src/daemon/core.rs:669` | Fleet-level observability gaps | Add global job registry or per-project discovery |

## Delta vs cass (for FrankenTerm use-case)

Reference cass dossier: `docs/flight-recorder/cass-two-tier-architecture-dossier.md:46`.

### Where xf is stronger than cass
| Area | xf advantage | Evidence |
|---|---|---|
| Operational daemonization | Built-in daemon client/server with auto-spawn/retries/timeouts/in-flight cap | `src/daemon/client.rs:133`, `src/daemon/core.rs:337` |
| Background job lifecycle | First-class embedding job table + progress/cancel APIs | `src/storage.rs:398`, `src/daemon/protocol.rs:87` |
| Two-tier blending in core search path | Two-tier mode is integrated into main query execution, not just optional side module | `src/main.rs:1538`, `src/hybrid.rs:261` |
| Vector artifact pragmatism | Atomic named vector artifact writes (default/fast/quality) + mmap fast path | `src/vector.rs:407`, `src/vector.rs:839` |
| Resource controls | Explicit nice/ionice/thread/idle knobs and memory-pressure monitor | `src/daemon/resource.rs:194`, `src/daemon/resource.rs:308` |

### Where cass is stronger than xf
| Area | cass advantage (from dossier) | Why it matters for FrankenTerm |
|---|---|---|
| Connector normalization boundary | Explicit normalized connector contracts (`NormalizedConversation`/`NormalizedMessage`) | Cleaner ingest abstraction for multi-source recorder feeds |
| ANN strategy | Optional HNSW ANN with stats/recall tuning (`ann_index`) | Better long-term latency scaling for very large corpora |
| Lexical fallback richness | Documented lexical fallback behavior for sparse matches | Helpful for noisy terminal text and partial tokens |
| Schema-hash rebuild discipline | Strong explicit schema-hash projection rebuild guards | Useful for safe rolling migrations across recorder versions |

### Net assessment
- Use xf as the primary template for **operations** (daemon/job/retry/resource/caching).
- Use cass as the primary template for **semantic architecture shape** (connector abstraction, ANN roadmap, clearer schema governance).
- Synthesis should intentionally combine both: xf runtime ops + cass data-model discipline.

## Synthesis Hooks for `ft-oegrb.1.4` (cross-project extraction matrix)
| Decision axis | xf evidence | cass evidence | Suggested synthesis default |
|---|---|---|---|
| Query modes contract | `src/hybrid.rs:50` | `docs/flight-recorder/cass-two-tier-architecture-dossier.md:57` | Keep `lexical/semantic/hybrid`; gate two-tier as optional extension |
| Fusion algorithm | `src/hybrid.rs:144` | `docs/flight-recorder/cass-two-tier-architecture-dossier.md:61` | Standardize on deterministic RRF core |
| Semantic index format | `src/vector.rs:249` | `docs/flight-recorder/cass-two-tier-architecture-dossier.md:47` | Start exact mmap format; add ANN sidecar later |
| Background embedding execution | `src/daemon/worker.rs:135` | `docs/flight-recorder/cass-two-tier-architecture-dossier.md:84` | Adopt xf-style job daemon for live systems |
| Rebuild strategy | `src/main.rs:892` | `docs/flight-recorder/cass-two-tier-architecture-dossier.md:127` | Avoid destructive rebuilds; use generation swap |
| Filter/identity layer | `src/vector.rs:66` | `docs/flight-recorder/cass-two-tier-architecture-dossier.md:101` | Replace fixed type codes with recorder-native typed IDs |

## Recommended Adaptation Sequence (FrankenTerm)
1. Define recorder-native canonical identity/filter model (pane/workspace/span/rule/snapshot).  
2. Implement lexical projection + search modes API (`lexical` first, semantic flags present).  
3. Add xf-style background embedding job service with cancellation/progress and overload guards.  
4. Add exact vector retrieval with mmap artifact + stale-cache checks.  
5. Add deterministic hybrid RRF fusion.  
6. Add optional two-tier fast/quality blend only after baseline relevance and ops telemetry are stable.  
7. Add ANN path only when corpus size/latency budget justifies complexity.

## Bottom Line
xf offers a strong, production-minded retrieval operations blueprint (daemonization, job control, retry/backoff, resource policies) with robust deterministic hybrid retrieval.  
For FrankenTerm flight-recorder search, the best path is to port xf’s operational patterns while pairing them with cass-style normalization and long-term ANN-ready architecture.
