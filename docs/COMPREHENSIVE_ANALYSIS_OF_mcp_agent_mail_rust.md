# Comprehensive Analysis of MCP Agent Mail (Rust)

> Bead: ft-2vuw7.11.1 / ft-2vuw7.11.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

MCP Agent Mail Rust (`/dp/mcp_agent_mail_rust`, binary: `am`) is an agent coordination platform implementing ~370K LOC across 14 Rust crates. It provides persistent messaging, file reservation negotiation, and hybrid search (FTS5 + semantic) via 34 MCP tools over HTTP/stdio. All crates enforce `#![forbid(unsafe_code)]`.

**Key characteristics:**
- 34 MCP tools in 9 clusters (infrastructure, identity, messaging, contacts, reservations, search, workflows, product bus, build slots)
- 20+ read-only MCP resources (inbox, outbox, threads, agents, diagnostics)
- Hybrid search: Tantivy FTS5 + frankensearch semantic re-ranking
- SQLite with `BEGIN CONCURRENT` (MVCC) + RaptorQ self-healing
- Git archive with Ed25519-signed commits for tamper detection
- 3-level backpressure: Green → Yellow (defer archives) → Red (shed tools)
- 15-screen ftui TUI dashboard
- Pre-commit guard blocks commits touching reserved files

**Integration relevance to FrankenTerm:** Very High. FrankenTerm agents already use MCP Agent Mail for coordination. Shared infrastructure: Agent identity, messaging protocol, file reservations, backpressure, health monitoring, TOON encoding.

---

## 2. Repository Topology

### 2.1 Workspace Structure (14 Crates)

```
/dp/mcp_agent_mail_rust/   (edition 2024, MSRV 1.95, #![forbid(unsafe_code)])
├── mcp-agent-mail/           (901 LOC)    — Main binary (MCP vs CLI mode switch)
├── mcp-agent-mail-server/    (78K LOC)    — HTTP server + 15-screen TUI
├── mcp-agent-mail-cli/       (43K LOC)    — Robot CLI + E2E harness
├── mcp-agent-mail-db/        (35K LOC)    — SQLite + MVCC + circuit breaker
├── mcp-agent-mail-tools/     (23K LOC)    — 34 MCP tools + metrics
├── mcp-agent-mail-core/      (25K LOC)    — Models, config, backpressure, diagnostics
├── mcp-agent-mail-search/    (30K LOC)    — Hybrid search (FTS5 + semantic)
├── mcp-agent-mail-share/     (20K LOC)    — Export/share + Git + identity
├── mcp-agent-mail-guard/     (3K LOC)     — Pre-commit guard
├── mcp-agent-mail-storage/   (11K LOC)    — Write-behind queue + archive
├── mcp-agent-mail-wasm/      (834 LOC)    — WASM bindings
├── mcp-agent-mail-conformance/ (494 LOC)  — E2E conformance tests
└── mcp-agent-mail-agent-detect/ (407 LOC) — Platform agent detection
```

### 2.2 Key Dependencies

| Dep | Purpose |
|-----|---------|
| fastmcp 0.2 | MCP framework (JSON-RPC 2.0) |
| asupersync 0.2 | Async runtime |
| sqlmodel-frankensqlite 0.2 | Pure-Rust SQLite with MVCC |
| tantivy 0.25 | Full-text search index |
| frankensearch | Semantic search + embedding |
| ftui 0.2 | TUI framework |
| ed25519-dalek 2 | Git commit signing |
| jsonwebtoken 10.3 | JWT validation |
| git2 0.20 | Git operations |
| beads_rust 0.1 | Issue tracker integration |
| serde / serde_json | Serialization |
| chrono | Timestamps |

---

## 3. Core Architecture

### 3.1 MCP Tools (34 in 9 Clusters)

| Cluster | Tools | Purpose |
|---------|-------|---------|
| Infrastructure (4) | health_check, ensure_project, ensure_product, whois | Setup + health |
| Identity (3) | create_agent_identity, register_agent, install_precommit_guard | Registration |
| Messaging (5) | send_message, reply_message, fetch_inbox, acknowledge, mark_read | Communication |
| Contacts (4) | request_contact, respond_contact, list_contacts, set_policy | Auth policies |
| Reservations (4) | reserve_paths, renew, release, force_release | File locking |
| Search (2) | search_messages, search_messages_product | Hybrid search |
| Macros (4) | start_session, prepare_thread, contact_handshake, reservation_cycle | Workflows |
| Product Bus (5) | Cross-project message/search operations | Multi-project |
| Build Slots (3) | acquire, renew, release build slots | Rate limiting |

### 3.2 Backpressure System

```
Green: All OK → accept all requests
Yellow (50% thresholds): Pool p95 > 50ms, WBQ > 50%
    → Defer archive writes, reduce logging
Red (80% thresholds): Pool p95 > 200ms, WBQ > 80%
    → Shed low-priority tools with 429 "overloaded"
```

### 3.3 Storage Tiers

1. **Hot**: SQLite with `BEGIN CONCURRENT` + connection pool
2. **Warm**: Git archive with Ed25519-signed commits
3. **Search**: Tantivy index with incremental updates

### 3.4 Pre-Commit Guard

- Installed via `install_precommit_guard(project)`
- Blocks commits touching files reserved by other agents
- Pattern-based: fnmatch glob matching
- Bypass: `AM_PRECOMMIT_GUARD_BYPASS=1`

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 Direct Integration (Already In Use)

FrankenTerm agents already register via `macro_start_session` and coordinate via messages and file reservations. Integration opportunities are about deepening this:

| Component | Current Use | Deeper Integration |
|-----------|-------------|-------------------|
| **Agent identity** | Registration at session start | Embed identity in pane metadata |
| **File reservations** | Manual `reserve/release` | Auto-reserve on file open, release on close |
| **Messaging** | Manual send/reply | Auto-notify on pane events |
| **Search** | Manual query | Integrated command palette search |
| **Health check** | Periodic polling | Dashboard widget |

### 4.2 Reusable Components

| Component | Source | FrankenTerm Use Case | Effort |
|-----------|--------|---------------------|--------|
| **Backpressure** | core/backpressure.rs | Lock-free health computation | Low |
| **TOON encoding** | core/toon.rs | Token-efficient MCP responses | Low |
| **Agent detection** | agent-detect | Detect Claude/Cursor/Codex | Low |
| **Models** | core/models.rs | Agent, Message, Project types | Low |
| **Config** | core/config.rs | Environment-driven config | Low |
| **KPI metrics** | core/kpi.rs | Anomaly detection for health | Medium |

### 4.3 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Large codebase (370K LOC) | Medium | Import only core/tools crates |
| asupersync runtime | Medium | Ensure compatibility with FrankenTerm's async |
| frankensearch dependency (heavy) | Medium | Feature-gate semantic search |
| ftui dependency | Low | Already shared between projects |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Agent coordination via MCP messaging + file reservations |
| **Size** | ~370K LOC, 14 crates, 73 files |
| **Architecture** | HTTP/stdio MCP server with SQLite + Git + Tantivy |
| **Safety** | `#![forbid(unsafe_code)]`, Ed25519 signing, contact policies |
| **Performance** | <500µs inbox, <1ms search, 3-level backpressure |
| **Integration Value** | Very High — already the coordination backbone for FrankenTerm agents |
| **Top Extraction** | Backpressure, TOON encoding, agent detection, models |
| **Risk** | Medium — heavy deps (frankensearch, asupersync), but feature-gatable |
