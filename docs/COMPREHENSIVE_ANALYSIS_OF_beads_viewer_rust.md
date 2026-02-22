# Comprehensive Analysis of Beads Viewer Rust

> Bead: ft-2vuw7.10.1 / ft-2vuw7.10.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

Beads Viewer Rust (`/dp/beads_viewer_rust`, binary: `bv`) is a graph-aware task triage and visualization tool implementing ~53.5K LOC in a single Rust crate. It reads beads_rust issue databases, computes PageRank/betweenness centrality scores, and provides both an interactive TUI and a robot-triage JSON output for AI agent task selection.

**Key characteristics:**
- Graph analysis: PageRank, betweenness centrality, blocker ratio, staleness, urgency scoring
- Interactive TUI with dependency graph visualization
- Robot-triage mode (`--robot-triage`) for AI agent task selection
- Reads beads_rust SQLite databases directly (fsqlite)
- Score breakdown with weighted composite ranking

**Integration relevance to FrankenTerm:** Medium. BV is the primary task prioritization tool used by all agents. The graph scoring algorithms could be reused for FrankenTerm's own dependency analysis.

---

## 2. Repository Topology

### 2.1 Structure (Single Crate)

```
/dp/beads_viewer_rust/   (edition 2024, #![forbid(unsafe_code)])
├── src/
│   ├── graph/            — PageRank, betweenness, dependency graph construction
│   ├── scoring/          — Composite scoring with configurable weights
│   ├── tui/              — Interactive TUI (ratatui-based)
│   ├── robot/            — Robot-triage JSON output for AI agents
│   ├── cli/              — clap CLI definitions
│   └── main.rs           — Entry point
├── tests/                — Integration tests
└── Cargo.toml
```

### 2.2 Key Dependencies

| Dep | Purpose |
|-----|---------|
| fsqlite | Read beads_rust SQLite databases |
| ratatui / crossterm | Interactive TUI |
| clap 4 | CLI parsing |
| serde / serde_json | JSON serialization |
| chrono | Timestamps |

---

## 3. Core Architecture

### 3.1 Graph Scoring Pipeline

```
Load issues from SQLite → Build dependency graph
    → Compute PageRank (convergence-based)
    → Compute betweenness centrality
    → Compute blocker ratio, staleness, urgency
    → Composite weighted score
    → Sort by score DESC
    → Output (TUI or JSON)
```

### 3.2 Scoring Components

| Component | Weight | Meaning |
|-----------|--------|---------|
| PageRank | configurable | Graph importance (inbound dependencies) |
| Betweenness | configurable | Critical path bottleneck position |
| Blocker ratio | configurable | How many issues this unblocks |
| Staleness | configurable | Time since last update |
| Priority boost | configurable | Priority level multiplier |
| Time-to-impact | configurable | Estimated downstream impact |
| Urgency | configurable | Due date proximity |
| Risk | configurable | Complexity/uncertainty factor |

### 3.3 Robot Triage Output

```json
{
  "generated_at": "2026-02-22T...",
  "triage": {
    "meta": { "issue_count": N, "open_count": N, "actionable_count": N },
    "quick_ref": { "top_picks": [...] },
    "recommendations": [
      {
        "id": "ft-xxx",
        "title": "...",
        "score": 0.289,
        "breakdown": { "pagerank": ..., "betweenness": ..., ... },
        "reasons": ["Unblocks N items", "Critical path bottleneck", ...]
      }
    ]
  }
}
```

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 Shared Components

| Component | FrankenTerm Use Case | Effort |
|-----------|---------------------|--------|
| **PageRank algorithm** | Rank pane importance by dependency graph | Low |
| **Betweenness centrality** | Identify critical-path bottlenecks | Low |
| **Composite scoring** | Multi-factor ranking for any domain | Low |
| **Robot-triage format** | Standard AI agent task selection protocol | Low |

### 4.2 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| fsqlite dependency | Low | Same as beads_rust |
| TUI-heavy (ratatui) | Low | Only import graph/scoring, not TUI |
| Single-crate | Low | Clean module boundaries |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Graph-aware task triage and visualization |
| **Size** | ~53.5K LOC, single crate |
| **Architecture** | Load → Graph → Score → Rank → Output |
| **Safety** | `#![forbid(unsafe_code)]` |
| **Integration Value** | Medium — graph algorithms, composite scoring, robot-triage format |
| **Top Extraction** | PageRank, betweenness, composite scorer, robot-triage protocol |
| **Risk** | Low — clean algorithms, well-defined output format |
