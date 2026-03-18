# Plan to Deeply Integrate franken_redis into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.2.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_franken_redis.md (ft-2vuw7.2.1)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **In-process session state backend**: Replace ad-hoc HashMap/serde_json state storage with franken_redis's type-safe `fr-store` for pane state, window layouts, and session metadata
2. **RESP-based IPC protocol**: Use `fr-protocol` as the wire format for inter-process communication between ft daemon, CLI, and MCP server
3. **Fail-closed policy model**: Adopt `fr-config`'s Strict/Hardened mode pattern as a reference for FrankenTerm's safety/policy engine
4. **AOF-based audit trail**: Use `fr-persist` to create a forensic replay log of all robot mode commands and pane interactions
5. **TTL-aware lifecycle management**: Use `fr-expire` semantics for automatic session/pane timeout and cleanup

### Constraints

- **No external dependency increase**: franken_redis core crates have zero external deps; this must be preserved
- **No unsafe code**: franken_redis uses `#![forbid(unsafe_code)]`; integration must maintain this invariant
- **Synchronous core**: franken_redis is synchronous; wrapping in async boundaries is acceptable but must not introduce deadlocks
- **Edition compatibility**: Both projects use Rust 2024 edition (no mismatch)
- **No feature regression**: Existing FrankenTerm search, pattern detection, and robot mode APIs must continue working

### Non-Goals

- **Running a standalone Redis server**: franken_redis is used as an embedded library, not a network service
- **Full Redis protocol compatibility**: We only use the subset of commands relevant to FrankenTerm's needs
- **Replacing SQLite**: fr-store complements SQLite (for session state), not replaces it (SQLite remains for FTS5 search, captured output storage)
- **Cluster mode / replication**: Not needed for single-host FrankenTerm deployment
- **HyperLogLog / Geo / Bitmap**: These Redis data types are not relevant to FrankenTerm use cases

---

## P2: Evaluate Integration Patterns

### Option A: Direct Embedding (Chosen)

Embed `fr-store`, `fr-protocol`, `fr-config`, `fr-expire`, `fr-persist` as workspace path dependencies in frankenterm-core.

**Pros**: Zero overhead, type-safe, compile-time checked, no IPC latency
**Cons**: Increases frankenterm-core compile surface by ~10K LOC

### Option B: Subprocess / Sidecar Service

Run franken_redis as a separate process with RESP socket communication.

**Pros**: Process isolation, independent lifecycle
**Cons**: IPC latency, connection management, deployment complexity
**Rejected**: FrankenTerm already has daemon/watcher complexity; adding another process is unwarranted

### Option C: Feature-Gated Adapter

Add franken_redis integration behind a `redis-session` feature flag.

**Pros**: Optional compilation, minimal impact on default build
**Cons**: Feature flag testing matrix complexity
**Considered for Phase 1**: Start with feature gate, remove once proven stable

### Decision: Option A with initial feature gate (Option C wrapper)

Phase 1 uses `#[cfg(feature = "redis-session")]` to gate all franken_redis integration. Phase 2 removes the gate after regression suite confirms no issues.

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── session_store.rs          # NEW: SessionStore wrapping fr-store
│   ├── session_protocol.rs       # NEW: RESP-based IPC helpers using fr-protocol
│   ├── session_audit.rs          # NEW: AOF-based audit trail using fr-persist
│   └── ...existing modules...
├── Cargo.toml                    # Add path deps: fr-store, fr-protocol, fr-config, ...
```

### Module Responsibilities

#### `session_store.rs` (wraps fr-store)
- `SessionStore` struct holding `fr_store::Store`
- Methods: `save_pane_state()`, `load_pane_state()`, `expire_idle_panes()`
- Key schema: `pane:{id}:state`, `window:{id}:layout`, `session:{id}:meta`
- TTL: Configurable per key type (default 24h for session, 1h for transient state)
- Used by: runtime.rs (watcher loop), watcher_client.rs, snapshot commands

#### `session_protocol.rs` (wraps fr-protocol)
- `encode_command()` / `decode_command()` for IPC frames
- `SessionCommand` enum mapping FrankenTerm operations to RESP frames
- Used by: IPC between ft daemon and CLI, MCP tool invocations

#### `session_audit.rs` (wraps fr-persist)
- `AuditLog` struct holding `Vec<fr_persist::AofRecord>`
- Methods: `record_action()`, `export_trail()`, `replay_from()`
- Records: robot mode sends, workflow executions, policy decisions
- Used by: policy.rs (audit trail), robot mode commands

### Dependency Wiring

```toml
# In crates/frankenterm-core/Cargo.toml
[dependencies]
fr-store = { path = "../../franken_redis/crates/fr-store" }
fr-protocol = { path = "../../franken_redis/crates/fr-protocol" }
fr-config = { path = "../../franken_redis/crates/fr-config" }
fr-expire = { path = "../../franken_redis/crates/fr-expire" }
fr-persist = { path = "../../franken_redis/crates/fr-persist" }
```

**Alternative**: Use `[patch]` section if franken_redis is published, or copy crates in-tree under `crates/frankenterm-core/vendored/`.

---

## P4: API Contracts and Crate/Subcrate Extraction Roadmap

### Public API Contract: SessionStore

```rust
pub struct SessionStore {
    store: fr_store::Store,
    config: SessionStoreConfig,
}

pub struct SessionStoreConfig {
    pub pane_state_ttl_ms: u64,      // Default: 86_400_000 (24h)
    pub window_layout_ttl_ms: u64,   // Default: 604_800_000 (7d)
    pub transient_state_ttl_ms: u64, // Default: 3_600_000 (1h)
}

impl SessionStore {
    pub fn new(config: SessionStoreConfig) -> Self;

    // Pane state
    pub fn save_pane_state(&mut self, pane_id: u64, state: &[u8], now_ms: u64);
    pub fn load_pane_state(&mut self, pane_id: u64, now_ms: u64) -> Option<Vec<u8>>;
    pub fn delete_pane_state(&mut self, pane_id: u64, now_ms: u64);

    // Window layout
    pub fn save_window_layout(&mut self, window_id: u64, layout: &[u8], now_ms: u64);
    pub fn load_window_layout(&mut self, window_id: u64, now_ms: u64) -> Option<Vec<u8>>;

    // Session metadata
    pub fn save_session_meta(&mut self, session_id: &str, meta: &[u8], now_ms: u64);
    pub fn load_session_meta(&mut self, session_id: &str, now_ms: u64) -> Option<Vec<u8>>;

    // Pane event stream (using Redis Streams)
    pub fn append_pane_event(&mut self, pane_id: u64, event: &[u8], now_ms: u64) -> (u64, u64);
    pub fn read_pane_events(&mut self, pane_id: u64, since: (u64, u64), count: usize, now_ms: u64) -> Vec<(u64, u64, Vec<u8>)>;

    // Maintenance
    pub fn run_expiry_cycle(&mut self, now_ms: u64) -> ExpiryCycleResult;
    pub fn key_count(&mut self, now_ms: u64) -> usize;
}
```

### Public API Contract: AuditLog

```rust
pub struct AuditLog {
    records: Vec<fr_persist::AofRecord>,
    max_records: usize,
}

impl AuditLog {
    pub fn new(max_records: usize) -> Self;
    pub fn record(&mut self, command: &[&[u8]]);
    pub fn export(&self) -> Vec<u8>;  // RESP-encoded AOF stream
    pub fn replay(&self) -> impl Iterator<Item = &fr_persist::AofRecord>;
    pub fn len(&self) -> usize;
    pub fn clear(&mut self);
}
```

### Crate Extraction Roadmap

**Phase 1**: Keep franken_redis crates as external path dependencies (simplest integration)

**Phase 2**: If franken_redis is published to crates.io, switch to version dependencies:
```toml
fr-store = "0.1"
fr-protocol = "0.1"
```

**Phase 3**: If tight coupling develops, vendor relevant crates into `crates/frankenterm-core/vendored/fr-store/` etc.

---

## P5: Data Migration/State Synchronization and Compatibility Posture

### Migration Strategy

**No migration needed** — this is a new feature, not a replacement of existing state storage:
- SQLite continues to store captured output segments, events, and search indices
- `SessionStore` is a new layer for in-memory session state (pane state, layouts, metadata)
- On startup, `SessionStore` starts empty; state populated from current pane discovery

### State Synchronization

- `SessionStore` is the fast path for session state (in-memory, TTL-aware)
- SQLite is the durable path (FTS5 search, historical output)
- No bidirectional sync needed — different concerns, different data

### Compatibility Posture

- **Additive only**: No existing APIs change; SessionStore adds new capabilities
- **Backward compatible**: If `redis-session` feature is disabled, behavior is identical to current
- **Forward compatible**: SessionStore key schema is versioned (prefix includes version: `v1:pane:{id}:state`)

---

## P6: Testing Strategy and Detailed Logging/Observability

### Unit Tests (per module)

#### `session_store.rs` tests (target: 30+)
- `test_save_load_pane_state` — round-trip correctness
- `test_pane_state_ttl_expiry` — verify TTL enforcement
- `test_nonexistent_key_returns_none` — miss handling
- `test_delete_pane_state` — explicit deletion
- `test_window_layout_save_load` — layout persistence
- `test_session_meta_save_load` — metadata handling
- `test_pane_event_stream_append_read` — stream operations
- `test_pane_event_stream_since` — incremental reads
- `test_expiry_cycle_evicts_expired` — active expiry
- `test_key_count_accurate` — bookkeeping
- `test_overwrite_preserves_ttl` — TTL reset on update
- `test_concurrent_pane_states` — multiple panes
- `test_large_state_payload` — 1MB+ state values
- `test_empty_store_operations` — edge cases
- Additional edge cases and boundary conditions to reach 30+

#### `session_protocol.rs` tests (target: 15+)
- `test_encode_decode_roundtrip` — RESP frame correctness
- `test_session_command_encoding` — custom command types
- `test_invalid_frame_rejection` — error handling
- `test_bulk_string_binary_safe` — non-UTF8 data
- Additional protocol edge cases

#### `session_audit.rs` tests (target: 15+)
- `test_record_and_export` — AOF stream correctness
- `test_replay_order` — command ordering
- `test_max_records_cap` — bounded growth
- `test_clear_empties_log` — reset behavior
- `test_export_reimportable` — round-trip via decode

### Integration Tests

- `test_session_store_with_runtime` — SessionStore integrated into watcher loop
- `test_audit_trail_robot_mode` — audit captures robot mode commands
- `test_session_protocol_ipc` — RESP IPC between daemon and CLI
- `test_feature_gate_disabled` — verify no regression when feature disabled

### Property-Based Tests (proptest)

- `proptest_session_store_set_get` — arbitrary keys/values maintain consistency
- `proptest_pane_event_stream_ordering` — stream maintains temporal order
- `proptest_audit_export_decode_roundtrip` — AOF export/import preserves all records
- `proptest_ttl_monotonic` — TTL countdown is monotonically decreasing
- `proptest_expiry_never_returns_expired` — no phantom reads after expiry

### Logging Requirements

All SessionStore operations emit structured tracing events:
```rust
tracing::debug!(
    pane_id = %pane_id,
    key = %key,
    ttl_ms = %ttl_ms,
    now_ms = %now_ms,
    "session_store.save_pane_state"
);
```

Fields:
- `pane_id`, `window_id`, `session_id` as applicable
- `operation`: save, load, delete, expire, stream_append, stream_read
- `ttl_ms`: configured TTL
- `now_ms`: current timestamp
- `key_count`: total keys after operation
- `expired_count`: keys evicted in expiry cycle

### End-to-End Script Suite (T3)

Define a deterministic E2E script matrix that runs daemon/CLI/MCP paths end-to-end:

1. **Happy path**
   - Start watcher + session store
   - Persist pane/window/session state
   - Read state via CLI + MCP
   - Verify parity and expected audit records
2. **Failure injection**
   - Inject malformed protocol frame
   - Force policy denial scenario
   - Simulate store-read miss and expiry race
   - Assert fail-closed behavior and reason-coded errors
3. **Recovery path**
   - Restart daemon, replay persisted records
   - Verify state restoration and audit continuity
4. **Rollback validation**
   - Disable `redis-session` feature path
   - Confirm fallback behavior remains equivalent to baseline

For each script capture:
- preconditions
- command sequence
- expected structured outputs
- explicit pass/fail assertions

### Deterministic Fixtures, Replay Corpus, and Test-Data Lifecycle (T5)

Establish fixture rules for durable, reproducible testing:

- **Fixture classes**
  - protocol frames (valid + malformed)
  - session state payloads (small/large/boundary)
  - expiry timelines (short/long/edge timestamps)
  - audit/replay streams (normal + corrupted/truncated)
- **Corpus layout**
  - `tests/fixtures/unit/*`
  - `tests/fixtures/integration/*`
  - `tests/fixtures/e2e/*`
  - `tests/fixtures/replay/*`
- **Determinism controls**
  - fixed timestamps in fixtures unless explicitly testing clock drift
  - stable ordering guarantees for replay comparisons
  - golden output snapshots for protocol + audit decoding
- **Lifecycle policy**
  - fixture version tag + changelog entry on schema changes
  - stale fixture pruning during release prep
  - mandatory fixture update when behavior contracts change

### Performance, Soak, and Anti-Flake Validation Scripts (T6)

Define sustained-load and reliability scripts with objective budgets:

- **Performance budgets**
  - session read/write latency envelopes (p50/p95/p99)
  - memory growth budget under steady load
  - command throughput floor for integration-critical paths
- **Soak scenarios**
  - 1h+ continuous pane event append/read workload
  - mixed watcher + robot + policy command pressure
  - periodic restart/recovery checkpoints during run
- **Anti-flake controls**
  - fixed seed where randomness is used
  - retry policy with failure signature bucketing
  - flaky-test quarantine criteria + owner assignment
- **Artifacts**
  - per-run metrics snapshot
  - failure signatures and correlation IDs
  - comparison summary vs previous baseline run

---

## P7: Rollout, Rollback, Risk Mitigation, and Acceptance Gates

### Rollout Plan

**Phase 1: Feature-Gated Integration (Week 1-2)**
1. Add fr-* crates as path dependencies behind `redis-session` feature
2. Implement `SessionStore`, `session_protocol`, `session_audit` modules
3. Write unit tests (30+ per module)
4. Verify no compile regression with feature disabled
5. Gate: `cargo check --workspace --all-targets` passes with and without feature

**Phase 2: Wiring Into Runtime (Week 3-4)**
1. Integrate `SessionStore` into watcher loop for pane state caching
2. Wire `AuditLog` into robot mode command execution
3. Add integration tests
4. Gate: All existing tests pass, SessionStore adds measurable value

**Phase 3: Default Enable (Week 5-6)**
1. Remove feature gate (always-on)
2. Run full regression suite
3. Performance benchmarking (memory, latency)
4. Gate: No performance regression >5% on any metric

### Rollback Plan

- **Phase 1 rollback**: Remove feature flag, revert Cargo.toml changes
- **Phase 2 rollback**: Feature flag disable, SessionStore becomes no-op
- **Phase 3 rollback**: Re-introduce feature flag, disable by default
- All rollbacks are single-commit operations

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Compile time increase | Medium | Low | fr-* crates are small (~10K LOC total) |
| Memory overhead from Store | Low | Medium | TTL-based expiry prevents unbounded growth |
| API instability in fr-store | Low | Medium | Pin to specific commit/version |
| Value cloning overhead | Medium | Low | Only for session state (small payloads) |
| Sync/async boundary issues | Low | High | All fr-* calls are synchronous, wrapped trivially |

### Acceptance Gates

1. `cargo check --workspace --all-targets` (with and without feature)
2. `cargo test --workspace` (all existing tests pass)
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. New module tests: 60+ tests across 3 modules
5. Integration tests: 4+ cross-module tests
6. Property-based tests: 5+ proptest scenarios
7. No memory regression in benchmark suite

---

## P8: Summary and Action Items

### Chosen Architecture

**Direct embedding** of 5 franken_redis crates (`fr-store`, `fr-protocol`, `fr-config`, `fr-expire`, `fr-persist`) as path dependencies, initially behind a `redis-session` feature gate.

### Three New Modules

1. **`session_store.rs`**: TTL-aware pane/window/session state with Redis Streams for event logs
2. **`session_protocol.rs`**: RESP-based IPC encoding/decoding
3. **`session_audit.rs`**: AOF-based forensic audit trail

### Implementation Order

1. Add Cargo.toml dependencies (feature-gated)
2. Implement `session_store.rs` with 30+ unit tests
3. Implement `session_audit.rs` with 15+ unit tests
4. Implement `session_protocol.rs` with 15+ unit tests
5. Wire into runtime (integration tests)
6. Property-based tests (proptest)
7. Benchmarking and performance validation
8. Remove feature gate (default-on)

### Upstream Tweak Proposals (for franken_redis)

1. **Add reference-based read access** to `fr-store`: `view_value()` method to avoid cloning
2. **Add `serde` feature gate** to `fr-store`: Enable Serialize/Deserialize on Store for snapshotting
3. **Extract `fr-expire` into micro-crate**: Already standalone, could be published independently
4. **Add background expiry support**: Timer-triggered active expiry cycle (currently manual-only)

### Beads Created/Updated

- `ft-2vuw7.2.1` (CLOSED): Research complete
- `ft-2vuw7.2.2` (CLOSED): Analysis document complete
- `ft-2vuw7.2.3` (THIS DOCUMENT): Integration plan complete
- Next: `ft-2vuw7.2.3.*` sub-beads → implementation beads

---

*Plan complete. Ready for review and implementation bead creation.*
