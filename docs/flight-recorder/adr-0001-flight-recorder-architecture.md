# ADR-0001: Universal Mux I/O Flight Recorder + Hybrid Search Architecture

Date: 2026-02-12  
Status: Accepted for `ft-oegrb` implementation track  
Owners: `WildSpring` (+ swarm implementers)

## Context
FrankenTerm needs a recorder system that:
1. Captures all mux ingress/egress text with replay-grade ordering metadata.
2. Writes durably at high throughput with bounded resource behavior.
3. Supports fast lexical search plus semantic and hybrid retrieval.
4. Preserves operational safety (privacy, backpressure, recovery, rollout gating).

Research inputs:
- `docs/flight-recorder/frankensqlite-append-log-dossier.md`
- `docs/flight-recorder/cass-two-tier-architecture-dossier.md`
- `docs/flight-recorder/xf-hybrid-retrieval-dossier.md`
- `docs/flight-recorder/cross-project-extraction-matrix.md`
- `docs/flight-recorder/tantivy-schema-v1.md`

## Decision
Adopt a four-plane architecture:
1. **Capture plane**: ingress/egress taps emitting canonical recorder events.
2. **Canonical durability plane**: append-only recorder log as source of truth, with deterministic replay/checkpoint semantics.
3. **Projection/search plane**: asynchronous lexical and semantic indexes derived from canonical offsets.
4. **Interface plane**: consistent CLI/robot/MCP query contracts over lexical/semantic/hybrid modes.

The canonical append log is authoritative; all indexes are rebuildable projections.

## Detailed Architecture

### 1) Capture plane (event contract)
- Use `ft-recorder-event-v1` envelope as stable producer/consumer boundary.
- Required fields include:
  - identity: workspace/session/pane/event IDs
  - ordering: monotonic sequence
  - causality: parent/trigger/root correlation IDs
  - timestamps: occurred_at + recorded_at
- Capture stage supports redaction policy boundaries before durable write.

Primary bead mapping:
- `ft-oegrb.2.1`, `.2.2`, `.2.3`, `.2.4`, `.2.5`, `.2.6`

### 2) Canonical durability plane (append-only source of truth)
- Recorder writes append-only segments with:
  - fixed header and version
  - length-prefixed event records
  - per-record integrity checksum
  - deterministic offset assignment
- Recovery uses valid-prefix scanning (torn-tail tolerant).
- Replay/checkpoint API provides idempotent resume for indexers and tooling.
- Queue admission is bounded and explicit; overload behavior is policy-driven and observable.

Primary bead mapping:
- `ft-oegrb.3.1`, `.3.2`, `.3.4`, `.3.6`

### 3) Storage backend strategy
- Introduce `RecorderStorage` abstraction with backend-gated configuration.
- Backend A (initial): local append-log implementation (lowest integration risk).
- Backend B (advanced): FrankenSQLite-informed adapter for high-concurrency durability experiments.
- Retention/partition lifecycle is explicit and policy-governed.

Primary bead mapping:
- `ft-oegrb.3.1`, `.3.3`, `.3.5`

### 4) Projection/search plane
- Lexical:
  - Tantivy schema versioned and tuned for terminal text.
  - Schema contract: `docs/flight-recorder/tantivy-schema-v1.md`.
  - Incremental index ingestion from canonical offsets.
  - Deterministic rebuild/backfill tooling from source log.
- Semantic:
  - Embedding provider abstraction with governance controls.
  - Chunking/windowing aligned to recorder event boundaries.
  - Vector retrieval pipeline keyed to canonical offsets.
- Hybrid:
  - Deterministic RRF-based fusion.
  - Lexical-safe fallback when semantic path is degraded.

Primary bead mapping:
- Lexical: `ft-oegrb.4.1`..`.4.6`
- Semantic/hybrid: `ft-oegrb.5.1`..`.5.6`

### 5) Interface plane
- Single query contract shared by:
  - `ft` CLI
  - `ft robot` (TOON-first for token efficiency)
  - MCP tool surface
- Modes:
  - `lexical`
  - `semantic`
  - `hybrid`
- Two-tier/quality-refinement mode is optional and feature-gated (not v1 baseline).

Primary bead mapping:
- `ft-oegrb.6.1`..`.6.6`

### 6) Governance, safety, and validation
- Privacy/redaction/access policy is a hard prerequisite for broad rollout.
- Validation is gate-based:
  - performance/load
  - failure injection
  - invariants/replay determinism
  - security/privacy regression suite
- Rollout uses phase gates and rollback criteria, not big-bang enablement.

Primary bead mapping:
- Validation: `ft-oegrb.7.1`..`.7.6`
- Rollout/governance: `ft-oegrb.8.1`..`.8.6`

## Phased Implementation Map

### Phase 0: Research completion
Goal:
- finalize cross-project synthesis and architecture ADR
Completion:
- `ft-oegrb.1.1`..`.1.5` closed

### Phase 1: Canonical capture + durability baseline
Goal:
- end-to-end capture -> append log with deterministic replay/checkpoint
Scope:
- `ft-oegrb.2.*`, `ft-oegrb.3.1`, `ft-oegrb.3.2`, `ft-oegrb.3.4`, `ft-oegrb.3.6`
Exit criteria:
- sustained ingest under target load
- replay correctness proven via invariant tests

### Phase 2: Lexical retrieval baseline
Goal:
- production-usable lexical search from canonical log
Scope:
- `ft-oegrb.4.*` + necessary interface subset from `ft-oegrb.6`
Exit criteria:
- lexical quality harness stable
- bounded index lag with restart-safe ingestion

### Phase 3: Semantic + hybrid expansion
Goal:
- controlled semantic retrieval and hybrid fusion
Scope:
- `ft-oegrb.5.*`
Exit criteria:
- quality gates show hybrid benefit over lexical baseline
- semantic path stays within latency/cost budgets

### Phase 4: Full interface and policy hardening
Goal:
- complete CLI/robot/MCP parity with policy-aware controls
Scope:
- remaining `ft-oegrb.6.*` + `ft-oegrb.8.3`
Exit criteria:
- consistent query semantics across all interfaces
- redaction and access controls validated

### Phase 5: Reliability proof + staged rollout
Goal:
- prove operational safety and enable controlled rollout
Scope:
- `ft-oegrb.7.*`, `ft-oegrb.8.*`
Exit criteria:
- CI/nightly quality gates enforced
- runbooks, migration, and rollback paths validated

## Alternatives Considered

### A) Big-bang direct FrankenSQLite storage transplant
Rejected because:
- high integration risk and scope explosion
- unnecessary to deliver recorder value in first release

### B) Search-only implementation without canonical append log
Rejected because:
- weak forensic guarantees
- rebuild/recovery trust issues
- index coupling risk

### C) Semantic-first rollout
Rejected because:
- lexical baseline is required fallback and safety anchor
- semantic cost/latency risk without operational controls

## Consequences

### Positive
- Strong forensic/replay guarantees from canonical append plane.
- Incremental rollout path with clear rollback options.
- Search improvements can ship safely as projections.

### Tradeoffs
- More components to operate (log + indexes + semantic jobs).
- Requires explicit governance/policy work before default-on.
- Higher upfront architecture work than ad-hoc logging.

## Risk Controls
- bounded admission and explicit overflow policies
- deterministic checkpoints and replay invariants
- corruption/failure injection in validation suite
- interface-level policy enforcement and auditability
- phase-gated rollout with measurable entry/exit criteria

## Immediate Next Step
Proceed to `ft-oegrb.1.6` feasibility spikes to validate hot-path assumptions before broad implementation.
