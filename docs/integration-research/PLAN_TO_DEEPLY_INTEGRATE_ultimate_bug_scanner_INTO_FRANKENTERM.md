# Plan to Deeply Integrate ultimate_bug_scanner into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.14.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_ultimate_bug_scanner.md (ft-2vuw7.14.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Pre-commit quality gate**: Wire UBS into FrankenTerm's agent workflow as a code quality check before agents commit changes
2. **Agent feedback loop**: Run `ubs` on files written by agents and surface findings as actionable feedback (critical bugs → block commit, warnings → inform agent)
3. **Baseline regression detection**: Use UBS's `--comparison` mode to detect regressions in agent-written code by comparing against project baselines
4. **SARIF for CI integration**: Generate SARIF reports from UBS scans for GitHub Code Scanning
5. **Scan-on-write automation**: Optionally trigger UBS scans when FrankenTerm detects file writes in agent panes

### Constraints

- **UBS is Bash, not Rust**: 27K LOC Bash; integration is subprocess-only
- **Requires ripgrep + jq**: UBS depends on external tools; `ubs doctor --fix` handles setup
- **Feature-gated**: UBS integration behind `code-scanning` feature flag
- **No Rust API**: UBS has no library interface; all integration via CLI subprocess
- **Scan time varies**: 0.8s small, 3.2s medium, 12s large projects — must run async

### Non-Goals

- **Reimplementing UBS in Rust**: UBS is a Bash meta-runner by design
- **Real-time scanning**: UBS scans take seconds; this is a batch operation, not keystroke-level
- **Replacing existing linters**: UBS supplements cargo clippy, not replaces it
- **Module download management**: Let UBS handle its own module downloads

---

## P2: Evaluate Integration Patterns

### Option A: Subprocess CLI (Chosen)

FrankenTerm spawns `ubs <path> --format=json` and parses JSON output.

**Pros**: Process isolation, zero Rust deps, UBS manages its own modules, well-structured JSON output
**Cons**: Subprocess latency (0.8-12s depending on project size), requires UBS installed
**Chosen**: Only viable option — UBS is Bash-only

### Decision: Option A — Subprocess CLI (only viable option)

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── code_scanner.rs           # NEW: UBS subprocess wrapper + result parser
│   └── ...existing modules...
├── Cargo.toml                    # No new dependencies
```

### Module Responsibilities

#### `code_scanner.rs` — UBS integration

- `scan_files(paths: &[PathBuf], profile: ScanProfile) -> ScanResult` — Run UBS on files
- `scan_staged(profile: ScanProfile) -> ScanResult` — Run UBS on git staged files
- `compare_baseline(baseline: &Path, current: &Path) -> RegressionReport` — Regression detection
- `ScanResult` — Totals (critical/warning/info), findings, timing
- `Finding` — File, line, category, severity, message, suggestion
- `ScanProfile` enum — Strict, Loose, Custom
- Async: Runs via `tokio::process::Command` to avoid blocking
- Graceful degradation: Returns empty result if UBS not installed

---

## P4: API Contracts

### Public API Contract

```rust
#[cfg(feature = "code-scanning")]
pub mod code_scanner {
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ScanResult {
        pub totals: ScanTotals,
        pub findings: Vec<Finding>,
        pub scan_time_ms: u64,
        pub modules_used: Vec<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ScanTotals {
        pub critical: usize,
        pub warning: usize,
        pub info: usize,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Finding {
        pub file: String,
        pub line: usize,
        pub category: String,
        pub severity: String,
        pub message: String,
        pub suggestion: Option<String>,
    }

    pub enum ScanProfile { Strict, Loose }

    pub struct CodeScanner {
        ubs_path: PathBuf,  // Default: "ubs" from PATH
    }

    impl CodeScanner {
        pub fn new() -> Self;
        pub async fn scan(&self, paths: &[PathBuf], profile: ScanProfile) -> Result<ScanResult>;
        pub async fn scan_staged(&self, profile: ScanProfile) -> Result<ScanResult>;
        pub async fn compare_baseline(&self, baseline: &Path) -> Result<RegressionReport>;
        pub fn is_available(&self) -> bool;
    }
}
```

---

## P5-P6: Migration, Testing

**No migration needed** — new code scanning capability.

### Unit Tests (30+)

- `test_parse_ubs_json_output` — parse real UBS JSON output
- `test_parse_empty_scan` — no findings returns empty result
- `test_parse_critical_findings` — critical findings parsed correctly
- `test_scan_profile_strict` — strict profile passes correct flags
- `test_scan_profile_loose` — loose profile passes correct flags
- `test_graceful_degradation_no_ubs` — returns empty when UBS not installed
- `test_scan_timeout` — long scans cancelled after timeout
- `test_regression_detection` — new findings vs baseline
- `test_sarif_output_parsing` — SARIF format parsed correctly
- Additional parsing and error handling tests for 30+ total

### Property-Based Tests

- `proptest_scan_never_panics` — any file list input succeeds or returns error
- `proptest_totals_sum_matches_findings` — critical+warning+info = findings.len()

---

## P7: Rollout

**Phase 1**: `code_scanner.rs` behind `code-scanning` feature (Week 1-2)
**Phase 2**: Wire into pre-commit hook for agent workflows (Week 3)
**Phase 3**: MCP tool `ft_scan_code` for agent-initiated scans (Week 4)
**Phase 4**: Baseline comparison automation (Week 5)

### Rollback: Disable feature flag; no scanning occurs.

---

## P8: Summary

**Subprocess CLI** wrapping `ubs --format=json`. Async execution via tokio. Zero Rust deps.

### One New Module
1. **`code_scanner.rs`**: UBS subprocess wrapper with JSON parsing, baseline comparison, scan profiles

### Upstream Tweak Proposals (for ultimate_bug_scanner)
1. **JSONL streaming output**: Stream findings as they're discovered for real-time display
2. **Exit code refinement**: Distinguish "has warnings" from "has criticals"
3. **Machine-readable baseline format**: Stable schema for baseline JSON
4. **Custom category filter via env**: `UBS_CATEGORIES=security,async` for targeted scans

### Beads: ft-2vuw7.14.1 (CLOSED), ft-2vuw7.14.2 (CLOSED), ft-2vuw7.14.3 (THIS DOCUMENT)

---

*Plan complete.*
