# Comprehensive Analysis of Beads Rust

> Bead: ft-2vuw7.9.1 / ft-2vuw7.9.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

Beads Rust (`/dp/beads_rust`, binary: `br`) is a local-first issue tracker for AI agents implementing ~53.5K LOC in a single Rust crate. It provides 38 CLI commands for issue lifecycle management, dependency tracking, and JSONL-based git synchronization. All code enforces `#![forbid(unsafe_code)]` and uses a pure-Rust SQLite engine (fsqlite) with MVCC and WAL support.

**Key characteristics:**
- 38 CLI commands across 7 categories (lifecycle, querying, dependencies, labels, comments, sync, diagnostics)
- SQLite + JSONL hybrid persistence (fast local queries + git-friendly collaboration)
- Agent-first design: every command supports `--json` for AI integration
- Blocked cache with dependency DAG analysis for ready/blocked queries
- Deterministic JSONL export with SHA-256 content hashing
- Property-based testing (proptest), snapshot testing (insta), 94+ test files

**Integration relevance to FrankenTerm:** Medium. Beads Rust is the issue tracker used by all FrankenTerm agents. Shared infrastructure: Issue/Status/Priority types, layered config, JSONL sync, audit trail, output formatting.

---

## 2. Repository Topology

### 2.1 Structure (Single Crate)

```
/dp/beads_rust/   (v0.1.14, edition 2024, MSRV 1.85, #![forbid(unsafe_code)])
├── src/
│   ├── cli/commands/     (37 files)  — Command implementations
│   ├── storage/          (3 files)   — SQLite persistence (6K LOC)
│   ├── sync/             (3 files)   — JSONL import/export (5K LOC)
│   ├── model/            — Issue, Status, Priority, Dependency, Event types
│   ├── config/           — Layered configuration system
│   ├── error/            — Structured error handling (40+ variants)
│   ├── format/           — Output: text, rich, JSON, CSV, TOON
│   ├── output/           — Display contexts and theme management
│   ├── validation/       — Input validation framework
│   └── util/             — ID generation, hashing, time, progress
├── tests/                (94 files)  — E2E, storage, conformance, proptest, benchmarks
├── benches/              — Criterion performance benchmarks
└── docs/                 (11 files)  — Architecture, agent integration, CLI reference
```

### 2.2 Key Dependencies

| Dep | Purpose |
|-----|---------|
| clap 4.5 | CLI parsing (derive + completions) |
| fsqlite 0.1 | Pure-Rust SQLite (MVCC, WAL, FTS5) |
| serde / serde_json / serde_yml | Serialization |
| schemars 1.0 | JSON Schema generation |
| chrono 0.4 | Timestamps (RFC3339) |
| sha2 0.10 | Content hashing (SHA-256) |
| rich_rust 0.2 | Rich terminal output |
| toon_rust 0.2 | Token-efficient encoding |
| indicatif 0.18 | Progress spinners |
| proptest 1.10 / insta 1.46 | Testing |

---

## 3. Core Architecture

### 3.1 Data Model

```rust
pub struct Issue {
    id: String,                    // e.g., "bd-abc123"
    title: String,                 // 1-500 chars
    status: Status,                // open|in_progress|blocked|deferred|closed|tombstone|pinned
    priority: Priority,            // 0 (Critical) to 4 (Backlog)
    issue_type: IssueType,         // task|bug|feature|epic|chore|docs|question
    assignee: Option<String>,
    labels: Vec<String>,
    dependencies: Vec<Dependency>, // blocks|parent-child|conditional-blocks|waits-for|...
    comments: Vec<Comment>,
    // ... timestamps, notes, external refs
}
```

### 3.2 CLI Commands (38 Total)

| Category | Commands |
|----------|----------|
| Lifecycle | init, create, q, show, update, close, reopen, delete |
| Querying | list, ready, blocked, search, stale, count, lint, graph, orphans, where |
| Dependencies | dep add/remove/list/tree/cycles |
| Labels | label add/remove/list/list-all |
| Comments | comments add/list |
| Sync | sync, config, doctor, history, audit, agents, changelog, query |
| System | version, upgrade, completions, info, schema, stats |

### 3.3 Persistence (SQLite + JSONL Hybrid)

```
CLI Command → Auto-Import (if JSONL newer than DB)
    → Execute (SQLite transaction + event recording + dirty tracking)
    → Auto-Flush (export dirty issues to JSONL)
```

- **SQLite**: Fast local queries, WAL mode, blocked cache
- **JSONL**: Git-friendly, one issue per line, SHA-256 integrity
- **Atomic writes**: Temp file + rename pattern
- **Dirty tracking**: Only export modified issues

### 3.4 Dependency Analysis

- DAG constraint enforced (cycle detection via DFS)
- Blocked cache precomputed for ready/blocked queries
- Cache invalidated on dependency changes
- Topological sort for dependency tree visualization

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 Shared Components

| Component | Source | FrankenTerm Use Case | Effort |
|-----------|--------|---------------------|--------|
| **Issue/Status/Priority types** | model/ | Shared issue definitions | Low |
| **Layered config** | config/ | CLI > env > project > user > defaults | Low |
| **JSONL sync** | sync/ | Git-friendly state export/import | Medium |
| **Output formatting** | format/ | Rich/JSON/CSV/TOON output modes | Low |
| **Audit trail** | storage/events | Structured action logging | Low |
| **Validation framework** | validation/ | Input validation patterns | Low |
| **ID generation** | util/id.rs | Hash-based unique IDs | Low |

### 4.2 Integration Patterns

**Phase 1 — Loose coupling**: FrankenTerm spawns `br` CLI as subprocess, parses JSON output
**Phase 2 — Library integration**: Add `beads_rust` as dependency, use Issue types directly
**Phase 3 — Shared abstractions**: Extract config, hashing, sync to shared crate

### 4.3 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| fsqlite (non-standard SQLite) | Medium | Pure-Rust, but different from rusqlite |
| Single-crate monolith (53K LOC) | Low | Clean module boundaries allow selective import |
| Nightly toolchain required | Low | Both use edition 2024 |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Local-first issue tracker for AI agents |
| **Size** | ~53.5K LOC, single crate, 79 source files, 94 tests |
| **Architecture** | SQLite + JSONL hybrid with auto-import/auto-flush |
| **Safety** | `#![forbid(unsafe_code)]`, atomic writes, DAG constraint |
| **Performance** | Blocked cache, dirty tracking, streaming JSONL |
| **Integration Value** | Medium — Issue types, config, sync, output formatting |
| **Top Extraction** | Issue model, layered config, JSONL sync, output formatters |
| **Risk** | Low — clean modules, agent-first CLI, deterministic output |
