# Plan to Deeply Integrate coding_agent_session_search into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.12.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_coding_agent_session_search.md (ft-2vuw7.12.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **FrankenTerm as a CASS data source**: Implement a CASS connector that discovers and indexes FrankenTerm flight recorder sessions for hybrid FTS/semantic search
2. **Session search from FrankenTerm**: Expose `cass search` functionality through FrankenTerm's MCP tools and CLI for agents to search past terminal sessions
3. **Warm model daemon sharing**: Share CASS's semantic embedding daemon for both session search and FrankenTerm's own content analysis
4. **Encrypted HTML export**: Use CASS's AES-GCM encrypted HTML export for sharing terminal sessions securely
5. **Analytics widget integration**: Reuse CASS's analytics patterns (time bucketing, dimension breakdowns) for FrankenTerm's own session analytics

### Constraints

- **CASS is the search platform, FrankenTerm is the data source**: Integration is primarily a new CASS connector + FrankenTerm export APIs
- **No fastembed/ORT dependency in FrankenTerm**: ML model inference stays in CASS's warm daemon; FrankenTerm queries it
- **Feature-gated**: FrankenTerm export APIs behind `cass-export` feature flag
- **Subprocess for search**: FrankenTerm calls `cass search --robot-format json` via subprocess
- **No DuckDB or Tantivy in FrankenTerm**: These are CASS-side dependencies

### Non-Goals

- **Embedding CASS's TUI**: FrankenTerm has its own dashboards
- **Bundling ML models**: CASS handles embedding; FrankenTerm is a data provider
- **Replacing FrankenTerm's flight recorder**: CASS indexes recorder output; doesn't replace it
- **Building custom search index**: Use CASS's Tantivy + HNSW infrastructure

---

## P2: Evaluate Integration Patterns

### Option A: Export API + External Connector (Chosen)

FrankenTerm exports session data as JSON/JSONL. CASS implements a `FrankenTermConnector` in its connector framework.

**Pros**: Clean separation, follows CASS's 15+ connector pattern, minimal FrankenTerm-side work
**Cons**: Two codebases to coordinate; data schema must be documented
**Chosen**: Follows established CASS connector pattern

### Option B: Direct SQLite Access

CASS reads FrankenTerm's SQLite recorder directly.

**Pros**: No export API needed
**Cons**: Schema coupling, concurrent access risks
**Rejected**: Violates encapsulation

### Option C: MCP Tool Bridge

CASS connector calls FrankenTerm's MCP tools.

**Pros**: Machine-friendly, standard protocol
**Cons**: Requires FrankenTerm MCP server always running
**Considered for Phase 2**: After JSON export proves viable

### Decision: Option A — Export API + CASS connector

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement (FrankenTerm Side)

```
frankenterm-core/
├── src/
│   ├── cass_export.rs            # NEW: Session export for CASS connector consumption
│   ├── session_search.rs         # NEW: CASS subprocess wrapper for search queries
│   └── ...existing modules...
```

### Module Responsibilities

#### `cass_export.rs` — Session data export

- `export_sessions(since: Option<DateTime<Utc>>, cursor: Option<i64>) -> Vec<SessionRecord>` — Export recorder sessions
- `export_session_content(session_id: &str) -> Vec<ContentChunk>` — Export session content for indexing
- `SessionRecord` — Session metadata (start, end, panes, agent, workspace)
- `ContentChunk` — Indexed text blocks with timestamps and pane IDs

#### `session_search.rs` — CASS search client

- `search_sessions(query: &str, limit: usize) -> Vec<SearchResult>` — Subprocess call to `cass search`
- `SearchResult` — Session ID, score, highlights, snippets
- Graceful degradation when `cass` not installed

---

## P4: API Contracts

### Export Contract

```rust
#[cfg(feature = "cass-export")]
pub mod cass_export {
    #[derive(Serialize)]
    pub struct SessionRecord {
        pub session_id: String,
        pub started_at: DateTime<Utc>,
        pub ended_at: Option<DateTime<Utc>>,
        pub agent_type: Option<String>,    // "claude-code", "cursor", etc.
        pub workspace: Option<String>,
        pub pane_ids: Vec<u64>,
        pub event_count: u64,
        pub content_tokens: u64,
    }

    #[derive(Serialize)]
    pub struct ContentChunk {
        pub session_id: String,
        pub pane_id: u64,
        pub timestamp: DateTime<Utc>,
        pub content: String,               // Text for indexing
        pub content_type: String,          // "input", "output", "error"
    }

    pub fn export_sessions(recorder: &RecorderHandle, since: Option<DateTime<Utc>>, cursor: Option<i64>, limit: usize) -> Result<ExportResult<Vec<SessionRecord>>>;
    pub fn export_content(recorder: &RecorderHandle, session_id: &str, limit: usize) -> Result<Vec<ContentChunk>>;
}
```

### Search Contract

```rust
pub mod session_search {
    pub struct SearchResult {
        pub session_id: String,
        pub score: f64,
        pub highlights: Vec<String>,
        pub agent_type: Option<String>,
        pub timestamp: String,
    }

    pub fn search(query: &str, limit: usize) -> Result<Vec<SearchResult>>;
    pub fn is_cass_available() -> bool;
}
```

---

## P5-P6: Migration, Testing

**No migration needed** — new export and search capabilities.

### Unit Tests (60+ across 2 modules)

- Session export: 30+ tests (pagination, cursor, filtering, serialization)
- Search client: 30+ tests (query parsing, result handling, degradation, subprocess)

### Property-Based Tests

- `proptest_export_cursor_monotone` — cursors advance
- `proptest_search_query_safe` — no injection
- `proptest_export_roundtrip` — serialize/deserialize preserves fields

---

## P7: Rollout

**Phase 1**: `cass_export.rs` + `session_search.rs` behind feature flags (Week 1-2)
**Phase 2**: CLI commands `ft robot sessions export`, `ft search` (Week 3)
**Phase 3**: CASS FrankenTermConnector implementation (Week 4-5, CASS codebase)
**Phase 4**: Encrypted HTML export via `cass export-html` integration (Week 6)

### Rollback: Remove feature flags; no existing functionality affected.

---

## P8: Summary

**Export API + subprocess search**: FrankenTerm exports sessions for CASS indexing; `cass search` provides hybrid FTS/semantic search from FrankenTerm CLI.

### Two New Modules
1. **`cass_export.rs`**: Session/content export for CASS connector
2. **`session_search.rs`**: CASS subprocess search wrapper

### Upstream Tweak Proposals (for coding_agent_session_search)
1. **FrankenTermConnector in CASS**: First-class connector for FrankenTerm sessions
2. **Daemon health MCP resource**: Expose warm model health as MCP resource
3. **Batch search API**: `cass search --batch` for multiple queries

### Beads: ft-2vuw7.12.1 (CLOSED), ft-2vuw7.12.2 (CLOSED), ft-2vuw7.12.3 (THIS DOCUMENT)

---

*Plan complete.*
