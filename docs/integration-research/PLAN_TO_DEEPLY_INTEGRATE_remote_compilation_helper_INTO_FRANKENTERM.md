# Plan to Deeply Integrate remote_compilation_helper into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.7.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_remote_compilation_helper.md (ft-2vuw7.7.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Command classification for terminal commands**: Embed RCH's 5-tier SIMD-accelerated classifier to categorize terminal commands by type (compilation, test, lint, script, etc.) for policy, analytics, and routing decisions
2. **Compilation offloading awareness**: Integrate with RCH's daemon to show remote compilation status in FrankenTerm's pane metadata (local vs. remote, worker name, build duration)
3. **Circuit breaker pattern reuse**: Extract RCH's circuit breaker (Closed→Open→HalfOpen) for FrankenTerm's own connection management (mux connections, SSH sessions)
4. **Error code taxonomy adoption**: Adopt RCH's structured error code system (E001-E599 ranges) as a pattern for FrankenTerm's own error reporting
5. **Worker fleet visibility**: Surface RCH worker health status through FrankenTerm's telemetry dashboard

### Constraints

- **No tokio version conflict**: RCH uses tokio 1.49; FrankenTerm must use a compatible version
- **Feature-gated**: All RCH integration behind `rch-integration` feature flag
- **No unsafe code**: Both projects enforce `#![forbid(unsafe_code)]` on main binaries
- **Unix-only SSH**: RCH's openssh is Unix-only; integration must handle this gracefully
- **rch-common only**: Import only `rch-common` (3.5K LOC), not the full workspace (231K LOC)

### Non-Goals

- **Re-implementing compilation offloading**: FrankenTerm delegates to RCH, doesn't replace it
- **Building a second hook system**: RCH hooks into Claude Code directly; FrankenTerm observes, not intercepts
- **Worker management UI**: FrankenTerm surfaces status; `rch fleet` commands manage workers
- **SSH client replacement**: FrankenTerm uses its own connection logic; RCH's SSH is not reused for terminal sessions

---

## P2: Evaluate Integration Patterns

### Option A: rch-common Library Import (Chosen)

Add `rch-common` as a path dependency to access patterns, error codes, config types, and circuit breaker.

**Pros**: Type-safe, compile-time checked, small dependency (3.5K LOC), clean API boundary
**Cons**: Ties to RCH's release cycle; path dependency requires `/dp/remote_compilation_helper`
**Chosen**: Best value-to-coupling ratio

### Option B: Full Workspace Import

Add `rch`, `rchd`, `rch-common` as dependencies.

**Pros**: Access to daemon protocol, fleet management
**Cons**: Pulls in 231K LOC, openssh, axum, prometheus; massive compile-time increase
**Rejected**: Too heavy for FrankenTerm's needs

### Option C: Subprocess + JSON

Shell out to `rch status --json` for compilation status and worker health.

**Pros**: Process isolation, no compile-time dependency
**Cons**: Latency, requires RCH installed, can't share command classifier in-process
**Rejected**: Command classification needs sub-millisecond latency for policy decisions

### Option D: Extract Command Classifier Only

Copy the 5-tier classification logic into FrankenTerm without importing rch-common.

**Pros**: Zero external dependency
**Cons**: Diverges from upstream, duplicates maintenance
**Rejected**: rch-common is small enough to import directly

### Decision: Option A — Import rch-common as path dependency

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── command_classify.rs       # NEW: Wrapper around rch-common command classifier
│   ├── circuit_breaker.rs        # NEW: Generic circuit breaker (extracted from RCH pattern)
│   ├── rch_bridge.rs             # NEW: RCH daemon status bridge (compilation awareness)
│   └── ...existing modules...
├── Cargo.toml                    # Add rch-common as optional path dep
```

### Module Responsibilities

#### `command_classify.rs` — Terminal command classification

Wraps RCH's 5-tier classifier for FrankenTerm's use cases:
- `classify_command(cmd: &str) -> CommandClassification` — Returns command type and metadata
- `CommandType` enum — Compilation, Test, Lint, Format, Script, Shell, Unknown
- Used by: policy engine (allow/deny), analytics (command distribution), VOI scheduler (high-value captures)

#### `circuit_breaker.rs` — Generic connection circuit breaker

Extracts the circuit breaker pattern from RCH for reuse across FrankenTerm:
- `CircuitBreaker<T>` — Generic wrapper around any connection type
- States: Closed → Open → HalfOpen
- Configurable: failure threshold, timeout, probe interval
- Used by: mux connection pool, SSH sessions, external service calls

#### `rch_bridge.rs` — RCH daemon status integration

Reads RCH daemon status via Unix socket to display in FrankenTerm:
- `RchStatus` — worker health, active compilations, recent builds
- Integration with FrankenTerm's pane metadata (show "compiling on worker-3" in status bar)
- Graceful degradation when RCH daemon is not running

### Dependency Wiring

```toml
# In crates/frankenterm-core/Cargo.toml

[features]
rch-integration = ["rch-common"]

[dependencies]
rch-common = { path = "/dp/remote_compilation_helper/rch-common", optional = true }
```

---

## P4: API Contracts and Crate/Subcrate Extraction Roadmap

### Public API Contract: CommandClassifier

```rust
#[cfg(feature = "rch-integration")]
pub mod command_classify {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum CommandType {
        Compilation,  // cargo build, gcc, rustc, go build
        Test,         // cargo test, pytest, jest
        Lint,         // cargo clippy, eslint, pylint
        Format,       // cargo fmt, prettier, black
        Script,       // python, node, ruby scripts
        Shell,        // cd, ls, cat, grep
        Package,      // cargo install, npm install, pip install
        Git,          // git commit, push, pull
        Unknown,
    }

    pub struct CommandClassification {
        pub command_type: CommandType,
        pub confidence: f64,              // 0.0-1.0
        pub offloadable: bool,            // Can be sent to remote worker
        pub tier: u8,                     // Classification tier (0-4)
        pub classification_time_us: u64,  // Time taken
    }

    pub fn classify_command(cmd: &str) -> CommandClassification;
}
```

### Public API Contract: CircuitBreaker

```rust
pub mod circuit_breaker {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CircuitState { Closed, Open, HalfOpen }

    pub struct CircuitBreakerConfig {
        pub failure_threshold: u32,      // Default: 5
        pub success_threshold: u32,      // Default: 3
        pub timeout_ms: u64,             // Default: 30_000
        pub half_open_max_calls: u32,    // Default: 1
    }

    pub struct CircuitBreaker {
        state: CircuitState,
        failure_count: u32,
        success_count: u32,
        last_failure_time: Option<Instant>,
        config: CircuitBreakerConfig,
    }

    impl CircuitBreaker {
        pub fn new(config: CircuitBreakerConfig) -> Self;
        pub fn state(&self) -> CircuitState;
        pub fn allow_request(&mut self) -> bool;
        pub fn record_success(&mut self);
        pub fn record_failure(&mut self);
        pub fn reset(&mut self);
    }
}
```

### Crate Extraction Roadmap

**Phase 1**: `command_classify.rs` wraps rch-common; `circuit_breaker.rs` is standalone (no external dep). `rch_bridge.rs` reads daemon socket.

**Phase 2**: If `circuit_breaker` proves useful across projects, extract to `ft-circuit-breaker` crate.

**Phase 3**: If RCH publishes `rch-common` to crates.io, switch from path to version dependency.

---

## P5: Data Migration/State Synchronization and Compatibility Posture

### Migration Strategy

**No migration needed** — all new capabilities:
- Command classifier is stateless
- Circuit breaker state is ephemeral (resets on restart)
- RCH bridge reads daemon status on-demand

### State Synchronization

- **Command classifier**: Stateless pure function
- **Circuit breaker**: Per-connection state; no cross-process sync needed
- **RCH bridge**: Reads daemon Unix socket; FrankenTerm is a client only

### Compatibility Posture

- **Additive only**: No existing APIs change
- **Backward compatible**: Without `rch-integration` feature, behavior identical
- **Graceful degradation**: If RCH daemon unavailable, bridge returns "unknown" status
- **Circuit breaker standalone**: Works without rch-common; no feature gate needed

---

## P6: Testing Strategy and Detailed Logging/Observability

### Unit Tests (per module)

#### `command_classify.rs` tests (target: 30+)

- `test_cargo_build_is_compilation` — cargo build classified correctly
- `test_cargo_test_is_test` — cargo test classified correctly
- `test_cargo_clippy_is_lint` — cargo clippy classified correctly
- `test_cargo_fmt_is_format` — cargo fmt classified correctly
- `test_python_script_is_script` — python file.py classified correctly
- `test_git_push_is_git` — git push classified correctly
- `test_cd_is_shell` — cd classified as shell
- `test_empty_is_unknown` — empty command returns Unknown
- `test_piped_command_classification` — pipes handled correctly
- `test_classification_sub_millisecond` — all classifications under 1ms
- `test_simd_keyword_filter` — memchr acceleration works
- Additional pattern and edge case tests for 30+ total

#### `circuit_breaker.rs` tests (target: 30+)

- `test_starts_closed` — initial state is Closed
- `test_closes_after_success_threshold` — HalfOpen → Closed after successes
- `test_opens_after_failure_threshold` — Closed → Open after failures
- `test_half_open_after_timeout` — Open → HalfOpen after timeout
- `test_half_open_reopens_on_failure` — HalfOpen → Open on failure
- `test_allow_request_when_closed` — always allows in Closed
- `test_deny_request_when_open` — always denies in Open
- `test_limited_requests_half_open` — only max_calls in HalfOpen
- `test_reset_returns_to_closed` — manual reset works
- `test_config_defaults` — default config values correct
- `test_custom_thresholds` — custom thresholds respected
- Additional state machine and timing tests for 30+ total

#### `rch_bridge.rs` tests (target: 30+)

- `test_status_when_daemon_running` — reads worker status
- `test_status_when_daemon_not_running` — graceful fallback
- `test_active_compilation_reported` — shows compilation in progress
- `test_worker_health_mapping` — maps RCH health to FrankenTerm format
- Additional socket protocol and error handling tests for 30+ total

### Integration Tests

- `test_classify_and_score_for_voi` — high-value commands get higher VOI
- `test_circuit_breaker_with_mux_pool` — circuit breaker wraps connection pool
- `test_rch_status_in_pane_metadata` — compilation status appears in pane info

### Property-Based Tests

- `proptest_classification_deterministic` — same command → same type
- `proptest_classification_always_succeeds` — never panics on any input
- `proptest_circuit_breaker_state_machine` — valid state transitions only
- `proptest_circuit_breaker_bounded_counters` — counters never overflow

### Logging Requirements

```rust
tracing::debug!(
    command_type = ?classification.command_type,
    confidence = classification.confidence,
    tier = classification.tier,
    time_us = classification.classification_time_us,
    "command_classify.classify"
);
```

Fields: `command_type`, `confidence`, `tier`, `classification_time_us`, `offloadable`

---

## P7: Rollout, Rollback, Risk Mitigation, and Acceptance Gates

### Rollout Plan

**Phase 1: Circuit Breaker (Week 1)**
1. Implement `circuit_breaker.rs` (standalone, no external deps)
2. Write 30+ unit tests
3. Gate: `cargo check --workspace`

**Phase 2: Command Classifier (Week 2-3)**
1. Add rch-common as optional path dependency
2. Implement `command_classify.rs` wrapping rch-common patterns
3. Write 30+ unit tests
4. Gate: Classification under 1ms for all test cases

**Phase 3: RCH Bridge (Week 4)**
1. Implement `rch_bridge.rs` for daemon status reading
2. Wire into pane metadata display
3. Write 30+ unit tests
4. Gate: Graceful degradation when daemon not running

**Phase 4: Wiring (Week 5-6)**
1. Wire circuit breaker into mux connection pool
2. Wire command classifier into VOI scheduler
3. Wire RCH status into telemetry dashboard
4. Integration tests
5. Gate: All existing tests pass

### Rollback Plan

- **Phase 1 rollback**: Remove circuit_breaker.rs (standalone module, clean removal)
- **Phase 2 rollback**: Disable `rch-integration` feature, remove rch-common dep
- **Phase 3 rollback**: Remove rch_bridge.rs, disable feature
- **Phase 4 rollback**: Unwire from downstream consumers

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| rch-common API changes | Medium | Low | Pin to specific commit |
| Path dependency unavailable | Low | Low | Feature-gated; degrades gracefully |
| memchr SIMD compatibility | Low | Low | Fallback to non-SIMD path exists |
| Daemon socket path mismatch | Medium | Low | Configurable socket path |
| Command classifier false positives | Low | Medium | Confidence threshold gating |

### Acceptance Gates

1. `cargo check --workspace --all-targets` (with and without feature)
2. `cargo test --workspace` (all tests pass)
3. `cargo clippy --workspace -- -D warnings`
4. 90+ new unit tests across 3 modules
5. 3+ integration tests
6. 4+ proptest scenarios
7. Command classification <1ms in benchmark

---

## P8: Summary and Action Items

### Chosen Architecture

**Library import** of `rch-common` (3.5K LOC) as optional path dependency for command classification and error patterns, plus **standalone circuit breaker** module and **daemon status bridge**.

### Three New Modules

1. **`command_classify.rs`**: 5-tier SIMD command classification for policy, analytics, VOI
2. **`circuit_breaker.rs`**: Generic Closed→Open→HalfOpen pattern for connection resilience
3. **`rch_bridge.rs`**: Read RCH daemon status for pane metadata display

### Implementation Order

1. `circuit_breaker.rs` — standalone, no deps
2. `command_classify.rs` — requires rch-common
3. `rch_bridge.rs` — Unix socket reader
4. Wire circuit breaker into mux pool
5. Wire classifier into VOI scheduler
6. Wire RCH status into telemetry
7. Integration and proptest
8. Performance benchmarking

### Upstream Tweak Proposals (for remote_compilation_helper)

1. **Publish `rch-common` to crates.io**: Enable version-based dependency instead of path
2. **Extract command classifier**: Make `CommandClassifier` a first-class public API with builder pattern
3. **Stable daemon socket protocol**: Document Unix socket protocol for third-party consumers
4. **Worker health events**: Publish worker state changes as events (not just status queries)

### Beads Created/Updated

- `ft-2vuw7.7.1` (CLOSED): Research complete
- `ft-2vuw7.7.2` (CLOSED): Analysis document complete
- `ft-2vuw7.7.3` (THIS DOCUMENT): Integration plan complete

---

*Plan complete. Ready for review and implementation bead creation.*
