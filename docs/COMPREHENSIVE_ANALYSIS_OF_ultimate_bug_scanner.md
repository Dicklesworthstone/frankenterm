# Comprehensive Analysis of Ultimate Bug Scanner

> Bead: ft-2vuw7.14.1 / ft-2vuw7.14.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

Ultimate Bug Scanner (`/dp/ultimate_bug_scanner`, binary: `ubs`) is a multi-language static analysis meta-runner implemented in ~27K LOC of **Bash** (not Rust). It orchestrates 8 language-specific scanner modules in parallel, detecting 1000+ bug patterns across 18 categories, with supply-chain integrity via SHA-256 checksums and minisign signatures.

**Key characteristics:**
- 8 language modules: JavaScript/TypeScript, Python, Go, Rust, Java, C/C++, Ruby, Swift
- 18 detection categories (null safety, security, async, memory leaks, injection, crypto, etc.)
- 5 output formats: text, JSON, JSONL, SARIF 2.1, TOON
- Parallel module execution with jq-based JSON merging
- Supply-chain security: SHA-256 module checksums, minisign signatures
- AST-level helpers (Go, Python, Java, TypeScript, Rust, Swift)
- Baseline comparison for regression detection

**Integration relevance to FrankenTerm:** Medium. UBS can serve as a pre-commit quality gate or CI pipeline stage. Integration via CLI subprocess with JSON output.

---

## 2. Repository Topology

### 2.1 Structure

```
/dp/ultimate_bug_scanner/   (v5.0.7, Bash, MIT)
├── ubs                      (2,890 LOC) — Main meta-runner script
├── modules/
│   ├── ubs-js.sh            (3,878 LOC) — JavaScript/TypeScript scanner
│   ├── ubs-python.sh        (3,006 LOC) — Python scanner
│   ├── ubs-golang.sh        (3,939 LOC) — Go scanner
│   ├── ubs-rust.sh          (2,529 LOC) — Rust scanner
│   ├── ubs-java.sh          (4,112 LOC) — Java scanner
│   ├── ubs-cpp.sh           (1,725 LOC) — C/C++ scanner
│   ├── ubs-ruby.sh          (1,853 LOC) — Ruby scanner
│   ├── ubs-swift.sh         (2,777 LOC) — Swift scanner
│   └── helpers/             — AST-level analysis (Go, Python, Java, TS, Rust, Swift)
├── test-suite/              (103 files) — Fixtures: buggy/, clean/, edge-cases/
├── scripts/                 — Checksum tools, dev setup
├── install.sh               (3,280 LOC) — Installer with supply-chain verification
├── Dockerfile               — OCI image (debian:bookworm-slim)
└── flake.nix                — Nix flake for reproducible builds
```

Total Bash LOC: ~26,709

### 2.2 Key Dependencies

| Dep | Purpose |
|-----|---------|
| bash 4.0+ | Meta-runner + all modules |
| ripgrep (rg) | Fast regex scanning |
| jq | JSON merging + SARIF transform |
| git | File discovery + metadata |
| ast-grep (optional) | AST-based JS/TS rules |
| python3 (optional) | AST helper execution |
| minisign (optional) | Signature verification |

---

## 3. Core Architecture

### 3.1 Execution Pipeline

```
CLI parsing → Language detection → Module verification (SHA-256)
    → Parallel module execution (background jobs)
    → Per-module: ripgrep + ast-grep + AST helpers → JSON summary
    → jq merge → Format conversion (text/JSON/JSONL/SARIF/TOON)
    → Exit code (0=clean, 1=critical, 2=env error)
```

### 3.2 Detection Categories (18)

| # | Category | Severity |
|---|----------|----------|
| 1 | Null Safety | CRITICAL |
| 2 | Security (XSS, eval, secrets) | CRITICAL |
| 3 | Async/Await bugs | CRITICAL |
| 4 | Memory Leaks | WARNING |
| 5 | Type Coercion | CRITICAL |
| 6 | Math Errors | WARNING |
| 7 | Error Handling | WARNING |
| 8 | Control Flow | WARNING |
| 9 | Debugging artifacts | CRITICAL |
| 10 | Variable Scope | WARNING |
| 11 | ReDoS | CRITICAL |
| 12 | Prototype Pollution | CRITICAL |
| 13 | Injection (SQL, NoSQL, cmd) | CRITICAL |
| 14 | Race Conditions (TOCTOU) | CRITICAL |
| 15 | Weak Crypto | CRITICAL |
| 16 | DOM Manipulation | WARNING |
| 17 | API Misuse | WARNING |
| 18 | Performance (N+1, blocking) | WARNING |

### 3.3 CLI

```bash
ubs <path> [output_file]          # Main scan
ubs --format=json|jsonl|sarif|toon # Output format
ubs --staged / --diff             # Git-aware scanning
ubs --only=js,python              # Language filter
ubs --category=resource-lifecycle # Category filter
ubs --profile=strict|loose        # Strictness
ubs --comparison=baseline.json    # Regression detection
ubs --ci --fail-on-warning        # CI mode
ubs doctor [--fix]                # Diagnostics
```

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 Integration Methods

| Method | Description | Effort |
|--------|-------------|--------|
| **Pre-commit hook** | `ubs --staged --fail-on-warning` | Low |
| **CI pipeline** | `ubs . --ci --format=sarif` | Low |
| **Agent feedback** | `ubs file.ts --format=json` on file write | Low |
| **Baseline regression** | Compare against saved baseline | Low |

### 4.2 Data Flow

```bash
# FrankenTerm agent writes code → UBS scans → JSON findings
ubs changed_file.rs --format=json | jq '.totals'
# → {"critical": 2, "warning": 5, "info": 12}
```

### 4.3 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Bash-based (not Rust library) | Medium | Subprocess integration only |
| Requires ripgrep + jq | Low | Common tools, `ubs doctor --fix` |
| Module download on first use | Low | Pre-cache in Docker/CI |
| No Rust API | Medium | JSON output is well-structured |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Multi-language static analysis meta-runner |
| **Size** | ~27K LOC Bash, 8 language modules, 18 categories |
| **Architecture** | Parallel module execution → jq merge → multi-format output |
| **Safety** | SHA-256 checksums, minisign signatures, fail-closed on env error |
| **Performance** | 0.8s small, 3.2s medium, 12s large, 58s huge projects |
| **Integration Value** | Medium — quality gate for agent-written code |
| **Top Extraction** | JSON output format, SARIF for GitHub, baseline comparison |
| **Risk** | Low — CLI tool, well-documented, agent-friendly output |
