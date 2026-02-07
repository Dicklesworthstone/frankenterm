# Timing Determinism Guidelines

wa uses condition-based waiting throughout. Every wait is driven by an
observable state change — a pattern match, a queue drain, or an external
signal — rather than a fixed delay. This document explains the available
patterns, when to use each, and how to write deterministic tests.

## The No-Sleep Rule

**Do not use `std::thread::sleep()` or `tokio::time::sleep()` with a
fixed duration in production code.** Fixed sleeps are probabilistic: they
either wait too long (slow) or not long enough (flaky).

The only sanctioned exception is the chaos module (`chaos.rs`), which
uses blocking sleep to simulate slow I/O as a deliberate fault injection.

If you find yourself reaching for `sleep(Duration::from_secs(2))`, use
one of the patterns below instead.

## Three Levels of Waiting

wa provides waiting primitives at three granularities. Choose the
narrowest one that fits your situation.

### Level 1: Generic `wait_for` with Backoff

For waiting on an arbitrary condition:

```rust
use wa_core::wait::{wait_for, wait_for_condition, Backoff};
use std::time::Duration;

// Simple boolean condition
let result = wait_for_condition(
    || async { check_something().await },
    Duration::from_secs(10),
).await?;

// With custom backoff
let result = wait_for_condition_with_backoff(
    || async { check_something().await },
    Duration::from_secs(10),
    Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_secs(2),
        factor: 2,
        max_retries: Some(20),
    },
).await?;
```

The `Backoff` struct controls polling intervals:

| Field | Default | Purpose |
|-------|---------|---------|
| `initial` | 25 ms | First poll interval |
| `max` | 1 s | Upper bound on interval |
| `factor` | 2 | Multiplier per retry |
| `max_retries` | None | Optional retry cap |

Backoff grows exponentially: 25 ms → 50 ms → 100 ms → ... → 1 s (cap).
The final poll is clamped to the remaining timeout so the total wait
never exceeds the deadline.

### Level 2: Pane Pattern Matching (`PaneWaiter`)

For waiting until a pane's output matches a pattern:

```rust
use wa_core::wezterm::{PaneWaiter, WaitMatcher, WaitOptions};

let waiter = PaneWaiter::new(source, WaitOptions::default());
let result = waiter.wait_for(
    pane_id,
    &WaitMatcher::substring("$ "),  // wait for shell prompt
    Duration::from_secs(30),
).await?;

match result {
    WaitResult::Matched { elapsed_ms, polls } => { /* success */ }
    WaitResult::TimedOut { elapsed_ms, polls, .. } => { /* handle timeout */ }
}
```

`WaitOptions` controls the polling behavior:

| Field | Default | Purpose |
|-------|---------|---------|
| `tail_lines` | 200 | Lines of output to check |
| `escapes` | false | Include ANSI escape sequences |
| `poll_initial` | 50 ms | Initial poll interval |
| `poll_max` | 1 s | Maximum poll interval |
| `max_polls` | 10,000 | Safety limit |

The CLI equivalent is `wa send --wait-for`:

```bash
wa send 3 "ls -la" --wait-for "\\$" --timeout-secs 30
wa send 3 "grep error" --wait-for-regex "found \\d+ matches"
```

Use `WaitMatcher::substring()` for simple prompts and
`WaitMatcher::regex()` for structured output.

### Level 3: System Quiescence

For waiting until all subsystems are idle (queues drained, no recent
activity):

```rust
use wa_core::wait::{
    QuiescenceDetector, QueueDepthGauge, ActivityTracker,
    wait_for_quiescence,
};

// Set up gauges for each queue
let ingest_gauge = Arc::new(QueueDepthGauge::new("ingest"));
let storage_gauge = Arc::new(QueueDepthGauge::new("storage"));
let activity = Arc::new(ActivityTracker::new());

let detector = QuiescenceDetector::new(Duration::from_millis(500))
    .with_gauge(ingest_gauge.clone())
    .with_gauge(storage_gauge.clone())
    .with_activity(activity.clone());

// In production code: increment/decrement gauges around work
ingest_gauge.increment();
// ... do work ...
ingest_gauge.decrement();
activity.record();

// Wait for all queues to drain and no activity for 500ms
wait_for_quiescence(detector, Duration::from_secs(10)).await?;
```

The quiescence detector considers the system quiet when:
1. All `QueueDepthGauge` totals are zero (no pending work)
2. `ActivityTracker` has not been touched for the configured quiet window

Use this for shutdown sequences, test teardown, and any situation where
you need all asynchronous pipelines to finish.

## Adaptive Polling in the Tailer

The per-pane tailer uses adaptive polling to balance responsiveness and
CPU usage:

| State | Interval | Behavior |
|-------|----------|----------|
| Active (changes detected) | 50 ms | Reset to fast polling |
| Idle (no changes) | 50 ms → 75 ms → 112 ms → ... → 1 s | Exponential backoff (1.5x) |
| Backpressure (5+ send failures) | Paused | Insert explicit GAP, reset |

Configuration lives in `TailerConfig`:

```rust
TailerConfig {
    min_interval: Duration::from_millis(50),
    max_interval: Duration::from_secs(1),
    backoff_multiplier: 1.5,
    max_concurrent: 10,
    overlap_size: 256,
    send_timeout: Duration::from_millis(100),
}
```

When a pane produces output, its tailer immediately drops back to
`min_interval`. When idle, it gradually slows to `max_interval`. This
avoids wasting CPU on quiet panes while responding quickly to active ones.

## Explicit GAP Semantics

When wa cannot guarantee continuity — overlap matching fails, alt-screen
blocks stable capture, or backpressure overflows — it records an explicit
**GAP segment** and emits a gap event.

GAPs are treated as uncertainty:
- Policy checks can require approval when recent gaps are present
- Search results flag text near gaps as potentially incomplete
- Diagnostics surface gap counts and causes

The design principle: **no data loss goes unrecorded.** A GAP is better
than silently dropping data or pretending continuity exists.

## Timeout Handling

All timeouts follow the same structure:

1. **Wall-clock deadline:** `start + timeout`
2. **Remaining time clamping:** Sleep duration never exceeds remaining
   time
3. **Max retry limit:** Optional `max_retries` stops polling early
4. **Safety limit:** `max_polls` cap prevents runaway loops

When a timeout fires, the error includes diagnostic context:

```rust
WaitError {
    expected: "prompt marker '$ '",
    last_observed: Some("... building project ..."),
    retries: 47,
    elapsed: Duration::from_secs(30),
}

// Display: "timeout waiting for prompt marker '$ ' after 30000ms
//          (retries=47, last_observed='... building project ...')"
```

This makes timeout failures actionable: the caller knows what was
expected, what was last seen, and how many attempts were made.

## Retry Policies

For external calls (WezTerm CLI, database writes), use `RetryPolicy`:

```rust
use wa_core::retry::RetryPolicy;

// Pre-configured policies
let wezterm = RetryPolicy::wezterm_cli();  // 3 attempts, 100ms initial
let db = RetryPolicy::db_write();          // 5 attempts, 50ms initial
```

Retry policies add jitter (±10% by default) to prevent thundering herd
on shared resources. They integrate with the circuit breaker for
connection-level fault tolerance.

## Writing Deterministic Tests

### Unit tests

Use `tokio::time::pause()` to control time in async tests:

```rust
#[tokio::test]
async fn test_wait_for_timeout() {
    tokio::time::pause();

    let result = wait_for_condition(
        || async { false },  // never succeeds
        Duration::from_secs(5),
    ).await;

    assert!(result.is_err());
    // Test completes instantly — no real wall-clock wait
}
```

For synchronous timing logic, inject a clock trait or use
`std::time::Instant` comparisons rather than `sleep()`.

### Integration tests

Use pattern-based waits, not fixed delays:

```rust
// BAD: flaky, slow, non-deterministic
tokio::time::sleep(Duration::from_secs(5)).await;
let text = get_pane_text(pane_id).await;
assert!(text.contains("ready"));

// GOOD: deterministic, fast, self-documenting
let result = waiter.wait_for(
    pane_id,
    &WaitMatcher::substring("ready"),
    Duration::from_secs(30),
).await?;
assert!(matches!(result, WaitResult::Matched { .. }));
```

### E2E tests

The E2E harness (`scripts/e2e_test.sh`) enforces timing discipline:

- **Global timeout:** Each scenario has a configurable timeout (default
  120s, exit code 4 on breach)
- **Artifact collection:** `duration_ms` is recorded per scenario for
  performance tracking
- **No real AI:** Scenarios use scripted output so results are
  deterministic
- **Pattern detection:** All waits use `wa robot wait-for` or equivalent,
  not `sleep`

```bash
# Run with custom timeout
./scripts/e2e_test.sh --timeout 60 my_scenario

# Verbose output includes timing
./scripts/e2e_test.sh --verbose --keep-artifacts my_scenario
```

## Contributor Checklist

Before submitting code that involves waiting or timing:

- [ ] No `std::thread::sleep()` in production code
- [ ] No `tokio::time::sleep()` with a fixed duration in production code
- [ ] Waits use `wait_for`, `PaneWaiter`, or `QuiescenceDetector`
- [ ] Timeouts include `WaitError` with `expected` and `last_observed`
- [ ] Backoff intervals start small (25–50 ms) and cap at 1–2 s
- [ ] Tests use `tokio::time::pause()` or pattern matching, not real
  delays
- [ ] Continuity gaps are recorded explicitly (GAP segments)
- [ ] Retry policies use jitter to prevent thundering herd

### Exception process

If you genuinely need a fixed delay (hardware settling, rate limit
cooldown with no signal), document the reason in a comment:

```rust
// Fixed delay: rate-limit API requires minimum 1s between calls
// and provides no retry-after header. See WA-4002.
tokio::time::sleep(Duration::from_secs(1)).await;
```

This makes the exception visible and grep-able for future cleanup.
