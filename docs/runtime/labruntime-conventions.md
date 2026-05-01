# LabRuntime Test Conventions

**Bead:** `ft-t9a6q.3` (BR-RC-RUNTIME-SEMANTICS.G14.0).
**Module:** [`crates/frankenterm-core/src/test_fixtures/lab_runtime.rs`](../../crates/frankenterm-core/src/test_fixtures/lab_runtime.rs).
**Doctrine:** [`runtime-proof-trait.md`](runtime-proof-trait.md).
**Sibling:** [`cx-propagation-lint.md`](cx-propagation-lint.md) (the static-analysis enforcer; this doc covers the runtime test enforcer).

LabRuntime is asupersync's deterministic, virtual-time test
runtime. It schedules tasks under a seedable scheduler, advances
virtual time at user request, and bails out cleanly if a test
deadlocks. The frankenterm-core fixture wraps it in a one-call
API so test files don't carry ~30 lines of LabRuntime boilerplate.

## When to use LabRuntime

Use LabRuntime when **any** of these apply to the code under test:

- It awaits an asupersync primitive (Mutex, Notify, mpsc, watch, broadcast, oneshot).
- It calls `sleep` / `yield_now` / `runtime::spawn` / `JoinHandle::await`.
- It reads or writes through a `Cx` checkpoint that records virtual time.
- It exercises deadline / timeout behavior the test wants to assert against deterministically.
- It races against another concurrent task and the test must pin the scheduling order.

If the code under test is purely synchronous, **don't** wrap it in
LabRuntime — the fixture is overhead for no win.

## The fixture API

Three entry points, ordered by typical use:

### `lab_runtime_test(|cx| async move { ... })`

Default seed (`0xC0FFEE`), default step budget (`50_000`),
single worker, auto-advance on. Replaces the ~30-line
`LabRuntime::new(...) → create_root_region → create_task →
schedule → run_with_auto_advance → assert termination` recipe
with a single call.

```rust
use frankenterm_core::test_fixtures::lab_runtime::{
    assert_ran_to_completion, lab_runtime_test,
};

#[test]
fn my_async_test() {
    let report = lab_runtime_test(|_cx| async move {
        // Body. Time advancement happens automatically when the
        // body awaits a deadline-bearing primitive.
    });
    assert_ran_to_completion(&report);
}
```

### `lab_runtime_test_with_seed(seed, |cx| async move { ... })`

Use when seed determinism matters — property tests that span
multiple seeds, shrink-friendly fuzzers, or regression-pinning
a specific scheduling decision.

```rust
let report = lab_runtime_test_with_seed(42, |_cx| async move {
    // Body, deterministic across runs that share the seed.
});
```

### `lab_runtime_test_with_config(config, |cx| async move { ... })`

Reach for this only when the test needs **unusual**
configuration:

- Multi-worker scheduling (`worker_count(2)`+).
- A custom step budget different from `DEFAULT_MAX_STEPS`.
- Disabled auto-advance (rarely needed; mostly when the test
  drives time manually via `runtime.virtual_time_*` operations).

```rust
use frankenterm_core::test_fixtures::lab_runtime::{
    LabConfig, lab_runtime_test_with_config,
};

let config = LabConfig::new(7)
    .with_auto_advance()
    .worker_count(2)
    .max_steps(200_000);
let report = lab_runtime_test_with_config(config, |_cx| async move {
    // Body
});
```

## Cx threading

The fixture constructs a fresh `Cx` via `Cx::for_testing()` and
hands it to the user's closure as an owned value. Two patterns:

### Inline body — accept `_cx`

When the body doesn't need to thread `Cx` deeper:

```rust
lab_runtime_test(|_cx| async move {
    // Body that doesn't call any &Cx-taking helper.
});
```

### Threaded body — pass `&cx` to helpers

When the body calls `&Cx`-taking helpers (which most production
code does, per the cx-propagation lint):

```rust
lab_runtime_test(|cx| async move {
    let cx = cx; // owned now
    do_work(&cx, &input).await;
    assert_eq!(some_state(&cx), expected);
});
```

The `cx` arrives owned so the test can decide whether to
borrow it (`&cx`) or move it into a sub-task.

## Assertion patterns

### Default — `assert_ran_to_completion`

Most tests want "the body finished naturally." The helper wraps
`matches!(report.termination, Quiescent)` with a uniform
diagnostic:

```rust
let report = lab_runtime_test(|_cx| async move { ... });
assert_ran_to_completion(&report);
```

### Asserting on step counts

When the test wants to *prove* a specific number of scheduling
steps (e.g. "the cooperative yield path took ≤ N steps"):

```rust
let report = lab_runtime_test(|_cx| async move { ... });
assert_ran_to_completion(&report);
assert!(
    report.steps < 1_000,
    "body completed in {} steps; budget was 1000",
    report.steps,
);
```

### Asserting on termination reason explicitly

When the test wants to verify a specific termination *other*
than `Quiescent`:

```rust
use frankenterm_core::test_fixtures::lab_runtime::AutoAdvanceTermination;

let config = LabConfig::new(0).with_auto_advance().worker_count(1).max_steps(50);
let report = lab_runtime_test_with_config(config, |_cx| async move {
    // Body that won't fit in 50 steps.
});
assert_eq!(report.termination, AutoAdvanceTermination::StepLimitReached);
```

## What the fixture intentionally panics on

`StuckBailout` — the auto-advance scheduler couldn't make
progress for 1 000 consecutive iterations. The fixture panics
with a named diagnostic:

> `LabRuntime stuck — auto-advance bailed after N steps. Most likely: the test future is awaiting a primitive that was never signaled. Check sleep durations, channel sends, and oneshot resolutions.`

This is the single most common LabRuntime failure mode. Naming
the symptom inline means a test author seeing the failure can
immediately recognize the cause without diving into the
asupersync internals.

## Migration playbook

To migrate an existing inline-LabRuntime test:

1. Identify the `LabRuntime::new(LabConfig::new(SEED).with_auto_advance().worker_count(N).max_steps(M))` call site.
2. Replace the entire ~30-line recipe with one fixture call:
   - If `SEED == DEFAULT_SEED` and `M == DEFAULT_MAX_STEPS` and `N == 1`: use `lab_runtime_test`.
   - If only `SEED` differs: use `lab_runtime_test_with_seed`.
   - Otherwise: use `lab_runtime_test_with_config`.
3. Move the body into the closure. The closure receives an owned
   `Cx`; replace any inline `Cx::for_testing()` with the
   parameter.
4. Replace `let report = runtime.run_with_auto_advance(); assert!(!matches!(report.termination, StuckBailout));` with `assert_ran_to_completion(&report);` (or drop entirely if the test doesn't care about the report).

## Substrate scope (this bead)

Shipped under ft-t9a6q.3:

- The function-style fixture (3 entry points + assertion helper).
- Re-exports of `LabConfig`, `Budget`, `AutoAdvanceTermination` so callers don't have to depend on `asupersync` directly.
- 9 unit tests on the fixture's contract (closure runs, Cx is passed, seed determinism, custom config, panic-on-stuck-bailout, completion-assertion behavior, re-exports resolve, demonstrative migration).
- This conventions doc.

## Wired-pass scope (named follow-ups)

Same substrate-pass / wired-pass split as ft-53zsr (tmux compat
matrix) and ft-t9a6q.1 (cx-propagation lint):

- **ft-t9a6q.3.cont.macro**: `lab_runtime_test!` proc-macro that
  rewrites `#[test] async fn body { ... }` into the function-call
  form. Drops in against the fixture's contract.
- **ft-t9a6q.3.cont.migrate**: migrate the 5 representative
  existing async tests (cpu_pressure.rs, native_events.rs,
  telemetry.rs, watchdog_real_mux.rs, snapshot_real_mux.rs) onto
  the fixture as a demonstrative pattern. Each migration is a
  separate commit so the diff shows the boilerplate-deletion
  delta cleanly.
- **ft-t9a6q.3.cont.ci**: nightly CI lane that runs every
  `lab_runtime_test`-marked test under both the default seed
  and a deterministic-multi-seed sweep so the fixture's
  determinism contract is regression-guarded across the full
  test corpus.
- **ft-t9a6q.3.cont.time**: time-advancement helpers for
  deadline tests (`runtime.advance_virtual_time(Duration)` style
  surface). The fixture's auto-advance covers the common case;
  manual advance is the deferred ergonomic.

## Cross-references

- [`crates/frankenterm-core/src/test_fixtures/lab_runtime.rs`](../../crates/frankenterm-core/src/test_fixtures/lab_runtime.rs) — the fixture.
- [`crates/frankenterm-core/src/cx.rs`](../../crates/frankenterm-core/src/cx.rs) — `Cx::for_testing()`.
- [`asupersync/src/lab/runtime.rs`](https://example.invalid/asupersync) — `LabRuntime` itself (vendored under `/Users/jemanuel/projects/asupersync/`).
- ft-t9a6q parent epic.
- ft-t9a6q.1 (closed) — cx-propagation analyzer; the static-analysis enforcer that complements LabRuntime's runtime enforcer.
- ft-t9a6q.2 — burn-down dashboard; consumes both this fixture's nightly-lane output and the cx-propagation analyzer's `--json`.
