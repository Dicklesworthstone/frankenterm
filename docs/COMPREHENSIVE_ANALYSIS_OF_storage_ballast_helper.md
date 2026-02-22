# Comprehensive Analysis of Storage Ballast Helper

> Bead: ft-2vuw7.6.1 / ft-2vuw7.6.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

Storage Ballast Helper (`/dp/storage_ballast_helper`, binary: `sbh`) is a production-grade disk pressure management daemon implementing ~86K LOC in a single Rust crate. It provides predictive monitoring (EWMA + PID controller), Bayesian artifact scoring, graduated cleanup (early warning → preemptive → emergency ballast release), and full decision auditing. All code enforces `#![forbid(unsafe_code)]`.

**Key characteristics:**
- Predictive EWMA rate estimation + PID controller for adaptive response
- Multi-factor Bayesian artifact scoring with false-positive/negative loss optimization
- Sacrificial ballast file system (instant space recovery)
- 4-thread daemon architecture (monitor, scanner, executor, logger)
- Dual-write logging (SQLite WAL + JSONL append-only)
- Policy modes: Observe → Canary → Enforce
- ftui-based interactive TUI dashboard (7 screens)

**Integration relevance to FrankenTerm:** High. Directly solves disk pressure issues that affect multi-agent workflows. Reusable components: EWMA estimator, PID controller, scoring engine, ballast manager, parallel directory walker, dual-write logger.

---

## 2. Repository Topology

### 2.1 Structure (Single Crate)

```
/dp/storage_ballast_helper/   (v0.2.1, edition 2024, #![forbid(unsafe_code)])
├── src/
│   ├── ballast/          (3 files)   — File allocation & release
│   ├── cli/              (8 files)   — Command implementations (15 subcommands)
│   ├── core/             (4 files)   — Config, errors, paths
│   ├── daemon/           (6 files)   — Service loop & policy (feature-gated)
│   ├── logger/           (4 files)   — Dual-write SQLite+JSONL (feature-gated sqlite)
│   ├── monitor/          (7 files)   — EWMA, PID, predictive, fs metrics
│   ├── platform/         (1 file)    — PAL trait: Linux/macOS/Windows
│   ├── scanner/          (8 files)   — Walk, pattern, score, delete
│   ├── tui/              (20 files)  — FrankenTUI dashboard (feature-gated)
│   ├── cli_app.rs        (6,002 LOC) — Top-level CLI router
│   └── lib.rs                        — Crate root
├── tests/                (21 files)  — E2E/stress/integration tests
└── docs/                 (14 files)  — ADRs, runbooks, compliance
```

### 2.2 Key Dependencies

| Dep | Purpose |
|-----|---------|
| serde / serde_json / toml | Serialization + config |
| chrono | Timestamps (RFC3339) |
| parking_lot | Mutex/RwLock (no async) |
| crossbeam-channel | Bounded message passing |
| regex | Artifact pattern classification |
| sha2 | Checksum verification |
| nix (Unix) | POSIX syscalls (statvfs, mount) |
| rusqlite (optional) | SQLite WAL logging |
| ftui (optional) | TUI dashboard |
| proptest (dev) | Property-based testing |

### 2.3 Feature Matrix

```
default = ["cli", "daemon", "sqlite", "tui"]
cli     = ["clap", "colored", "crossterm", "sqlite", "daemon"]
daemon  = ["signal-hook"]
sqlite  = ["rusqlite"]
tui     = ["ftui", "ftui-backend", "ftui-tty"]
```

---

## 3. Core Architecture

### 3.1 Daemon (4-Thread Model)

```
Monitor Thread (1s poll) ──→ Scanner Thread (60s max) ──→ Executor Thread (batch)
        │                                                         │
        └── Logger Thread (dual-write SQLite + JSONL) ◄──────────┘
```

### 3.2 Pressure Monitoring
- **EWMA estimator**: Velocity, acceleration, time-to-exhaustion
- **PID controller**: Urgency output drives graduated response
- **Prediction**: Early warning (60min), preemptive (30min), imminent danger (5min)
- **4 pressure levels**: Green (≥25%), Yellow (≥20%), Orange (≥15%), Red (≥10%)

### 3.3 Artifact Scoring (Multi-Factor Bayesian)
- **5 factors**: Location (0.15), Name (0.25), Age (0.20), Size (0.20), Structure (0.20)
- **Classifier**: 30+ built-in artifact patterns (cargo target, node_modules, __pycache__, etc.)
- **Structural signals**: has_incremental, has_deps, has_build, has_fingerprint
- **Decision**: Bayesian posterior + expected loss → keep/review/delete
- **Hard vetoes**: .git directories, open files, protected paths, too-recent files

### 3.4 Ballast System
- Pre-allocated files (default: 10 × 1 GiB) on each volume
- Instant space recovery via deletion (no scan needed)
- Graduated release based on urgency level
- Auto-replenishment with cooldown timer

### 3.5 CLI (15 Subcommands)

```bash
sbh daemon                     # Background monitoring
sbh status / stats / check     # Observability
sbh scan / clean / emergency   # Cleanup
sbh ballast status/provision/release/verify
sbh explain --id <decision>    # Decision evidence
sbh dashboard                  # Live TUI (7 screens)
sbh protect/unprotect          # Path protection
sbh config show/set/validate   # Configuration
sbh tune / update              # Tuning & updates
```

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 High-Value Reusable Components

| Component | Source | FrankenTerm Use Case | Effort |
|-----------|--------|---------------------|--------|
| **EWMA Estimator** | monitor/ewma.rs | Predict disk/memory/network exhaustion | Low |
| **PID Controller** | monitor/pid.rs | Control cleanup aggressiveness, scan frequency | Low |
| **Scoring Engine** | scanner/scoring.rs | Score temp files, logs, caches for cleanup | Medium |
| **Ballast Manager** | ballast/manager.rs | Emergency space recovery for FrankenTerm daemon | Low |
| **Directory Walker** | scanner/walker.rs | Parallel file operations | Low |
| **Dual-Write Logger** | logger/dual.rs | Flight recorder decision logging | Medium |
| **Policy Engine** | daemon/policy.rs | Feature rollout (Observe→Canary→Enforce) | Low |

### 4.2 Direct Integration Path

```rust
// In frankenterm-core, feature-gated:
#[cfg(feature = "ballast")]
pub use storage_ballast_helper::{
    monitor::ewma::DiskRateEstimator,
    scanner::scoring::ScoringEngine,
    ballast::manager::BallastManager,
    platform::pal::detect_platform,
};
```

### 4.3 FrankenTerm-Specific Opportunities
1. **Agent resource tracking**: Use `blame` logic to track which agent consumed most disk
2. **Predictive agent scaling**: Use EWMA predictions to scale down agents before disk fills
3. **Per-agent quotas**: Integrate ballast pool concept for per-agent reservations
4. **TUI widget**: Embed sbh dashboard as a sidebar in FrankenTerm's TUI
5. **MCP tool**: Expose candidate list via FrankenTerm's MCP server

### 4.4 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Single-crate monolith (86K LOC) | Medium | Import specific modules, not entire crate |
| No async runtime (threads) | Low | Both synchronous in core paths |
| 4 critical audit findings | High | Fix C1-C4 before production integration |
| ftui dependency (local path) | Low | Feature-gate TUI integration |

### 4.5 Known Bugs (from audit)
- **C1**: Launchd plist updater replaces ALL `<string>` elements
- **C2**: Ballast fallocate skips `sync_all()`
- **C3**: Ballast fallocate over-allocates by 4096 bytes
- **C4**: `ballast status` panics on usize underflow

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Predictive disk pressure management with Bayesian cleanup |
| **Size** | ~86K LOC, single crate, 61 source files |
| **Architecture** | 4-thread daemon: monitor → scanner → executor → logger |
| **Safety** | `#![forbid(unsafe_code)]`, hard vetoes, policy modes |
| **Performance** | 1s poll, 60s scan budget, parallel walk, circuit breaker |
| **Integration Value** | High — EWMA, PID, scoring engine, ballast, walker all reusable |
| **Top Extraction** | EWMA estimator, PID controller, scoring engine, ballast manager |
| **Risk** | Medium — 4 critical bugs need fixing, monolithic crate |
