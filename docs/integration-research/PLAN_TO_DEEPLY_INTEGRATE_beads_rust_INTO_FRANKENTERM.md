# Plan to Deeply Integrate beads_rust into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.9.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_beads_rust.md (ft-2vuw7.9.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Issue type sharing**: Import beads_rust `Issue`, `Status`, `Priority`, `IssueType`, `Dependency` types for representing tasks within FrankenTerm's workflow engine
2. **CLI subprocess integration**: Use `br` CLI with `--json` for issue queries from FrankenTerm's robot mode and MCP tools
3. **JSONL sync format**: Use beads_rust's JSONL format for persisting FrankenTerm workflow state alongside issue data in git
4. **Blocked-by cache integration**: Reuse beads_rust's DAG dependency analysis for FrankenTerm's own task scheduling (which panes depend on which tasks)
5. **Output formatting**: Adopt beads_rust's multi-format output patterns (Rich, JSON, CSV, TOON) as a model for FrankenTerm's own output formatting

### Constraints

- **Single 53K LOC crate**: beads_rust is a monolith; importing selectively requires subprocess or type extraction
- **Uses fsqlite**: beads_rust uses pure-Rust SQLite (fsqlite), which differs from FrankenTerm's rusqlite usage
- **No unsafe code**: Both projects enforce `#![forbid(unsafe_code)]`
- **CLI stability**: br CLI JSON output must be treated as a stable contract
- **No async**: beads_rust is synchronous; subprocess calls from FrankenTerm's async runtime use `tokio::process::Command`

### Non-Goals

- **Replacing FrankenTerm's SQLite with fsqlite**: Different concerns; beads_rust manages issues, not terminal sessions
- **Embedding beads_rust as a library**: 53K LOC single crate is too heavy
- **Building a custom beads TUI**: Use `bv` (beads_viewer_rust) for visual triage
- **Bidirectional sync**: FrankenTerm reads from br; writes are direct `br` commands

---

## P2: Evaluate Integration Patterns

### Option A: Subprocess CLI + Shared Types (Chosen)

FrankenTerm spawns `br` CLI with `--json` flag, parses JSON output into shared type definitions.

**Pros**: Zero Rust dependency cost, stable CLI contract, process isolation
**Cons**: Subprocess latency (~50ms per call), requires br installed
**Chosen**: br CLI is the canonical interface; JSON output is well-structured

### Option B: Library Import

Add beads_rust as a path dependency.

**Pros**: Type-safe, no subprocess overhead
**Cons**: 53K LOC monolith, fsqlite dependency, compile time
**Rejected**: Too heavy for FrankenTerm's needs

### Option C: Extract `beads-types` Micro-Crate

Extract Issue/Status/Priority types to a shared crate.

**Pros**: Type-safe without full crate
**Cons**: Upstream coordination required; types may diverge
**Considered for Phase 2**: Once subprocess integration proves the data model

### Decision: Option A — Subprocess CLI with JSON parsing

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── beads_bridge.rs           # NEW: br CLI subprocess wrapper
│   ├── beads_types.rs            # NEW: Shared type definitions (serde)
│   └── ...existing modules...
├── Cargo.toml                    # No new dependencies
```

### Module Responsibilities

#### `beads_bridge.rs` — CLI subprocess wrapper

- `list_ready_issues() -> Result<Vec<Issue>>` — spawns `br ready --json`
- `show_issue(id: &str) -> Result<Issue>` — spawns `br show <id> --json`
- `update_issue(id: &str, status: Status, assignee: Option<&str>) -> Result<()>`
- `create_issue(title: &str, priority: Priority, labels: &[&str]) -> Result<String>`
- `search_issues(query: &str) -> Result<Vec<Issue>>` — spawns `br search`
- `triage_robot() -> Result<TriageResult>` — spawns `bv --robot-triage`
- Async variants using `tokio::process::Command`

#### `beads_types.rs` — Type definitions matching br JSON output

- `Issue` struct with id, title, status, priority, labels, assignee, dependencies
- `Status` enum: Open, InProgress, Closed, Deleted
- `Priority` enum: P0, P1, P2, P3
- `IssueType` enum: Task, Bug, Feature, Epic
- `Dependency` struct: from_id, to_id, kind (blocks, parent-child)
- `TriageResult` with recommendations, scores, breakdown

### Dependency Wiring

```toml
# In crates/frankenterm-core/Cargo.toml

[features]
beads-integration = []  # No new deps — subprocess + serde (already present)
```

Zero new Rust dependencies. Uses `std::process::Command` and existing serde.

---

## P4: API Contracts and Crate/Subcrate Extraction Roadmap

### Public API Contract: BeadsBridge

```rust
#[cfg(feature = "beads-integration")]
pub mod beads_bridge {
    pub struct BeadsBridge {
        project_dir: PathBuf,
        br_path: PathBuf,     // Default: "br" (from PATH)
    }

    impl BeadsBridge {
        pub fn new(project_dir: PathBuf) -> Self;
        pub fn with_br_path(mut self, path: PathBuf) -> Self;
        pub fn list_ready(&self) -> Result<Vec<Issue>>;
        pub fn show(&self, id: &str) -> Result<Issue>;
        pub fn update(&self, id: &str, status: Status, assignee: Option<&str>) -> Result<()>;
        pub fn create(&self, title: &str, priority: Priority) -> Result<String>;
        pub fn search(&self, query: &str) -> Result<Vec<Issue>>;
        pub fn robot_triage(&self) -> Result<TriageResult>;
        pub fn is_available(&self) -> bool; // Check if br binary exists
    }
}
```

### Public API Contract: BeadsTypes

```rust
#[cfg(feature = "beads-integration")]
pub mod beads_types {
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Issue {
        pub id: String,
        pub title: String,
        pub status: Status,
        pub priority: Priority,
        pub issue_type: IssueType,
        pub labels: Vec<String>,
        pub assignee: Option<String>,
        pub owner: Option<String>,
        pub created_at: String,
        pub updated_at: String,
        pub description: Option<String>,
        pub dependencies: Vec<Dependency>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Status { Open, InProgress, Closed }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Priority { P0, P1, P2, P3 }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TriageResult {
        pub recommendations: Vec<TriageRecommendation>,
        pub meta: TriageMeta,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TriageRecommendation {
        pub id: String,
        pub title: String,
        pub score: f64,
        pub reasons: Vec<String>,
        pub unblocks: usize,
    }
}
```

### Crate Extraction Roadmap

**Phase 1**: Implement types in frankenterm-core (serde derive from br JSON output).
**Phase 2**: If upstream extracts `beads-types` crate, switch to it.
**Phase 3**: If multiple tools need beads interop, extract `ft-beads-bridge` shared crate.

---

## P5: Data Migration/State Synchronization and Compatibility Posture

### Migration Strategy

**No migration needed** — subprocess integration reads br's existing database.

### State Synchronization

- **Read-only from FrankenTerm**: FrankenTerm queries br; writes go through `br update` CLI
- **No shared state**: Each tool maintains its own database
- **JSONL sync**: Both tools can read `.beads/issues.jsonl` for git-portable issue state

### Compatibility Posture

- **CLI contract**: br `--json` output is the stable interface
- **Graceful degradation**: If br not installed, `BeadsBridge::is_available()` returns false, features disabled
- **Version detection**: Parse br `--version` to detect API changes

---

## P6: Testing Strategy and Detailed Logging/Observability

### Unit Tests (per module)

#### `beads_bridge.rs` tests (target: 30+)

- `test_parse_ready_json` — parse br ready --json output
- `test_parse_show_json` — parse br show --json output
- `test_parse_triage_json` — parse bv --robot-triage output
- `test_empty_ready_list` — empty project returns empty vec
- `test_issue_with_dependencies` — dependencies parsed correctly
- `test_update_builds_correct_args` — correct CLI arguments for update
- `test_create_returns_id` — create returns new issue ID
- `test_search_query_escaping` — special characters handled
- `test_br_not_available` — graceful fallback when br missing
- `test_br_error_handling` — non-zero exit code handled
- Additional parsing and edge case tests for 30+ total

#### `beads_types.rs` tests (target: 30+)

- `test_status_roundtrip` — serialize/deserialize Status enum
- `test_priority_roundtrip` — serialize/deserialize Priority enum
- `test_issue_from_json` — full Issue deserialization
- `test_issue_minimal_fields` — Issue with only required fields
- `test_triage_result_parsing` — TriageResult from bv output
- `test_recommendation_score_bounds` — score in [0, 1]
- `test_dependency_types` — blocks vs parent-child
- Additional type validation tests for 30+ total

### Integration Tests

- `test_br_ready_integration` — actual br call with test fixture
- `test_br_show_integration` — actual issue lookup
- `test_triage_integration` — actual bv --robot-triage call
- `test_feature_gate_disabled` — no beads functionality without feature

### Property-Based Tests

- `proptest_status_roundtrip` — all Status variants round-trip
- `proptest_priority_ordering` — P0 > P1 > P2 > P3
- `proptest_issue_json_roundtrip` — serialize/deserialize preserves fields
- `proptest_search_query_safe` — no injection in search queries

### Logging Requirements

```rust
tracing::debug!(
    issue_count = result.len(),
    command = "br ready --json",
    elapsed_ms = elapsed.as_millis(),
    "beads_bridge.list_ready"
);
```

---

## P7: Rollout, Rollback, Risk Mitigation, and Acceptance Gates

### Rollout Plan

**Phase 1: Types and Bridge (Week 1-2)**
1. Implement `beads_types.rs` with serde-compatible types
2. Implement `beads_bridge.rs` with subprocess wrapper
3. Write 30+ unit tests per module
4. Gate: `cargo check --workspace` with and without feature

**Phase 2: CLI Commands (Week 3)**
1. Add `ft robot beads ready` command
2. Add `ft robot beads show <id>` command
3. Add `ft robot beads triage` command (wraps bv --robot-triage)
4. Gate: Commands produce correct output

**Phase 3: Workflow Integration (Week 4)**
1. Wire triage recommendations into FrankenTerm's task suggestion system
2. Display current issue assignment in pane metadata
3. Gate: Agents can use FrankenTerm to discover and claim beads

### Rollback Plan

- **All phases**: Remove feature flag; no existing functionality affected

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| br CLI not installed | Medium | Low | `is_available()` check; graceful degradation |
| br JSON output format change | Low | Medium | Version detection; schema versioning |
| Subprocess latency | Low | Low | Cache results; batch queries |
| br CLI crash | Low | Low | Timeout + error handling |

### Acceptance Gates

1. `cargo check --workspace --all-targets` (with and without feature)
2. `cargo test --workspace` (all tests pass)
3. 60+ new unit tests across 2 modules
4. 4+ proptest scenarios
5. br CLI integration test passes

---

## P8: Summary and Action Items

### Chosen Architecture

**Subprocess CLI** integration via `br --json` + `bv --robot-triage`. Zero new Rust dependencies.

### Two New Modules

1. **`beads_types.rs`**: Serde-compatible type definitions matching br JSON output
2. **`beads_bridge.rs`**: Async-compatible subprocess wrapper for br/bv CLI

### Implementation Order

1. `beads_types.rs` — type definitions
2. `beads_bridge.rs` — subprocess wrapper
3. CLI commands: `ft robot beads ready/show/triage`
4. Workflow integration
5. Property-based tests

### Upstream Tweak Proposals (for beads_rust)

1. **Extract `beads-types` micro-crate**: Issue, Status, Priority as shared types
2. **Stable JSON schema version**: Add `schema_version` to JSON output
3. **Robot envelope format**: Wrap `--json` output in RobotEnvelope
4. **Batch query API**: `br show --batch id1,id2,id3` to reduce subprocess calls

### Beads Created/Updated

- `ft-2vuw7.9.1` (CLOSED): Research complete
- `ft-2vuw7.9.2` (CLOSED): Analysis document complete
- `ft-2vuw7.9.3` (THIS DOCUMENT): Integration plan complete

---

*Plan complete. Ready for review and implementation bead creation.*
