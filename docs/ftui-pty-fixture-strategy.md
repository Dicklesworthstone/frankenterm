# Deterministic PTY Fixture Seed and Timing Strategy

**Bead:** wa-1qr1 (FTUI-07.3.a)
**Date:** 2026-02-09
**Parent:** wa-3gii (FTUI-07.3 Build PTY E2E scenario pack)
**Blocks:** wa-308u (FTUI-07.3.b PTY failure artifact schema)

## 1  Problem Statement

The existing FTUI-07.3 E2E tests (in `ftui_stub.rs`) run headless: they feed
key events into the `WaModel` and snapshot the in-memory `Frame` buffer. This
validates rendering logic but misses:

- Real terminal escape sequence handling
- PTY read/write timing and buffering
- Crossterm/ftui terminal initialization/teardown
- Raw mode, alternate screen, cursor visibility interactions
- Terminal resize (SIGWINCH) propagation
- Multi-byte character and paste bracket handling

PTY E2E tests address these gaps by running wa in a real pseudoterminal and
interacting via scripted input/output assertions.

## 2  Seed Policy

### 2.1  Deterministic Scenario Seeds

Each PTY scenario is identified by a `ScenarioSeed`:

```rust
struct ScenarioSeed {
    /// Scenario name (unique identifier)
    name: &'static str,
    /// Deterministic seed for reproducible fixture data
    seed: u64,
    /// Terminal dimensions
    cols: u16,
    rows: u16,
}
```

The `seed` value drives:
- Mock data generation (via seeded PRNG for pane counts, event counts, etc.)
- Input timing jitter (bounded, deterministic)
- Resize sequence ordering

Seeds are fixed constants in the test source, not random:

```rust
const SEED_BASIC_NAVIGATION: ScenarioSeed = ScenarioSeed {
    name: "basic_navigation",
    seed: 0xCAFE_0001,
    cols: 80,
    rows: 24,
};
```

### 2.2  PRNG Contract

All randomized aspects use `rand::rngs::StdRng::seed_from_u64(seed)`:

| Aspect | How Seed Controls It |
|--------|---------------------|
| Fixture data count | `rng.gen_range(min..=max)` for pane/event/triage counts |
| Input inter-key delay | `rng.gen_range(5..=50)` ms (bounded jitter) |
| Resize timing | `rng.gen_range(0..=scenario_steps)` for insertion point |
| Fixture field values | `rng.gen_range(...)` for severity, state, timestamps |

### 2.3  Seed Reproduction

When a test fails, the seed is printed in the failure message:

```
PTY E2E FAILED: scenario=basic_navigation seed=0xCAFE0001 cols=80 rows=24
  assertion failed at step 5: expected view "Events", got "Home"
  transcript: /tmp/wa-pty-e2e/basic_navigation_0xCAFE0001.transcript
```

Rerunning with the same seed produces identical behavior.

## 3  Timing Strategy

### 3.1  Event-Driven, Not Sleep-Based

PTY tests must NEVER use fixed `sleep()` calls. Instead, use event-driven
synchronization:

| Anti-Pattern | Replacement |
|-------------|-------------|
| `sleep(100ms)` then check output | `wait_for_output_matching(pattern, timeout)` |
| `sleep(500ms)` for render to complete | `wait_for_stable_frame(timeout)` |
| `sleep(1s)` for startup | `wait_for_prompt(timeout)` |

### 3.2  Synchronization Primitives

```rust
/// Wait until PTY output contains the expected pattern.
/// Returns the full output buffer on match, or Err on timeout.
fn wait_for_output(
    pty: &mut PtyReader,
    pattern: &str,
    timeout: Duration,
) -> Result<String, TimeoutError>;

/// Wait until two consecutive reads produce the same screen content.
/// This indicates rendering has stabilized.
fn wait_for_stable_frame(
    pty: &mut PtyReader,
    stability_window: Duration,
    timeout: Duration,
) -> Result<String, TimeoutError>;

/// Send input and wait for the expected output change.
fn send_and_expect(
    pty: &mut PtyHandle,
    input: &[u8],
    expected_pattern: &str,
    timeout: Duration,
) -> Result<String, TimeoutError>;
```

### 3.3  Timeout Policy

| Phase | Default Timeout | Rationale |
|-------|----------------|-----------|
| Startup (wait for TUI) | 5s | Cold start with DB init |
| Key input → render | 500ms | Model update + frame render |
| Frame stabilization | 200ms | Two consecutive identical reads |
| Shutdown (after quit) | 2s | Terminal restoration |
| Total scenario | 30s | Hard ceiling per scenario |

All timeouts are configurable via `PtyTestConfig` to accommodate CI load.

### 3.4  Deterministic Delays

When a scenario requires inter-step pauses (e.g., to test refresh behavior),
use seeded delays:

```rust
let delay_ms = rng.gen_range(5..=50);
tokio::time::sleep(Duration::from_millis(delay_ms)).await;
```

These are small (5-50ms), bounded, and reproducible via seed.

## 4  Anti-Flake Constraints

### 4.1  Retry Policy

PTY E2E tests do NOT retry on failure. Flaky tests indicate a real timing
or synchronization bug. Instead:

- Fix the root cause (missing synchronization point)
- Increase timeout if CI is consistently slower
- Add a synchronization primitive at the flaky point

### 4.2  Jitter Controls

| Control | Value | Purpose |
|---------|-------|---------|
| Max inter-key delay | 50ms | Prevents tests from being unreasonably slow |
| Min inter-key delay | 5ms | Prevents faster-than-human input storms |
| Stability window | 200ms | Prevents declaring frame stable too early |
| Read buffer | 4096 bytes | Consistent read granularity |

### 4.3  Environment Isolation

PTY tests must NOT depend on:
- User's shell configuration (`.bashrc`, `.zshrc`)
- Locale settings (force `LANG=C.UTF-8`)
- Terminal type (force `TERM=xterm-256color`)
- Color theme (force `NO_COLOR=1` for output comparison)

```rust
fn pty_env() -> Vec<(String, String)> {
    vec![
        ("LANG".into(), "C.UTF-8".into()),
        ("TERM".into(), "xterm-256color".into()),
        ("NO_COLOR".into(), "1".into()),
        ("HOME".into(), tempdir().into()),
        ("WA_DB_PATH".into(), ":memory:".into()),
    ]
}
```

### 4.4  Resource Cleanup

Each PTY test creates its own:
- Temporary directory (cleaned up on drop)
- In-memory database (`WA_DB_PATH=:memory:`)
- PTY pair (master fd closed on drop)

No shared state between tests. Tests can run in parallel.

## 5  Artifact Requirements

### 5.1  Required Artifacts on Failure

When a PTY E2E test fails, it must produce:

| Artifact | Format | Purpose |
|----------|--------|---------|
| Transcript | `.transcript` (raw bytes + timestamps) | Full PTY I/O replay |
| Screenshot | `.txt` (terminal text at failure point) | Visual state at failure |
| Input log | `.input.json` (timestamped key events) | Reproduce the exact input sequence |
| Scenario metadata | `.meta.json` | Seed, dimensions, env vars, timeouts |
| Diagnostic dump | `.diag.txt` | Model state, view, filter values |

### 5.2  Transcript Format

```json
[
  {"t": 0, "dir": "out", "data": "base64..."},
  {"t": 5, "dir": "in",  "data": "base64..."},
  {"t": 12, "dir": "out", "data": "base64..."},
  {"t": 15, "dir": "in",  "data": "base64...", "key": "Tab"},
  ...
]
```

Fields:
- `t`: milliseconds since scenario start (monotonic)
- `dir`: `"in"` (sent to PTY) or `"out"` (read from PTY)
- `data`: base64-encoded raw bytes
- `key`: optional human-readable key name (for input events)

### 5.3  Metadata Format

```json
{
  "scenario": "basic_navigation",
  "seed": "0xCAFE0001",
  "cols": 80,
  "rows": 24,
  "env": { "TERM": "xterm-256color", "LANG": "C.UTF-8" },
  "timeouts": { "startup": 5000, "key_render": 500, "total": 30000 },
  "started_at": "2026-02-09T12:00:00Z",
  "failed_at_step": 5,
  "assertion": "expected view Events, got Home"
}
```

### 5.4  Artifact Location

```
target/pty-e2e-artifacts/
  {scenario_name}_{seed}/
    transcript.jsonl
    screenshot.txt
    input.json
    meta.json
    diag.txt
```

Artifacts are only written on failure (not on success) to avoid disk bloat.

## 6  Scenario Structure

### 6.1  Scenario Definition

```rust
struct PtyScenario {
    seed: ScenarioSeed,
    steps: Vec<ScenarioStep>,
    config: PtyTestConfig,
}

enum ScenarioStep {
    /// Send raw bytes to PTY input
    SendInput(Vec<u8>),
    /// Send a named key (resolved to escape sequence)
    PressKey(KeyName),
    /// Type a string character by character with seeded delays
    TypeString(String),
    /// Wait for output to contain pattern
    WaitForOutput { pattern: String, timeout: Duration },
    /// Wait for frame to stabilize
    WaitForStable { timeout: Duration },
    /// Assert current screen contains text
    AssertContains(String),
    /// Assert current screen does NOT contain text
    AssertNotContains(String),
    /// Assert current view (by checking tab bar highlight)
    AssertView(String),
    /// Resize the PTY
    Resize { cols: u16, rows: u16 },
    /// Deterministic delay (seeded)
    Delay { min_ms: u64, max_ms: u64 },
}
```

### 6.2  Representative Scenarios

| Scenario | Seed | Steps | Validates |
|----------|------|-------|-----------|
| `startup_and_quit` | 0x0001 | Start → wait for Home → press q → verify exit | Basic lifecycle |
| `view_navigation` | 0x0002 | Start → Tab through all views → verify each | View switching |
| `events_filter` | 0x0003 | Start → go to Events → type filter → verify filtered | Filter input |
| `resize_during_render` | 0x0004 | Start → resize 5 times → verify no crash | Resize handling |
| `command_handoff` | 0x0005 | Start → trigger command → verify suspend/resume | Handoff lifecycle |
| `rapid_input` | 0x0006 | Start → 50 rapid keys → verify no corruption | Input buffering |
| `long_filter_input` | 0x0007 | Start → type 100+ chars → verify no overflow | Buffer limits |
| `degraded_system` | 0x0008 | Start with unhealthy mock → verify all views render | Error states |

## 7  CI Integration

### 7.1  Test Execution

```bash
# Run all PTY E2E tests
cargo test -p wa-core --features ftui -- pty_e2e

# Run a specific scenario
cargo test -p wa-core --features ftui -- pty_e2e::startup_and_quit

# Run with increased timeouts (CI)
WA_PTY_TIMEOUT_FACTOR=2 cargo test -p wa-core --features ftui -- pty_e2e
```

### 7.2  Timeout Scaling

CI environments are slower. The `WA_PTY_TIMEOUT_FACTOR` env var scales all
timeouts by the given factor:

```rust
fn scaled_timeout(base: Duration) -> Duration {
    let factor: f64 = std::env::var("WA_PTY_TIMEOUT_FACTOR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);
    Duration::from_millis((base.as_millis() as f64 * factor) as u64)
}
```

### 7.3  Artifact Upload

CI uploads artifacts only on failure:

```yaml
- name: Upload PTY E2E artifacts
  if: failure()
  uses: actions/upload-artifact@v4
  with:
    name: pty-e2e-failures
    path: target/pty-e2e-artifacts/
    retention-days: 7
```

## 8  Relationship to Headless E2E Tests

| Aspect | Headless (FTUI-07.3 existing) | PTY (this strategy) |
|--------|-------------------------------|---------------------|
| Terminal | None — model + Frame buffer | Real PTY pair |
| Input | Direct `WaModel::update(WaMsg::Key)` | Bytes written to PTY slave |
| Output | `frame_to_text(&frame)` | Bytes read from PTY master |
| Timing | Instant (no I/O) | Real I/O with synchronization |
| Determinism | Data-deterministic (MockQuery) | Seed-deterministic (seeded PRNG) |
| Speed | ~1ms per test | ~1-5s per scenario |
| Coverage | Model logic, rendering, state | Full terminal lifecycle |
| Flake risk | Zero (no I/O) | Low (event-driven sync) |

Both test levels are complementary. Headless tests run fast and cover logic;
PTY tests run slower and cover the terminal integration boundary.

## References

- `crates/wa-core/src/tui/ftui_stub.rs:6432-7045` — existing headless E2E tests
- `crates/wa-core/src/tui/ftui_stub.rs:6441-6539` — E2eSession helper
- `docs/ftui-command-handoff-traces.md` — handoff state machine (wa-bjvg)
- `docs/ftui-teardown-harness.md` — teardown invariants (wa-3fed)
- `docs/ftui-subprocess-forwarding-contract.md` — PTY capture contract (wa-3gsu)
