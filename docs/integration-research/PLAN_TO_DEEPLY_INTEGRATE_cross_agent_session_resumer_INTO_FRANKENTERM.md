# Plan to Deeply Integrate cross_agent_session_resumer into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.27.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_cross_agent_session_resumer.md (ft-2vuw7.27.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Cross-agent session resume**: Enable FrankenTerm to resume AI sessions in different agent panes
2. **Session discovery**: List all agent sessions across installed providers for unified view
3. **Resume automation**: Generate and execute resume commands in the correct FrankenTerm pane
4. **Canonical IR reuse**: Share CanonicalSession model for session indexing and search

### Constraints

- **casr is synchronous**: Wrap in `spawn_blocking` for async FrankenTerm contexts
- **Feature-gated**: Behind `session-resume` feature flag
- **Library or subprocess**: casr exposes both library API and CLI
- **14 providers**: Must handle provider discovery and conflict resolution

### Non-Goals

- **Reimplementing session conversion**: Delegate entirely to casr
- **Modifying casr providers**: Upstream responsibility
- **Real-time session tracking**: casr is a point-in-time converter

---

## P2: Evaluate Integration Patterns

### Option A: Subprocess CLI (Chosen)

FrankenTerm calls `casr <target> resume <session-id> --json` for session conversion.

**Pros**: Zero compile-time deps, process isolation, casr evolves independently
**Cons**: Subprocess latency (100-1000ms), requires casr installed
**Chosen**: Simplest integration; avoids pulling in casr's 18.7K LOC + rusqlite dependency

### Option B: Library Import

Add casr as optional workspace dependency.

**Pros**: Type-safe, no subprocess overhead
**Cons**: Pulls in rusqlite, sha2, walkdir; 18.7K LOC; synchronous API
**Considered for Phase 2**: If subprocess latency becomes an issue

### Decision: Option A — Subprocess CLI

---

## P3: Target Placement

```
frankenterm-core/
├── src/
│   ├── session_resume.rs      # NEW: casr subprocess wrapper
```

### Module Responsibilities

#### `session_resume.rs`

- `resume_session(session_id: &str, target: &str) -> ResumeResult` — Convert and get resume command
- `list_sessions(provider: Option<&str>) -> Vec<SessionSummary>` — List discoverable sessions
- `ResumeResult` — resume command, target session ID, written paths
- `SessionSummary` — session ID, provider, workspace, message count
- Graceful degradation: Return error if casr not installed

---

## P4: API Contract

```rust
#[cfg(feature = "session-resume")]
pub mod session_resume {
    pub struct ResumeResult {
        pub resume_command: String,
        pub target_session_id: String,
        pub source_provider: String,
        pub target_provider: String,
    }

    pub struct SessionSummary {
        pub session_id: String,
        pub provider: String,
        pub workspace: Option<String>,
        pub message_count: usize,
    }

    pub struct SessionResumer;

    impl SessionResumer {
        pub async fn resume(&self, session_id: &str, target: &str) -> Result<ResumeResult>;
        pub async fn list(&self, provider: Option<&str>) -> Result<Vec<SessionSummary>>;
        pub fn is_available(&self) -> bool;
    }
}
```

---

## P5-P8: Testing, Rollout

**No migration needed.**

**Tests**: 30+ unit tests (JSON parsing, session listing, error handling, unavailable casr).

**Rollout**: Phase 1 (session_resume.rs behind feature) → Phase 2 (wire into pane management) → Phase 3 (MCP tool `ft_resume_session`).

**Rollback**: Disable feature flag.

### Summary

**Subprocess CLI** wrapping `casr --json`. One new module: `session_resume.rs`. Zero compile-time deps. Feature-gated behind `session-resume`.

---

*Plan complete.*
