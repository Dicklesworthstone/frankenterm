# Comprehensive Analysis of frankensearch

> Analysis document for FrankenTerm bead `ft-2vuw7.21.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**frankensearch** is a 244K LOC, 12-crate Rust workspace implementing hybrid FTS/semantic search with two-tier progressive delivery. It combines Tantivy BM25 full-text search with neural embedding via potion-128M (fast) and MiniLM-L6 (quality), fusing results through Reciprocal Rank Fusion (RRF). Already integrated into FrankenTerm via the `frankensearch` feature gate.

**Integration Value**: Very High — already a first-class dependency; deepening integration is natural.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Total LOC** | ~244,000 |
| **Crate Count** | 12 |
| **Rust Edition** | 2024 |
| **MSRV** | 1.85 |
| **License** | MIT + OpenAI/Anthropic Rider |
| **Unsafe Code** | `#![forbid(unsafe_code)]` in core; ONNX runtime in separate crate |

### Workspace Structure

```
frankensearch/
├── crates/
│   ├── frankensearch/           # Facade crate (re-exports)
│   ├── frankensearch-core/      # Core search engine
│   ├── frankensearch-index/     # Tantivy BM25 indexing
│   ├── frankensearch-embed/     # Neural embedding (potion-128M, MiniLM-L6)
│   ├── frankensearch-fusion/    # RRF result fusion
│   ├── frankensearch-protocol/  # Search API types
│   ├── frankensearch-server/    # HTTP search server
│   ├── frankensearch-client/    # Client library
│   ├── frankensearch-cli/       # CLI tool
│   ├── frankensearch-macros/    # Proc macros
│   ├── frankensearch-output/    # Terminal rendering
│   └── frankensearch-types/     # Shared types
```

---

## Core Architecture

### Two-Tier Progressive Search

1. **Fast tier** (potion-128M): ~50ms latency, lower quality — immediate results
2. **Quality tier** (MiniLM-L6): ~200ms latency, higher quality — progressive upgrade
3. **Tantivy BM25**: Full-text search with traditional ranking
4. **RRF Fusion**: Combines all result sets into final ranking

### Key Abstractions

- **SearchEngine**: Orchestrates index + embed + fusion pipeline
- **Document**: Indexable content unit with metadata
- **SearchResult**: Ranked result with score breakdown (BM25 + semantic components)
- **EmbeddingDaemon**: Warm model server for low-latency inference
- **IndexWriter/Reader**: Tantivy-backed FTS with real-time updates

### Data Flow

```
Query → [Tokenize] → [BM25 Search] ──────────────┐
                   → [potion-128M embed] → [ANN] ──┤→ [RRF Fusion] → Results
                   → [MiniLM-L6 embed] → [ANN] ───┘
```

---

## FrankenTerm Integration Status

**Already integrated** via:
- `frankensearch` feature gate in `frankenterm-core/Cargo.toml`
- `hybrid_search.rs` RRF bridge module
- MCP search tool with `quality_timeout_ms` config
- Domain removal support for scoped searches

### Current Integration Points

| Component | Status |
|-----------|--------|
| Feature gate in Cargo.toml | Active |
| RRF bridge (hybrid_search.rs) | Implemented |
| MCP search tool | Wired |
| Quality timeout config | Configurable |
| Domain removal | Supported |

---

## Deepening Opportunities

1. **Progressive search in flight recorder**: Use two-tier delivery for recorder content search
2. **Embedding daemon sharing**: Share warm model with CASS and other consumers
3. **Real-time index updates**: Wire pane content changes into Tantivy index
4. **Semantic similarity for pane dedup**: Use embeddings to detect duplicate pane content

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| ONNX runtime size | Medium | Low | Already gated behind feature |
| Model download on first use | Low | Medium | Pre-download in CI/setup |
| Embedding quality regression | Low | Low | Quality tier provides fallback |

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | 12-crate hybrid FTS/semantic search |
| **Key Innovation** | Two-tier progressive delivery (fast potion + quality MiniLM) |
| **FrankenTerm Status** | Already integrated via feature gate |
| **Deepening Priority** | High — extend to flight recorder, share daemon |
| **New Modules Needed** | 0 (existing bridge sufficient) |
| **Dependencies Added** | 0 (already a dependency) |

---

*Analysis complete.*
