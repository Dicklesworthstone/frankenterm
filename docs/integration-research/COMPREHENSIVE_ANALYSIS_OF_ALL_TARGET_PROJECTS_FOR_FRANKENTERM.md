# Comprehensive Cross-Project Analysis Synthesis for FrankenTerm

> **Bead**: ft-2vuw7.15 (META Cross-Project Analysis)
> **Author**: DarkMill (claude-code, opus-4.6)
> **Date**: 2026-02-22
> **Prerequisite**: All 24 per-project COMPREHENSIVE_ANALYSIS docs

---

## 1. Executive Summary

This synthesis compares 24 sibling projects in the `/dp` ecosystem across seven dimensions: architecture, runtime compatibility, integration complexity, risk profile, shared primitives, synergy potential, and strategic value to FrankenTerm.

**Key findings**:
- **3 projects** share the asupersync runtime and form a natural integration cluster (fastapi_rust, fastmcp_ruse, process_triage)
- **2 projects** are already deeply integrated via feature gates (frankensearch, franken_agent_detection)
- **8 projects** are pure CLI tools invokable via subprocess — lowest risk integration pattern
- **4 projects** have no meaningful runtime overlap and are skipped
- **The MCP stack** (fastmcp_ruse + mcp_agent_mail_rust) represents the highest-value integration cluster

---

## 2. Comparison Rubric

Each project scored 1-5 across these dimensions:

| Dimension | Weight | Description |
|-----------|--------|-------------|
| **Runtime Compatibility** | 25% | Shared async runtime (asupersync), Rust edition, dependency overlap |
| **Functional Overlap** | 20% | Reusable algorithms, shared data types, common patterns |
| **Integration Complexity** | 20% | LOC to integrate, dependency count, API surface area |
| **Strategic Value** | 20% | Impact on FrankenTerm's core mission (agent swarm management) |
| **Risk Profile** | 15% | Maintenance burden, upstream stability, breaking change likelihood |

### Scoring Scale
- **5**: Exceptional fit / minimal risk
- **4**: Strong fit / low risk
- **3**: Moderate fit / manageable risk
- **2**: Weak fit / elevated risk
- **1**: Poor fit / high risk or no value

---

## 3. Comparative Score Matrix

| # | Project | LOC | Runtime | Overlap | Complexity | Strategic | Risk | **Weighted** |
|---|---------|-----|---------|---------|------------|-----------|------|-------------|
| 1 | frankensqlite | 38K | 5 | 5 | 4 | 5 | 5 | **4.80** |
| 26 | fastmcp_ruse | 105K | 5 | 5 | 2 | 5 | 3 | **4.05** |
| 21 | frankensearch | 244K | 4 | 5 | 3 | 5 | 4 | **4.20** |
| 24 | franken_agent_detection | 22K | 4 | 4 | 4 | 5 | 4 | **4.20** |
| 25 | fastapi_rust | 111K | 5 | 4 | 2 | 4 | 3 | **3.65** |
| 5 | process_triage | 29K | 4 | 4 | 3 | 4 | 4 | **3.80** |
| 27 | cross_agent_session_resumer | 19K | 3 | 3 | 3 | 4 | 4 | **3.40** |
| 12 | coding_agent_session_search | 21K | 3 | 3 | 4 | 4 | 4 | **3.55** |
| 11 | mcp_agent_mail_rust | 12K | 3 | 3 | 3 | 4 | 4 | **3.40** |
| 28 | coding_agent_usage_tracker | 40K | 3 | 3 | 4 | 4 | 4 | **3.55** |
| 13 | destructive_command_guard | 8K | 3 | 2 | 4 | 4 | 5 | **3.50** |
| 14 | ultimate_bug_scanner | 15K | 3 | 2 | 4 | 3 | 4 | **3.15** |
| 9 | beads_rust | 32K | 3 | 3 | 4 | 3 | 4 | **3.35** |
| 10 | beads_viewer_rust | 18K | 2 | 3 | 4 | 3 | 4 | **3.10** |
| 6 | storage_ballast_helper | 11K | 3 | 4 | 3 | 3 | 4 | **3.40** |
| 7 | remote_compilation_helper | 24K | 3 | 3 | 3 | 3 | 3 | **3.05** |
| 4 | franken_mermaid | 45K | 2 | 3 | 3 | 3 | 3 | **2.85** |
| 8 | vibe_cockpit | 35K | 2 | 2 | 3 | 3 | 3 | **2.55** |
| 3 | frankentui | 28K | 3 | 4 | 3 | 3 | 3 | **3.25** |
| 23 | rano | 13K | 2 | 2 | 4 | 3 | 4 | **2.90** |
| 2 | franken_redis | 15K | 2 | 2 | 3 | 2 | 3 | **2.35** |
| 22 | rust_proxy | 18K | 1 | 1 | 5 | 1 | 4 | **2.05** |
| 29 | automated_plan_reviser_pro | 7K | 1 | 1 | 5 | 1 | 3 | **1.90** |
| 30 | agentic_coding_flywheel_setup | 5K | 1 | 1 | 5 | 1 | 3 | **1.90** |

---

## 4. Integration Clusters

### Cluster 1: Core Runtime (asupersync shared)

```
fastapi_rust ←──── asupersync 0.2.0 ────→ fastmcp_ruse
                         ↑
                   process_triage
                         ↑
              runtime_compat.rs + cx.rs (already in FT)
```

**Shared contracts**: `Cx` capability context, cancellation tokens, timeout propagation.
**Risk**: asupersync API churn affects all three simultaneously.
**Mitigation**: Pin to workspace version, batch-update.

### Cluster 2: Agent Intelligence

```
franken_agent_detection ──→ per-pane agent identity
coding_agent_session_search ──→ agent session history
cross_agent_session_resumer ──→ session portability
coding_agent_usage_tracker ──→ rate limit awareness
mcp_agent_mail_rust ──→ inter-agent coordination
```

**Shared primitives**: Agent identity strings, session IDs, provider enums.
**Synergy**: Combined, these give FrankenTerm comprehensive agent lifecycle management — detection → tracking → coordination → session preservation.

### Cluster 3: Safety & Guardrails

```
destructive_command_guard ──→ command interception
ultimate_bug_scanner ──→ code quality gate
beads_rust ──→ task tracking/auditing
```

**Shared primitives**: Decision types (Allow/Deny/Warn), finding/report structures.
**Synergy**: DCG prevents dangerous commands; UBS scans for code issues; beads tracks remediation.

### Cluster 4: Observability

```
storage_ballast_helper ──→ disk pressure
remote_compilation_helper ──→ build status
rano ──→ network attribution
vibe_cockpit ──→ session telemetry export
```

**Shared primitives**: Pressure/tier enums (Green/Yellow/Red/Black), metric snapshots.
**Synergy**: All feed into backpressure system. Combined, they provide holistic resource awareness.

### Cluster 5: Already Integrated

```
frankensearch ──→ search_bridge.rs (feature: frankensearch)
franken_agent_detection ──→ agent_detection.rs (feature: agent-detection)
frankensqlite ──→ recorder_storage.rs (core dep)
frankentui ──→ tui/ (feature: ftui, rollout)
```

**Status**: Foundation complete. Deepening = incremental enhancements.

---

## 5. Shared Primitives & Reusable Patterns

### 5.1 Types That Recur Across Projects

| Type Pattern | Where Used | FrankenTerm Equivalent |
|-------------|-----------|----------------------|
| Pressure tier (Green/Yellow/Red/Black) | caut, sbh, rano, rch | `backpressure.rs::BackpressureTier` |
| Agent identity string | fad, cass, casr, caut | `agent_correlator.rs::AgentId` |
| Session ID (UUID) | cass, casr, vibe_cockpit | `session_dna.rs::SessionId` |
| Provider enum (Claude/GPT/Gemini/...) | caut, cass, casr | Should be unified |
| Decision (Allow/Deny/Warn) | dcg, ubs | `command_guard.rs::GuardDecision` |
| JSON subprocess envelope | All CLI bridges | Standardize via `RobotEnvelope<T>` |
| Error with context | All projects | `error.rs::Error::Runtime(String)` |

### 5.2 Patterns That Should Be Extracted

1. **Subprocess bridge template**: Every CLI bridge repeats binary discovery, timeout, JSON parsing, fail-open. Extract into a `SubprocessBridge` base struct.

2. **Backpressure signal adapter**: Multiple projects (caut, sbh, rano) map domain-specific metrics to 4-tier backpressure. Create a `BackpressureAdapter` trait.

3. **Provider registry**: Agent providers (Claude, GPT, Gemini, Codex) appear in caut, cass, casr, fad. Unify into a shared `AgentProvider` enum.

4. **Feature availability check**: Every integration needs `is_available()` (binary exists, service reachable). Standardize the pattern.

---

## 6. Conflict & Incompatibility Analysis

### 6.1 No Direct Conflicts Found

No two projects produce incompatible APIs or compete for the same module namespace. The integration decisions are architecturally compatible.

### 6.2 Potential Tension Points

| Tension | Projects | Resolution |
|---------|----------|-----------|
| MCP server ownership | fastmcp_ruse vs existing mcp.rs | Strangler fig migration (settled) |
| Web server ownership | fastapi_rust vs existing web.rs | Extract-and-replace (settled) |
| Runtime conflict | asupersync vs tokio | runtime_compat bridge (built) |
| Provider enum duplication | caut, cass, casr, fad | Unify in shared_types or provider_registry module |
| Feature gate explosion | 12 new gates | Minimal inter-gate deps; CI matrix validates |
| Binary availability | 8 subprocess bridges | Fail-open design; no hard deps |

### 6.3 Dependency Version Risks

| Dependency | Used By | Risk |
|-----------|---------|------|
| asupersync 0.2.0 | fastapi_rust, fastmcp_ruse, process_triage, cx.rs | Version drift if upstream moves fast |
| serde 1.x | All projects | Stable, minimal risk |
| rusqlite | frankensqlite, recorder_storage | Stable, minimal risk |
| tantivy | frankensearch | Major version changes require reindex |

---

## 7. Strategic Synergy Ranking

Ranked by combined strategic value when multiple integrations activate together:

### Tier 1: Transformative (Score ≥ 4.0)

1. **frankensqlite** (4.80) — Core storage foundation. Already deeply integrated. Minimal remaining work.

2. **frankensearch** (4.20) — Production search replacing DIY. Already feature-gated. Deepening adds recorder content search.

3. **franken_agent_detection** (4.20) — Agent identity per-pane. Already feature-gated. Deepening adds session indexing.

4. **fastmcp_ruse** (4.05) — Replaces 7K-line monolithic mcp.rs with production framework. Highest-complexity but highest-ROI single integration.

### Tier 2: High Value (Score 3.4-3.9)

5. **process_triage** (3.80) — Intelligent zombie process management. Already has CLI adapter.

6. **fastapi_rust** (3.65) — Production web framework replacing DIY HTTP. Large scope but clear migration path.

7. **coding_agent_session_search** (3.55) — Agent session history for context injection.

8. **coding_agent_usage_tracker** (3.55) — Rate limit awareness for agent scheduling.

9. **destructive_command_guard** (3.50) — Safety guardrail for destructive commands.

10. **cross_agent_session_resumer** (3.40) — Session portability across agent providers.

11. **mcp_agent_mail_rust** (3.40) — Inter-agent coordination via MCP.

12. **storage_ballast_helper** (3.40) — Disk pressure monitoring for proactive eviction.

### Tier 3: Moderate Value (Score 2.8-3.3)

13. **beads_rust** (3.35) — Task tracking integration.
14. **frankentui** (3.25) — TUI framework migration (already in rollout).
15. **ultimate_bug_scanner** (3.15) — Code quality gate.
16. **beads_viewer_rust** (3.10) — Graph algorithms for task prioritization.
17. **remote_compilation_helper** (3.05) — Build awareness and command classification.
18. **rano** (2.90) — Network attribution.
19. **franken_mermaid** (2.85) — Diagram visualization.

### Tier 4: Low/No Value (Score < 2.8)

20. **vibe_cockpit** (2.55) — Session telemetry export.
21. **franken_redis** (2.35) — Deferred, no clear FT use case.
22. **rust_proxy** (2.05) — Skip (Linux-only, no overlap).
23. **automated_plan_reviser_pro** (1.90) — Skip (Bash, orthogonal).
24. **agentic_coding_flywheel_setup** (1.90) — Skip (provisioning, different lifecycle).

---

## 8. Architectural Compatibility Matrix

| Project | `#![forbid(unsafe)]` | Rust Edition | Async Runtime | FrankenTerm Compatible |
|---------|---------------------|-------------|---------------|----------------------|
| frankensqlite | Yes | 2024 | asupersync | Direct workspace dep |
| franken_redis | Yes | 2024 | tokio | Needs runtime bridge |
| frankentui | Yes | 2024 | — (sync) | Already in rollout |
| franken_mermaid | Yes | 2024 | — (sync) | Direct import |
| process_triage | Yes | 2024 | asupersync | CLI adapter exists |
| storage_ballast_helper | Yes | 2024 | — (sync) | Component extraction |
| remote_compilation_helper | Yes | 2024 | tokio | Subset extraction |
| vibe_cockpit | Yes | 2024 | tokio | Export API only |
| beads_rust | Yes | 2024 | — (sync) | CLI subprocess |
| beads_viewer_rust | Yes | 2024 | — (sync) | Algorithm extraction |
| mcp_agent_mail_rust | Yes | 2024 | — (MCP client) | MCP tool wrapping |
| coding_agent_session_search | Yes | 2024 | tokio | CLI subprocess |
| destructive_command_guard | Yes | 2024 | — (sync) | MCP/CLI subprocess |
| ultimate_bug_scanner | Yes | 2024 | tokio | CLI subprocess |
| frankensearch | Yes | 2024 | asupersync | Already integrated |
| rust_proxy | Yes | 2024 | tokio | **Incompatible** (Linux-only) |
| rano | Yes | 2024 | tokio | CLI subprocess |
| franken_agent_detection | Yes | 2024 | — (sync) | Already integrated |
| fastapi_rust | Yes | 2024 | asupersync | Workspace dep |
| fastmcp_ruse | Yes | 2024 | asupersync | Workspace dep |
| cross_agent_session_resumer | Yes | 2024 | tokio | CLI subprocess |
| coding_agent_usage_tracker | Yes | 2024 | tokio | CLI subprocess |
| automated_plan_reviser_pro | — | Bash | — | **Skip** |
| agentic_coding_flywheel_setup | — | TypeScript/Bash | — | **Skip** |

All Rust projects use `#![forbid(unsafe_code)]` — compatible with FrankenTerm's safety constraint.

---

## 9. Common Architecture Patterns Across Projects

### 9.1 Layered Plugin Architecture
Most /dp projects follow: `Core Library` → `CLI binary` → `Optional MCP/HTTP server`. FrankenTerm integrates at the library layer when possible, CLI layer otherwise.

### 9.2 JSON-first IPC
All CLI tools output structured JSON via `--robot` or `--json` flags. FrankenTerm's `robot_types.rs` already handles this pattern.

### 9.3 Provider Abstraction
Projects handling multiple AI providers (caut, cass, casr, fad) use similar provider enum patterns. A shared `AgentProvider` enum should be defined in FrankenTerm core.

### 9.4 Graceful Degradation
Every project is designed to degrade gracefully when unavailable. This maps directly to FrankenTerm's fail-open integration pattern.

---

## 10. Recommendations

1. **Prioritize Cluster 1** (asupersync shared runtime): The fastmcp_ruse migration is the single highest-value integration and should start immediately.

2. **Wave 1 subprocess bridges are embarrassingly parallel**: Assign one agent per bridge; all 7 can execute simultaneously.

3. **Unify provider enums before Wave 3**: Create a shared `AgentProvider` enum in core before implementing agent intelligence bridges.

4. **Extract `SubprocessBridge` base**: Before Wave 1, create a reusable subprocess template to avoid pattern duplication across 8 bridges.

5. **Monitor asupersync upstream**: Pin version but watch for breaking changes that would affect all three library imports simultaneously.

---

*Cross-project analysis synthesis complete. See MASTER_PLAN_TO_DEEPLY_INTEGRATE_ALL_TARGET_PROJECTS_INTO_FRANKENTERM.md for execution strategy.*
