# Comprehensive Analysis of Destructive Command Guard

> Bead: ft-2vuw7.13.1 / ft-2vuw7.13.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

Destructive Command Guard (`/dp/destructive_command_guard`, binary: `dcg`) is a high-performance safety hook for AI coding agents implementing ~107K LOC in a single Rust crate. It blocks destructive commands (git reset --hard, rm -rf, DROP TABLE, etc.) via 49+ modular pattern packs, SIMD-accelerated keyword rejection, AST-based heredoc scanning, and a layered allowlist system.

**Key characteristics:**
- 49+ modular pattern packs across 15+ categories (git, filesystem, database, k8s, cloud, etc.)
- 5-stage evaluation pipeline with <1ms quick-reject (memchr SIMD + Aho-Corasick)
- AST-Grep integration for heredoc/inline-script scanning (bash, python, JS, Go, etc.)
- Fail-open design: any error → allow (never blocks agent on failure)
- Layered allowlist: project > user > system with HMAC-SHA256 allow-once codes
- MCP server for direct agent integration (no shell-hook overhead)
- SARIF output for GitHub Code Scanning integration

**Integration relevance to FrankenTerm:** High. DCG is the safety layer for all agent commands. Can integrate via PreToolUse hook, MCP server, or direct Rust library import.

---

## 2. Repository Topology

### 2.1 Structure (Single Crate)

```
/dp/destructive_command_guard/   (v0.4.1, edition 2024, nightly, #![forbid(unsafe_code)])
├── src/
│   ├── cli.rs              (13.3K LOC) — Unified CLI (31+ subcommands)
│   ├── evaluator.rs        (4K LOC)    — Core evaluation pipeline
│   ├── scan.rs             (6.3K LOC)  — File scanning (Dockerfile, Makefile, etc.)
│   ├── heredoc.rs          (1.4K LOC)  — AST-based heredoc extraction
│   ├── ast_matcher.rs      (3.2K LOC)  — AST-Grep pattern matching
│   ├── context.rs          (3.3K LOC)  — Context-aware sanitization
│   ├── allowlist.rs        (2K LOC)    — Layered allowlist system
│   ├── pending_exceptions.rs (2.2K LOC) — HMAC allow-once codes
│   ├── history/schema.rs   (5.7K LOC)  — SQLite history + FTS5
│   ├── packs/              (49+ files) — Modular pattern packs
│   │   ├── core/           — git.rs (690), filesystem.rs (865) [always enabled]
│   │   ├── database/       — PostgreSQL, MySQL, MongoDB, Redis, SQLite
│   │   ├── containers/     — Docker, Compose, Podman
│   │   ├── kubernetes/     — kubectl, Helm, Kustomize
│   │   ├── cloud/          — AWS, GCP, Azure
│   │   └── [30+ more]     — Storage, CI/CD, secrets, monitoring, etc.
│   ├── mcp.rs              — MCP server (check_command, scan_file, explain)
│   ├── hook.rs             — Claude Code hook protocol
│   └── confidence.rs, highlight.rs, sarif.rs, simulate.rs, suggest.rs
├── tests/                  (69 files)  — Integration + regression tests
└── benches/                            — Criterion benchmarks
```

### 2.2 Key Dependencies

| Dep | Purpose |
|-----|---------|
| fancy-regex 0.17 | Advanced regex (lookahead/lookbehind) |
| regex-automata 0.4 | Fallback regex engine |
| memchr 2.7 | SIMD substring search |
| aho-corasick 1.1 | Multi-pattern keyword matching |
| ast-grep-core/language 0.40 | AST-based pattern matching |
| fsqlite 0.1 | Concurrent SQLite (history) |
| clap 4.5 | CLI parsing |
| sha2 / hmac | Audit hashing + allow-once codes |
| rust-mcp-sdk 0.8 | MCP server |
| tokio 1.49 | Async runtime (MCP server) |

---

## 3. Core Architecture

### 3.1 5-Stage Evaluation Pipeline

```
[1] Parse JSON input → validate
[2] Check bypass (DCG_BYPASS=1)
[3] Load config (env > project > user > system > defaults)
[4] Load packs + allowlists
[5] Evaluate:
    5a. Config overrides (explicit ALLOW/BLOCK)
    5b. Heredoc scanning (AST extraction, fail-open on timeout)
    5c. Quick rejection (memchr SIMD for pack keywords)
    5d. Context sanitization (mask safe strings)
    5e. Command normalization (strip paths, wrappers)
    5f. Pack registry (SAFE patterns first, then DESTRUCTIVE)
    5g. Confidence scoring (optional downgrade)
→ Allowlist override check
→ Decision: ALLOW / DENY / WARN
```

### 3.2 Pattern Packs (49+)

| Category | Packs |
|----------|-------|
| Core (always on) | git (20 patterns), filesystem (rm -rf + temp logic) |
| Database | PostgreSQL, MySQL, MongoDB, Redis, SQLite |
| Containers | Docker, Compose, Podman |
| Kubernetes | kubectl, Helm, Kustomize |
| Cloud | AWS, GCP, Azure |
| Storage | S3, GCS, MinIO, Azure Blob |
| Infrastructure | Ansible, Pulumi, Terraform |
| CI/CD | GitHub Actions, GitLab CI, CircleCI, Jenkins |
| Secrets | AWS Secrets, Doppler, 1Password, Vault |
| Monitoring | Datadog, New Relic, PagerDuty, Prometheus |
| + 15 more | DNS, Email, CDN, API gateways, messaging, etc. |

### 3.3 CLI Commands (31+)

```bash
dcg                          # Hook mode (stdin JSON)
dcg test <cmd>               # Evaluate command
dcg explain <cmd>            # Detailed trace
dcg packs / pack info <id>   # Pack management
dcg allowlist add/list/remove # Allowlist management
dcg allow-once <code>        # Use short code
dcg install / uninstall      # Hook installation
dcg scan <path>              # File scanning
dcg simulate <log>           # Policy simulation
dcg suggest-allowlist <log>  # Generate allowlist from history
dcg history / stats          # History management
dcg mcp-server               # Start MCP server
dcg doctor / config / init   # Diagnostics
```

### 3.4 MCP Server Tools

- `check_command(command)` → Decision JSON
- `scan_file(path)` → ScanReport
- `explain_pattern(rule_id)` → Pattern details

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 Integration Methods

| Method | Description | Effort |
|--------|-------------|--------|
| **PreToolUse hook** | Claude Code JSON hook (current) | Low (already works) |
| **MCP server** | Direct tool calls, no shell overhead | Low |
| **Rust library** | Import evaluator directly | Medium |
| **CLI subprocess** | `dcg test <cmd> --format json` | Low |

### 4.2 Shared Components

| Component | FrankenTerm Use Case | Effort |
|-----------|---------------------|--------|
| **Evaluator** | Validate commands before execution | Low |
| **Pattern packs** | Extend with FrankenTerm-specific patterns | Low |
| **Allowlist system** | Per-project safety policies | Low |
| **History database** | Audit trail for agent commands | Low |
| **SARIF output** | GitHub Code Scanning integration | Low |
| **File scanner** | Scan Dockerfiles, Makefiles, etc. | Low |

### 4.3 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Single-crate monolith (107K LOC) | Medium | Import via library API, not full binary |
| Nightly toolchain | Low | Both use edition 2024 |
| ast-grep dependency (heavy) | Medium | Feature-gate heredoc scanning |
| fancy-regex (ReDoS risk) | Low | Fallback to regex-automata + timeout |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Block destructive commands before execution |
| **Size** | ~107K LOC, single crate, 49+ pattern packs |
| **Architecture** | 5-stage pipeline: parse → bypass → config → evaluate → decide |
| **Safety** | `#![forbid(unsafe_code)]`, fail-open, HMAC audit, deadline system |
| **Performance** | <1ms quick-reject (SIMD), 1-10ms regex, 100ms heredoc max |
| **Integration Value** | High — safety layer for all agent commands |
| **Top Extraction** | Evaluator, pattern packs, allowlist system, MCP server |
| **Risk** | Low — fail-open, well-tested, multiple integration paths |
