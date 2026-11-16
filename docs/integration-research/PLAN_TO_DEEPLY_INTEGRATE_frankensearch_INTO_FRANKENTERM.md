# Plan to Deeply Integrate frankensearch into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.21.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_frankensearch.md (ft-2vuw7.21.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Deepen existing integration**: frankensearch is already a feature-gated dependency; extend to flight recorder content search
2. **Progressive search in recorder**: Use two-tier delivery (fast potion + quality MiniLM) for searching recorded terminal content
3. **Embedding daemon sharing**: Share warm model daemon across FrankenTerm, CASS, and other consumers
4. **Real-time index updates**: Wire pane content changes into Tantivy index for live search

### Constraints

- **Already integrated**: `frankensearch` feature gate exists with RRF bridge in `hybrid_search.rs`
- **ONNX runtime**: Embedding models behind feature gate; heavy binary size
- **Feature-gated**: Existing `frankensearch` feature gate preserved

### Non-Goals

- **Replacing Tantivy**: frankensearch uses Tantivy internally; no alternative indexer
- **Custom ML models**: Use frankensearch's provided models (potion-128M, MiniLM-L6)
- **Reimplementing RRF**: Existing fusion bridge is sufficient

---

## P2: Evaluate Integration Patterns

### Option A: Deepen Existing Feature Gate (Chosen)

Extend the existing `frankensearch` integration with recorder search and daemon sharing.

**Pros**: No new dependencies, builds on working integration, incremental
**Cons**: Tight coupling to frankensearch API surface
**Chosen**: Natural extension of existing pattern

### Decision: Option A — Deepen existing integration

---

## P3-P4: Target Placement and API

### Existing Module (extend)

`hybrid_search.rs` — Already bridges frankensearch RRF into FrankenTerm.

### New Functionality

```rust
// In hybrid_search.rs (existing module)
pub async fn search_recorder_content(query: &str, limit: usize, quality_timeout_ms: u64) -> Result<Vec<RecorderSearchResult>>;
pub async fn index_pane_content(pane_id: u64, content: &str) -> Result<()>;
pub fn is_search_daemon_healthy() -> bool;
```

No new modules needed — extend existing `hybrid_search.rs`.

---

## P5-P8: Migration, Testing, Rollout

**No migration** — extending existing integration.

**Tests**: Add 10+ tests to existing hybrid_search test suite for recorder search paths.

**Rollout**: Phase 1 (recorder search) → Phase 2 (real-time indexing) → Phase 3 (daemon health monitoring).

**Rollback**: Existing feature gate disables everything.

### Summary

Deepen existing `frankensearch` feature-gated integration. No new modules, no new dependencies. Extend `hybrid_search.rs` with recorder content search and real-time pane indexing.

---

*Plan complete.*
