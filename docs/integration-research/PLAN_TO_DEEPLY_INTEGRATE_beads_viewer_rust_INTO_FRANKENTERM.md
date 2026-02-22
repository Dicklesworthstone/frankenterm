# Plan to Deeply Integrate beads_viewer_rust into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.10.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_beads_viewer_rust.md (ft-2vuw7.10.2)

---

## P1: Objectives, Constraints, and Non-Goals

### Objectives

1. **Graph scoring algorithms**: Import PageRank and betweenness centrality algorithms for FrankenTerm's own dependency analysis (pane relationships, workflow dependencies)
2. **Composite scoring framework**: Reuse the multi-factor scoring system (PageRank + betweenness + staleness + urgency + priority) for ranking panes, agents, and tasks
3. **Robot-triage JSON format**: Adopt bv's robot-triage output schema as the standard for FrankenTerm's automated task selection
4. **CLI subprocess integration**: Use `bv --robot-triage` for agent task prioritization from FrankenTerm

### Constraints

- Single 53K LOC crate; import graph/scoring modules selectively
- Uses ratatui for TUI (not needed for library usage)
- Uses fsqlite for database reads

### Non-Goals

- Replacing bv's TUI (bv owns its visualization)
- Importing ratatui dependency through bv
- Building FrankenTerm-specific graph visualization (use franken_mermaid for that)

---

## P2: Integration Pattern

### Decision: Algorithm Extraction + Subprocess CLI

**Phase 1**: Extract PageRank and betweenness centrality algorithms into a `graph-scoring` micro-crate (upstream tweak)
**Phase 2**: FrankenTerm imports `graph-scoring` for internal dependency analysis
**Phase 3**: FrankenTerm uses `bv --robot-triage` subprocess for bead prioritization

---

## P3: Target Placement

```
frankenterm-core/
├── src/
│   ├── graph_scoring.rs   # NEW: PageRank + betweenness + composite scorer
│   └── bv_bridge.rs       # NEW: bv subprocess wrapper for robot-triage
```

### Module Responsibilities

#### `graph_scoring.rs` — Graph algorithms
- `pagerank(edges: &[(usize, usize)], n: usize, damping: f64, iters: usize) -> Vec<f64>`
- `betweenness_centrality(edges: &[(usize, usize)], n: usize) -> Vec<f64>`
- `CompositeScorer` with configurable weights for multi-factor ranking

#### `bv_bridge.rs` — bv subprocess wrapper
- `get_robot_triage() -> TriageResult` — spawns `bv --robot-triage`, parses JSON
- `TriageResult`, `Recommendation`, `ScoreBreakdown` types matching bv's schema

---

## P4-P7: Dependency, Testing, Rollout

**Dependency cost**: Zero if using subprocess; minimal if extracting graph algorithms
**Testing**: 20+ unit tests for graph algorithms, 10+ for bv bridge, proptest for scoring invariants
**Rollout**: Feature-gated behind `graph-scoring` feature, Phase 1 Week 1-2

---

## P8: Summary

Two-pronged integration: (1) extract graph scoring algorithms (PageRank, betweenness, composite scorer) for FrankenTerm's internal use, (2) subprocess bridge to `bv --robot-triage` for bead prioritization. Feature-gated.

---

*Plan complete.*
