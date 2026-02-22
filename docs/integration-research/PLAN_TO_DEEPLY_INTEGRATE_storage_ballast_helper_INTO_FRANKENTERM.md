# Plan to Deeply Integrate storage_ballast_helper into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.6.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_storage_ballast_helper.md (ft-2vuw7.6.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Disk pressure awareness for multi-agent workflows**: Embed the EWMA estimator and PID controller to predict disk exhaustion and throttle agent activity (capture rate, log verbosity, cache retention)
2. **Ballast-based emergency recovery**: Use the sacrificial ballast system to guarantee instant space recovery when FrankenTerm's flight recorder or session logs fill the disk
3. **Artifact scoring for smart cleanup**: Reuse the Bayesian scoring engine to identify and rank FrankenTerm-generated artifacts (old session DBs, scrollback exports, temp files) for cleanup
4. **Pressure-aware capture scheduling**: Wire disk pressure levels (Green/Yellow/Orange/Red) into FrankenTerm's VOI scheduler and backpressure system to reduce capture frequency under pressure
5. **Unified health reporting**: Expose disk pressure metrics through FrankenTerm's existing telemetry and MCP tools

### Constraints

- **No new runtime threads**: SBH uses a 4-thread daemon model; FrankenTerm integration must use async tasks within the existing tokio runtime, not spawn OS threads
- **Feature-gated**: All SBH integration behind `disk-pressure` feature flag to avoid pulling in rusqlite (SBH's) and platform-specific deps
- **No unsafe code**: Both projects enforce `#![forbid(unsafe_code)]`
- **Synchronous core**: SBH is synchronous (crossbeam-channel); wrapping in async boundaries is acceptable
- **No daemon mode in FrankenTerm**: FrankenTerm runs its own event loop; SBH's daemon thread model is not needed

### Non-Goals

- **Replacing FrankenTerm's own storage management**: SBH components supplement, not replace, existing storage logic
- **Running SBH daemon alongside FrankenTerm**: Use extracted components, not the full daemon
- **SBH TUI integration**: FrankenTerm has its own TUI; SBH's ftui dashboard is not embedded
- **Cross-platform parity**: macOS-first; Linux support follows naturally via SBH's PAL trait
- **Full Bayesian model import**: Extract scoring formula, not the entire scanner module

---

## P2: Evaluate Integration Patterns

### Option A: Component Extraction (Chosen)

Extract EWMA estimator, PID controller, and scoring formula as standalone functions/structs. Wire into FrankenTerm's existing async architecture.

**Pros**: Minimal dependency surface, FrankenTerm controls lifecycle, testable in isolation
**Cons**: Must maintain sync with upstream SBH changes
**Chosen**: Best balance of value and coupling

### Option B: Library Dependency

Add `storage_ballast_helper` as a path dependency and import modules directly.

**Pros**: Automatic upstream sync, access to full API
**Cons**: Pulls in 86K LOC monolith, rusqlite, ftui, daemon modules; compile time increase
**Rejected**: Too heavy; FrankenTerm needs specific components, not the full crate

### Option C: Subprocess Integration

Shell out to `sbh status --format=json` for disk pressure data.

**Pros**: Process isolation, independent deployment
**Cons**: 50ms+ latency per call, requires SBH installed, can't share in-process state
**Rejected**: FrankenTerm needs sub-millisecond pressure checks for capture scheduling

### Decision: Option A — Extract EWMA, PID, scoring, and ballast manager as standalone modules

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── disk_pressure.rs          # NEW: EWMA + PID disk pressure monitor
│   ├── disk_scoring.rs           # NEW: Bayesian artifact scoring for cleanup
│   ├── disk_ballast.rs           # NEW: Ballast file management
│   └── ...existing modules...
├── Cargo.toml                    # Add nix (optional) for statvfs
```

### Module Responsibilities

#### `disk_pressure.rs` — Pressure monitoring and prediction

- `DiskPressureMonitor` — polls filesystem stats, computes EWMA velocity/acceleration
- `PressureLevel` enum — Green (≥25%), Yellow (≥20%), Orange (≥15%), Red (≥10%)
- `PressurePrediction` — time-to-exhaustion estimate with confidence
- `PidController` — adaptive response tuning (Kp, Ki, Kd configurable)
- Integration point: wired into `BackpressureConfig` as an additional signal

#### `disk_scoring.rs` — Artifact cleanup ranking

- `ArtifactScore` — multi-factor Bayesian posterior (location, name, age, size, structure)
- `ArtifactClassifier` — pattern-based classification (cargo target, node_modules, etc.)
- `CleanupCandidate` — ranked list with expected-loss optimization
- Integration point: used by maintenance tasks to identify cleanup targets

#### `disk_ballast.rs` — Emergency space recovery

- `BallastManager` — create, verify, release sacrificial files
- `BallastConfig` — count, size, path, auto-replenish settings
- Integration point: FrankenTerm's startup provisions ballast; emergency path releases it

### Dependency Wiring

```toml
# In crates/frankenterm-core/Cargo.toml

[features]
disk-pressure = ["nix"]

[dependencies]
nix = { version = "0.29", features = ["fs"], optional = true }
```

No additional crate dependencies beyond `nix` (already used by SBH for statvfs).

---

## P4: API Contracts and Crate/Subcrate Extraction Roadmap

### Public API Contract: DiskPressureMonitor

```rust
#[cfg(feature = "disk-pressure")]
pub mod disk_pressure {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PressureLevel { Green, Yellow, Orange, Red }

    pub struct PressurePrediction {
        pub level: PressureLevel,
        pub available_bytes: u64,
        pub total_bytes: u64,
        pub usage_pct: f64,
        pub velocity_bytes_per_sec: f64,
        pub time_to_exhaustion_secs: Option<f64>,
    }

    pub struct DiskPressureConfig {
        pub poll_interval_ms: u64,       // Default: 5000
        pub ewma_alpha: f64,             // Default: 0.3
        pub green_threshold_pct: f64,    // Default: 25.0
        pub yellow_threshold_pct: f64,   // Default: 20.0
        pub orange_threshold_pct: f64,   // Default: 15.0
        pub red_threshold_pct: f64,      // Default: 10.0
    }

    pub struct DiskPressureMonitor { /* ... */ }

    impl DiskPressureMonitor {
        pub fn new(path: &Path, config: DiskPressureConfig) -> Result<Self>;
        pub fn update(&mut self) -> Result<PressurePrediction>;
        pub fn current_level(&self) -> PressureLevel;
        pub fn prediction(&self) -> &PressurePrediction;
    }
}
```

### Public API Contract: BallastManager

```rust
#[cfg(feature = "disk-pressure")]
pub mod disk_ballast {
    pub struct BallastConfig {
        pub directory: PathBuf,
        pub file_count: usize,           // Default: 5
        pub file_size_bytes: u64,        // Default: 1 GiB
        pub auto_replenish: bool,        // Default: true
        pub replenish_cooldown_secs: u64, // Default: 300
    }

    pub struct BallastManager { /* ... */ }

    impl BallastManager {
        pub fn new(config: BallastConfig) -> Result<Self>;
        pub fn provision(&mut self) -> Result<usize>;   // Returns files created
        pub fn release(&mut self, count: usize) -> Result<u64>; // Returns bytes freed
        pub fn release_all(&mut self) -> Result<u64>;
        pub fn status(&self) -> BallastStatus;
        pub fn verify(&self) -> Result<bool>;
    }
}
```

### Crate Extraction Roadmap

**Phase 1**: Implement directly in frankenterm-core behind feature flag. Reference SBH source for algorithms.

**Phase 2**: If SBH extracts `sbh-core` (EWMA, PID, scoring as a library crate), switch to using it as a dependency.

**Phase 3**: If multiple tools need disk pressure monitoring, extract `ft-disk-pressure` as a shared crate.

---

## P5: Data Migration/State Synchronization and Compatibility Posture

### Migration Strategy

**No migration needed** — this adds new monitoring capability:
- EWMA state is ephemeral (computed on startup)
- Ballast files are new (provisioned on first run)
- Scoring engine is stateless (evaluates on-demand)

### State Synchronization

- **DiskPressureMonitor**: Updates on configurable poll interval; state is local to the monitor instance
- **BallastManager**: Persists ballast file paths in FrankenTerm's existing config store
- **ArtifactScorer**: Stateless — takes filesystem metadata as input, returns scores

### Compatibility Posture

- **Additive only**: No existing APIs change
- **Backward compatible**: Without `disk-pressure` feature, behavior is identical
- **Graceful degradation**: On non-Unix platforms, `DiskPressureMonitor::new()` returns a no-op that always reports Green

---

## P6: Testing Strategy and Detailed Logging/Observability

### Unit Tests (per module)

#### `disk_pressure.rs` tests (target: 30+)

- `test_pressure_level_green` — above 25% free reports Green
- `test_pressure_level_yellow` — 20-25% free reports Yellow
- `test_pressure_level_orange` — 15-20% free reports Orange
- `test_pressure_level_red` — below 10% free reports Red
- `test_ewma_convergence` — EWMA converges to stable rate
- `test_ewma_spike_damping` — spike doesn't cause false alarm
- `test_ewma_gradual_increase` — gradual fill detected
- `test_time_to_exhaustion_stable` — correct prediction for constant rate
- `test_time_to_exhaustion_none_when_freeing` — no exhaustion when disk freeing
- `test_pid_proportional_response` — P term drives immediate response
- `test_pid_integral_response` — I term accumulates for persistent pressure
- `test_pid_derivative_response` — D term dampens oscillation
- `test_pid_windup_prevention` — integral doesn't overflow
- `test_config_defaults` — default config produces valid state
- `test_config_custom_thresholds` — custom thresholds respected
- Additional edge cases for 30+ total

#### `disk_scoring.rs` tests (target: 30+)

- `test_cargo_target_classified` — cargo target dir identified
- `test_node_modules_classified` — node_modules scored correctly
- `test_git_dir_vetoed` — .git never marked for deletion
- `test_recent_file_vetoed` — files under age threshold protected
- `test_large_artifact_scored_higher` — size factor applied
- `test_bayesian_posterior_bounds` — score always in [0, 1]
- `test_expected_loss_optimization` — false-positive loss > false-negative for protected paths
- Additional pattern and edge case tests for 30+ total

#### `disk_ballast.rs` tests (target: 30+)

- `test_provision_creates_files` — correct number of files created
- `test_provision_correct_size` — files are requested size
- `test_release_one_frees_space` — releasing one file works
- `test_release_all_frees_all` — releasing all files works
- `test_verify_intact` — verification passes for intact files
- `test_verify_tampered` — verification fails for modified files
- `test_status_reports_counts` — status shows correct counts
- `test_auto_replenish_cooldown` — respects cooldown timer
- Additional lifecycle and error tests for 30+ total

### Integration Tests

- `test_pressure_drives_backpressure` — DiskPressureMonitor Red → BackpressureConfig escalation
- `test_pressure_drives_voi_scheduler` — Orange → VOI scheduler reduces capture frequency
- `test_ballast_emergency_recovery` — Red → ballast release → back to Green
- `test_scoring_with_real_filesystem` — score actual temp directories
- `test_graceful_degradation_non_unix` — non-Unix reports Green always

### Property-Based Tests

- `proptest_ewma_monotone` — EWMA velocity sign matches data trend
- `proptest_pressure_levels_ordered` — Red < Orange < Yellow < Green thresholds
- `proptest_pid_bounded_output` — PID output within configured bounds
- `proptest_scoring_deterministic` — same input → same score
- `proptest_ballast_provision_release_invariant` — provision(n) + release(n) = original state

### Logging Requirements

```rust
tracing::debug!(
    level = ?prediction.level,
    available_gb = prediction.available_bytes as f64 / 1e9,
    velocity_mbps = prediction.velocity_bytes_per_sec / 1e6,
    tte_secs = ?prediction.time_to_exhaustion_secs,
    "disk_pressure.update"
);
```

Fields: `level`, `available_bytes`, `total_bytes`, `usage_pct`, `velocity_bytes_per_sec`, `time_to_exhaustion_secs`, `pid_output`, `ewma_alpha`

---

## P7: Rollout, Rollback, Risk Mitigation, and Acceptance Gates

### Rollout Plan

**Phase 1: Core Monitors (Week 1-2)**
1. Implement `disk_pressure.rs` with EWMA + PID (extracted from SBH)
2. Implement `disk_ballast.rs` with provision/release lifecycle
3. Write 30+ unit tests per module
4. Gate: `cargo check --workspace` with and without feature

**Phase 2: Scoring and Wiring (Week 3-4)**
1. Implement `disk_scoring.rs` with Bayesian artifact scoring
2. Wire `DiskPressureMonitor` into `BackpressureConfig` as additional signal
3. Wire pressure levels into VOI scheduler's capture frequency
4. Integration tests
5. Gate: All tests pass, pressure changes correctly throttle capture

**Phase 3: Ballast Integration (Week 5)**
1. Add `ft ballast provision/release/status` CLI commands
2. Wire Red pressure → automatic ballast release
3. Auto-replenish ballast when pressure returns to Green
4. Gate: End-to-end emergency recovery tested

**Phase 4: Default Enable (Week 6)**
1. Consider enabling `disk-pressure` by default
2. Performance benchmarking (poll overhead)
3. Gate: <0.1% CPU overhead from pressure monitoring

### Rollback Plan

- **Phase 1 rollback**: Remove feature flag, revert Cargo.toml (single commit)
- **Phase 2 rollback**: Disable feature; backpressure and VOI revert to non-disk-aware behavior
- **Phase 3 rollback**: Disable feature; ballast files remain inert on disk

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| statvfs overhead on poll | Low | Low | 5s default interval; <1ms per call |
| False positive pressure detection | Medium | Medium | EWMA smoothing + PID damping |
| Ballast files consuming disk | Low | Medium | Default 5 GiB total; configurable |
| SBH bugs C1-C4 in extracted code | Medium | High | Fix during extraction (not blindly copy) |
| Non-Unix platform failure | Low | Low | Graceful degradation to Green |

### Acceptance Gates

1. `cargo check --workspace --all-targets` (with and without feature)
2. `cargo test --workspace` (all tests pass)
3. `cargo clippy --workspace -- -D warnings`
4. 90+ new unit tests across 3 modules
5. 5+ integration tests
6. 5+ proptest scenarios
7. <0.1% CPU overhead in benchmark

---

## P8: Summary and Action Items

### Chosen Architecture

**Component extraction** of EWMA estimator, PID controller, Bayesian scoring formula, and ballast manager from SBH into 3 new frankenterm-core modules, behind `disk-pressure` feature flag.

### Three New Modules

1. **`disk_pressure.rs`**: EWMA + PID disk pressure prediction with 4-level graduated response
2. **`disk_scoring.rs`**: Bayesian artifact scoring for smart cleanup candidate ranking
3. **`disk_ballast.rs`**: Sacrificial file management for instant emergency space recovery

### Implementation Order

1. `disk_pressure.rs` — EWMA estimator + PID controller + PressureLevel
2. `disk_ballast.rs` — BallastManager + provision/release
3. `disk_scoring.rs` — ArtifactClassifier + BayesianScorer
4. Wire into BackpressureConfig and VOI scheduler
5. CLI commands (`ft ballast provision/release/status`)
6. Integration and proptest
7. Performance benchmarking
8. Consider default-enable

### Upstream Tweak Proposals (for storage_ballast_helper)

1. **Extract `sbh-core` library crate**: EWMA, PID, scoring as reusable library without daemon/CLI deps
2. **Fix C1-C4 bugs**: Launchd plist, ballast sync_all, over-allocation, usize underflow
3. **Async-compatible API**: Wrap crossbeam channels with tokio::sync::mpsc adapters
4. **Platform trait improvements**: Make PAL trait public for downstream consumers

### Beads Created/Updated

- `ft-2vuw7.6.1` (CLOSED): Research complete
- `ft-2vuw7.6.2` (CLOSED): Analysis document complete
- `ft-2vuw7.6.3` (THIS DOCUMENT): Integration plan complete

---

*Plan complete. Ready for review and implementation bead creation.*
