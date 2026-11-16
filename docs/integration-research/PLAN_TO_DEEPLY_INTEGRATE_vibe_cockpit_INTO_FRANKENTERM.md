# Plan to Deeply Integrate vibe_cockpit into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.8.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_vibe_cockpit.md (ft-2vuw7.8.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **FrankenTerm as a Vibe Cockpit collector**: Implement a VC collector that ingests FrankenTerm's flight recorder data (session events, pane activity, capture snapshots) into VC's DuckDB store
2. **Cursor-based incremental export**: Add an export API to FrankenTerm's flight recorder that supports VC's cursor types (Timestamp, PrimaryKey) for efficient incremental collection
3. **RobotEnvelope adoption**: Adopt VC's `RobotEnvelope<T>` format for FrankenTerm's robot-mode CLI output (schema version, staleness, warnings)
4. **Alert rule integration**: Wire FrankenTerm's health metrics (backpressure level, mux latency, capture rate) as VC alert sources so fleet-wide anomaly detection covers terminal state
5. **MCP tool exposure**: Expose FrankenTerm session data through VC's MCP server, enabling agents to query terminal history alongside other fleet data

### Constraints

- **VC is the consumer, not FrankenTerm**: FrankenTerm exports data; VC imports it. The integration is primarily on the VC side (new collector) with supporting export APIs on the FrankenTerm side
- **No DuckDB dependency in FrankenTerm**: DuckDB (1.4) is large; FrankenTerm exports JSON/JSONL, VC handles storage
- **Feature-gated**: FrankenTerm export APIs behind `vc-export` feature flag
- **Async-compatible**: VC collectors are async (tokio); FrankenTerm exports must be callable from async contexts
- **No VC runtime dependency**: FrankenTerm works without VC installed

### Non-Goals

- **Embedding VC's TUI in FrankenTerm**: VC has its own TUI (13 screens); FrankenTerm shows its own dashboards
- **Duplicating VC's DuckDB store**: FrankenTerm keeps its SQLite recorder; VC manages the OLAP layer
- **Building VC collectors inside FrankenTerm**: The collector implementation lives in VC's codebase
- **Replacing FrankenTerm's health system**: VC's Guardian augments, not replaces, FrankenTerm's backpressure/telemetry
- **Real-time streaming**: Phase 1 uses polling-based collection; streaming is a Phase 3+ goal

---

## P2: Evaluate Integration Patterns

### Option A: Export API + External Collector (Chosen)

FrankenTerm implements export APIs (JSON/JSONL). VC implements a `FrankenTermCollector` that calls these APIs.

**Pros**: Clean separation, minimal coupling, each project evolves independently, FrankenTerm stays lightweight
**Cons**: Two codebases need coordinating; data contract must be documented
**Chosen**: Follows VC's existing collector pattern; 14 other collectors work this way

### Option B: Shared Library Crate

Extract `ft-vc-bridge` shared crate used by both projects.

**Pros**: Single source of truth for data types
**Cons**: Creates bidirectional dependency; versioning complexity
**Rejected**: Premature; export API is sufficient for Phase 1

### Option C: MCP Tool Bridge

FrankenTerm exposes MCP tools; VC collector calls MCP tools instead of CLI/JSON.

**Pros**: Machine-friendly protocol, no subprocess overhead
**Cons**: Requires FrankenTerm MCP server running; more complex than JSON export
**Considered for Phase 2**: After JSON export proves the data model

### Option D: Direct SQLite Access

VC collector reads FrankenTerm's SQLite recorder directly.

**Pros**: No export API needed; fastest data access
**Cons**: Tight coupling to SQLite schema; concurrent access risks; no access control
**Rejected**: Violates encapsulation; schema changes would break VC collector

### Decision: Option A — FrankenTerm exports; VC collects

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement (FrankenTerm Side)

```
frankenterm-core/
├── src/
│   ├── vc_export.rs              # NEW: Export API for VC collector consumption
│   ├── robot_envelope.rs         # NEW: RobotEnvelope<T> standard output wrapper
│   └── ...existing modules...
├── Cargo.toml
```

### Architecture Placement (VC Side — for reference)

```
vibe_cockpit/
├── vc_collect/src/
│   ├── collectors/
│   │   ├── frankenterm.rs        # NEW: FrankenTerm collector implementation
│   │   └── ...existing collectors...
```

### Module Responsibilities

#### `vc_export.rs` — Export API for VC consumption

Provides structured data export from FrankenTerm's flight recorder:

- `export_sessions(since: Option<DateTime<Utc>>, cursor: Option<i64>) -> ExportResult<Vec<SessionExport>>` — Export session metadata with cursor
- `export_events(pane_id: Option<u64>, since: Option<DateTime<Utc>>, limit: usize) -> ExportResult<Vec<EventExport>>` — Export pane events
- `export_health(snapshot: bool) -> ExportResult<HealthExport>` — Current health metrics
- `ExportCursor` — Tracks export position for incremental collection
- All exports return JSONL-serializable structures

#### `robot_envelope.rs` — Standard agent-consumption wrapper

Adopts VC's `RobotEnvelope<T>` pattern for all FrankenTerm robot-mode output:

- `RobotEnvelope<T>` — Wraps any payload with schema version, timestamp, staleness, warnings
- Used by: `ft robot sessions`, `ft robot status`, `ft robot health`
- Enables VC and other tools to detect stale data and version mismatches

### Dependency Wiring

```toml
# In crates/frankenterm-core/Cargo.toml

[features]
vc-export = []  # No new deps — uses existing serde/chrono

# No new dependencies needed — export uses existing serde + chrono
```

---

## P4: API Contracts and Crate/Subcrate Extraction Roadmap

### Public API Contract: VcExport

```rust
#[cfg(feature = "vc-export")]
pub mod vc_export {
    use chrono::{DateTime, Utc};

    #[derive(Serialize)]
    pub struct SessionExport {
        pub session_id: String,
        pub started_at: DateTime<Utc>,
        pub ended_at: Option<DateTime<Utc>>,
        pub pane_count: usize,
        pub event_count: u64,
        pub agent_name: Option<String>,
        pub workspace: Option<String>,
    }

    #[derive(Serialize)]
    pub struct EventExport {
        pub event_id: i64,
        pub pane_id: u64,
        pub timestamp: DateTime<Utc>,
        pub event_type: String,        // "capture", "resize", "process_change"
        pub payload_bytes: usize,
        pub metadata: serde_json::Value,
    }

    #[derive(Serialize)]
    pub struct HealthExport {
        pub backpressure_level: String, // "green", "yellow", "red", "black"
        pub mux_latency_ms: f64,
        pub capture_rate_hz: f64,
        pub active_panes: usize,
        pub fd_usage_pct: f64,
        pub disk_pressure: Option<String>,
    }

    #[derive(Serialize)]
    pub struct ExportResult<T> {
        pub data: T,
        pub cursor: Option<i64>,       // Next cursor for incremental
        pub has_more: bool,
    }

    pub fn export_sessions(
        recorder: &RecorderHandle,
        since: Option<DateTime<Utc>>,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<ExportResult<Vec<SessionExport>>>;

    pub fn export_events(
        recorder: &RecorderHandle,
        pane_id: Option<u64>,
        since: Option<DateTime<Utc>>,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<ExportResult<Vec<EventExport>>>;

    pub fn export_health(monitor: &HealthMonitor) -> Result<HealthExport>;
}
```

### Public API Contract: RobotEnvelope

```rust
pub mod robot_envelope {
    use chrono::{DateTime, Utc};

    #[derive(Serialize)]
    pub struct RobotEnvelope<T: Serialize> {
        pub schema_version: &'static str,   // "1.0.0"
        pub generated_at: DateTime<Utc>,
        pub staleness_secs: Option<f64>,
        pub warnings: Vec<String>,
        pub data: T,
    }

    impl<T: Serialize> RobotEnvelope<T> {
        pub fn new(data: T) -> Self;
        pub fn with_warning(mut self, warning: String) -> Self;
        pub fn with_staleness(mut self, secs: f64) -> Self;
    }
}
```

### Crate Extraction Roadmap

**Phase 1**: Implement in frankenterm-core behind feature flag.

**Phase 2**: If VC and FrankenTerm share `RobotEnvelope` definition, extract to `ft-robot-envelope` micro-crate (shared types only).

**Phase 3**: If the export API stabilizes, document as a formal contract for any collector to implement.

---

## P5: Data Migration/State Synchronization and Compatibility Posture

### Migration Strategy

**No migration needed** — this adds new export capability:
- Export reads existing flight recorder data
- No schema changes to the recorder database
- VC creates its own DuckDB tables for FrankenTerm data

### State Synchronization

- **Export cursor**: VC maintains cursor state (PrimaryKey or Timestamp) for incremental collection
- **One-directional flow**: FrankenTerm → (JSON/JSONL) → VC. No feedback loop.
- **Eventual consistency**: VC may lag behind FrankenTerm's latest data by one collection interval

### Data Contract

```
VC Collection Cycle:
1. vc_collect polls FrankenTerm every 60s (configurable)
2. Calls `ft robot sessions --since=<cursor> --format=jsonl`
3. Parses JSONL output
4. Ingests into DuckDB
5. Stores new cursor for next collection
```

### Compatibility Posture

- **Additive only**: No existing APIs change
- **Schema versioned**: `RobotEnvelope.schema_version` enables backward compatibility
- **Graceful degradation**: If FrankenTerm not installed, VC collector skips silently
- **Forward compatible**: New export fields are added to metadata map, not as breaking schema changes

---

## P6: Testing Strategy and Detailed Logging/Observability

### Unit Tests (per module)

#### `vc_export.rs` tests (target: 30+)

- `test_export_sessions_empty_recorder` — empty recorder returns empty list
- `test_export_sessions_with_data` — sessions returned correctly
- `test_export_sessions_cursor_pagination` — cursor advances correctly
- `test_export_sessions_since_filter` — time filter applied
- `test_export_sessions_has_more` — has_more flag correct
- `test_export_events_by_pane` — pane filter works
- `test_export_events_limit` — respects limit parameter
- `test_export_events_cursor_incremental` — incremental export works
- `test_export_health_green` — green backpressure reported
- `test_export_health_red` — red backpressure reported
- `test_export_health_with_disk_pressure` — disk pressure included
- `test_export_serialization_json` — JSON output valid
- `test_export_serialization_jsonl` — JSONL output valid
- Additional edge cases and boundary conditions for 30+ total

#### `robot_envelope.rs` tests (target: 30+)

- `test_envelope_default_schema_version` — correct version string
- `test_envelope_timestamp_utc` — generated_at is UTC
- `test_envelope_no_warnings_default` — empty warnings by default
- `test_envelope_with_warning` — warning appended correctly
- `test_envelope_with_staleness` — staleness set correctly
- `test_envelope_serialization` — JSON output valid
- `test_envelope_generic_payload` — works with any Serialize type
- `test_envelope_nested_data` — complex nested payloads
- Additional serialization and edge case tests for 30+ total

### Integration Tests

- `test_export_sessions_roundtrip` — export → deserialize → verify
- `test_export_events_consistency` — events reference valid sessions
- `test_robot_cli_uses_envelope` — `ft robot status` uses RobotEnvelope
- `test_vc_collector_contract` — simulated VC collector can parse export output

### Property-Based Tests

- `proptest_export_cursor_monotone` — cursors always advance
- `proptest_export_pagination_complete` — iterating all pages yields all records
- `proptest_envelope_round_trip` — serialize → deserialize preserves all fields
- `proptest_export_limit_respected` — result never exceeds requested limit

### Logging Requirements

```rust
tracing::debug!(
    session_count = result.data.len(),
    has_more = result.has_more,
    cursor = ?result.cursor,
    since = ?since,
    "vc_export.export_sessions"
);
```

Fields: `export_type` (sessions/events/health), `record_count`, `cursor`, `has_more`, `export_time_us`

---

## P7: Rollout, Rollback, Risk Mitigation, and Acceptance Gates

### Rollout Plan

**Phase 1: Export API (Week 1-2)**
1. Implement `vc_export.rs` with session/event/health exports
2. Implement `robot_envelope.rs` with generic envelope wrapper
3. Write 30+ unit tests per module
4. Gate: `cargo check --workspace` with and without feature

**Phase 2: CLI Integration (Week 3-4)**
1. Add `ft robot sessions --since=<ts> --cursor=<id> --format=jsonl` command
2. Add `ft robot events --pane=<id> --format=jsonl` command
3. Add `ft robot health --format=json` command
4. Wrap all robot commands in RobotEnvelope
5. Gate: CLI output parseable by VC collector (contract test)

**Phase 3: VC Collector (Week 5-6)**
1. Implement `FrankenTermCollector` in VC codebase (separate PR)
2. Register in VC's collector registry
3. Add DuckDB migration for FrankenTerm tables
4. End-to-end test: FrankenTerm running → VC collects → DuckDB query works

**Phase 4: Alert Integration (Week 7-8)**
1. Define FrankenTerm alert rules in VC (backpressure, latency, capture rate)
2. Wire health export into VC's alert evaluation pipeline
3. Gate: Alerts fire correctly on simulated anomalies

### Rollback Plan

- **Phase 1 rollback**: Remove feature flag, delete export modules
- **Phase 2 rollback**: Remove CLI commands, existing commands unchanged
- **Phase 3 rollback**: Remove VC collector (VC-side only; FrankenTerm unaffected)
- **Phase 4 rollback**: Remove alert rules (VC-side only)

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Data contract mismatch | Medium | Medium | Schema version + contract tests |
| Export performance impact | Low | Medium | Cursor-based pagination; limit defaults |
| VC collector errors | Low | Low | VC collector fails silently; no FrankenTerm impact |
| Large export payloads | Medium | Low | Default limit (1000 records); streaming for Phase 3 |
| Schema evolution breaking VC | Medium | Medium | RobotEnvelope version field; backward compat policy |

### Acceptance Gates

1. `cargo check --workspace --all-targets` (with and without feature)
2. `cargo test --workspace` (all tests pass)
3. `cargo clippy --workspace -- -D warnings`
4. 60+ new unit tests across 2 modules
5. 4+ integration tests
6. 4+ proptest scenarios
7. Contract test: VC collector can parse FrankenTerm export output

---

## P8: Summary and Action Items

### Chosen Architecture

**Export API + External Collector**: FrankenTerm implements JSONL export APIs with cursor-based pagination. VC implements a `FrankenTermCollector` following its existing collector pattern.

### Two New Modules (FrankenTerm Side)

1. **`vc_export.rs`**: Cursor-based export of sessions, events, and health for VC collector consumption
2. **`robot_envelope.rs`**: Standard `RobotEnvelope<T>` wrapper for all robot-mode CLI output

### Implementation Order

1. `robot_envelope.rs` — generic envelope (no deps)
2. `vc_export.rs` — session/event/health exports (reads flight recorder)
3. CLI commands: `ft robot sessions/events/health`
4. Wrap existing robot commands in RobotEnvelope
5. VC collector implementation (VC codebase)
6. DuckDB migration for FrankenTerm tables (VC codebase)
7. Alert rule definitions (VC codebase)
8. End-to-end integration test

### Cross-Project Coordination

| Action | Owner | Codebase |
|--------|-------|----------|
| Export API + envelope | FrankenTerm | frankenterm-core |
| CLI robot commands | FrankenTerm | frankenterm (binary) |
| FrankenTermCollector | VC | vc_collect |
| DuckDB schema | VC | vc_store |
| Alert rules | VC | vc_alert |
| Contract tests | Both | Both |

### Upstream Tweak Proposals (for vibe_cockpit)

1. **Collector template**: Provide a scaffold/example for new collector implementations
2. **Cursor type standardization**: Document cursor types (Timestamp, PrimaryKey, FileOffset) as a stable API
3. **RobotEnvelope as shared crate**: Extract `RobotEnvelope` to a micro-crate for multi-project use
4. **Collector health metrics**: Expose per-collector success/failure rates via Prometheus

### Beads Created/Updated

- `ft-2vuw7.8.1` (CLOSED): Research complete
- `ft-2vuw7.8.2` (CLOSED): Analysis document complete
- `ft-2vuw7.8.3` (THIS DOCUMENT): Integration plan complete

---

*Plan complete. Ready for review and implementation bead creation.*
