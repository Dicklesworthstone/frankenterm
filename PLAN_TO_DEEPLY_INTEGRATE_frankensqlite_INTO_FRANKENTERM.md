# PLAN_TO_DEEPLY_INTEGRATE_frankensqlite_INTO_FRANKENTERM

Status: `completed`  
Track: `ft-2vuw7.1.3`  
Current slice completed in this revision: `ft-2vuw7.1.3.8` (P8)

## 0. Intent and framing

This plan defines how FrankenTerm (`ft`) will deeply and natively integrate FrankenSQLite (`frankensqlite`) as part of the long-term terminal-runtime control plane, not as a shallow bolt-on adapter.

Deep native integration in this context means:
- integration is aligned with `ft`'s core architecture boundaries and policy model;
- backend behavior is explicit, testable, and observable;
- upstream changes in `frankensqlite` are allowed where needed;
- crate/subcrate extraction is allowed where it improves stable interfaces.

---

## 1. P1 - Integration objectives, hard constraints, non-goals, and success criteria

### 1.1 Objectives

1. Establish a first-class recorder storage backend path for FrankenSQLite behind `RecorderStorage`, preserving the existing backend identity model (`AppendLog` and `FrankenSqlite`) in `crates/frankenterm-core/src/recorder_storage.rs`.
2. Preserve FrankenTerm's platform direction: swarm-native orchestration, machine-optimized control surfaces (`ft robot`, MCP), and policy-gated actions/reads as documented in `README.md` and `AGENTS.md`.
3. Maintain strict canonical-vs-projection separation:
- canonical recorder durability plane remains authoritative;
- lexical/semantic search remain derived projections and rebuildable.
4. Improve durability and concurrency characteristics using proven FrankenSQLite patterns (bounded admission, deterministic recovery, explicit failure classes) without forcing premature adoption of unfinished upstream runtime wiring.
5. Keep integration implementation-driven: each migration phase must map to measurable SLO/acceptance gates and produce deterministic rollback/fallback behavior.
6. Avoid technical debt and compatibility shims; prefer direct, target-architecture transitions per repo policy.

### 1.2 Hard constraints

1. Safety/language:
- Rust 2024 workspace expectations;
- `#![forbid(unsafe_code)]` posture preserved end-to-end;
- no unsafe escape hatches introduced by integration glue.
2. Runtime/control-plane invariants:
- `ft` observe-vs-act split remains intact;
- policy-gated reads/sends and secret redaction behavior remain intact for all robot/MCP surfaces.
3. Storage contract invariants:
- `RecorderStorage` trait remains the durability boundary;
- append idempotency (`batch_id`), monotonic offsets/checkpoints, and explicit error classification semantics remain mandatory.
4. Operational constraints:
- no unbounded hidden queues; explicit overload/backpressure semantics only;
- deterministic fallback policy (backend startup/runtime failure behavior must be explicit and observable).
5. Compatibility and migration constraints:
- no feature loss across existing `ft` capabilities (watch/state/get-text/search/events/workflows/audit);
- canonical data safety over convenience during migration;
- no destructive migration path without explicit rollback strategy.
6. Upstream realism constraints:
- FrankenSQLite currently exposes a broad crate architecture, but its default connection path is still centered on `MemDatabase` + snapshot persistence while full pager/WAL/B-tree wiring is still in progress (see `/dp/frankensqlite/README.md` "Current Implementation Status");
- therefore integration must target stable boundaries and phase-gate deeper backend coupling.
7. Performance constraints:
- integration must not regress existing storage/query latency budgets and queue health thresholds codified in `crates/frankenterm-core/src/storage_targets.rs`.

### 1.3 Non-goals

1. Not a wholesale immediate transplant of the full FrankenSQLite stack into FrankenTerm.
2. Not an all-at-once replacement of every `rusqlite` usage in `crates/frankenterm-core/src/storage.rs`.
3. Not immediate dependency on advanced FrankenSQLite durability features (for example, full repair/FEC pipelines) before baseline backend correctness/observability is proven.
4. Not coupling search/index projection internals to storage backend internals.
5. Not preserving legacy behavior via long-lived compatibility wrappers/shims.
6. Not introducing hidden retries, silent data drops, or implicit failover semantics.
7. Not changing user-visible control surfaces (`ft`/robot/MCP contracts) in this P1 scope.

### 1.4 Success criteria (P1 exit gate)

P1 is complete when the following are true:

1. Objectives/constraints/non-goals are explicit and implementation-driving (this section), and are traceable to concrete architecture/runtime sources in both repos.
2. The constraints are specific enough to unblock:
- P2 integration-pattern evaluation,
- P3 subsystem placement decisions,
- P4 API/extraction roadmap.
3. The plan explicitly allows upstream modifications and crate extraction while protecting FrankenTerm's control-plane and safety invariants.
4. The plan establishes no-ambiguity boundaries that prevent scope explosion ("transplant everything") during downstream beads.

---

## 2. P2 - Evaluate integration patterns (embed/service/adapter/hybrid)

Status: `completed` (`ft-2vuw7.1.3.2`)

### 2.1 Evaluation criteria

Patterns are scored against the following criteria:

1. Alignment with current FrankenTerm storage and recorder boundaries (`RecorderStorage` trait, canonical-vs-projection split).
2. Feasibility given current FrankenSQLite maturity (default runtime still centered on `MemDatabase` + snapshot persistence, with full pager/WAL/B-tree wiring not yet default).
3. Safety/policy compatibility with FrankenTerm control-plane invariants.
4. Operational risk (rollback/fallback clarity, blast radius).
5. Incremental deliverability for multi-agent execution.

### 2.2 Pattern options

#### Option A - Direct in-process embed (single-step replacement)

Description:
- add direct crate-level integration into FrankenTerm runtime/storage paths and replace recorder + storage internals in one migration.

Pros:
- maximum long-term architectural coherence if it lands successfully;
- avoids long-lived dual-stack complexity.

Cons:
- highest immediate risk and coupling;
- not aligned with current FrankenSQLite default-runtime maturity;
- difficult rollback boundary if migration is broad.

Assessment:
- not selected for initial deep-integration rollout.

#### Option B - Sidecar service boundary

Description:
- run FrankenSQLite as a separate process/service and integrate over RPC/IPC.

Pros:
- strong fault/process isolation;
- clear blast-radius boundary.

Cons:
- adds protocol and lifecycle overhead;
- duplicates policy/authorization and observability concerns across process boundaries;
- weak fit for first-step native integration in same-owner mono-development environment.

Assessment:
- viable for later federation/distribution needs, not selected as primary initial path.

#### Option C - Compatibility adapter only

Description:
- keep FrankenTerm internals mostly unchanged and introduce a thin adapter that leverages compatibility-mode file semantics, minimizing runtime refactor.

Pros:
- lower immediate implementation cost;
- preserves existing operational behavior.

Cons:
- risks becoming a shallow adapter anti-pattern;
- limited leverage of FrankenSQLite architecture improvements;
- may accumulate technical debt and deferred complexity.

Assessment:
- useful as a transitional tactic inside a phased plan, but insufficient as end-state architecture.

#### Option D - Hybrid phased native integration (recommended)

Description:
- keep FrankenTerm's stable `RecorderStorage` boundary;
- introduce FrankenSQLite-backed backend implementation behind that boundary;
- phase migration with explicit fallback to append-log and strict observability gates;
- progressively expand from recorder backend to broader storage/search intersections only after evidence gates pass.

Pros:
- best balance of architectural quality and delivery risk;
- matches existing backend abstraction work (`AppendLog` + `FrankenSqlite` backend kinds);
- supports incremental rollout and deterministic rollback.

Cons:
- temporary dual-backend complexity;
- requires disciplined phase gates to avoid indefinite intermediate state.

Assessment:
- selected.

### 2.3 Decision matrix

| Option | Architecture alignment | Delivery risk | Maturity fit | Rollback clarity | Overall |
|---|---:|---:|---:|---:|---:|
| A. Direct embed now | 5 | 1 | 2 | 2 | 2.5 |
| B. Sidecar service | 3 | 3 | 3 | 4 | 3.25 |
| C. Compatibility adapter only | 2 | 4 | 4 | 4 | 3.5 |
| D. Hybrid phased native | 5 | 4 | 4 | 5 | 4.5 |

Scoring scale: `1` poor, `5` strong.

### 2.4 Selected strategy and rationale

Selected strategy: **Option D (Hybrid phased native integration)**.

Rationale:

1. It preserves FrankenTerm's existing stability points (`RecorderStorage`, policy-gated control surfaces, canonical-vs-projection separation).
2. It respects current FrankenSQLite runtime maturity while still allowing deep native convergence.
3. It enables explicit operator-safe fallback behavior instead of all-or-nothing migration risk.
4. It gives clean sequencing for downstream beads:
- P3: place integration inside concrete FrankenTerm subsystems,
- P4: define crate/API extraction boundaries,
- P5-P7: define migration, tests, rollout/rollback gates.

### 2.5 Phase-gate shape (for downstream sections)

1. Phase 1: Implement FrankenSQLite recorder backend behind `RecorderStorage` with fallback to append-log.
2. Phase 2: Prove parity on durability/checkpoint/error semantics and observability before broader coupling.
3. Phase 3: Expand into selected storage/search intersections only when phase gates pass.
4. Phase 4: tighten and simplify interfaces (remove temporary transitional surfaces; no long-lived shims).

## 3. P3 - Target placement within FrankenTerm subsystems

Status: `completed` (`ft-2vuw7.1.3.3`)

### 3.1 Placement principles

1. Keep the canonical durability boundary at `RecorderStorage` (`crates/frankenterm-core/src/recorder_storage.rs`), not at append-log file APIs.
2. Keep search/index subsystems as projections over recorder events, never as alternate canonical stores.
3. Keep policy/workflow control paths backend-agnostic; storage backend choice must not change authorization semantics.
4. Keep watcher/runtime composition explicit: backend selection happens once at startup with observable health/fallback behavior.
5. Keep migration incremental: remove append-log-specific assumptions from shared subsystems before expanding FrankenSQLite scope.

### 3.2 Subsystem placement map

| Subsystem | Current anchors | Placement decision | Why this placement |
|---|---|---|---|
| Recorder durability plane | `crates/frankenterm-core/src/recorder_storage.rs` (`RecorderStorage`, `RecorderBackendKind::{AppendLog,FrankenSqlite}`) | Implement a first-class `FrankenSqlite` backend behind `RecorderStorage` (same module family), preserving existing request/response/checkpoint/error semantics. | Trait is already the stable durability boundary and already models a FrankenSQLite backend identity. |
| Watcher/runtime composition root | `crates/frankenterm/src/main.rs` (`run_watcher`) + `crates/frankenterm-core/src/runtime.rs` (`ObservationRuntime`) | Add recorder-backend construction/selection in watcher startup and inject backend handle(s) into runtime recording/indexing surfaces. | `run_watcher` already composes storage/pattern/event/workflow/policy services; backend choice belongs here, not in inner subsystems. |
| Incremental lexical indexing | `crates/frankenterm-core/src/tantivy_ingest.rs` | Refactor indexer input from append-log file reader coupling (`AppendLogReader` + `IndexerConfig.data_path`) to a storage-native event stream/cursor boundary sourced via `RecorderStorage`. | Current generic storage usage is partial (checkpoints only); this is the primary coupling seam blocking true backend neutrality. |
| Full reindex/backfill | `crates/frankenterm-core/src/tantivy_reindex.rs` | Same decoupling as incremental indexer: remove hard `AppendLogReader`/path dependency and consume recorder events through a storage-native cursor/range interface. | Reindex and backfill currently assume append-log files directly; this must align with backend abstraction before deeper integration. |
| Policy-gated ingress and workflow actions | `crates/frankenterm-core/src/policy.rs` (`PolicyGatedInjector`) + workflow wiring in `run_watcher` | Keep unchanged as backend-agnostic control-plane components; continue auditing via `StorageHandle` and ingress tap, independent of recorder backend implementation details. | Policy decisions and audit semantics are orthogonal to recorder backend mechanics and must remain stable. |
| Operational storage + SLO telemetry | `crates/frankenterm-core/src/storage.rs`, `crates/frankenterm-core/src/storage_targets.rs` | Keep existing operational SQLite/FTS store in place for runtime/workflow/audit metadata; extend telemetry to include recorder-backend health/lag dimensions. | Prevents scope explosion while preserving current SLO framework and observability posture. |

### 3.3 Ownership and lifecycle wiring

1. **Startup ownership (`run_watcher`)**
- select recorder backend from explicit config (new recorder backend selector);
- initialize backend and perform startup health probe;
- publish selected backend and health state to logs/telemetry at watcher boot.
2. **Runtime ownership (`ObservationRuntime` + recording/indexing workers)**
- runtime consumes recorder backend via trait-owned handles/cursors only;
- no direct append-log path assumptions in workers after refactor.
3. **Control-plane ownership (policy/workflows/robot/MCP)**
- continues to depend on policy + operational storage surfaces, not backend internals;
- recorder backend impacts captured-event durability path, not authorization contract shape.

### 3.4 Required refactor seams implied by placement

1. Replace append-log file-path assumptions in indexing configs with backend-neutral cursor/config surfaces.
2. Introduce recorder backend selection config and startup validation in watcher composition.
3. Standardize recorder health/lag metrics export by backend kind so rollout gates can compare `append_log` vs `franken_sqlite`.
4. Keep temporary dual-backend behavior explicit and removable; no long-lived compatibility shim layer.

### 3.5 P3 exit checks

P3 is complete when:
1. every affected subsystem has an explicit target placement and ownership boundary;
2. append-log-only couplings are identified as concrete refactor seams (not deferred implicitly);
3. control-plane invariants remain backend-agnostic by design;
4. downstream P4/P5 can proceed without ambiguity about where integration work lands.

## 4. P4 - API contracts and crate/subcrate extraction roadmap

Status: `completed` (`ft-2vuw7.1.3.4`)

### 4.1 Contract families (what must be stable)

1. **Write/flush contract (canonical durability plane)**
- anchor: `RecorderStorage::{append_batch, flush}` in `crates/frankenterm-core/src/recorder_storage.rs`;
- must remain backend-neutral and preserve idempotency + durability semantics.
2. **Checkpoint contract (indexer/reindex resume semantics)**
- anchor: `RecorderStorage::{read_checkpoint, commit_checkpoint}`;
- must remain monotonic and backend-independent for all projection consumers.
3. **Read-stream contract (new seam required by P3)**
- current gap: `tantivy_ingest` and `tantivy_reindex` still read append-log files via `AppendLogReader`;
- required stable seam: storage-native ordered event cursor/range API so indexers no longer depend on file paths.
4. **Error taxonomy contract**
- anchor: `RecorderStorageError` + `RecorderStorageErrorClass`;
- must stay stable for retry/policy/rollout decisions and observability gates.
5. **Health/lag telemetry contract**
- anchor: `RecorderStorage::{health, lag_metrics}`;
- must feed common SLO evaluation independent of backend kind.

### 4.2 API boundary decisions for FrankenTerm

1. Keep `RecorderStorage` as the single canonical write/checkpoint boundary.
2. Add a companion read abstraction for ordered recorder-event scanning used by indexing/reindex pipelines (instead of file-path coupling).
3. Keep `IndexerConfig`/`ReindexConfig` backend-neutral by replacing direct `data_path` assumptions with source descriptors/cursor options.
4. Keep policy/workflow APIs unchanged; they consume operational storage/audit surfaces, not recorder internals.

Illustrative shape for the new read seam (conceptual, not final names):

```rust
#[allow(async_fn_in_trait)]
pub trait RecorderEventSource: Send + Sync {
    async fn open_from_checkpoint(
        &self,
        consumer: &CheckpointConsumerId,
    ) -> Result<Box<dyn RecorderEventCursor>, RecorderStorageError>;
}

#[allow(async_fn_in_trait)]
pub trait RecorderEventCursor: Send {
    async fn next_batch(&mut self, max: usize)
        -> Result<Vec<(RecorderOffset, RecorderEvent)>, RecorderStorageError>;
}
```

### 4.3 Crate/subcrate extraction roadmap

#### Phase A (immediate, minimal churn)

1. Keep contracts and first FrankenSQLite backend implementation in `frankenterm-core` under the `recorder_storage` module family.
2. Refactor index/reindex internals to consume the new read seam without creating new public crates yet.
3. Avoid direct dependence of indexers on append-log internals once the seam lands.

#### Phase B (optional extraction after seam stabilization)

1. **Candidate ft-side extraction:** `frankenterm-recorder-contracts`
- contains backend-agnostic recorder DTOs/traits (`RecorderOffset`, checkpoints, error classes, read cursor traits);
- extracted only if cross-crate reuse in ft workspace proves persistent.
2. **Candidate ft-side extraction:** `frankenterm-recorder-index-source`
- shared adapter logic for indexer/reindex consumers;
- extracted only if duplication remains after refactor.

#### Phase C (upstream frankensqlite extraction/alignment)

1. Keep the integration entrypoint anchored on stable public `fsqlite` facade first (`/dp/frankensqlite/crates/fsqlite/src/lib.rs`) rather than internal crate fan-out from `frankenterm`.
2. If adapter responsibilities become broad, extract a dedicated upstream integration crate (candidate: `fsqlite-recorder-adapter`) in `/dp/frankensqlite` exposing:
- append/checkpoint primitives mapped to FrankenTerm recorder contracts;
- deterministic error-class mapping;
- health/lag snapshot hooks for rollout gating.
3. Treat deep internal crates (`fsqlite-pager`, `fsqlite-wal`, `fsqlite-btree`, `fsqlite-vdbe`) as upstream-owned internals unless a stable library surface is explicitly promoted.

### 4.4 Dependency guardrails

1. `frankenterm-core` index/reindex modules must not import append-log-only readers after seam migration.
2. `frankenterm-core` must not bind directly to unstable frankensqlite internal crates for core control-plane flows; use stable adapter/facade boundary.
3. Control-plane modules (`policy`, `workflows`, robot/MCP handlers) must remain agnostic to recorder backend internals.
4. Any extraction must reduce coupling and API blast radius; otherwise keep code in-place.

### 4.5 P4 exit checks

P4 is complete when:
1. stable contract families are explicitly defined (write, checkpoint, read-stream, error, telemetry);
2. index/reindex append-log path coupling is replaced by an explicit API target (not hand-waved);
3. crate/subcrate extraction is phase-gated with clear criteria rather than pre-emptive fragmentation;
4. downstream P5/P6 have concrete contract assumptions to build migration/testing plans.

## 5. P5 - Data migration/state synchronization and compatibility posture

Status: `completed` (`ft-2vuw7.1.3.5`)

### 5.1 State domains that must stay coherent

1. **Canonical recorder event stream**
- source-of-truth append history used for replay and index derivation.
2. **Consumer checkpoints**
- per-consumer monotonic resume state (`read_checkpoint`/`commit_checkpoint`).
3. **Derived projections**
- lexical/semantic indexes and backfill state derived from canonical offsets.
4. **Operational metadata**
- workflow/policy/audit data in `StorageHandle` (`crates/frankenterm-core/src/storage.rs`) that must remain independent of recorder backend migration.

### 5.2 Migration strategy (selected posture)

Selected posture: **quiesced deterministic cutover** (correctness-first), with optional later optimization to near-zero-downtime shadow catch-up.

Why selected:
1. avoids dual-write divergence complexity in early integration;
2. keeps canonical source singular at every step;
3. provides deterministic rollback point with explicit artifacts.

### 5.3 Phase-by-phase synchronization plan

1. **Phase M0 - Preflight and freeze**
- validate source backend health (`append_log`) and checkpoint monotonicity;
- capture migration manifest seed (source backend, event count estimate, checkpoint snapshot, schema versions);
- stop new recorder appends (quiesce writer path in watcher).
2. **Phase M1 - Canonical export from source backend**
- flush source backend to required durability;
- stream all canonical events in ordinal order from source;
- record deterministic digest/counts (first ordinal, last ordinal, total events, per-pane counts).
3. **Phase M2 - Canonical import to FrankenSQLite backend**
- import events preserving order and idempotency keys;
- verify imported cardinality + digest constraints against manifest;
- reject cutover on any mismatch.
4. **Phase M3 - Checkpoint synchronization**
- migrate checkpoint consumers only after M2 verification;
- preserve consumer monotonicity guarantees (no regression writes);
- if ordinal continuity cannot be proven, reset consumers to safe replay points and force projection rebuild.
5. **Phase M4 - Projection reconciliation**
- run deterministic reindex/backfill from canonical backend after cutover;
- verify projection health/lag returns to SLO envelope before declaring migration complete.
6. **Phase M5 - Activate target backend**
- switch recorder backend selector to `franken_sqlite`;
- emit explicit lifecycle marker event + telemetry tag (`migration_epoch`, source/target backend kinds);
- keep source append-log artifacts in retention window for rollback.

### 5.4 Invariants and validation rules

1. No event loss, duplication, or order inversion across M1→M2.
2. Checkpoint state remains monotonic per consumer.
3. Canonical-vs-projection split remains intact: projection rebuild never mutates canonical recorder history.
4. Policy/workflow control-plane behavior remains unchanged by migration (backend-internal only).
5. Migration is considered incomplete until index lag/health gates pass on target backend.

### 5.5 Compatibility posture (explicitly supported states)

1. **Supported state A:** `append_log` canonical backend.
2. **Supported state B:** migration-in-progress with source canonical and target staging.
3. **Supported state C:** `franken_sqlite` canonical backend.

Not supported:
1. indefinite dual-canonical write mode;
2. implicit fallback loops between backends;
3. hidden compatibility shims that mask backend-specific invariant violations.

### 5.6 Rollback posture

1. Rollback trigger conditions:
- import/verification mismatch,
- checkpoint migration invariants violated,
- target backend health/lag outside acceptance envelope.
2. Rollback action:
- re-enable source backend as canonical,
- restore pre-cutover checkpoint snapshot (or conservative replay-safe checkpoint floor),
- invalidate target projection state and rebuild from restored canonical source if needed.
3. Rollback precondition:
- source append-log data/checkpoint artifacts retained until post-cutover soak completes.

### 5.7 P5 exit checks

P5 is complete when:
1. migration phases M0-M5 are explicitly defined with ownership and validation points;
2. checkpoint/projection synchronization behavior is deterministic and replay-safe;
3. supported compatibility states are explicit, with anti-pattern states explicitly forbidden;
4. rollback triggers/actions are concrete enough for runbook execution in P7.

## 6. P6 - Testing strategy and detailed logging/observability

Status: `completed` (`ft-2vuw7.1.3.6`)

### 6.1 Verification objectives

1. Prove recorder backend parity for correctness-critical contracts (append, checkpoint, replay order, error classes).
2. Prove that projection pipelines remain rebuildable and backend-agnostic after seam refactors.
3. Prove policy/workflow control-plane behavior is unchanged by recorder backend selection.
4. Produce triage-ready diagnostics for every failure mode (no opaque test failures).

### 6.2 T1 - Unit test matrix (contract-level)

Required unit suites (backend-neutral where possible):

1. recorder contract semantics (`recorder_storage`)
- append idempotency by `batch_id`;
- monotonic `RecorderOffset.ordinal`;
- checkpoint monotonicity and regression rejection;
- error-class mapping stability (`RecorderStorageErrorClass`).
2. indexer/reindex seam semantics
- resume-from-checkpoint correctness;
- schema-version mismatch skip behavior;
- dedup-on-replay behavior;
- cursor EOF and partial-batch boundary handling.
3. migration helpers
- manifest digest/calculation determinism;
- checkpoint snapshot serialization stability;
- rollback precondition checks.

### 6.3 T2 - Integration harness (subsystem seams)

Integration suites must cover:

1. runtime composition
- watcher startup with selected backend;
- health probe and startup policy behavior (`fail_closed` vs fallback posture as configured).
2. ingest -> canonical durability -> projection handoff
- capture event append to recorder backend;
- incremental index catch-up from canonical offsets;
- reindex/backfill consistency.
3. policy/workflow unchanged behavior
- `PolicyGatedInjector` decisions and audit writes under both backend selections;
- workflow event handling unaffected by backend type.

### 6.4 T3 - End-to-end scenarios

E2E matrix (happy + adversarial):

1. steady-state swarm capture and search (`ft watch` + robot read/search commands).
2. backend cutover flow (M0-M5 from P5) with verification checkpoints.
3. injected failure scenarios:
- backend init failure,
- transient write failure,
- checkpoint commit conflict/regression,
- projection lag spike and recovery.
4. rollback drill:
- forced cutover abort and source-backend restore with replay/index verification.

### 6.5 T4 - Logging, tracing, and diagnostics assertions

All critical paths emit structured logs/traces with at least:

1. `backend_kind`, `batch_id`, `consumer_id`, `first_ordinal`, `last_ordinal`
2. `schema_version`, `checkpoint_outcome`, `error_class`, `degraded`
3. `migration_epoch`, `phase` (M0..M5), `cutover_id` for migration/cutover runs
4. latency metrics per operation (`append_ms`, `flush_ms`, `checkpoint_ms`, `index_lag_ms`)

Diagnostics requirements:

1. failed tests must include machine-readable context bundle (input events, checkpoint state, backend config, error class);
2. reindex/indexer failures must print the failing offset/ordinal and consumer id;
3. rollback path tests must emit explicit reason code for rollback decision.

### 6.6 T5 - Fixtures, replay corpus, test-data lifecycle

1. Maintain deterministic corpus fixtures for:
- normal append/checkpoint flows,
- out-of-order checkpoint rejection,
- corrupt/torn-record recovery paths,
- migration/cutover sample manifests.
2. Keep fixture schema-versioned and checksum-pinned.
3. Require fixture updates to ship with corresponding contract/test changes.

### 6.7 T6 - Performance, soak, and flake-resilience gates

Performance/soak validation aligns with `crates/frankenterm-core/src/storage_targets.rs`:

1. append and indexing lag SLO checks (including `indexing_lag_ceiling`).
2. sustained ingest/queue pressure behavior with explicit overload signaling (no hidden unbounded queues).
3. soak runs for long-lived capture/index pipelines with lag stability and no checkpoint drift.
4. flake gate: repeated run threshold for key migration/cutover scenarios before promotion.

Heavy verification commands must run via remote offload:

```bash
rch exec -- cargo test --workspace
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
rch exec -- cargo test -p frankenterm-core recorder_storage
rch exec -- cargo test -p frankenterm-core tantivy_ingest
rch exec -- cargo test -p frankenterm-core tantivy_reindex
```

### 6.8 T1-T6 deliverable mapping

1. `T1`: contract/unit correctness matrix complete.
2. `T2`: seam-level integration harness complete.
3. `T3`: E2E happy/failure/recovery/rollback matrix complete.
4. `T4`: structured logging + diagnostics assertions complete.
5. `T5`: deterministic fixtures/replay corpus lifecycle complete.
6. `T6`: performance/soak/flake gates complete.

### 6.9 P6 exit checks

P6 is complete when:
1. test strategy explicitly spans unit/integration/e2e/perf/soak dimensions;
2. diagnostic logging requirements are specific, structured, and assertion-driven;
3. T1-T6 child scopes are unambiguous and execution-ready;
4. P7 rollout/acceptance gates can consume concrete evidence outputs from this section.

## 7. P7 - Rollout, rollback, risk mitigation, acceptance gates

Status: `completed` (`ft-2vuw7.1.3.7`)

### 7.1 Rollout stages (gated, not time-based)

1. **R0 - Baseline hardening on source backend (`append_log`)**
- objective: prove observability/test harness quality before any target cutover;
- entry: P4-P6 artifacts complete;
- exit: P6 evidence green and rollback drill design validated.
2. **R1 - Shadow validation (target backend non-canonical)**
- objective: run migration/import/reindex verification without changing canonical writer;
- entry: R0 complete + migration manifest tooling ready;
- exit: M0-M4 (from P5) reproducible with no invariant violations.
3. **R2 - Canary cutover (single workspace / controlled operator scope)**
- objective: run full M0-M5 cutover in production-like conditions;
- entry: R1 green + rollback runbook approved;
- exit: canary soak window passes health/lag/correctness gates.
4. **R3 - Progressive rollout**
- objective: expand cutover to broader workspace set with staged concurrency limits;
- entry: R2 green + no unresolved high-severity incidents;
- exit: sustained SLO adherence and no unresolved data-integrity anomalies.
5. **R4 - Default backend promotion**
- objective: promote `franken_sqlite` as preferred recorder backend;
- entry: R3 green + rollback rehearsed against latest release candidate;
- exit: promotion criteria met and source-backend fallback remains operationally available during guard period.

### 7.2 Rollback playbooks

Rollback classes:

1. **Immediate rollback (during cutover window)**
- trigger: invariant break (event count/digest mismatch, checkpoint regression, corrupt import);
- action: abort cutover, reactivate source backend, restore pre-cutover checkpoint snapshot.
2. **Post-cutover rollback (during soak)**
- trigger: sustained SLO breach, repeated high-severity write/index failures, policy/audit regression evidence;
- action: execute controlled backend reversion + projection rebuild from restored canonical source.
3. **Data-integrity emergency rollback**
- trigger: confirmed or strongly suspected canonical data loss/corruption;
- action: freeze writes, preserve forensic artifacts, restore last known-good canonical source, require explicit human re-enable.

Runbook requirements:

1. exact command sequence with dry-run checks where possible;
2. explicit abort criteria at each step;
3. artifact checklist (manifest, digests, checkpoint snapshot, logs, error-class summary).

### 7.3 Risk register and mitigation strategy

Top risks and mitigations:

1. **Canonical divergence risk** (source vs target mismatch)
- mitigation: quiesced cutover + deterministic digest validation + no dual-canonical mode.
2. **Checkpoint drift risk**
- mitigation: monotonic checkpoint validation; replay-safe reset path when continuity cannot be proven.
3. **Projection lag/rebuild risk**
- mitigation: mandatory post-cutover reindex/backfill verification and lag gate checks.
4. **Operational blind spots**
- mitigation: structured telemetry contract (P6/T4) with explicit failure bundle requirements.
5. **Rollback fragility**
- mitigation: staged rollback rehearsals in R1/R2 and retained source artifacts through guard period.

### 7.4 Acceptance gates (must pass to advance stages)

1. **Correctness gate**
- zero unexplained event loss/duplication/order inversion in migration verification.
2. **Checkpoint gate**
- no consumer checkpoint regressions; replay resume behavior verified.
3. **Projection gate**
- lexical/semantic projection rebuild passes and indexing lag returns within configured ceiling.
4. **Policy/workflow parity gate**
- no regression in policy decision outcomes, audit emission, workflow trigger handling.
5. **Performance/soak gate**
- sustained ingest and latency metrics remain within defined budgets (`storage_targets` envelope).
6. **Rollback gate**
- latest rollback playbook executed successfully in rehearsal for current rollout stage.

### 7.5 Operational observability gate package

Each stage handoff must publish:

1. stage summary (entry criteria, exit criteria, pass/fail outcome);
2. migration manifest and verification digest report;
3. health/lag/perf snapshots by backend kind;
4. incident/error-class summary with unresolved items.

No stage promotion occurs on qualitative confidence alone; promotion requires all gate artifacts.

### 7.6 P7 exit checks

P7 is complete when:
1. rollout is stage-gated with objective entry/exit criteria;
2. rollback classes/triggers/actions are explicit and executable;
3. risk mitigations map directly to known failure classes from P5/P6;
4. acceptance gates are measurable and evidence-driven for P8 finalization.

## 8. P8 - Finalize cohesive integration plan

Status: `completed` (`ft-2vuw7.1.3.8`)

### 8.1 Cohesive execution roadmap (implementation-facing)

1. **Wave W0 - Seam hardening before backend swap**
- implement P4 contract seams: backend-neutral read cursor for index/reindex;
- remove append-log file-path coupling from shared projection pipelines.
2. **Wave W1 - FrankenSQLite backend implementation + migration plumbing**
- implement recorder backend behind `RecorderStorage`;
- implement migration manifest/export/import/checkpoint sync flow from P5.
3. **Wave W2 - Verification program execution**
- execute P6 T1-T6 suites and collect evidence artifacts;
- enforce SLO and diagnostics gates before canary cutover.
4. **Wave W3 - Stage-gated rollout**
- execute P7 rollout stages R0-R4 with mandatory gate packages and rollback rehearsals.
5. **Wave W4 - Post-promotion simplification**
- remove temporary transitional surfaces that are no longer needed;
- keep architecture strict: canonical durability plane + rebuildable projections.

### 8.2 Dependency path to implementation beadization

This plan directly unblocks:

1. `ft-2vuw7.1.4.1` (convert plan into executable bead taxonomy),
2. `ft-2vuw7.16.1` (cross-project unified architecture synthesis consuming project-level P8 outputs).

Execution principle:

1. beadization must preserve all scopes in P1-P7 (no silent simplification);
2. implementation beads must carry explicit test/logging gates from P6 and rollout gates from P7;
3. each implementation phase must preserve deterministic rollback posture from P5.

### 8.3 Final readiness checklist

1. objectives/constraints/non-goals defined and traceable (P1): complete.
2. integration strategy selected with rationale (P2): complete.
3. subsystem placement map and ownership boundaries (P3): complete.
4. API contracts + extraction roadmap (P4): complete.
5. migration/state sync + compatibility posture (P5): complete.
6. testing/logging/observability program (P6 + T1..T6): complete.
7. rollout/rollback/risk/acceptance gates (P7): complete.

### 8.4 Final plan acceptance statement

The frankensqlite integration plan is now cohesive, dependency-aware, and implementation-ready.  
It is explicitly correctness-first, rollback-capable, and aligned with FrankenTerm's architecture boundaries (canonical durability vs rebuildable projections, policy-gated control plane invariants, and measurable operational gates).
