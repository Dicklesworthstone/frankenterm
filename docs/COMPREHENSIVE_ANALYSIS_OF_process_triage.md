# Comprehensive Analysis of Process Triage

> Bead: ft-2vuw7.5.1 / ft-2vuw7.5.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

Process Triage (`/dp/process_triage`) is a Bayesian inference and decision-making engine for intelligent process lifecycle management. It implements ~264K LOC across 7 crates, with 39 statistical inference modules, 34 decision theory modules, and comprehensive action execution with TOCTOU revalidation. The project handles process scanning, probabilistic classification (Abandoned/Active/Stuck), policy-gated decision making, and supervised action execution.

**Key characteristics:**
- Bayesian posterior computation for process state classification
- 39 inference modules (belief propagation, change-point detection, Kalman, Hawkes, IMM filters)
- 34 decision modules (FDR gates, expected loss, active sensing, DRO, CVaR risk)
- TOCTOU revalidation protocol using `<boot_id>:<start_time_ticks>:<pid>` identity
- Redaction engine for sensitive data (cmdlines, env vars, paths)
- Arrow/Parquet telemetry with Hive-style partitioning
- MCP server for AI agent orchestration
- ftui-based interactive TUI for human approval

**Integration relevance to FrankenTerm:** High. Shared infrastructure: process identity model, redaction engine, telemetry schemas, session model, output formatting, and MCP server patterns.

---

## 2. Repository Topology

### 2.1 Workspace Structure (7 crates)

```
/dp/process_triage/   (edition 2021, MSRV 1.88)
├── pt-core/          (264K LOC)  — Main engine + CLI (229 files)
│   ├── collect/      (18 files)  — Process scanning (ps, /proc, cgroup, GPU, network)
│   ├── inference/    (39 files)  — Bayesian/sequential/ML inference engines
│   ├── decision/     (34 files)  — FDR gates, expected loss, active sensing
│   ├── action/       (13 files)  — Signal kill, supervision, recovery trees
│   ├── session/      (11 files)  — Lifecycle, resumability, fleet aggregation
│   ├── plan/         (1 file)    — Decision → action plan conversion
│   ├── tui/          (6 files)   — ftui interactive approval UI
│   ├── mcp/          — Model Context Protocol server
│   └── daemon/       — Background monitoring (feature-gated)
├── pt-common/        (~5K LOC)   — Shared types, IDs, errors, schemas
├── pt-math/          (~2K LOC)   — Log-domain numerics, Beta/Dirichlet distributions
├── pt-config/        (~3K LOC)   — Config loading, validation, presets
├── pt-bundle/        (~4K LOC)   — ZIP bundling + ChaCha20-Poly1305 encryption
├── pt-redact/        (~3K LOC)   — Redaction engine (regex + entropy detection)
├── pt-telemetry/     (~2K LOC)   — Arrow/Parquet schemas and batched writer
└── pt-report/        (~2K LOC)   — HTML report generation (Askama templates)
```

### 2.2 Core Pipeline

```
scan (collect/) → infer (inference/) → decide (decision/) → plan (plan/)
    → TUI approval (tui/) → apply (action/) → verify
```

### 2.3 Key Dependencies

| Dep | Purpose |
|-----|---------|
| arrow 53 / parquet 53 | Columnar telemetry storage |
| serde / serde_json | Serialization |
| clap 4 | CLI |
| chrono | Timestamps |
| uuid | Session/process IDs |
| tracing | Logging |
| ftui 0.2 | Interactive TUI (optional) |
| p256 / ecdsa | Binary signature verification |
| chacha20poly1305 | Bundle encryption |
| prometheus (optional) | Metrics endpoint |

---

## 3. Core Architecture

### 3.1 Inference Engine (39 modules)
- Posterior computation, belief propagation, change-point detection
- Beta-Stacy binning, Compound Poisson, CTW prediction
- Conformal prediction, drift detection, extreme value theory
- Hazard modeling, IMM filters, Kalman tracking, Hawkes processes
- Martingale tests, copulas, Wasserstein distance

### 3.2 Decision Engine (34 modules)
- FDR/alpha-investing gates, expected loss
- Active sensing (VOI), submodular selection
- DRO (distributionally robust optimization), CVaR risk
- Goal optimizer, contextual bandits, causal interventions
- Policy enforcement, rate limiting, respawn loop detection

### 3.3 Action Execution (13 modules)
- Signal-based killing (SIGTERM → SIGKILL escalation)
- Supervision (systemd, launchd, Docker)
- Recovery trees with fallback plans
- CPU throttling, cgroup quarantine, process freezing
- TOCTOU revalidation before each action

### 3.4 CLI (20+ subcommands)
```bash
pt run                    # Golden path: scan → infer → decide → approve → apply
pt scan / pt deep-scan    # Process scanning
pt agent plan/apply/verify # Structured AI agent workflows
pt bundle create/inspect  # Session bundling
pt report                 # HTML reports
pt mcp                    # MCP server for AI agents
pt daemon                 # Background monitoring (feature-gated)
```

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 High-Value Extraction Candidates

| Component | Source | FrankenTerm Use Case | Effort |
|-----------|--------|---------------------|--------|
| **Process Identity** | pt-common | Mux-pool process tracking, PID reuse detection | Low |
| **Redaction Engine** | pt-redact | Flight recorder data governance, telemetry sanitization | Medium |
| **Telemetry Schemas** | pt-telemetry | Arrow/Parquet flight recorder output | Medium |
| **Session Model** | pt-core session/ | Flight recorder session lifecycle | Medium |
| **Output Formatting** | pt-core output/ | Token-efficient MCP responses | Low |
| **Bundle Format** | pt-bundle | Session export/sharing | Low |

### 4.2 Shared Abstractions

1. **Process Identity Model**: `start_id = "<boot_id>:<start_time_ticks>:<pid>"` with TOCTOU revalidation — directly applicable to FrankenTerm's mux-pool process management

2. **Redaction Engine**: Field-class-aware redaction (cmdline, env vars, paths, hostnames) with HMAC hashing — reusable by FrankenTerm's telemetry and flight recorder

3. **Telemetry Schemas**: Arrow/Parquet schemas with Hive-style partitioning — compatible with FrankenTerm's storage_telemetry module

4. **MCP Server Patterns**: JSON-RPC 2.0 over stdio with tools/resources — same pattern as FrankenTerm's MCP integration

5. **Mathematical Libraries**: Log-domain numerics, Beta/Dirichlet distributions — pt-math could supplement FrankenTerm's survival.rs and bayesian_ledger

### 4.3 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Monolithic pt-core (264K LOC) | High | Extract specific modules, don't depend on whole crate |
| Edition 2021 (vs FrankenTerm 2024) | Low | pt-common and pt-redact compile fine on 2024 |
| Heavy deps (Arrow 53, Parquet) | Medium | Feature-gate telemetry integration |
| No async runtime | Low | Both projects synchronous in core paths |
| libc usage in pt-core | Medium | pt-common/pt-redact/pt-math are safe |

### 4.4 Recommended Integration Path

**Phase 1: Shared Types (1-2 days)**
- Use pt-common's `ProcessId`, `StartId`, `ProcessIdentity` in FrankenTerm
- Adopt `ErrorCategory` pattern for structured errors

**Phase 2: Redaction (3-5 days)**
- Integrate pt-redact into flight recorder export pipeline
- Map `FieldClass` to recorder_export's access tiers

**Phase 3: Telemetry (1-2 weeks)**
- Align Arrow schemas between pt-telemetry and storage_telemetry
- Share Parquet writing infrastructure

**Phase 4: MCP Integration (1 week)**
- Wire pt-core's MCP server as optional FrankenTerm tool
- Enable AI agents to orchestrate process management via FrankenTerm

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Bayesian process lifecycle management with safety-gated actions |
| **Size** | ~264K LOC, 7 crates, 290 files |
| **Architecture** | Scan → Infer → Decide → Plan → Approve → Apply → Verify |
| **Safety** | TOCTOU revalidation, FDR gates, recovery trees, redaction |
| **Performance** | Synchronous, parallel tool execution, Parquet compression |
| **Integration Value** | High — process identity, redaction, telemetry schemas |
| **Top Extraction** | pt-common, pt-redact, pt-math, pt-telemetry schemas |
| **Risk** | Medium — monolithic core needs selective extraction |
