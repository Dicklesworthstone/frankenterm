# Plan to Deeply Integrate beads_rust into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.9.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_beads_rust.md (ft-2vuw7.9.2)

---

## P1: Objectives, Constraints, and Non-Goals

### Objectives

1. **Issue type sharing**: Import beads_rust `Issue`, `Status`, `Priority`, `IssueType`, `Dependency` types for representing tasks within FrankenTerm's workflow engine
2. **CLI subprocess integration**: Use `br` CLI with `--json` for issue queries from FrankenTerm's robot mode and MCP tools
3. **JSONL sync format**: Use beads_rust's JSONL format for persisting FrankenTerm workflow state alongside issue data in git
4. **Output formatting**: Reuse beads_rust's `format/` module patterns (Rich, JSON, CSV, TOON) for FrankenTerm's own output formatting

### Constraints

- beads_rust is a single 53K LOC crate; importing selectively requires careful module boundary management
- Uses `fsqlite` (pure-Rust SQLite), which differs from FrankenTerm's rusqlite usage
- No unsafe code (`#![forbid(unsafe_code)]`)

### Non-Goals

- Replacing FrankenTerm's own SQLite usage with fsqlite
- Embedding beads_rust as a library dependency (too large, single crate)
- Building a custom TUI for beads (use `bv` for that)

---

## P2: Integration Pattern

### Decision: Subprocess CLI + Shared Type Definitions

**Phase 1**: FrankenTerm spawns `br` CLI with `--json` flag, parses JSON output
**Phase 2**: Extract shared types (Issue, Status, Priority) to a `beads-types` micro-crate
**Phase 3**: Use JSONL format for workflow state persistence

This avoids importing the full 53K LOC crate while gaining type-safe interop.

---

## P3: Target Placement

```
frankenterm-core/
├── src/
│   ├── beads_bridge.rs    # NEW: br CLI subprocess wrapper
│   └── beads_types.rs     # NEW: Shared type definitions (or from beads-types crate)
```

### Module Responsibilities

#### `beads_bridge.rs` — CLI subprocess wrapper
- `list_ready_issues() -> Vec<Issue>` — spawns `br ready --json`
- `show_issue(id: &str) -> Issue` — spawns `br show <id> --json`
- `update_issue(id: &str, status: Status) -> Result<()>` — spawns `br update`
- `create_issue(title: &str, ...) -> String` — spawns `br create --json`

#### `beads_types.rs` — Type definitions
- `Issue`, `Status`, `Priority`, `IssueType`, `Dependency` matching beads_rust schema
- `serde` derive for JSON deserialization from `br --json` output

---

## P4-P5: Dependency, Migration, Compatibility

**No migration needed** — subprocess-based integration, no shared state
**Compatibility**: br CLI is stable; JSON output schema is versioned
**Dependency cost**: Zero new Rust deps (subprocess spawning uses std)

---

## P6: Testing Strategy

- 15+ unit tests for beads_bridge JSON parsing
- 10+ tests for type deserialization from br CLI output
- 5+ integration tests with mock br binary
- Property-based tests for Status/Priority round-trip

---

## P7: Rollout

**Phase 1**: beads_bridge.rs + beads_types.rs behind `beads-integration` feature (Week 1-2)
**Phase 2**: Wire into ft robot commands: `ft robot beads ready`, `ft robot beads show` (Week 3)
**Phase 3**: Extract beads-types crate upstream if warranted (Week 4+)

---

## P8: Summary

Subprocess-based integration via `br --json` CLI. Two new modules: `beads_bridge.rs` (subprocess wrapper) and `beads_types.rs` (shared types). Zero new Rust dependencies. Feature-gated behind `beads-integration`.

---

*Plan complete.*
