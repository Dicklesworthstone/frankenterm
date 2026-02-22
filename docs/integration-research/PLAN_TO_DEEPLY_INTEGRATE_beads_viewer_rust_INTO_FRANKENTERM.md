# Plan to Deeply Integrate beads_viewer_rust into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.10.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_beads_viewer_rust.md (ft-2vuw7.10.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Graph scoring algorithms**: Extract PageRank and betweenness centrality algorithms for ranking pane importance, workflow step criticality, and dependency bottleneck detection
2. **Composite scoring framework**: Reuse the multi-factor weighted scoring model for FrankenTerm's own ranking needs (pane priority, capture importance, resource allocation)
3. **Robot-triage protocol**: Adopt bv's robot-triage JSON format as the standard task selection interface for AI agents working within FrankenTerm
4. **Task prioritization for agents**: Provide agents with `ft robot triage` commands that return graph-scored, ranked task lists

### Constraints

- **Single 53.5K LOC crate**: Like beads_rust, importing selectively requires algorithm extraction
- **Graph algorithms are self-contained**: PageRank and betweenness are pure functions of adjacency data; no external dependencies
- **ratatui TUI not needed**: FrankenTerm has its own TUI; bv's visualization is not embedded
- **fsqlite dependency**: bv reads beads_rust databases via fsqlite; FrankenTerm doesn't need this

### Non-Goals

- **Embedding bv's TUI**: FrankenTerm has its own dashboards
- **Replacing bv**: bv remains the standalone triage tool; FrankenTerm reuses algorithms only
- **Direct database access**: Use `bv --robot-triage` subprocess, not bv's SQLite reader
- **Full graph visualization**: FrankenTerm may render simplified dependency views, not bv's full graph

---

## P2: Evaluate Integration Patterns

### Option A: Algorithm Extraction + Subprocess (Chosen)

Extract PageRank and betweenness centrality as standalone functions in frankenterm-core. Use `bv --robot-triage` for issue-level triage via subprocess.

**Pros**: Clean algorithms (pure math), no dependencies, testable, reusable beyond beads
**Cons**: Must maintain sync with upstream if algorithms change
**Chosen**: Algorithms are stable mathematical operations; unlikely to diverge

### Option B: Library Import

Add beads_viewer_rust as a path dependency.

**Pros**: Automatic sync, access to scoring framework
**Cons**: 53.5K LOC, ratatui, fsqlite dependencies
**Rejected**: Too heavy for FrankenTerm's needs

### Option C: Subprocess Only

Use only `bv --robot-triage` without extracting algorithms.

**Pros**: Zero code duplication
**Cons**: Can't apply graph algorithms to FrankenTerm's own data (pane topology, workflow DAGs)
**Rejected**: FrankenTerm needs graph scoring for its own internal structures

### Decision: Option A — Extract algorithms + subprocess for bead triage

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── graph_scoring.rs          # NEW: PageRank, betweenness, composite scorer
│   └── ...existing modules...
├── Cargo.toml                    # No new dependencies
```

### Module Responsibilities

#### `graph_scoring.rs` — Graph analysis algorithms

- `pagerank(adjacency: &[(usize, usize)], n: usize, damping: f64, iterations: usize) -> Vec<f64>` — PageRank for directed graphs
- `betweenness_centrality(adjacency: &[(usize, usize)], n: usize) -> Vec<f64>` — Betweenness centrality via BFS
- `CompositeScorer` — Multi-factor weighted scorer with configurable weights
- `ScoredItem<T>` — Generic scored result with score breakdown
- Used by: pane prioritization, workflow step ranking, capture importance, MCP tool responses

---

## P4: API Contracts and Crate/Subcrate Extraction Roadmap

### Public API Contract: GraphScoring

```rust
pub mod graph_scoring {
    pub fn pagerank(
        edges: &[(usize, usize)],  // (from, to) pairs
        node_count: usize,
        damping: f64,              // Default: 0.85
        max_iterations: usize,     // Default: 100
        tolerance: f64,            // Default: 1e-6
    ) -> Vec<f64>;

    pub fn betweenness_centrality(
        edges: &[(usize, usize)],
        node_count: usize,
    ) -> Vec<f64>;

    pub struct ScoringWeight {
        pub name: &'static str,
        pub weight: f64,
    }

    pub struct CompositeScorer {
        weights: Vec<ScoringWeight>,
    }

    impl CompositeScorer {
        pub fn new(weights: Vec<ScoringWeight>) -> Self;
        pub fn score(&self, factors: &[f64]) -> f64;
        pub fn score_with_breakdown(&self, factors: &[f64]) -> ScoreBreakdown;
    }

    pub struct ScoreBreakdown {
        pub total: f64,
        pub components: Vec<(String, f64)>,
    }

    pub struct ScoredItem<T> {
        pub item: T,
        pub score: f64,
        pub breakdown: ScoreBreakdown,
    }

    pub fn rank<T>(items: Vec<T>, scorer: &CompositeScorer, extract_factors: impl Fn(&T) -> Vec<f64>) -> Vec<ScoredItem<T>>;
}
```

### Crate Extraction Roadmap

**Phase 1**: Implement in frankenterm-core (no feature gate — pure math, zero deps).
**Phase 2**: If multiple projects need graph scoring, extract to `ft-graph-scoring` crate.

---

## P5: Data Migration/State Synchronization and Compatibility Posture

### Migration Strategy

**No migration needed** — algorithms operate on in-memory data structures.

### Compatibility Posture

- **Additive only**: New public functions, no changes to existing APIs
- **No feature gate needed**: Pure math functions with zero dependencies
- **Stable API**: PageRank and betweenness centrality are well-defined algorithms

---

## P6: Testing Strategy and Detailed Logging/Observability

### Unit Tests (target: 30+)

- `test_pagerank_single_node` — single node gets rank 1.0
- `test_pagerank_two_nodes_mutual` — equal rank for symmetric graph
- `test_pagerank_star_topology` — center node highest rank
- `test_pagerank_chain_topology` — rank decreases along chain
- `test_pagerank_convergence` — converges within tolerance
- `test_pagerank_damping_factor` — damping affects distribution
- `test_pagerank_empty_graph` — empty graph returns zeros
- `test_pagerank_disconnected` — disconnected components handled
- `test_betweenness_bridge_node` — bridge node has highest centrality
- `test_betweenness_leaf_node` — leaf has zero centrality
- `test_betweenness_star_center` — center of star has max centrality
- `test_betweenness_chain` — middle nodes higher than endpoints
- `test_betweenness_empty_graph` — empty graph returns zeros
- `test_composite_scorer_equal_weights` — equal weights give average
- `test_composite_scorer_single_factor` — single factor passes through
- `test_composite_scorer_zero_weight` — zero weight ignored
- `test_composite_scorer_breakdown` — breakdown sums to total
- `test_rank_sorts_descending` — highest score first
- `test_rank_preserves_items` — all items present in output
- `test_scored_item_breakdown` — breakdown matches score
- Additional edge cases and boundary conditions for 30+ total

### Property-Based Tests

- `proptest_pagerank_sums_to_n` — PageRank values sum to node count
- `proptest_pagerank_nonnegative` — all values >= 0
- `proptest_betweenness_nonnegative` — all values >= 0
- `proptest_composite_scorer_monotone` — higher factors produce higher score
- `proptest_rank_length_preserved` — output length equals input length

### Logging Requirements

```rust
tracing::debug!(
    node_count = node_count,
    edge_count = edges.len(),
    iterations = actual_iterations,
    converged = converged,
    "graph_scoring.pagerank"
);
```

---

## P7: Rollout, Rollback, Risk Mitigation, and Acceptance Gates

### Rollout Plan

**Phase 1: Core Algorithms (Week 1)**
1. Implement `graph_scoring.rs` with PageRank, betweenness, composite scorer
2. Write 30+ unit tests
3. Gate: `cargo check --workspace`

**Phase 2: FrankenTerm Wiring (Week 2-3)**
1. Score panes by dependency graph
2. Score workflow steps by betweenness
3. Wire into VOI scheduler
4. Gate: Scoring produces meaningful rankings

**Phase 3: Agent-Facing API (Week 4)**
1. Add `ft robot triage` wrapping bv --robot-triage
2. Add `ft robot rank-panes` using internal graph scoring
3. Gate: Agent-consumable JSON output

### Rollback Plan

- **All phases**: Delete `graph_scoring.rs`; no existing functionality affected

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Algorithm divergence from bv | Low | Low | Stable mathematical operations |
| Performance on large graphs | Low | Medium | FrankenTerm graphs are small |
| Incorrect implementation | Medium | Medium | Validate against bv's output |

### Acceptance Gates

1. `cargo check --workspace --all-targets`
2. `cargo test --workspace`
3. 30+ unit tests
4. 5+ proptest scenarios
5. Results match bv's PageRank for known test graphs

---

## P8: Summary and Action Items

### Chosen Architecture

**Algorithm extraction** of PageRank, betweenness centrality, and composite scoring. Plus `bv --robot-triage` subprocess for bead-level triage.

### One New Module

1. **`graph_scoring.rs`**: PageRank, betweenness centrality, generic composite scorer, ranked output

### Upstream Tweak Proposals (for beads_viewer_rust)

1. **Extract `bv-algorithms` library crate**: PageRank, betweenness, scoring as reusable library
2. **Configurable scoring weights via CLI**: `bv --robot-triage --weights="pagerank:0.3,betweenness:0.5"`
3. **Machine-readable score breakdown**: Include full breakdown in robot-triage JSON

### Beads Created/Updated

- `ft-2vuw7.10.1` (CLOSED): Research complete
- `ft-2vuw7.10.2` (CLOSED): Analysis document complete
- `ft-2vuw7.10.3` (THIS DOCUMENT): Integration plan complete

---

*Plan complete. Ready for review and implementation bead creation.*
