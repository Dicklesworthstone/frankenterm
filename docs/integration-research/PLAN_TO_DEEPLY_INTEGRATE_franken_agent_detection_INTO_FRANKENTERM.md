# Plan to Deeply Integrate franken_agent_detection into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.24.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_franken_agent_detection.md (ft-2vuw7.24.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Per-pane agent identity**: Use detection to identify which AI agent runs in each FrankenTerm pane
2. **Session indexing via connectors**: Feed NormalizedConversation data into flight recorder search
3. **Token usage extraction**: Extract token counts from agent sessions for cost attribution
4. **Agent health dashboard**: Periodic detection re-runs to monitor agent installation status

### Constraints

- **Already integrated**: `agent-detection` feature gate is default-on; re-exported via `agent_detection.rs`
- **Synchronous API**: franken_agent_detection is fully sync; wrap in `spawn_blocking` for async contexts
- **No new dependencies**: Already a crate dependency

### Non-Goals

- **Reimplementing detection**: Use franken_agent_detection as-is
- **Adding new connectors**: Upstream responsibility
- **Real-time monitoring**: Detection is a point-in-time probe, not a daemon

---

## P2: Evaluate Integration Patterns

### Option A: Deepen Existing Feature Gate (Chosen)

Extend the existing `agent-detection` feature integration with per-pane identity and session indexing.

**Pros**: No new dependencies, builds on working integration
**Cons**: Synchronous API needs spawn_blocking wrapper
**Chosen**: Natural extension

### Decision: Option A — Deepen existing integration

---

## P3-P4: Target Placement and API

### Existing Module (extend)

`agent_detection.rs` — Already re-exports franken_agent_detection.

### New Functionality

```rust
// In agent_detection.rs (existing module)
pub fn detect_pane_agent(pane_pid: u32) -> Option<String>;
pub async fn scan_agent_sessions(connector_slug: &str) -> Result<Vec<NormalizedConversation>>;
pub fn agent_health_report() -> InstalledAgentDetectionReport;
```

Alternatively, add a thin `pane_agent_identity.rs` if the existing module grows too large.

---

## P5-P8: Testing, Rollout

**No migration needed.**

**Tests**: Add 15+ tests to existing test suite for per-pane identity detection.

**Rollout**: Phase 1 (per-pane identity) → Phase 2 (session indexing for recorder) → Phase 3 (health dashboard widget).

**Rollback**: Existing feature gate disables everything.

### Summary

Deepen existing `agent-detection` feature-gated integration. No new modules (extend existing `agent_detection.rs`). No new dependencies. Add per-pane identity, session indexing, and health reporting.

---

*Plan complete.*
