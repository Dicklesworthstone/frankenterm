# FrankenSQLite Append-Log + Self-Healing Durability Dossier (`ft-oegrb.1.1`)

Date: 2026-02-12  
Author: `WildSpring`  
Source repo studied: `/Users/jemanuel/projects/frankensqlite`

## Scope
Deep source study of FrankenSQLite focused on recorder-relevant design surfaces:
- append-only logging formats and torn-tail behavior
- concurrent writer/backpressure orchestration
- WAL integrity + self-healing/repair pathways
- asynchronous repair pipeline patterns
- operational/performance evidence useful for FrankenTerm adoption

## Executive Summary
- FrankenSQLite has multiple reusable primitives for an ultra-performant recorder design:
  - append-only segment formats with fixed headers, checksums, deterministic offsets, and torn-tail tolerance
  - strict non-blocking overload behavior (`Busy`/drop-on-overflow semantics) instead of unbounded queues
  - separation of commit critical path vs asynchronous repair generation
  - explicit integrity routing for corruption outcomes (repair-attempt vs truncate/fallback)
- The best adaptation for FrankenTerm is architectural, not literal database-port:
  - reuse logging and durability patterns
  - adapt record schema and queueing behavior to mux ingress/egress events
  - keep lexical/semantic indexing decoupled from raw append log
- Important caveat from FrankenSQLite itself: the README explicitly notes storage-stack wiring is still phase-progressive and not the default runtime backend yet. Treat it as a pattern source, not a drop-in component.

## Evidence Map (FrankenSQLite internals)

### 1) WAL chain integrity and append semantics
- WAL file abstraction with explicit frame append/read/reset logic:
  - `crates/fsqlite-wal/src/wal.rs`
- Open path validates header checksum and scans frame chain until first invalid frame (salt or checksum mismatch), yielding valid prefix:
  - `crates/fsqlite-wal/src/wal.rs`
- Checksum helpers and corruption routing:
  - `crates/fsqlite-wal/src/checksum.rs`
  - `attempt_wal_fec_repair`: `crates/fsqlite-wal/src/checksum.rs`
  - `recover_wal_frame_checksum_mismatch`: `crates/fsqlite-wal/src/checksum.rs`

### 2) `.wal-fec` sidecar pattern and recovery
- Append-only WAL-FEC sidecar model:
  - metadata record + repair symbol records
  - source symbols remain in WAL; sidecar stores only repair payloads
  - `crates/fsqlite-wal/src/wal_fec.rs`
- Sidecar lifecycle and parsing APIs:
  - `ensure_wal_with_fec_sidecar`
  - `append_wal_fec_group`
  - `scan_wal_fec`
  - `identify_damaged_commit_group`
  - `recover_wal_fec_group_with_decoder`
  - `recover_wal_fec_group_with_config`
  - all in: `crates/fsqlite-wal/src/wal_fec.rs`
- Recovery behavior is configuration-driven (`recovery_enabled`) with explicit fallback reasons and structured recovery logs:
  - `crates/fsqlite-wal/src/wal_fec.rs`

### 3) Asynchronous repair pipeline (critical-path isolation)
- WAL-FEC repair pipeline uses bounded sync-channel queue + dedicated worker thread + counters:
  - `WalFecRepairPipeline`: `crates/fsqlite-wal/src/wal_fec.rs`
- Backpressure is explicit:
  - queue full -> typed error
  - shutdown/cancel paths are explicit
  - `enqueue`, `flush`, `shutdown`: `crates/fsqlite-wal/src/wal_fec.rs`
- Design pattern: keep commit path focused on source durability, do repair generation asynchronously.

### 4) Append-only symbol log and marker stream patterns
- Symbol log:
  - fixed segment header with checksum
  - append-only active segment
  - torn-tail tolerant scans
  - rotation to immutable segments
  - `crates/fsqlite-core/src/symbol_log.rs`
- Marker stream:
  - fixed record size and O(1) seek formulas
  - valid-prefix recovery under torn tail
  - dense sequence invariant checks
  - `crates/fsqlite-core/src/commit_marker.rs`

### 5) Overload/backpressure posture
- Bulkhead admission gate rejects overflow immediately with `Busy`:
  - `crates/fsqlite-core/src/lib.rs`
- Two-phase bounded commit queue with reservation/send sequencing and explicit capacity:
  - `crates/fsqlite-core/src/commit_repair.rs`
- These are strong templates for recorder ingest queue semantics where we must avoid unbounded memory growth.

### 6) Performance and durability evidence
- Microbench matrix and tuning guidance:
  - `docs/raptorq_microbench_matrix.md`
- Crash and WAL integrity harnesses:
  - `crates/fsqlite-harness/tests/bd_3a7d_crash_recovery_wal_integrity.rs`
  - `crates/fsqlite-harness/tests/bd_m0l2_raptorq_e2e_integration.rs`

### 7) Maturity caveat (important)
- README states current runtime still centers on in-memory backend with snapshot persistence while full storage stack wiring remains in-progress:
  - `README.md` ("Current Implementation Status")
- Implication: extract and adapt architecture patterns; do not assume every component is production-hardened in its current integration form.

## Copy / Adapt / Avoid Decisions for FrankenTerm

### Copy (high confidence)
| Item | Why | Compatibility (1-5) |
|---|---|---|
| Segment header + record checksum discipline | Detects torn/corrupt tails cheaply and deterministically | 5 |
| Torn-tail valid-prefix scanning | Crash-safe append behavior without destructive recovery | 5 |
| Bounded queue + explicit backpressure errors | Prevents runaway memory under burst ingest | 5 |
| Structured recovery outcomes and fallback reasons | Operator/agent observability for reliability incidents | 5 |
| Async repair pipeline split from hot path | Preserves ingest latency while enabling stronger durability | 5 |

### Adapt (recommended)
| Item | Required adaptation | Compatibility (1-5) |
|---|---|---|
| WAL-FEC sidecar concept | Replace page/frame semantics with recorder event batch/group semantics | 4 |
| Marker stream sequence logic | Re-key to recorder sequence/correlation IDs and pane/session dimensions | 4 |
| Two-phase reservation queue | Map to recorder append batches and indexer handoff checkpoints | 4 |
| Bulkhead policy | Translate `Busy` behavior into recorder backpressure + drop/degrade policies | 4 |
| Hash/checksum stack | Use similar layered integrity checks but tuned for text event payloads | 4 |

### Avoid (for first implementation)
| Item | Why avoid now | Compatibility (1-5) |
|---|---|---|
| Direct port of full FrankenSQLite storage stack | Scope and integration risk too high for initial recorder feature | 2 |
| Immediate RaptorQ-everywhere requirement | Adds major complexity before baseline recorder correctness/perf is proven | 2 |
| Mixing search index durability with raw log durability in one subsystem | Increases blast radius; keep source log and projections independent | 1 |

## Recorder-Specific Adaptation Blueprint

### A) Raw append plane (source of truth)
1. Introduce recorder segment file format:
- fixed file header (magic/version/config checksum)
- length-prefixed event records
- per-record integrity checksum + sequence fields
2. Adopt torn-tail scan behavior:
- on open/recovery, retain valid prefix, ignore incomplete suffix
3. Keep write API append-only with deterministic offsets.

### B) Backpressure and admission control
1. Implement bounded ingest queue (capacity + drop/busy behavior).
2. Expose metrics:
- queue depth
- rejected appends
- append latency/throughput
- tail truncation events
3. Add policy knobs: fail-fast, degrade-to-sampling, or block-with-timeout.

### C) Repair and validation plane
1. Defer full erasure coding to phase 2+, but reserve API boundaries now.
2. Start with:
- checksum verification
- structured recovery logs
- replay/checkpoint reconciliation
3. Add optional sidecar repair mechanism only after baseline recorder pipeline is stable.

### D) Indexing decoupling
1. Treat append log as canonical.
2. Tantivy/vector indexes consume offsets/checkpoints asynchronously.
3. Rebuild paths must never mutate canonical log segments.

## Proposed immediate design choices for `ft-oegrb` tracks
- `ft-oegrb.3`: adopt segment + checkpoint + bounded queue model directly from studied patterns.
- `ft-oegrb.4`: drive ingestion from canonical offsets, preserving idempotent replay semantics.
- `ft-oegrb.7`: include torn-tail and checksum corruption fault-injection tests modeled after FrankenSQLite harness style.
- `ft-oegrb.8`: include explicit fallback behavior docs (what happens when repair/validation fails).

## Risks and Mitigations
| Risk | Why it matters | Mitigation |
|---|---|---|
| Over-engineering early with full FEC pipeline | Delays core recorder value and increases complexity | Stage rollout: checksum + append correctness first, sidecar/FEC later |
| Queue policy mismatch under burst mux output | Could drop valuable data or stall runtime | Make overflow policy explicit + measurable + configurable |
| Canonical/projection coupling | Index incidents could threaten primary log | Strict one-way flow from append log -> projections |
| Hidden recovery behavior | Difficult incident triage | Structured recovery logs and deterministic fallback taxonomy |

## Bottom Line
FrankenSQLite provides highly relevant architecture patterns for FrankenTerm’s recorder:
- append-only segment discipline,
- bounded backpressure-first ingestion,
- deterministic recovery behavior,
- and hot-path vs repair-path separation.

The right path is to **adapt these patterns** to recorder event semantics and phase delivery, not to transplant FrankenSQLite wholesale.
