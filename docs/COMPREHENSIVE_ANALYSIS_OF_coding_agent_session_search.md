# Comprehensive Analysis of Coding Agent Session Search

> Bead: ft-2vuw7.12.1 / ft-2vuw7.12.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

Coding Agent Session Search (`/dp/coding_agent_session_search`, binary: `cass`) is a production-grade agent session search and analytics platform implementing ~152K LOC in a single Rust crate. It provides full-text + semantic hybrid search across AI coding agent sessions (Claude Code, ChatGPT, Cursor, Codex, Aider, etc.), with an ftui-based Elm-architecture TUI, remote SSH sync, and a warm model daemon for fast inference.

**Key characteristics:**
- Hybrid search: Tantivy FTS5 + fastembed semantic + HNSW ANN + RRF fusion
- 15+ agent connectors via franken_agent_detection crate
- Elm-architecture TUI with 7 analytics view modes (Dashboard, Explorer, Heatmap, etc.)
- Warm model daemon (Unix Domain Socket) for sub-second semantic queries
- Remote SSH sync with rsync/SFTP for multi-machine session aggregation
- AES-GCM encrypted HTML export with PBKDF2 key derivation
- Two-tier progressive search (fast lexical → quality semantic)

**Integration relevance to FrankenTerm:** Very High. CASS is the primary search tool for agent sessions. FrankenTerm's flight recorder data could be indexed by CASS, and CASS search could be embedded in FrankenTerm's TUI.

---

## 2. Repository Topology

### 2.1 Structure (Single Crate)

```
/dp/coding_agent_session_search/   (v0.2.0, edition 2024, nightly)
├── src/
│   ├── ui/app.rs           (37.5K LOC) — ftui Elm-architecture TUI model
│   ├── lib.rs              (16.6K LOC) — CLI definitions (40+ commands)
│   ├── search/query.rs     (10.1K LOC) — Query parsing & execution
│   ├── storage/sqlite.rs   (6.9K LOC)  — SQLite schema + lazy connection
│   ├── search/             (25+ files) — Semantic search, embedders, rerankers
│   ├── connectors/         (15+ stubs) — Agent detection (via franken_agent_detection)
│   ├── indexer/            (3.5K LOC)  — Tantivy FTS + semantic indexing
│   ├── sources/            (8 modules) — Remote SSH sync, provenance
│   ├── pages/              (25 modules)— TUI page implementations
│   ├── analytics/          (5 modules) — Time bucketing, query engine
│   ├── daemon/             (5 modules) — Warm model daemon (Unix only)
│   ├── html_export/        (5 modules) — Encrypted HTML export
│   └── scan.rs, highlight.rs, encryption.rs, mcp.rs
├── tests/                  (76.7K LOC) — 100+ test files
└── benches/                            — 6 criterion suites
```

Total: ~152K LOC across 130+ files

### 2.2 Key Dependencies

| Dep | Purpose |
|-----|---------|
| tantivy | Full-text search engine |
| fastembed | ML embeddings via ORT |
| hnsw_rs | Approximate nearest neighbor |
| frankensearch 0.1 | Hybrid search infrastructure |
| franken_agent_detection 0.1 | 15+ agent connectors |
| ftui 0.2 / ftui-extras | Elm-architecture TUI |
| asupersync 0.2 | Async runtime |
| rusqlite | SQLite (bundled) |
| rmp-serde | MessagePack (daemon protocol) |
| aes-gcm / pbkdf2 / sha2 | Encryption |
| ssh2 | SSH client for remote sync |
| rayon | Data parallelism |

---

## 3. Core Architecture

### 3.1 Search Pipeline

```
Query → Parse (Boolean/Phrase/Wildcard)
    → Prefix cache (LRU + Bloom filter)
    → Tantivy FTS (BM25 scoring)
    → Optional semantic search (fastembed → HNSW ANN)
    → Hybrid: RRF fusion (reciprocal rank)
    → Post-processing (dedup, tool noise filter)
    → Cache result
```

### 3.2 CLI Commands (40+)

| Command | Purpose |
|---------|---------|
| tui | Interactive TUI (13 screens, macro recording) |
| index | Build/rebuild FTS + semantic indexes |
| search | One-off search (lexical/semantic/hybrid) |
| stats | Index statistics |
| health / status | Quick health check |
| view | Source file viewer with context |
| export / export-html | Session export (markdown/JSON/encrypted HTML) |
| doctor | Installation diagnostics + repair |
| context | Find related sessions |
| completions / man | Shell completions + man pages |

### 3.3 Agent Connectors (15+)

Via franken_agent_detection: Claude Code, ChatGPT, Cursor, Codex, Aider, Cline, Continue, Gemini, GitHub Copilot, OpenCode, and more.

### 3.4 Warm Model Daemon

```
UDS socket: /tmp/semantic-daemon-{USER}.sock
Protocol: MessagePack framed messages
Requests: Embed, Rerank, SubmitEmbeddingJob, Health, Status
Models: Resident in memory for fast inference
```

### 3.5 Analytics

- 7 view modes: Dashboard, Explorer, Heatmap, Breakdowns, Tools, Plans, Coverage
- Dimensions: Agent, Workspace, Source, Model
- Metrics: ApiTokens, ContentTokens, Messages, ToolCalls, Cost
- Time bucketing: Hour/Day/Week/Month

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 Direct Integration

| Component | FrankenTerm Use Case | Effort |
|-----------|---------------------|--------|
| **Search API** | `cass search --robot-format json` from FrankenTerm | Low |
| **Daemon** | Shared semantic inference for both tools | Medium |
| **ftui components** | Reuse analytics widgets, command palette | Medium |
| **Session export** | Export/share terminal sessions | Low |
| **Remote sync** | Multi-machine session aggregation | Medium |
| **HTML export** | Encrypted session sharing | Low |

### 4.2 FrankenTerm as CASS Data Source

FrankenTerm's flight recorder → CASS connector → indexed + searchable sessions:
```rust
pub struct FrankenTermConnector {
    recorder_db: PathBuf,
}
impl Connector for FrankenTermConnector {
    fn discover(&self) -> Vec<Session> { ... }
}
```

### 4.3 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Large codebase (152K LOC) | Medium | Use as subprocess, not library |
| Heavy ML deps (fastembed, ORT) | High | Feature-gate semantic search |
| Single-crate monolith | Medium | Clean module boundaries |
| Daemon Unix-only | Low | FrankenTerm also Unix-focused |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Agent session search + analytics with hybrid FTS/semantic |
| **Size** | ~152K LOC, single crate, 130+ files |
| **Architecture** | Tantivy FTS + fastembed + HNSW + ftui TUI + daemon |
| **Safety** | No unsafe in cass crate; delegated to dependencies |
| **Performance** | <5ms cached, 50-200ms FTS, 100-1000ms semantic |
| **Integration Value** | Very High — primary session search for all agents |
| **Top Extraction** | Search API, daemon protocol, ftui analytics, HTML export |
| **Risk** | Medium — heavy ML deps, but feature-gatable |
