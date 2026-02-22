# Comprehensive Analysis of Vibe Cockpit

> Bead: ft-2vuw7.8.1 / ft-2vuw7.8.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

Vibe Cockpit (`/dp/vibe_cockpit`, binary: `vc`) is a fleet observability and orchestration platform implementing ~57K LOC across 12 Rust crates. It collects metrics from 14+ upstream tools via pluggable collectors, stores them in DuckDB (70+ tables, 26 migrations), and exposes health/cost/anomaly queries through CLI, TUI, web dashboard, and MCP server.

**Key characteristics:**
- 14+ pluggable collectors (sysmoni, caut, cass, caam, mcp_agent_mail, ntm, rch, dcg, pt, bv/br, github, etc.)
- DuckDB OLAP store with 70+ tables and cursor-based incremental collection
- Oracle module: rate limit prediction, agent DNA fingerprinting, A/B experiments
- Guardian module: automated playbooks, autopilot with approval workflows
- Knowledge base: solutions, patterns, prompts, debug logs with feedback scoring
- MCP server (9 tools, 2 resources) for AI agent querying
- ratatui TUI (13 screens) + axum web dashboard + WebSocket

**Integration relevance to FrankenTerm:** Very High. Vibe Cockpit is the natural consumer of FrankenTerm's flight recorder data. FrankenTerm could become a VC collector, and VC could expose FrankenTerm session data via MCP tools.

---

## 2. Repository Topology

### 2.1 Workspace Structure (12 Crates)

```
/dp/vibe_cockpit/   (edition 2024, nightly, #![forbid(unsafe_code)])
├── vc_config/        (1,462 LOC)  — TOML configuration + linting + discovery
├── vc_store/         (5,602 LOC)  — DuckDB schema + 26 migrations + query layer
├── vc_collect/       (793 LOC)    — Collector trait + coordinator (14+ collectors)
├── vc_query/         (1,031 LOC)  — Canonical health/cost/anomaly queries
├── vc_oracle/        (588 LOC)    — Prediction: rate limits, DNA, experiments
├── vc_guardian/      (803 LOC)    — Self-healing playbooks + autopilot
├── vc_knowledge/     (1,002 LOC)  — Knowledge base with feedback scoring
├── vc_alert/         (1,787 LOC)  — Alert rules + condition evaluation + routing
├── vc_tui/           (429 LOC)    — Terminal UI (13 screens, ratatui-based)
├── vc_web/           (2,170 LOC)  — axum HTTP + WebSocket + auth
├── vc_cli/           (5,524 LOC)  — clap CLI + robot mode envelopes
└── vc_mcp/           (1,349 LOC)  — MCP server (JSON-RPC 2.0 over stdio)
```

### 2.2 Key Dependencies

| Dep | Purpose |
|-----|---------|
| tokio 1.49 | Async runtime |
| duckdb 1.4 | OLAP database (bundled) |
| serde / serde_json / toml | Serialization |
| clap 4.5 | CLI parsing |
| ratatui 0.30 / crossterm 0.29 | TUI |
| axum 0.8 / tower 0.5 | Web server |
| russh 0.49 | SSH client |
| reqwest 0.13 | HTTP client |
| chrono 0.4 | Timestamps |
| dashmap 6.1 | Concurrent maps |
| proptest 1.10 / mockall 0.14 | Testing |

---

## 3. Core Architecture

### 3.1 Collector Framework

```
Collector.collect(CollectContext)
    → CollectResult { RowBatch, warnings, new_cursor }
    → Executor (local process OR SSH)
    → Output parsing (JSON / JSONL / stdout)
    → VcStore.ingest(rows) → DuckDB
```

**Cursor types** for incremental collection:
- `Timestamp(DateTime<Utc>)` — time-bounded queries
- `FileOffset { inode, offset }` — JSONL tail tracking
- `PrimaryKey(i64)` — last-seen DB row ID

### 3.2 14+ Built-in Collectors

| Collector | Source Tool | Data |
|-----------|------------|------|
| sysmoni | System monitor | CPU, memory, disk, processes |
| caut | Claude usage | Token/API usage, costs |
| cass | Session search | Agent sessions |
| caam | Account manager | Account profiles, costs |
| mcp_agent_mail | Agent mail | Messages, reservations |
| ntm | Tmux manager | Sessions, agents, activity |
| rch | Remote compilation | Build metrics, compilations |
| rano | Network observer | Network events |
| dcg | Destructive cmd guard | Blocked commands |
| pt | Process triage | Process analysis |
| bv_br | Beads/bv | Issue triage, graphs |
| github | GitHub API | Issues, PRs |
| afsc | Fault sim | Chaos engineering |
| cloud_bench | Cloud benchmarks | Performance data |

### 3.3 DuckDB Store (70+ Tables)

Key table groups:
- **Infrastructure**: machines, collector_status, ingestion_cursors
- **System Metrics**: sys_samples, sys_top_processes, sys_filesystems
- **Repo & Build**: repos, rch_metrics, rch_compilations
- **Accounts & Cost**: account_usage, cost_attribution, cost_anomalies
- **Sessions**: agent_sessions, mail_messages, mail_file_reservations
- **Fleet Health**: health_factors, health_summary, incidents
- **Prediction**: predictions, agent_dna, experiments
- **Automation**: guardian_playbooks, autopilot_decisions
- **Audit**: audit_events, retention_policies, redaction_events

### 3.4 MCP Server (9 Tools + 2 Resources)

**Tools:**
- `vc_fleet_status` — Fleet overview
- `vc_query_machines` — Machine list + filters
- `vc_query_alerts` — Active/recent alerts
- `vc_query_sessions` — Session history search
- `vc_query_incidents` — Incident list
- `vc_query_nl` — Natural language query
- `vc_collector_status` — Collector health
- `vc_playbook_drafts` — Pending playbook executions
- `vc_audit_log` — Recent audit events

**Resources:**
- `vc://fleet/overview` — Fleet snapshot
- `vc://machines` — Machine inventory

### 3.5 CLI with Robot Mode

```bash
vc status [--machine <id>]       # Fleet health
vc collect [--collector] [--machine]  # Manual collection
vc watch [--events] [--changes-only]  # Event streaming
vc robot <subcommand>            # Agent-consumption mode
vc dashboard                     # TUI (13 screens)
vc daemon [--foreground]         # Polling loop
```

Robot mode wraps output in `RobotEnvelope<T>` with schema version, staleness, warnings.

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 FrankenTerm as a VC Collector

FrankenTerm's flight recorder naturally becomes a VC data source:
```rust
pub struct FrankenTermRecorderCollector {
    db_path: PathBuf,  // frankenterm's SQLite recorder
}
impl Collector for FrankenTermRecorderCollector {
    fn name(&self) -> &'static str { "frankenterm_recorder" }
    async fn collect(&self, ctx: &CollectContext) -> Result<CollectResult, CollectError> { ... }
}
```

### 4.2 Shared Abstractions

| Abstraction | VC Source | FrankenTerm Use |
|-------------|----------|-----------------|
| **Collector trait** | vc_collect | Pluggable data ingestion |
| **Cursor types** | vc_collect | Incremental state tracking |
| **RobotEnvelope** | vc_cli | Standard agent-consumption format |
| **QueryBuilder** | vc_query | Safe SQL generation |
| **AlertRule engine** | vc_alert | Threshold/pattern/absence/rate-of-change |
| **Guardian playbooks** | vc_guardian | Automated remediation |
| **Audit trail** | vc_store | Structured action logging |

### 4.3 Data Flow Integration

```
FrankenTerm Flight Recorder
    ↓ (SQLite exports / MCP tools / CLI JSON)
VC Collector
    ↓
DuckDB (frankenterm_sessions, frankenterm_events)
    ↓
VC MCP Server → Claude Code agents query terminal history
```

### 4.4 Phased Integration

**Phase 1**: FrankenTerm exports recorder state as JSON snapshot → VC collector reads it
**Phase 2**: FrankenTerm MCP tools → VC collectors call them directly
**Phase 3**: FrankenTerm CLI `robot sessions --format json` → VC real-time ingestion
**Phase 4**: Unified MCP proxy — VC wraps FrankenTerm's MCP tools

### 4.5 Code Sharing Opportunities
- **Cursor** enum → shared lib (both track incremental state)
- **RobotEnvelope** → standard agent-consumption format
- **QueryBuilder + guardrails** → safe SQL generation
- **Collector trait** → unified data source framework
- **AlertRule + ThresholdOp** → unified alert definitions

### 4.6 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| DuckDB bundled dependency (large) | Medium | Feature-gate VC store integration |
| Async runtime (tokio) | Low | Both async-compatible |
| 70+ tables schema complexity | Medium | Only use relevant table subset |
| Nightly toolchain | Low | Both use edition 2024 |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Fleet observability + orchestration for AI agent infrastructure |
| **Size** | ~57K LOC, 12 crates, 73 files |
| **Architecture** | Collectors → DuckDB → Queries → TUI/Web/MCP/CLI |
| **Safety** | `#![forbid(unsafe_code)]`, SQL injection prevention, audit logging |
| **Performance** | Cursor-based incremental, DuckDB OLAP, bounded concurrency |
| **Integration Value** | Very High — FrankenTerm is a natural VC data source |
| **Top Extraction** | Collector trait, Cursor types, RobotEnvelope, AlertRule engine |
| **Risk** | Low — clean crate boundaries, well-defined collector contract |
