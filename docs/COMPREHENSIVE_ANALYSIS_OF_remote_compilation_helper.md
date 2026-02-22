# Comprehensive Analysis of Remote Compilation Helper (RCH)

> Bead: ft-2vuw7.7.1 / ft-2vuw7.7.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

Remote Compilation Helper (`/dp/remote_compilation_helper`, binary: `rch`) is a production-grade compilation offloading system implementing ~231K LOC across 5 Rust crates. It intercepts compilation commands via Claude Code's PreToolUse hook, classifies them through a 5-tier SIMD-accelerated pipeline, and transparently offloads to remote workers via SSH with delta transfer and artifact verification.

**Key characteristics:**
- 5-tier command classification (<5ms latency, SIMD keyword filter)
- 5 worker selection strategies (Priority, Fastest, Balanced, CacheAffinity, FairFastest)
- Circuit breaker pattern (Closed→Open→HalfOpen) for worker resilience
- Blake3 binary hash verification for compilation correctness
- Fail-open design (any error → local fallback, never blocks agent)
- Tokio async runtime with Unix socket daemon protocol
- Prometheus metrics endpoint + OpenTelemetry export

**Integration relevance to FrankenTerm:** High. RCH already integrates with Claude Code hook system. Shared infrastructure: command classification, SSH client, error codes, toolchain management, telemetry.

---

## 2. Repository Topology

### 2.1 Workspace Structure (5 Crates)

```
/dp/remote_compilation_helper/   (v1.0.10, edition 2024, nightly)
├── rch/              (~4K LOC)   — PreToolUse hook CLI + fleet management
├── rchd/             (~5K LOC)   — Daemon: worker selection, health, circuit breaker
├── rch-wkr/          (~100 LOC)  — Worker agent: command execution + cache
├── rch-common/       (~3.5K LOC) — Shared: SSH, protocol, patterns, config, errors
└── rch-telemetry/    (~1K LOC)   — System metrics + SpeedScore composite
```

Total: 189 .rs files, ~231K LOC

### 2.2 Key Dependencies

| Dep | Purpose |
|-----|---------|
| tokio 1.49 | Async runtime (full features) |
| clap 4.5 | CLI parsing |
| openssh 0.11 | SSH client (Unix only) |
| axum 0.8 | HTTP server (metrics) |
| ratatui 0.30 | TUI dashboard |
| blake3 1.8 | Binary hash verification |
| zstd 0.13 | Transfer compression |
| memchr 2.8 | SIMD keyword search |
| serde / serde_json | Serialization |
| prometheus 0.14 | Metrics collection |
| opentelemetry 0.31 | OTEL export |
| rusqlite 0.38 | Telemetry storage (optional) |
| proptest 1.10 / insta 1.46 / criterion 0.8 | Testing |

### 2.3 Safety

- `#![forbid(unsafe_code)]` on all main binaries (rch, rchd, rch-wkr, rch-telemetry)
- `#![deny(unsafe_code)]` on rch-common (allows test-only env var manipulation)
- macOS: `sysctl`, `ps`, `pgrep` via `std::process::Command` (not FFI)

---

## 3. Core Architecture

### 3.1 Hook Pipeline

```
Claude Code (agent) → stdin JSON (HookInput)
    → rch hook CLI
    → 5-tier classification (<5ms)
    → daemon (Unix socket) → worker selection
    → SSH to worker → execute
    → stream output → return artifacts
    → stdout JSON (HookOutput) → Claude Code
```

### 3.2 5-Tier Command Classification

| Tier | Latency | Action |
|------|---------|--------|
| 0 - Instant Reject | <0.05ms | Non-Bash, empty commands |
| 1 - Structure | <0.1ms | Pipes, redirects, background |
| 2 - SIMD Keywords | <0.2ms | memchr scan for cargo/rustc/gcc/etc. |
| 3 - Never-Intercept | <0.5ms | cargo fmt, cargo watch, etc. |
| 4 - Full Classification | <5ms | CompilationKind enum (17 types) |

### 3.3 Worker Selection Strategies

| Strategy | Selection Criteria |
|----------|--------------------|
| Priority | Manual priority + available slots |
| Fastest | Max SpeedScore only |
| Balanced | Composite: speed (0.5) + slots (0.4) + health (0.3) + affinity (0.2) |
| CacheAffinity | Prefer workers with cached project |
| FairFastest | Weighted random favoring high score |

### 3.4 Circuit Breaker

```
HEALTHY ↔ DEGRADED (auto: slow response)
   ↓          ↓
   ↓      UNREACHABLE (heartbeat failure)
DRAINING (manual) → DRAINED → DISABLED
```

Circuit breaker: Closed → Open (after repeated failures) → HalfOpen (probe) → Closed

### 3.5 Error Code Taxonomy

| Range | Category |
|-------|----------|
| E001-E099 | Configuration |
| E100-E199 | Network (SSH, DNS) |
| E200-E299 | Worker (selection, health) |
| E300-E399 | Build (compilation, artifacts) |
| E400-E499 | Transfer (rsync, compression) |
| E500-E599 | Internal |

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 Shared Components

| Component | Source | FrankenTerm Use Case | Effort |
|-----------|--------|---------------------|--------|
| **Command Classifier** | rch-common::patterns | Classify terminal commands for policy | Low |
| **SSH Client** | rch-common::ssh | Remote terminal operations | Medium |
| **Error Codes** | rch-common::api | Unified error reporting | Low |
| **Toolchain Mgmt** | rch-common::toolchain | Detect/wrap rustup commands | Low |
| **Worker Discovery** | rch-common::discovery | Auto-discover SSH hosts | Low |
| **SpeedScore** | rch-telemetry | Machine performance ranking | Low |
| **Circuit Breaker** | rchd::workers | Resilient connection management | Medium |

### 4.2 Hook Integration

FrankenTerm and RCH can coexist in the PreToolUse hook chain:
```
Claude Code → FrankenTerm Hook (safety/validation)
    → RCH Hook (compilation offloading)
    → Local execution (fallback)
```

### 4.3 Data Sharing
- **Build history** (JSONL): Shared file for unified tracing
- **Worker config** (workers.toml): Same worker fleet
- **Telemetry** (SQLite): Shared metrics database
- **Prometheus**: Shared scrape endpoint

### 4.4 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Large codebase (231K LOC) | Medium | Import rch-common only, not full workspace |
| Tokio async runtime | Low | FrankenTerm also async-compatible |
| Unix-only SSH (openssh) | Medium | Windows stubs exist but untested |
| Nightly toolchain | Low | Both projects use edition 2024 |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Transparent compilation offloading via Claude Code hooks |
| **Size** | ~231K LOC, 5 crates, 189 files |
| **Architecture** | Stateless hook + stateful daemon + remote workers |
| **Safety** | `#![forbid(unsafe_code)]`, fail-open, Blake3 verification |
| **Performance** | 5-tier classification <5ms, SIMD keywords, delta transfer |
| **Integration Value** | High — command classifier, SSH, error codes, circuit breaker |
| **Top Extraction** | rch-common (patterns, ssh, api, toolchain, discovery) |
| **Risk** | Low — clean crate boundaries, well-documented APIs |
