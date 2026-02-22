# Comprehensive Analysis of FrankenRedis

> Bead: ft-2vuw7.2.1 / ft-2vuw7.2.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

FrankenRedis (`/dp/frankenredis`) is a clean-room Rust reimplementation of Redis targeting full drop-in compatibility. It is a ~42K LOC workspace of 10 crates, all enforcing `#![forbid(unsafe_code)]`, with only 3 direct external dependencies (serde, serde_json, sha2). The project is synchronous (no async runtime yet), single-threaded, and implements 100+ Redis commands across all major data type families.

**Key architectural features:**
- Dual-mode runtime: Strict (fail-closed) vs Hardened (bounded defense with allowlist)
- Evidence ledger: tamper-evident structured logging of every security/compatibility decision
- RESP2 protocol parser/encoder with fail-closed semantics for unknown RESP3 types
- AOF persistence with deterministic round-trip serialization
- Conformance testing harness with differential analysis against live Redis oracle

**Integration relevance to FrankenTerm:** Medium-term. Both projects plan to adopt Asupersync as async runtime. Shared infrastructure opportunities exist in evidence logging, structured event schemas, and possibly TTL/expiry evaluation primitives.

---

## 2. Repository Topology

### 2.1 Workspace Structure

```
/dp/frankenredis/
├── Cargo.toml          (workspace root, resolver = 2, edition = 2024)
├── rust-toolchain.toml (nightly channel)
└── crates/
    ├── fr-protocol/     (684 LOC)  — RESP2 parser/encoder
    ├── fr-expire/       (48 LOC)   — TTL evaluation primitives
    ├── fr-config/       (925 LOC)  — strict/hardened policy + TLS config
    ├── fr-repl/         (447 LOC)  — replication state machine
    ├── fr-eventloop/    (970 LOC)  — tick budgeting + phase ordering
    ├── fr-store/        (7103 LOC) — in-memory keyspace (8 data types)
    ├── fr-command/      (12387 LOC)— 100+ command dispatch
    ├── fr-persist/      (119 LOC)  — AOF record serialization
    ├── fr-runtime/      (3021 LOC) — orchestrator: auth, policy, evidence
    │   └── src/ecosystem.rs (1926 LOC) — ecosystem management
    └── fr-conformance/  (5295 LOC lib + 5143 LOC bins)
        ├── src/log_contract.rs    (532 LOC)
        ├── src/phase2c_schema.rs  (1394 LOC)
        └── src/bin/ (13 binaries)  — conformance tooling
```

### 2.2 Dependency Graph (Internal)

```
fr-protocol (leaf)          fr-expire (leaf)       fr-config (leaf)
    │                           │                      │
    ├──────────────┐            │                      │
    │              │            │                      │
fr-store ─────────┘            │                      │
    │                          │                      │
fr-command ───────────────────┘                      │
    │                                                 │
fr-persist ── fr-protocol                            │
    │                                                 │
fr-repl (leaf)                                       │
    │                                                 │
fr-eventloop (leaf)                                  │
    │                                                 │
fr-runtime ── all above ─────────────────────────────┘
    │
fr-conformance ── fr-runtime + serde + serde_json + sha2
```

### 2.3 External Dependencies

| Direct Dep | Version | Used By | Purpose |
|------------|---------|---------|---------|
| serde | 1.0.228 | fr-conformance | JSON fixture serialization |
| serde_json | 1.0.149 | fr-conformance | JSON parsing |
| sha2 | 0.10.9 | fr-conformance | Integrity digests |

**Total transitive crates: 21.** Extremely minimal dependency footprint.

### 2.4 Build Configuration

- **Edition:** 2024 (requires nightly)
- **No feature flags** defined
- **No build.rs** scripts
- **No proc macros** (only serde_derive used transitively)
- **All crates:** `#![forbid(unsafe_code)]`

---

## 3. Module Architecture & Data Flow

### 3.1 Core Data Pipeline

```
Client bytes → fr-protocol::parse_frame() → RespFrame
    → fr-command::frame_to_argv() → Vec<Vec<u8>>
    → fr-command::dispatch_argv(argv, &mut store, now_ms) → RespFrame
    → fr-runtime (policy gate + evidence emit)
    → fr-persist (AOF append)
    → fr-repl (replication offset increment)
    → RespFrame::encode_into() → response bytes
```

### 3.2 Core Components

#### fr-protocol — RESP Parser/Encoder
- Stateless, incremental parser
- Returns `ParseResult { frame, consumed }` for streaming
- Null semantics: `$-1` (null bulk), `*-1` (null array)
- Rejects RESP3 types (fail-closed: `UnsupportedResp3Type`)
- **Zero external dependencies**

#### fr-store — In-Memory Keyspace
- 8 data types: String, Hash, List, Set, SortedSet, Stream, HyperLogLog, Bitmap
- Uses BTreeMap/BTreeSet for deterministic iteration order
- Lazy TTL evaluation via `fr-expire::evaluate_expiry(now_ms, expires_at_ms)`
- Type-safe operations: `StoreError::WrongType` on type mismatch
- Active expire cycle with cursor-based key sampling
- Maxmemory enforcement with eviction loop + safety gate

#### fr-command — Command Dispatch
- 100+ Redis commands implemented
- Strict arity validation per Redis semantics
- `is_write_command()` classifier for replication filtering
- Redis-compatible error messages

#### fr-runtime — Orchestrator
- **Main entry:** `Runtime::execute_frame(frame, now_ms) → RespFrame`
- **Alternate:** `Runtime::execute_bytes(input, now_ms) → Vec<u8>`
- Dual-mode policy gate (strict vs hardened)
- Evidence ledger for every security/compatibility decision
- Auth state: requirepass + ACL user registry
- TLS runtime state management
- AOF recording + replay

#### fr-config — Policy & TLS
- `RuntimePolicy { mode, gate, emit_evidence_ledger, hardened_allowlist }`
- 8 threat classes (ParserAbuse, MetadataAmbiguity, VersionSkew, etc.)
- 4 drift severity levels (S0-S3)
- TLS configuration with protocol version enforcement
- Every error has a `reason_code()` for audit trails

#### fr-conformance — Testing Harness
- Differential testing against live Redis oracle
- JSON fixture format for reproducible test cases
- 13 binary tools for conformance validation
- Phase2C schema gate for optimization validation

---

## 4. Public API Surface

### 4.1 Primary Integration Points

```rust
// Main entry point
let mut rt = Runtime::new(RuntimePolicy::default_strict());
let response: RespFrame = rt.execute_frame(frame, now_ms);

// Or raw bytes
let response: Vec<u8> = rt.execute_bytes(input, now_ms);

// AOF replay
let results = rt.replay_aof_stream(aof_bytes, now_ms)?;

// Evidence inspection
let events: &[EvidenceEvent] = rt.evidence().events();

// Memory pressure
let pressure = rt.maxmemory_pressure_state();
```

### 4.2 Key Types

| Type | Purpose | Module |
|------|---------|--------|
| `RespFrame` | Protocol data unit | fr-protocol |
| `Store` | Mutable keyspace | fr-store |
| `Runtime` | Request handler | fr-runtime |
| `RuntimePolicy` | Security/compat config | fr-config |
| `EvidenceEvent` | Audit trail entry | fr-runtime |
| `AofRecord` | Persistence record | fr-persist |
| `ReplProgress` | Replication tracking | fr-repl |
| `TickPlan` | Event loop scheduling | fr-eventloop |

---

## 5. Reliability, Performance & Security

### 5.1 Error Handling
- Every error enum has `reason_code() → &'static str` for structured logging
- Protocol layer: fail-closed on unknown frame types
- Command layer: Redis-compatible error strings
- Store layer: type mismatch and overflow detection
- Runtime layer: policy-gated decisions with evidence trail

### 5.2 Performance Profile
- **Current:** Single-threaded, synchronous, no async runtime
- **Target benchmarks:** p95 RESP parse ≤ 120us, command dispatch ≤ 1.5ms, ≥ 150k ops/s
- **Planned:** Asupersync integration for structured concurrency
- **Optimizations deferred:** ART for keyspace prefix queries, S3-FIFO eviction policy

### 5.3 Security Surface
- Input validation: frame length bounds, UTF-8 verification, arity checks
- ACL system: user registry with password verification
- TLS: configuration model complete, listener integration pending
- Threat model: 8 threat classes with per-class decision actions
- Evidence ledger: input/output digests, state digests, replay commands

---

## 6. Integration Opportunities with FrankenTerm

### 6.1 Immediate Extraction Candidates (Low Effort)

| Module | Lines | Dependencies | FrankenTerm Use Case |
|--------|-------|-------------|---------------------|
| `fr-protocol` | 684 | None | RESP log parsing, metrics format |
| `fr-expire` | 48 | None | Session/lease timeout evaluation |
| `fr-config` (threat model) | 925 | None | Pane corruption/desync classification |

### 6.2 Shared Infrastructure Opportunities

1. **Evidence Ledger Schema** — Both projects use structured event logging. FrankenRedis's `EvidenceEvent` schema overlaps with FrankenTerm's `recorder_audit.rs` hash chain audit log. A shared schema could enable cross-project forensic analysis.

2. **Asupersync Runtime** — Both projects plan to adopt Asupersync. FrankenRedis's `fr-eventloop` tick budgeting maps directly to Asupersync's structured concurrency model. Shared adoption would reduce integration friction.

3. **RaptorQ Durability** — FrankenRedis uses RaptorQ for conformance artifact durability. FrankenTerm's flight recorder could benefit from the same approach for snapshot integrity.

4. **Threat/Drift Classification** — FrankenRedis's `ThreatClass`/`DriftSeverity` enums could be generalized for FrankenTerm's pane health classification (currently uses Weibull survival model + Bayesian ledger).

### 6.3 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Nightly-only toolchain | Medium | FrankenRedis requires nightly for edition 2024; FrankenTerm also uses 2024 edition |
| No async runtime | Low | Both projects converging on Asupersync |
| Single-threaded model | Medium | FrankenTerm is heavily async; shared types must be Send+Sync |
| Minimal test deps | Low | FrankenRedis avoids proptest/tokio; integration tests may need adaptation |

### 6.4 Upstream Change Candidates

1. **Add `Send + Sync` bounds** to `Store` and `Runtime` (currently single-threaded, may not be Send)
2. **Extract `fr-protocol` as standalone crate** on crates.io for reuse
3. **Add feature flag for serde derives** on core types (currently only in conformance)
4. **Add `Display` impls** for error types (currently only `Debug`)

---

## 7. Coupling & Dependency Hotspots

### 7.1 High-Coupling Areas

- **fr-command ↔ fr-store**: 12K LOC command module directly mutates store. Every new command requires both modules.
- **fr-runtime ↔ everything**: Runtime imports all crates. Changes to any module may require runtime updates.
- **fr-conformance ↔ fr-runtime**: Conformance harness tests through Runtime, making it integration-heavy.

### 7.2 Low-Coupling Areas (Good Extraction Targets)

- **fr-protocol**: Zero internal dependencies, fully self-contained
- **fr-expire**: Single pure function, zero dependencies
- **fr-config**: Policy logic independent of data flow
- **fr-repl**: State machine independent of protocol details

---

## 8. Current Maturity Assessment

| Dimension | Status | Completeness |
|-----------|--------|-------------|
| RESP2 Protocol | Complete | 100% |
| Core Commands | In Progress | ~75% of target surface |
| Data Types | Complete | 8/8 types implemented |
| Persistence (AOF) | Complete | Round-trip verified |
| Persistence (RDB) | Not Started | 0% |
| Replication | Scaffold | ~20% (FSM + offset tracking) |
| TLS | Config Complete | ~60% (listener pending) |
| Transactions | Scaffold | ~10% (MULTI/EXEC stubs) |
| Lua Scripting | Not Started | 0% |
| Cluster | Not Started | 0% (slot routing stubs) |
| Conformance Suite | Active | 6+ fixture families |
| Async Runtime | Not Started | 0% (Asupersync planned) |

---

## 9. Recommended Integration Roadmap

### Phase 1: Shared Types (1-2 days)
- Extract `fr-protocol` and `fr-expire` as workspace dependencies available to FrankenTerm
- Add serde derives behind feature flag for cross-project serialization

### Phase 2: Evidence Integration (3-5 days)
- Align `EvidenceEvent` schema with FrankenTerm's `recorder_audit` event format
- Create shared evidence schema crate for cross-project audit trails

### Phase 3: Runtime Integration (1-2 weeks)
- Both projects adopt Asupersync concurrently
- Wire FrankenRedis evidence ledger into FrankenTerm's flight recorder
- Add FrankenRedis health metrics to FrankenTerm's telemetry pipeline

### Phase 4: Operator Experience (2-3 weeks)
- FrankenTUI panel for FrankenRedis conformance drift visualization
- Cross-project forensic query engine spanning both audit trails
