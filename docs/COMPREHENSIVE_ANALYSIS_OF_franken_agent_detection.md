# Comprehensive Analysis of franken_agent_detection

> Analysis document for FrankenTerm bead `ft-2vuw7.24.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**franken-agent-detection** is a 21.8K LOC single-crate Rust library (edition 2024, MSRV 1.85) providing deterministic, local-first detection of 16 AI coding agents via filesystem probing. Published on crates.io. Already integrated into FrankenTerm via the `agent-detection` feature gate (default-on).

**Integration Value**: Very High — already a first-class dependency with stable API.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Total LOC** | ~21,800 |
| **Crate Count** | 1 (library) |
| **Rust Edition** | 2024 |
| **MSRV** | 1.85 |
| **License** | MIT |
| **Unsafe Code** | `#![forbid(unsafe_code)]` |
| **Published** | crates.io (0.1.0) |
| **Runtime** | Fully synchronous (no tokio) |

### Module Structure

```
franken-agent-detection/
├── src/
│   ├── lib.rs            # Detection engine (652 LOC)
│   ├── types.rs          # Core types (318 LOC)
│   └── connectors/       # 15 provider implementations (~20.8K LOC)
│       ├── claude_code.rs, codex.rs, gemini.rs, cursor.rs
│       ├── cline.rs, amp.rs, aider.rs, copilot.rs
│       ├── chatgpt.rs, opencode.rs, clawdbot.rs
│       ├── vibe.rs, factory.rs, openclaw.rs, pi_agent.rs
│       ├── scan.rs, token_extraction.rs, utils.rs
│       ├── workspace_cache.rs, path_trie.rs
│       └── mod.rs         # Connector trait + registry
```

### Supported Agents (16)

aider, amp, chatgpt, claude, clawdbot, cline, codex, cursor, factory, gemini, github-copilot, opencode, openclaw, pi_agent, vibe, windsurf

---

## Core Architecture

### Detection Flow

```
detect_installed_agents(&AgentDetectOptions)
  → normalize connector slugs (canonical aliasing)
  → validate against KNOWN_CONNECTORS
  → merge env var + explicit root_overrides
  → for each connector: probe default/override roots
  → collect evidence + detected bool
  → generate JSON-serializable report (format_version=1)
```

### Key Types

- **InstalledAgentDetectionReport**: Top-level report (versioned, serde)
- **InstalledAgentDetectionEntry**: Per-agent status (slug, detected, evidence, roots)
- **DetectionResult**: Minimal detection object (detected bool + evidence + paths)
- **NormalizedConversation** (feature-gated): Full session data for indexing
- **Connector trait**: `detect()` + `scan(&ScanContext)` per provider

### Design Principles

1. **Deterministic**: Pure filesystem probes, no network
2. **Local-first**: No external dependencies when `connectors` feature disabled
3. **Runtime-neutral**: Synchronous API, zero tokio coupling
4. **Test-friendly**: `root_overrides` for fixture-based testing

---

## FrankenTerm Integration Status

**Already integrated**:
- Feature gate: `agent-detection = ["dep:franken-agent-detection"]` (default-on)
- Re-export: `crates/frankenterm-core/src/agent_detection.rs` → `pub use franken_agent_detection::*`
- Used by MCP search tools and agent discovery

### Current Integration Points

| Component | Status |
|-----------|--------|
| Feature gate in Cargo.toml | Active (default-on) |
| Module re-export | `frankenterm_core::agent_detection` |
| MCP search tool | Wired |
| Quality timeout | Configurable |
| Domain removal | Supported |

---

## Deepening Opportunities

1. **Per-pane agent identity**: Use detection to identify which agent runs in each pane
2. **Connector scan for session indexing**: Feed NormalizedConversation into flight recorder search
3. **Token usage tracking**: Extract token counts from agent sessions for cost attribution
4. **Agent health monitoring**: Periodic detection re-runs to track agent installation changes

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| New agent formats | Medium | Low | Connector model is extensible |
| Detection false positives | Low | Low | Evidence-based, not heuristic |
| API schema changes | Low | Medium | format_version=1 provides stability |

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | Single-crate library with 15 connector implementations |
| **Key Innovation** | Deterministic local-first agent detection, no network |
| **FrankenTerm Status** | Already integrated (default-on feature) |
| **Deepening Priority** | Medium — extend per-pane identity + session indexing |
| **New Modules Needed** | 0 (existing re-export sufficient) |
| **Dependencies Added** | 0 (already a dependency) |

---

*Analysis complete.*
