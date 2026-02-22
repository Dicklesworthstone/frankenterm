# Plan to Deeply Integrate mcp_agent_mail_rust into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.11.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_mcp_agent_mail_rust.md (ft-2vuw7.11.2)

---

## P1: Objectives, Constraints, and Non-Goals

### Objectives

1. **Agent coordination bridge**: Expose MCP Agent Mail's messaging and identity tools through FrankenTerm's robot mode for agent-to-agent coordination
2. **File reservation enforcement**: Integrate Agent Mail's pre-commit guard with FrankenTerm's workflow engine to prevent conflicting edits
3. **Backpressure reuse**: Import Agent Mail's 3-level backpressure system (Green/Yellow/Red) for FrankenTerm's own pane management
4. **Health monitoring**: Poll Agent Mail health endpoint from FrankenTerm's telemetry subsystem

### Constraints

- 370K LOC across 14 crates; import only the models/types, not the server
- Agent Mail runs as a separate MCP server process; FrankenTerm is a client
- Uses fastmcp 0.2 for MCP protocol (FrankenTerm already has `mcp-server` feature)
- SQLite via sqlmodel-frankensqlite (different from FrankenTerm's rusqlite)

### Non-Goals

- Embedding the Agent Mail server inside FrankenTerm
- Replacing FrankenTerm's own SQLite storage with Agent Mail's storage
- Building a custom Agent Mail UI (use existing MCP tools)
- Importing tantivy/frankensearch through Agent Mail (FrankenTerm has its own)

---

## P2: Integration Pattern

### Decision: MCP Client + Shared Type Definitions

**Phase 1**: FrankenTerm calls Agent Mail MCP tools via existing MCP client infrastructure
**Phase 2**: Extract shared types (AgentIdentity, Message, FileReservation) for type-safe interop
**Phase 3**: Import backpressure system patterns for internal use

This leverages Agent Mail's existing MCP tool surface (34 tools) without importing server code.

---

## P3: Target Placement

```
frankenterm-core/
├── src/
│   ├── agent_mail_bridge.rs   # NEW: MCP tool caller for Agent Mail
│   └── agent_mail_types.rs    # NEW: Shared type definitions
```

### Module Responsibilities

#### `agent_mail_bridge.rs` — MCP tool wrapper
- `register_agent(project: &str, program: &str, model: &str) -> AgentIdentity`
- `send_message(from: &str, to: &[&str], subject: &str, body: &str) -> MessageId`
- `fetch_inbox(agent: &str, since: Option<&str>) -> Vec<InboxMessage>`
- `reserve_files(agent: &str, paths: &[&str], ttl_secs: u64) -> ReservationResult`
- `release_files(agent: &str) -> ReleaseResult`
- `health_check() -> HealthStatus`

#### `agent_mail_types.rs` — Type definitions
- `AgentIdentity`, `InboxMessage`, `FileReservation`, `HealthStatus` matching Agent Mail's JSON schemas
- `serde` derive for JSON deserialization from MCP tool responses

---

## P4-P7: Dependency, Testing, Rollout

**Dependency cost**: Zero new Rust deps — uses existing MCP client (serde_json for JSON parsing)
**Testing**: 15+ unit tests for type deserialization, 10+ for bridge functions, 5+ integration tests with mock MCP responses
**Rollout**: Feature-gated behind `agent-mail` feature, Phase 1 Week 1-2

---

## P8: Summary

MCP client-based integration via Agent Mail's 34 existing tools. Two new modules: `agent_mail_bridge.rs` (MCP tool wrapper) and `agent_mail_types.rs` (shared types). Zero new Rust dependencies. Feature-gated behind `agent-mail`.

---

*Plan complete.*
