# Master Plan: Deeply Integrate All /dp Target Projects into FrankenTerm

> **Bead**: ft-2vuw7.16 (META Synthesis)
> **Author**: DarkMill (claude-code, opus-4.6)
> **Date**: 2026-02-22
> **Status**: Active
> **Prerequisite**: All 24 per-project PLAN_TO_DEEPLY_INTEGRATE docs (ft-2vuw7.{1..14,21..30}.3)

---

## 1. Executive Summary

This document synthesizes 24 individual deep-integration plans into a unified architecture and phased execution strategy for integrating the entire `/dp` ecosystem into FrankenTerm. Of 24 sibling projects analyzed:

- **4 skip integration** (no runtime value): rust_proxy, automated_plan_reviser_pro, agentic_coding_flywheel_setup, franken_redis (deferred — pure session cache, no clear FT need yet)
- **3 deepen existing gates**: frankensearch, franken_agent_detection, frankensqlite
- **2 in-process library imports**: fastapi_rust (web framework), fastmcp_ruse (MCP framework)
- **8 subprocess CLI bridges**: beads_rust, coding_agent_session_search, coding_agent_usage_tracker, cross_agent_session_resumer, destructive_command_guard, rano, ultimate_bug_scanner, vibe_cockpit
- **3 component extractions**: storage_ballast_helper (disk monitoring), remote_compilation_helper (command classification + circuit breaker), franken_mermaid (diagram rendering)
- **4 hybrid/multi-phase**: process_triage (CLI → embed), mcp_agent_mail_rust (MCP tools), beads_viewer_rust (algorithm extraction), frankentui (already in rollout)

**Total new modules**: ~45 across all integrations
**Total estimated LOC**: ~15,000 new, ~3,000 refactored
**Total estimated tests**: ~1,800 (30+ per module minimum)

---

## 2. Unified Target Architecture

### 2.1 Architectural Layers

```
┌─────────────────────────────────────────────────────────────────┐
│                        USER INTERFACES                          │
│  TUI (ratatui/frankentui)  │  Web (fastapi_rust)  │  CLI/Robot │
├────────────────────────────┼─────────────────────┼─────────────┤
│                        API LAYER                                │
│  MCP Server (fastmcp_ruse) │  IPC (Unix socket)  │  HTTP REST  │
├─────────────────────────────────────────────────────────────────┤
│                    ORCHESTRATION LAYER                           │
│  Workflows  │  Approval  │  Policy  │  Plan  │  Dry-Run        │
├─────────────────────────────────────────────────────────────────┤
│                     INTEGRATION BRIDGES                          │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐          │
│  │ beads_   │ │ cass     │ │ caut     │ │ command_ │          │
│  │ bridge   │ │ (search) │ │ (usage)  │ │ guard    │          │
│  ├──────────┤ ├──────────┤ ├──────────┤ ├──────────┤          │
│  │ session_ │ │ network_ │ │ code_    │ │ agent_   │          │
│  │ resume   │ │ observer │ │ scanner  │ │ mail_    │          │
│  │          │ │          │ │          │ │ bridge   │          │
│  ├──────────┤ ├──────────┤ ├──────────┤ ├──────────┤          │
│  │ vc_      │ │ rch_     │ │ diagram_ │ │ process_ │          │
│  │ export   │ │ bridge   │ │ bridge   │ │ triage   │          │
│  └──────────┘ └──────────┘ └──────────┘ └──────────┘          │
├─────────────────────────────────────────────────────────────────┤
│                      CORE ENGINE                                │
│  Ingest │ Storage │ Patterns │ Events │ Recorder │ Search      │
│  Backpressure │ Telemetry │ Snapshot │ Config │ Error          │
├─────────────────────────────────────────────────────────────────┤
│                    DATA STRUCTURES                               │
│  CRDT │ Merkle │ Persistent │ Bloom │ XOR │ Interval │ etc.   │
├─────────────────────────────────────────────────────────────────┤
│                    RUNTIME LAYER                                 │
│  asupersync (Cx) │ runtime_compat │ pool │ circuit_breaker     │
└─────────────────────────────────────────────────────────────────┘
```

### 2.2 Integration Categories

#### Category A: In-Process Library Imports (Workspace Dependencies)

These projects share the asupersync runtime and are tightly coupled:

| Project | Workspace Crate | Feature Gate | Target Module(s) |
|---------|----------------|--------------|-------------------|
| fastapi_rust | `fastapi-rust` | `web` (existing) | `web/` directory (20+ sub-modules) |
| fastmcp_ruse | `fastmcp-rust` | `mcp-server` (existing), `mcp-client` (new) | `mcp_bridge.rs`, `mcp_tools.rs`, `mcp_resources.rs`, `mcp_middleware.rs`, `mcp_client.rs`, `mcp_proxy.rs`, `mcp_error.rs` |

**Critical shared dependency**: asupersync 0.2.0 at commit `c7c15f6`. Both fastapi_rust and fastmcp_ruse depend on it. FrankenTerm already has `runtime_compat.rs` and `cx.rs` bridging asupersync.

#### Category B: Component Extraction (Library Import, Subset)

Extract specific algorithms/components without pulling the entire upstream crate:

| Project | Extracted Component | Feature Gate | Target Module(s) |
|---------|-------------------|--------------|-------------------|
| storage_ballast_helper | EWMA+PID disk monitoring, Bayesian scoring | `disk-pressure` (existing) | `disk_pressure.rs`, `disk_scoring.rs`, `disk_ballast.rs` |
| remote_compilation_helper | Command classification, circuit breaker | `rch-integration` (new) | `command_classify.rs`, `rch_bridge.rs` |
| franken_mermaid | IR translation + terminal rendering | `diagram-viz` (new) | `diagram_bridge.rs`, `diagram_render.rs` |
| beads_viewer_rust | PageRank, betweenness centrality | (none — pure math) | `graph_scoring.rs` |

#### Category C: Subprocess CLI Bridges

External tools invoked via `std::process::Command` with JSON output parsing:

| Project | Binary | Feature Gate | Target Module | Existing? |
|---------|--------|--------------|---------------|-----------|
| beads_rust | `br` | `beads-integration` (new) | `beads_bridge.rs` + `beads_types.rs` | No |
| coding_agent_session_search | `cass` | `cass-export` (new) | `cass.rs` (extend) | **Yes** |
| coding_agent_usage_tracker | `caut` | `usage-tracking` (new) | `caut.rs` (extend) | **Yes** |
| cross_agent_session_resumer | `casr` | `session-resume` (new) | `casr_types.rs`, `session_resume.rs`, `recorder_casr_export.rs` | No |
| destructive_command_guard | `dcg` (MCP) | `command-guard` (new) | `command_guard.rs` (extend) | **Yes** |
| rano | `rano` | `network-observer` (new) | `network_observer.rs` | No |
| ultimate_bug_scanner | `ubs` | `code-scanning` (new) | `code_scanner.rs` | No |
| vibe_cockpit | (collector) | `vc-export` (new) | `vc_export.rs`, `robot_envelope.rs` | No |

#### Category D: Deepen Existing Feature Gates

Already integrated; extend with richer capabilities:

| Project | Feature Gate | Existing Module | Enhancement |
|---------|-------------|----------------|-------------|
| frankensearch | `frankensearch` | `search_bridge.rs` | Recorder content search, real-time indexing, daemon health |
| franken_agent_detection | `agent-detection` | `agent_detection.rs` | Per-pane identity, session indexing, health reports |
| frankensqlite | (core dep) | `recorder_storage.rs` | Enhanced error taxonomy, health/lag telemetry |

#### Category E: Hybrid / Multi-Phase

| Project | Phase 1 | Phase 2+ | Feature Gate |
|---------|---------|----------|--------------|
| process_triage | CLI adapter (`process_triage.rs` exists) | Embedded provider | (none) |
| mcp_agent_mail_rust | MCP tool wrapper | Auto-reserve, broadcast, backpressure import | `agent-mail` (new) |
| frankentui | Already in `rollout` phase | Full migration | `ftui` (existing) |

#### Category F: Skip Integration

| Project | Reason |
|---------|--------|
| rust_proxy | Linux-only HTTP CONNECT proxy; no terminal multiplexer overlap |
| automated_plan_reviser_pro | Bash tool for GPT Pro spec refinement; orthogonal to FT |
| agentic_coding_flywheel_setup | VPS provisioning (TypeScript/Bash); runs before FT |
| franken_redis | Pure session cache; deferred until concrete FT use case emerges |

---

## 3. Feature Gate Inventory

### 3.1 Existing Feature Gates (Preserved)

| Gate | Modules | Status |
|------|---------|--------|
| `agent-detection` | `agent_detection.rs` | Active, extend |
| `asupersync-runtime` | `cx.rs` | Active |
| `browser` | `browser/` | Active |
| `disk-pressure` | `disk_ballast.rs`, `disk_pressure.rs`, `disk_scoring.rs` | Stubs, implement |
| `frankensearch` | `search_bridge.rs` | Active, extend |
| `ftui` | `tui/` (FrankenTUI backend) | Rollout |
| `mcp` | `mcp.rs` | Active, refactor |
| `metrics` | `metrics.rs` | Active |
| `native-wezterm` | `native_events.rs` | Active |
| `recorder-lexical` | `tantivy_*.rs` | Active |
| `rollout` | TUI dual-backend | Migration |
| `sync` | `sync.rs` | Active |
| `tui` | `tui/` (ratatui backend) | Active |
| `vendored` | `vendored.rs`, `wezterm_native.rs` | Active |
| `web` | `web.rs` | Active, refactor |

### 3.2 New Feature Gates (Introduced by Integration)

| Gate | Modules | Depends On |
|------|---------|-----------|
| `agent-mail` | `agent_mail_bridge.rs` | `mcp` |
| `beads-integration` | `beads_bridge.rs`, `beads_types.rs` | — |
| `cass-export` | `cass.rs` (extended) | — |
| `code-scanning` | `code_scanner.rs` | — |
| `command-guard` | `command_guard.rs` (extended) | — |
| `diagram-viz` | `diagram_bridge.rs`, `diagram_render.rs` | — |
| `mcp-client` | `mcp_client.rs`, `mcp_proxy.rs` | `mcp` |
| `network-observer` | `network_observer.rs` | — |
| `rch-integration` | `command_classify.rs`, `rch_bridge.rs` | — |
| `session-resume` | `casr_types.rs`, `session_resume.rs`, `recorder_casr_export.rs` | — |
| `usage-tracking` | `caut.rs` (extended) | — |
| `vc-export` | `vc_export.rs`, `robot_envelope.rs` | — |

### 3.3 Feature Gate Dependency Graph

```
web ──────────→ asupersync-runtime
mcp ──────────→ asupersync-runtime
mcp-client ──→ mcp
agent-mail ──→ mcp
frankensearch → recorder-lexical (optional, enhances)
session-resume (standalone)
diagram-viz (standalone)
beads-integration (standalone)
All subprocess bridges (standalone, no deps)
```

---

## 4. Phased Integration Waves

### Wave 0: Foundation (Already Complete / In Progress)

**Objective**: Shared runtime, core storage, existing feature gate hardening.

| Work Item | Status | Module |
|-----------|--------|--------|
| asupersync runtime_compat | Complete | `runtime_compat.rs` |
| asupersync Cx adapter | Complete | `cx.rs` |
| frankensqlite recorder storage | Complete | `recorder_storage.rs` |
| process_triage CLI adapter | Complete | `process_triage.rs` |
| cass subprocess wrapper | Complete | `cass.rs` |
| caut subprocess wrapper | Complete | `caut.rs` |
| command_guard MCP wrapper | Complete | `command_guard.rs` |
| disk-pressure stubs | Stubs exist | `disk_*.rs` |

### Wave 1: Subprocess Bridges (Low Risk, High Value)

**Objective**: Wire up external tool bridges behind feature gates. All use `std::process::Command` — no new dependencies.

**Duration**: ~2 weeks per module (each is 200-500 LOC + 30+ tests)

| Priority | Module | Project | New Feature Gate |
|----------|--------|---------|-----------------|
| 1.1 | `beads_bridge.rs` + `beads_types.rs` | beads_rust | `beads-integration` |
| 1.2 | `network_observer.rs` | rano | `network-observer` |
| 1.3 | `code_scanner.rs` | ultimate_bug_scanner | `code-scanning` |
| 1.4 | `vc_export.rs` + `robot_envelope.rs` | vibe_cockpit | `vc-export` |
| 1.5 | Extend `cass.rs` (export API) | coding_agent_session_search | `cass-export` |
| 1.6 | Extend `caut.rs` (backpressure) | coding_agent_usage_tracker | `usage-tracking` |
| 1.7 | Extend `command_guard.rs` (SARIF) | destructive_command_guard | `command-guard` |

**Parallelism**: All 7 items are independent; can run in parallel across agents.

**Test requirement**: 30+ unit tests per module, subprocess mocking via fixture files.

### Wave 2: Component Extractions (Medium Risk)

**Objective**: Extract domain algorithms from upstream crates.

| Priority | Module | Project | New Feature Gate |
|----------|--------|---------|-----------------|
| 2.1 | `disk_pressure.rs` (implement) | storage_ballast_helper | `disk-pressure` (existing) |
| 2.2 | `disk_scoring.rs` (implement) | storage_ballast_helper | `disk-pressure` |
| 2.3 | `disk_ballast.rs` (implement) | storage_ballast_helper | `disk-pressure` |
| 2.4 | `command_classify.rs` | remote_compilation_helper | `rch-integration` |
| 2.5 | `rch_bridge.rs` | remote_compilation_helper | `rch-integration` |
| 2.6 | `graph_scoring.rs` | beads_viewer_rust | (none) |
| 2.7 | `diagram_bridge.rs` + `diagram_render.rs` | franken_mermaid | `diagram-viz` |

**Parallelism**: All items independent except 2.1-2.3 (sequential within disk-pressure).

### Wave 3: Session & Agent Bridges (Medium Risk)

**Objective**: Cross-agent session portability and agent mail coordination.

| Priority | Module | Project | New Feature Gate |
|----------|--------|---------|-----------------|
| 3.1 | `casr_types.rs` (vendored IR) | cross_agent_session_resumer | `session-resume` |
| 3.2 | `session_resume.rs` (orchestrator) | cross_agent_session_resumer | `session-resume` |
| 3.3 | `recorder_casr_export.rs` (converter) | cross_agent_session_resumer | `session-resume` |
| 3.4 | `agent_mail_bridge.rs` | mcp_agent_mail_rust | `agent-mail` |

**Dependencies**: 3.2 depends on 3.1; 3.3 depends on 3.1; 3.4 independent.

### Wave 4: Framework Migrations (High Risk, High Value)

**Objective**: Replace DIY web and MCP servers with production frameworks.

#### 4A: fastmcp_ruse Migration (Strangler Fig)

This is the highest-complexity integration. The existing `mcp.rs` (249KB, 7,035 LOC) must be incrementally migrated to fastmcp_ruse proc macros.

| Step | Module | Description |
|------|--------|-------------|
| 4A.1 | `mcp_error.rs` | Error code extraction and bidirectional mapping |
| 4A.2 | `mcp_bridge.rs` | `FtMcpState` and `ServerBuilder` wrapper |
| 4A.3 | `mcp_middleware.rs` | FormatNegotiation + AuditMiddleware |
| 4A.4 | `mcp_tools.rs` | Migrate tools one-by-one to `#[tool]` macros |
| 4A.5 | `mcp_resources.rs` | Migrate resources to `#[resource]` macros |
| 4A.6 | `mcp_client.rs` | Outbound MCP client (new `mcp-client` gate) |
| 4A.7 | `mcp_proxy.rs` | Multi-server composition/proxying |

**Migration strategy**: Strangler fig. New `mcp_bridge.rs` wraps the old `mcp.rs` handlers. Tools/resources migrate one-by-one. Old code is deleted only after equivalent new code passes all tests. Dual-stack during transition.

#### 4B: fastapi_rust Web Refactor

The existing `web.rs` (84KB) is refactored into a `web/` directory:

| Step | Module | Description |
|------|--------|-------------|
| 4B.1 | `web/mod.rs` | Move web.rs → web/mod.rs, create module skeleton |
| 4B.2 | `web/handlers/*.rs` | Extract handlers into per-domain sub-modules |
| 4B.3 | `web/extractors.rs` | Typed request extractors (FromRequest) |
| 4B.4 | `web/streaming/websocket.rs` | Terminal WebSocket protocol |
| 4B.5 | `web/streaming/sse.rs` | SSE event streaming (migrate existing) |
| 4B.6 | `web/middleware.rs` | Auth, CORS, rate limiting |
| 4B.7 | `web/openapi.rs` | OpenAPI spec generation |
| 4B.8 | `web/error.rs` | Structured error responses |

**Migration strategy**: Extract-and-replace. web.rs becomes web/mod.rs re-exporting sub-modules. Each handler file is extracted, then the old monolithic code is removed.

### Wave 5: Deepen Existing Gates (Low Risk)

**Objective**: Enhance already-integrated projects with richer capabilities.

| Priority | Enhancement | Project | Module |
|----------|-------------|---------|--------|
| 5.1 | Recorder content search | frankensearch | `search_bridge.rs` |
| 5.2 | Real-time pane indexing | frankensearch | `search_bridge.rs` |
| 5.3 | Per-pane agent identity | franken_agent_detection | `agent_detection.rs` |
| 5.4 | Session indexing for search | franken_agent_detection | `agent_correlator.rs` |
| 5.5 | Enhanced error taxonomy | frankensqlite | `recorder_storage.rs` |
| 5.6 | Process triage embedded mode | process_triage | `process_triage.rs` |

---

## 5. Shared Contracts

### 5.1 Subprocess Bridge Contract

All subprocess CLI bridges follow the same pattern:

```rust
pub struct FooBridge {
    binary_path: PathBuf,  // auto-discovered or configured
    timeout: Duration,     // default 10s
}

impl FooBridge {
    pub fn new() -> Self { /* discover binary, set defaults */ }
    pub fn is_available(&self) -> bool { /* check binary exists */ }
    // ... domain-specific methods returning Result<T, Error>
}
```

**Shared patterns**:
- Binary discovery: `which(name)` then configurable override
- Timeout: 10s default, configurable
- Output parsing: `serde_json::from_str` on stdout
- Error handling: `Error::Runtime(String)` for subprocess failures
- Fail-open: if binary unavailable, return sensible defaults (not errors)
- Feature-gated: each bridge behind its own feature flag

### 5.2 Backpressure Integration Contract

Multiple bridges feed backpressure signals into the existing `backpressure.rs` system:

| Source | Signal | Mapping |
|--------|--------|---------|
| `caut.rs` | `used_percent` | <50% Green, <80% Yellow, <95% Red, ≥95% Black |
| `network_observer.rs` | connection count | Threshold-based tier mapping |
| `disk_pressure.rs` | EWMA + PID output | `PressureLevel` → BackpressureTier |
| `agent_mail_bridge.rs` | imported from peer agents | Direct tier injection |

### 5.3 RobotEnvelope Contract

Standard wrapper for robot-mode JSON output, reusable across vibe_cockpit and other bridges:

```rust
pub struct RobotEnvelope<T> {
    pub schema_version: u32,      // starts at 1
    pub generated_at: String,     // ISO 8601
    pub staleness_ms: Option<u64>,
    pub warnings: Vec<String>,
    pub data: T,
}
```

### 5.4 asupersync Shared Runtime Contract

Projects using asupersync (fastapi_rust, fastmcp_ruse, process_triage) share:
- `Cx` capability context from `cx.rs`
- `runtime_compat.rs` bridging layer
- Cancellation propagation via `Cx::cancelled()`
- Timeout enforcement via `Cx::with_timeout()`

**Version pin**: asupersync 0.2.0 at `/dp/asupersync` commit `c7c15f6`

---

## 6. Module Placement Summary

All new modules go in `crates/frankenterm-core/src/`:

| Module | Feature Gate | LOC Est. | Tests Est. | Wave |
|--------|-------------|----------|------------|------|
| `agent_mail_bridge.rs` | `agent-mail` | 400 | 30+ | 3 |
| `beads_bridge.rs` | `beads-integration` | 300 | 30+ | 1 |
| `beads_types.rs` | `beads-integration` | 200 | 30+ | 1 |
| `casr_types.rs` | `session-resume` | 250 | 34+ | 3 |
| `code_scanner.rs` | `code-scanning` | 400 | 30+ | 1 |
| `command_classify.rs` | `rch-integration` | 300 | 30+ | 2 |
| `diagram_bridge.rs` | `diagram-viz` | 400 | 30+ | 2 |
| `diagram_render.rs` | `diagram-viz` | 400 | 30+ | 2 |
| `graph_scoring.rs` | (none) | 400 | 30+ | 2 |
| `mcp_bridge.rs` | `mcp` | 400 | 30+ | 4A |
| `mcp_client.rs` | `mcp-client` | 350 | 30+ | 4A |
| `mcp_error.rs` | `mcp` | 200 | 30+ | 4A |
| `mcp_middleware.rs` | `mcp` | 350 | 30+ | 4A |
| `mcp_proxy.rs` | `mcp-client` | 300 | 30+ | 4A |
| `mcp_resources.rs` | `mcp` | 500 | 30+ | 4A |
| `mcp_tools.rs` | `mcp` | 600 | 30+ | 4A |
| `network_observer.rs` | `network-observer` | 200 | 30+ | 1 |
| `rch_bridge.rs` | `rch-integration` | 300 | 30+ | 2 |
| `recorder_casr_export.rs` | `session-resume` | 450 | 35+ | 3 |
| `robot_envelope.rs` | `vc-export` | 150 | 30+ | 1 |
| `session_resume.rs` | `session-resume` | 400 | 35+ | 3 |
| `vc_export.rs` | `vc-export` | 350 | 30+ | 1 |
| `web/mod.rs` | `web` | 200 | — | 4B |
| `web/app_builder.rs` | `web` | 150 | 30+ | 4B |
| `web/auth.rs` | `web` | 250 | 30+ | 4B |
| `web/error.rs` | `web` | 200 | 30+ | 4B |
| `web/extractors.rs` | `web` | 400 | 30+ | 4B |
| `web/handlers/*.rs` (10) | `web` | 2,500 | 300+ | 4B |
| `web/middleware.rs` | `web` | 350 | 30+ | 4B |
| `web/openapi.rs` | `web` | 400 | 30+ | 4B |
| `web/streaming/sse.rs` | `web` | 500 | 30+ | 4B |
| `web/streaming/websocket.rs` | `web` | 600 | 30+ | 4B |
| `web/types.rs` | `web` | 300 | 30+ | 4B |

**Extensions to existing modules** (not new files):

| Module | Enhancement | Feature Gate | Tests Added |
|--------|------------|--------------|-------------|
| `agent_detection.rs` | Per-pane identity, health reports | `agent-detection` | 15+ |
| `cass.rs` | Export API, session search | `cass-export` | 30+ |
| `caut.rs` | Backpressure mapping | `usage-tracking` | 30+ |
| `command_guard.rs` | SARIF output, pipeline patterns | `command-guard` | 30+ |
| `disk_pressure.rs` | EWMA+PID implementation | `disk-pressure` | 30+ |
| `disk_scoring.rs` | Bayesian artifact scoring | `disk-pressure` | 30+ |
| `disk_ballast.rs` | Ballast management | `disk-pressure` | 30+ |
| `process_triage.rs` | Embedded provider mode | (none) | 30+ |
| `recorder_storage.rs` | Error taxonomy, health telemetry | (core) | 15+ |
| `search_bridge.rs` | Recorder search, real-time index | `frankensearch` | 10+ |

---

## 7. Dependency and Sequencing Constraints

### 7.1 Hard Dependencies (Must Complete Before)

```
Wave 0 (runtime_compat, cx) ──→ Wave 4A (fastmcp_ruse)
Wave 0 (runtime_compat, cx) ──→ Wave 4B (fastapi_rust)
Wave 3.1 (casr_types) ────────→ Wave 3.2 (session_resume)
Wave 3.1 (casr_types) ────────→ Wave 3.3 (recorder_casr_export)
Wave 4A.1 (mcp_error) ────────→ Wave 4A.2-7 (all mcp_* modules)
Wave 4B.1 (web/mod.rs) ───────→ Wave 4B.2-8 (all web/* modules)
```

### 7.2 Soft Dependencies (Enhances If Present)

```
frankensearch ──enhances──→ recorder content search
agent-detection ──enhances──→ session indexing
cass-export ──enhances──→ cross_agent_session_resumer
beads-integration ──enhances──→ graph_scoring display
```

### 7.3 Critical Path

```
Wave 0 → Wave 4A.1 → 4A.2 → 4A.4 (tool migration, one-by-one) → 4A.5 → old mcp.rs deletion
```

The critical path runs through the MCP migration because it's the highest-complexity refactor and the MCP server is the primary agent API surface.

### 7.4 Parallelism Opportunities

- **All of Wave 1** can execute in parallel (7 independent subprocess bridges)
- **All of Wave 2** can execute in parallel (except disk_pressure 2.1→2.2→2.3)
- **Wave 3** items 3.1-3.3 are sequential; 3.4 is independent
- **Wave 4A and 4B** are independent of each other
- **Wave 5** items are independent of each other

---

## 8. Risk Matrix

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| asupersync API churn breaks Wave 4 | Medium | High | Pin to specific commit; integration tests in CI |
| mcp.rs migration introduces regression | High | High | Strangler fig; dual-stack; per-tool migration with test gates |
| web.rs refactor breaks existing endpoints | Medium | High | Extract-and-replace; keep old paths active until all handlers migrated |
| Subprocess binary not installed | Low | Low | Fail-open design; `is_available()` checks; graceful degradation |
| Feature gate combinatorial explosion | Medium | Medium | CI matrix tests all gate combinations; minimal inter-gate dependencies |
| Concurrent agent conflicts during migration | Medium | Medium | File reservations via agent mail; surgical git add |
| Upstream crate version drift | Low | Medium | Pin to workspace version; periodic batch updates |

---

## 9. Testing Strategy

### 9.1 Per-Module Minimums

- **Every new module**: 30+ unit tests
- **Every proptest module**: 15+ properties
- **Integration test files**: 20+ tests per cross-module boundary

### 9.2 Feature Gate Testing

CI must validate these combinations (at minimum):
1. `cargo check -p frankenterm-core --lib` (default features)
2. `cargo check -p frankenterm-core --all-features`
3. `cargo check -p frankenterm-core --no-default-features`
4. Each new feature gate individually: `cargo check -p frankenterm-core --features <gate>`

### 9.3 Migration Testing (Wave 4)

- Dual-stack tests: old mcp.rs handler and new mcp_tools.rs handler produce identical output
- A/B endpoint tests: old web.rs handler and new web/handlers/*.rs return same responses
- Regression suite: existing MCP tool tests must pass unchanged during migration

---

## 10. Rollback Strategy

### Wave 1-3 (Bridges)
Rollback: disable feature gate. Zero impact on core functionality.

### Wave 4A (MCP Migration)
Rollback: revert `mcp_bridge.rs` delegation; old `mcp.rs` remains functional.
The strangler fig pattern ensures old code is never deleted until new code is proven.

### Wave 4B (Web Refactor)
Rollback: revert `web/mod.rs` to single-file `web.rs`. All handlers remain in one file.
Since this is an internal refactor (same API surface), rollback is mechanical.

### Wave 5 (Deepening)
Rollback: revert individual enhancement PRs. Each is a small, isolated change.

---

## 11. Success Metrics

| Metric | Target |
|--------|--------|
| All new modules have 30+ tests | 100% |
| Feature gate CI passes all combinations | Green |
| No `unsafe` code introduced | 0 violations |
| MCP migration: zero tool regression | All existing tests pass |
| Web refactor: zero endpoint regression | All existing tests pass |
| Subprocess bridges: fail-open verified | Each bridge has unavailable-binary test |
| Total new test count | ≥1,500 |
| All integration beads closed | 24/24 plans, 20/20 active integrations |

---

## 12. Project-by-Project Summary Table

| # | Project | Decision | New Modules | Feature Gate | Wave | Tests |
|---|---------|----------|-------------|-------------|------|-------|
| 1 | frankensqlite | Deepen (core) | 0 | — | 0/5 | 15+ |
| 2 | franken_redis | Skip (deferred) | 0 | — | — | 0 |
| 3 | frankentui | Deepen (rollout) | 0 | `ftui` | 0 | — |
| 4 | franken_mermaid | Extract | 2 | `diagram-viz` | 2 | 60+ |
| 5 | process_triage | Hybrid (CLI→embed) | 0 (extend) | — | 0/5 | 30+ |
| 6 | storage_ballast_helper | Extract | 3 | `disk-pressure` | 2 | 90+ |
| 7 | remote_compilation_helper | Extract | 2 | `rch-integration` | 2 | 60+ |
| 8 | vibe_cockpit | Subprocess | 2 | `vc-export` | 1 | 60+ |
| 9 | beads_rust | Subprocess | 2 | `beads-integration` | 1 | 60+ |
| 10 | beads_viewer_rust | Extract | 1 | — | 2 | 30+ |
| 11 | mcp_agent_mail_rust | MCP client | 1 | `agent-mail` | 3 | 30+ |
| 12 | coding_agent_session_search | Subprocess | 0 (extend) | `cass-export` | 1 | 30+ |
| 13 | destructive_command_guard | MCP client | 0 (extend) | `command-guard` | 1 | 30+ |
| 14 | ultimate_bug_scanner | Subprocess | 1 | `code-scanning` | 1 | 30+ |
| 21 | frankensearch | Deepen | 0 (extend) | `frankensearch` | 5 | 10+ |
| 22 | rust_proxy | Skip | 0 | — | — | 0 |
| 23 | rano | Subprocess | 1 | `network-observer` | 1 | 30+ |
| 24 | franken_agent_detection | Deepen | 0 (extend) | `agent-detection` | 5 | 15+ |
| 25 | fastapi_rust | Library import | 20+ | `web` | 4B | 659+ |
| 26 | fastmcp_ruse | Library import | 7 | `mcp`/`mcp-client` | 4A | 220+ |
| 27 | cross_agent_session_resumer | Subprocess+vendored | 3 | `session-resume` | 3 | 160+ |
| 28 | coding_agent_usage_tracker | Subprocess | 0 (extend) | `usage-tracking` | 1 | 30+ |
| 29 | automated_plan_reviser_pro | Skip | 0 | — | — | 0 |
| 30 | agentic_coding_flywheel_setup | Skip | 0 | — | — | 0 |

---

## 13. Execution Recommendation

**Immediate next steps** (can run in parallel):

1. **Wave 1 sprint**: Assign 4-7 agents to implement all subprocess bridges simultaneously. Each is independent, ~200-500 LOC, and can be completed in a single session.

2. **Wave 2 sprint**: Assign 3-4 agents to component extractions. disk_pressure is the most impactful (feeds backpressure).

3. **Wave 4A kickoff**: Begin `mcp_error.rs` extraction as the foundation for the MCP strangler fig.

**Deferred** (until Wave 1-2 complete):

4. Wave 3 (session/agent bridges) — depends on Wave 1 patterns being proven
5. Wave 4B (web refactor) — large scope, lower urgency than MCP
6. Wave 5 (deepening) — incremental, can interleave with other work

---

*Master plan complete. All 24 projects classified, sequenced, and architecturally placed.*
