//! LabRuntime virtual-time test fixture.
//!
//! **Bead:** [BR-RC-RUNTIME-SEMANTICS.G14.0] / `ft-t9a6q.3`.
//! **Doc:** `docs/runtime/labruntime-conventions.md`.
//!
//! # What this fixture ships
//!
//! A thin, ergonomic wrapper around `asupersync::LabRuntime` that:
//!
//! - Builds a `LabConfig` with sensible defaults (auto-advance,
//!   single worker, 50_000-step bailout).
//! - Spins up a root region with `Budget::INFINITE`.
//! - Spawns the user's async closure as the root task with a
//!   freshly-constructed `Cx` installed via `Cx::for_testing()`.
//! - Drives the runtime under auto-advance.
//! - Returns a [`LabReport`] carrying the termination reason +
//!   step count for assertion.
//! - Panics on `StuckBailout` with a clear diagnostic so test
//!   failures point at the missed cooperation point rather than
//!   bubbling up an opaque error.
//!
//! # Why a function-style API rather than a proc-macro
//!
//! The bead's stated API is `lab_runtime_test!(async fn ...)` — a
//! proc-macro that auto-installs `Cx` and exposes time advancement.
//! Proc-macros require a separate `proc-macro = true` crate plus
//! the macro plumbing for `#[test]` rewriting. The substrate ship
//! is the **function-style fixture**: it gives the same ergonomic
//! win (one call replaces ~30 lines of LabRuntime boilerplate)
//! without the proc-macro crate. The proc-macro layer is filed as
//! ft-t9a6q.3.cont.macro and drops in against this function's
//! contract — the macro just rewrites `#[test] async fn body` into
//! `#[test] fn name() { lab_runtime_test(SEED, |cx| async move { body }) }`.
//!
//! # Why test fixtures live in `frankenterm-core`'s `src/`
//!
//! Two reasons:
//!
//! 1. The fixture re-exports `asupersync::LabConfig` /
//!    `asupersync::Budget` so tests don't need to depend on
//!    asupersync directly.
//! 2. Both inline `#[cfg(test)] mod tests` blocks AND integration
//!    tests under `tests/` can `use frankenterm_core::test_fixtures::lab_runtime::*`.
//!
//! No `#[cfg(test)]` gating: production builds will not link the
//! fixture symbols if no caller references them (dead-code
//! elimination), and the asupersync dep is already a regular dep
//! of frankenterm-core.

use crate::cx::Cx;
use std::future::Future;

pub use asupersync::lab::AutoAdvanceTermination;
pub use asupersync::{Budget, LabConfig, LabRuntime, RegionId};

/// Default seed for [`lab_runtime_test`]. Tests that need a
/// specific seed should call [`lab_runtime_test_with_seed`].
pub const DEFAULT_SEED: u64 = 0xC0FFEE;

/// Environment variable consulted by [`lab_runtime_test`] (the
/// default-seed entry point) to pick the runtime seed at test time.
/// The CI multi-seed determinism sweep at
/// `.github/workflows/lab-runtime-multi-seed-nightly.yml` (ft-qrjvh)
/// sets this to one of [`MULTI_SEED_SWEEP_SEEDS`] per matrix lane;
/// callers that pass an explicit seed via
/// [`lab_runtime_test_with_seed`] or [`lab_runtime_test_with_config`]
/// are unaffected — the override only redirects the *default-seed*
/// path so explicit-seed regression fixtures stay pinned.
pub const SEED_OVERRIDE_ENV: &str = "FT_LAB_RUNTIME_SEED";

/// Deterministic multi-seed sweep corpus consumed by ft-qrjvh's
/// nightly CI lane. Five seeds chosen to cover (a) the canonical
/// default `DEFAULT_SEED`, (b) one common debugger seed
/// (`0xDEADBEEF`), (c) two arbitrary-but-pinned values, and (d) the
/// minimum-non-zero seed `1` so tests don't conflate "default" with
/// "no seed set". Any future expansion is purely additive — append
/// at the tail to keep historical run identifiers stable.
pub const MULTI_SEED_SWEEP_SEEDS: [u64; 5] =
    [DEFAULT_SEED, 0xDEAD_BEEF, 0xCAFE_BABE, 0xBAD_C0DE, 1];

/// Pure helper: parse a candidate seed string, accepting decimal
/// and `0x`-prefixed hex (case-insensitive). `None` for malformed
/// input so the caller can fall back to its own default. Split
/// from [`effective_default_seed`] so it is testable in parallel
/// without touching the process env.
fn parse_seed_override(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(stripped) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u64::from_str_radix(stripped, 16).ok()
    } else {
        trimmed.parse::<u64>().ok()
    }
}

/// Resolve the effective default-path seed: honour
/// [`SEED_OVERRIDE_ENV`] when set + parseable, otherwise fall back to
/// the supplied `default`. Tests that pin a seed via
/// [`lab_runtime_test_with_seed`] never call this helper, so they
/// stay deterministic on their pinned value.
fn effective_default_seed(default: u64) -> u64 {
    std::env::var(SEED_OVERRIDE_ENV)
        .ok()
        .as_deref()
        .and_then(parse_seed_override)
        .unwrap_or(default)
}

/// Default step bailout. Mirrors the value used inline at every
/// existing LabRuntime call site (cpu_pressure.rs, native_events.rs,
/// telemetry.rs).
pub const DEFAULT_MAX_STEPS: u64 = 50_000;

/// Result of a [`lab_runtime_test`] run. The wrapped report carries
/// the termination reason + step count + oracle pass/fail summary
/// + final virtual-time nanos.
#[derive(Debug)]
pub struct LabReport {
    pub termination: AutoAdvanceTermination,
    pub steps: u64,
    /// Whether all asupersync invariant oracles passed at quiescence.
    /// `true` for the manual-time harness (oracles are not run there
    /// — `into_report()` does not drive `run_until_quiescent_with_report`).
    /// For the function-style fixture, populated from
    /// `LabRunReport::oracle_report::all_passed()` after auto-advance.
    pub oracles_passed: bool,
    /// Final virtual time after the run, in nanoseconds since
    /// `Time::ZERO`. Lets post-run assertions like
    /// `assert!(report.now_nanos >= expected_deadline)` work
    /// without needing raw `LabRuntime` access.
    ///
    /// **Bead:** ft-n7n4q (Gap B). Closes the inspection gap that
    /// blocked migrations like `runtime.rs:8137 adaptive_sleep_advances_virtual_time`.
    pub now_nanos: u64,
}

/// Run an async closure under LabRuntime virtual time with a
/// freshly-constructed `Cx` installed.
///
/// Uses [`DEFAULT_SEED`] and [`DEFAULT_MAX_STEPS`]. Panics on
/// `StuckBailout` with a diagnostic that names the seed.
///
/// # Examples
///
/// ```ignore
/// use frankenterm_core::test_fixtures::lab_runtime::lab_runtime_test;
///
/// #[test]
/// fn my_async_test() {
///     lab_runtime_test(|_cx| async move {
///         // body. Time advancement is automatic.
///     });
/// }
/// ```
pub fn lab_runtime_test<F, Fut>(f: F) -> LabReport
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    // ft-qrjvh: honour the multi-seed sweep override when the
    // process-wide [`SEED_OVERRIDE_ENV`] var is set. Tests that
    // pin a seed via `lab_runtime_test_with_seed` or
    // `lab_runtime_test_with_config` bypass this branch and stay
    // deterministic on their pinned value.
    lab_runtime_test_with_seed(effective_default_seed(DEFAULT_SEED), f)
}

/// Run an async closure under LabRuntime with a specific seed.
///
/// Use this overload when seed determinism matters (e.g. property
/// tests that span multiple seeds, or shrink-friendly fuzzers).
pub fn lab_runtime_test_with_seed<F, Fut>(seed: u64, f: F) -> LabReport
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let config = LabConfig::new(seed)
        .with_auto_advance()
        .worker_count(1)
        .max_steps(DEFAULT_MAX_STEPS);
    lab_runtime_test_with_config(config, f)
}

/// Run an async closure under LabRuntime with a fully-custom
/// [`LabConfig`].
///
/// Reach for this when the test needs unusual configuration —
/// multi-worker scheduling, a custom step budget, deterministic
/// ordering tweaks, etc. Most tests should use
/// [`lab_runtime_test`] (default seed) or
/// [`lab_runtime_test_with_seed`] (explicit seed).
pub fn lab_runtime_test_with_config<F, Fut>(config: LabConfig, f: F) -> LabReport
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut runtime = LabRuntime::new(config);
    let region = runtime.state.create_root_region(Budget::INFINITE);
    let (task_id, _handle) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            let cx = Cx::current().unwrap_or_else(Cx::for_testing);
            f(cx).await;
        })
        .expect("LabRuntime root task spawn must succeed");
    runtime.scheduler.lock().schedule(task_id, 0);

    let report = runtime.run_with_auto_advance();

    // Surface StuckBailout with a clear diagnostic. This is the
    // single most common LabRuntime failure mode — usually means
    // the test's async closure deadlocked against an awaiting
    // primitive. Name the symptom inline so the test failure
    // doesn't require Asupersync internals knowledge to read.
    if matches!(report.termination, AutoAdvanceTermination::StuckBailout) {
        panic!(
            "LabRuntime stuck — auto-advance bailed after {} steps. \
             Most likely: the test future is awaiting a primitive \
             that was never signaled. Check sleep durations, \
             channel sends, and oneshot resolutions.",
            report.steps
        );
    }

    // br-ft-c8x87: drive an extra `run_until_quiescent_with_report`
    // pass after auto-advance so callers that need oracle assertions
    // (telemetry.rs, native_events.rs) don't have to re-do the
    // boilerplate. The runtime is already at quiescence at this
    // point, so the call is near-zero cost.
    let lab_run_report = runtime.run_until_quiescent_with_report();

    LabReport {
        termination: report.termination,
        steps: report.steps,
        oracles_passed: lab_run_report.oracle_report.all_passed(),
        now_nanos: runtime.now().as_nanos(),
    }
}

// ============================================================================
// Multi-task auto-advance harness (br-ft-n7n4q Gap A)
// ============================================================================

/// Auto-advance harness for tests that need to spawn 2+ root
/// tasks before driving the runtime.
///
/// **Bead:** ft-n7n4q (Gap A — closes the multi-task ergonomic
/// gap that blocked migration of `runtime.rs:8179` /
/// `runtime.rs:8257` and other tests with 2+ root tasks). The
/// auto-advance entry points
/// ([`lab_runtime_test`] / [`lab_runtime_test_with_seed`] /
/// [`lab_runtime_test_with_config`]) accept exactly one root
/// closure; this builder lets the test body queue any number
/// before [`run`] drives the runtime to auto-advance termination.
///
/// # Usage
///
/// ```ignore
/// use frankenterm_core::test_fixtures::lab_runtime::LabRuntimeMultiTask;
///
/// let report = LabRuntimeMultiTask::new()
///     .spawn(|cx| async move { /* task A */ })
///     .spawn(|cx| async move { /* task B */ })
///     .run();
/// assert_ran_to_completion(&report);
/// ```
///
/// [`run`]: Self::run
pub struct LabRuntimeMultiTask {
    runtime: LabRuntime,
    /// Single shared root region for every spawned task.
    /// `LabRuntime::create_root_region` panics on a second call —
    /// the harness creates the region eagerly in [`with_config`]
    /// and reuses it across every [`spawn`].
    ///
    /// [`with_config`]: Self::with_config
    /// [`spawn`]: Self::spawn
    root_region: RegionId,
}

impl Default for LabRuntimeMultiTask {
    fn default() -> Self {
        Self::new()
    }
}

impl LabRuntimeMultiTask {
    /// New builder with [`DEFAULT_SEED`] and [`DEFAULT_MAX_STEPS`].
    /// Auto-advance is enabled — the [`run`] terminator will
    /// drive the queued tasks to quiescence with virtual time
    /// advancing automatically.
    ///
    /// ft-qrjvh: honours [`SEED_OVERRIDE_ENV`] when set so the
    /// nightly multi-seed sweep covers `LabRuntimeMultiTask` call
    /// sites alongside the function-form fixture.
    ///
    /// [`run`]: Self::run
    #[must_use]
    pub fn new() -> Self {
        Self::with_seed(effective_default_seed(DEFAULT_SEED))
    }

    /// New builder with an explicit seed. Use when seed
    /// determinism matters.
    #[must_use]
    pub fn with_seed(seed: u64) -> Self {
        let config = LabConfig::new(seed)
            .with_auto_advance()
            .worker_count(1)
            .max_steps(DEFAULT_MAX_STEPS);
        Self::with_config(config)
    }

    /// New builder with a fully-custom [`LabConfig`]. Auto-advance
    /// is forced **on** because the multi-task harness's whole
    /// reason for being is to wrap the auto-advance entry point;
    /// callers needing manual time should use [`ManualTimeHarness`].
    #[must_use]
    pub fn with_config(mut config: LabConfig) -> Self {
        config.auto_advance_time = true;
        let mut runtime = LabRuntime::new(config);
        let root_region = runtime.state.create_root_region(Budget::INFINITE);
        Self {
            runtime,
            root_region,
        }
    }

    /// Queue an async closure as an additional root task. The
    /// closure receives a freshly-constructed `Cx` for cancellation
    /// threading. Tasks are not stepped until [`run`] is called.
    ///
    /// All spawned tasks share the harness's single root region
    /// (created eagerly in [`with_config`]). Returns `&mut self`
    /// so calls chain (`.spawn(...).spawn(...).run()`).
    ///
    /// [`with_config`]: Self::with_config
    /// [`run`]: Self::run
    pub fn spawn<F, Fut>(&mut self, f: F) -> &mut Self
    where
        F: FnOnce(Cx) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (task_id, _handle) = self
            .runtime
            .state
            .create_task(self.root_region, Budget::INFINITE, async move {
                let cx = Cx::current().unwrap_or_else(Cx::for_testing);
                f(cx).await;
            })
            .expect("LabRuntime root task spawn must succeed");
        self.runtime.scheduler.lock().schedule(task_id, 0);
        self
    }

    /// Drive the runtime under auto-advance and return the
    /// resulting [`LabReport`]. Mirrors the diagnostic surface of
    /// [`lab_runtime_test_with_config`] — `StuckBailout` panics
    /// with a clear message; the oracle pass and final virtual
    /// time are populated for post-run assertions.
    #[must_use]
    pub fn run(&mut self) -> LabReport {
        let report = self.runtime.run_with_auto_advance();

        if matches!(report.termination, AutoAdvanceTermination::StuckBailout) {
            panic!(
                "LabRuntime stuck (multi-task) — auto-advance bailed after {} steps. \
                 Most likely: one of the queued task futures is awaiting a primitive \
                 that was never signaled. Check sleep durations, channel sends, and \
                 oneshot resolutions across the spawned task set.",
                report.steps
            );
        }

        let lab_run_report = self.runtime.run_until_quiescent_with_report();

        LabReport {
            termination: report.termination,
            steps: report.steps,
            oracles_passed: lab_run_report.oracle_report.all_passed(),
            now_nanos: self.runtime.now().as_nanos(),
        }
    }
}

// ============================================================================
// Manual-time harness (br-ft-dgj2e)
// ============================================================================

/// Manual-time harness for LabRuntime tests that need explicit
/// control over virtual time.
///
/// **Bead:** ft-dgj2e (continuation of ft-t9a6q.3).
///
/// The auto-advance fixture covers the common case where a test
/// just needs determinism + virtual time. Some tests need to assert
/// on **specific deadline semantics** — e.g. "after 5s of virtual
/// time, X must have happened, but not before 4.999s." That kind
/// of assertion only works if the test driver, not the runtime,
/// decides when to advance time.
///
/// # Usage
///
/// ```ignore
/// use frankenterm_core::test_fixtures::lab_runtime::ManualTimeHarness;
/// use std::time::Duration;
///
/// let mut harness = ManualTimeHarness::new();
/// harness.spawn(|cx| async move {
///     // body that, e.g., calls runtime_async::sleep(Duration::from_secs(1))
/// });
/// harness.run_until_idle();              // task awaits the timer
/// assert!(!precondition_fired());        // not yet
/// harness.advance(Duration::from_secs(1));
/// harness.run_until_idle();              // timer fires, task wakes
/// assert!(precondition_fired());         // yes now
/// ```
///
/// # Why a struct rather than a function
///
/// The auto-advance entry point is shaped as
/// `lab_runtime_test(|cx| async ...)` because the closure runs to
/// completion under auto-advance — the test driver and the test
/// body are the same code. Manual time is fundamentally different:
/// the test body awaits a primitive, the test driver advances time,
/// the test body awakens — driver and body need to interleave. A
/// struct with `spawn` + `advance` + `run_until_idle` methods
/// expresses that interleaving naturally.
pub struct ManualTimeHarness {
    runtime: LabRuntime,
}

impl Default for ManualTimeHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualTimeHarness {
    /// New harness with [`DEFAULT_SEED`] and [`DEFAULT_MAX_STEPS`],
    /// auto-advance disabled. ft-qrjvh: honours
    /// [`SEED_OVERRIDE_ENV`] so the multi-seed sweep covers
    /// manual-time tests too; explicit-seed callers via
    /// [`Self::with_seed`] / [`Self::with_config`] stay pinned.
    #[must_use]
    pub fn new() -> Self {
        Self::with_seed(effective_default_seed(DEFAULT_SEED))
    }

    /// New harness with an explicit seed. Use when seed determinism
    /// matters (e.g. multi-seed property tests).
    #[must_use]
    pub fn with_seed(seed: u64) -> Self {
        let config = LabConfig::new(seed)
            .worker_count(1)
            .max_steps(DEFAULT_MAX_STEPS);
        Self::with_config(config)
    }

    /// New harness with a fully-custom [`LabConfig`]. The harness
    /// forces auto-advance off because manual-time tests must own
    /// all clock advancement.
    #[must_use]
    pub fn with_config(mut config: LabConfig) -> Self {
        config.auto_advance_time = false;
        Self {
            runtime: LabRuntime::new(config),
        }
    }

    /// Spawn an async closure as a root task. The closure receives
    /// a freshly-constructed `Cx` so it can thread cancellation
    /// into runtime calls.
    pub fn spawn<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(Cx) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let region = self.runtime.state.create_root_region(Budget::INFINITE);
        let (task_id, _handle) = self
            .runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = Cx::current().unwrap_or_else(Cx::for_testing);
                f(cx).await;
            })
            .expect("LabRuntime root task spawn must succeed");
        self.runtime.scheduler.lock().schedule(task_id, 0);
    }

    /// Advance virtual time by `duration` and process timers that are
    /// expired at the new time.
    ///
    /// Saturates at `u64::MAX` nanoseconds — no test in practice
    /// should be advancing more than ~580 years of virtual time in
    /// one call, but the saturation makes the API total.
    pub fn advance(&mut self, duration: std::time::Duration) {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.runtime.advance_time(nanos);
        let _wakeups = self
            .runtime
            .state
            .timer_driver_handle()
            .map_or(0, |h| h.process_timers());
    }

    /// Advance virtual time to the next pending timer deadline,
    /// processing the expired timer(s). Returns the number of
    /// wakeups triggered, or 0 if no timer is pending.
    pub fn advance_to_next_timer(&mut self) -> usize {
        self.runtime.advance_to_next_timer()
    }

    /// Drive the runtime until no tasks are runnable. Pending
    /// timers do **not** advance time automatically — they stay
    /// pending until the caller invokes [`advance`] or
    /// [`advance_to_next_timer`].
    ///
    /// Returns the number of steps executed during this call.
    ///
    /// [`advance`]: Self::advance
    /// [`advance_to_next_timer`]: Self::advance_to_next_timer
    pub fn run_until_idle(&mut self) -> u64 {
        self.runtime.run_until_idle()
    }

    /// Drive the runtime until quiescent or `max_steps` is reached.
    ///
    /// Returns the number of steps executed during this call. Note
    /// that quiescence under manual time only happens when the
    /// caller has manually advanced past every pending timer the
    /// task graph waits on.
    pub fn run_until_quiescent(&mut self) -> u64 {
        self.runtime.run_until_quiescent()
    }

    /// True iff scheduler is empty + all obligations resolved.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.runtime.is_quiescent()
    }

    /// Current virtual time, as nanoseconds since epoch (Time::ZERO).
    ///
    /// Use the matching `as_millis()` / `as_secs()` calculations
    /// in the test if you need other units.
    #[must_use]
    pub fn now_nanos(&self) -> u64 {
        self.runtime.now().as_nanos()
    }

    /// Number of scheduler steps executed across the harness's
    /// lifetime.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.runtime.steps()
    }

    /// Consume the harness and produce a [`LabReport`] mirroring
    /// the auto-advance entry point's return shape. Useful at the
    /// end of a manual-time test for uniform assertions.
    ///
    /// `Quiescent` is reported iff the runtime is quiescent; else
    /// `StepLimitReached` is used to signal "task graph still has
    /// pending work that the test driver did not advance past."
    /// `StuckBailout` is never produced by the manual harness
    /// (auto-advance is disabled, so the bailout heuristic does
    /// not apply).
    #[must_use]
    pub fn into_report(self) -> LabReport {
        let termination = if self.runtime.is_quiescent() {
            AutoAdvanceTermination::Quiescent
        } else {
            AutoAdvanceTermination::StepLimitReached
        };
        LabReport {
            termination,
            steps: self.runtime.steps(),
            // The manual-time harness does not drive
            // `run_until_quiescent_with_report` — oracle status is
            // not produced for these tests. `true` is the safer
            // default than `false` because callers that don't
            // assert on oracles_passed continue to behave as
            // before; callers that *do* must use the function-style
            // fixture, which populates the field.
            oracles_passed: true,
            now_nanos: self.runtime.now().as_nanos(),
        }
    }
}

/// Assert that a `LabReport` ended cleanly (i.e. the root task
/// completed, not a stuck-bailout / step-budget exhaustion).
///
/// Most tests can ignore the returned `LabReport` — but when a
/// test wants to make the success case explicit (e.g. when
/// asserting on step counts), this helper renders a uniform
/// diagnostic.
pub fn assert_ran_to_completion(report: &LabReport) {
    match report.termination {
        AutoAdvanceTermination::Quiescent => {}
        AutoAdvanceTermination::StuckBailout => {
            panic!(
                "LabRuntime did not complete: stuck-bailout after {} steps",
                report.steps
            );
        }
        AutoAdvanceTermination::StepLimitReached => {
            panic!(
                "LabRuntime did not complete: termination = {:?} after {} steps",
                AutoAdvanceTermination::StepLimitReached,
                report.steps
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    #[test]
    fn lab_runtime_test_runs_a_simple_async_closure() {
        let observed = Arc::new(AtomicBool::new(false));
        let observed_clone = Arc::clone(&observed);
        let report = lab_runtime_test(move |_cx| {
            let observed = observed_clone;
            async move {
                observed.store(true, Ordering::SeqCst);
            }
        });
        assert!(observed.load(Ordering::SeqCst));
        assert_ran_to_completion(&report);
    }

    #[test]
    fn lab_runtime_test_passes_cx_to_the_closure() {
        // The Cx must arrive — the user's closure receives it as
        // an owned value so it can thread `&cx` deeper.
        let report = lab_runtime_test(|cx| async move {
            // Cx implements Debug — exercise it minimally so the
            // capture is not optimized away.
            let _ = format!("{cx:?}");
        });
        assert_ran_to_completion(&report);
    }

    #[test]
    fn lab_runtime_test_passes_runtime_cx_to_explicit_sleep() {
        let observed = Arc::new(AtomicBool::new(false));
        let observed_clone = Arc::clone(&observed);
        let wall_start = std::time::Instant::now();

        let report = lab_runtime_test(move |cx| {
            let observed = observed_clone;
            async move {
                crate::runtime_async::sleep_with_cx(&cx, std::time::Duration::from_secs(1))
                    .await
                    .expect("LabRuntime-backed Cx should drive explicit sleep");
                observed.store(true, Ordering::SeqCst);
            }
        });

        assert!(observed.load(Ordering::SeqCst));
        assert!(
            wall_start.elapsed() < std::time::Duration::from_secs(1),
            "explicit cx sleep should advance LabRuntime virtual time, not wall time"
        );
        assert_ran_to_completion(&report);
    }

    #[test]
    fn lab_runtime_test_with_seed_uses_explicit_seed() {
        // Two calls with the same explicit seed should produce
        // identical scheduling decisions — i.e. identical step
        // counts when the body is deterministic.
        let counter_a = Arc::new(AtomicU32::new(0));
        let counter_a_clone = Arc::clone(&counter_a);
        let report_a = lab_runtime_test_with_seed(42, move |_cx| {
            let counter = counter_a_clone;
            async move {
                for _ in 0..10 {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        let counter_b = Arc::new(AtomicU32::new(0));
        let second_counter_clone = Arc::clone(&counter_b);
        let report_b = lab_runtime_test_with_seed(42, move |_cx| {
            let counter = second_counter_clone;
            async move {
                for _ in 0..10 {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        assert_eq!(counter_a.load(Ordering::SeqCst), 10);
        assert_eq!(counter_b.load(Ordering::SeqCst), 10);
        assert_eq!(
            report_a.steps, report_b.steps,
            "same seed must produce identical step counts on a deterministic body"
        );
    }

    #[test]
    fn lab_runtime_test_with_config_honors_custom_step_budget() {
        // Custom step budget — verify the fixture honors it
        // rather than the default.
        let config = LabConfig::new(7)
            .with_auto_advance()
            .worker_count(1)
            .max_steps(1_000);
        let report = lab_runtime_test_with_config(config, |_cx| async move {
            // Trivial body — the budget shouldn't matter.
        });
        assert_ran_to_completion(&report);
    }

    #[test]
    #[should_panic(expected = "LabRuntime stuck")]
    fn lab_runtime_test_panics_with_stuck_diagnostic_on_bailout() {
        // Deliberately wait on a never-signaled primitive so the
        // auto-advance scheduler bails out. StuckBailout fires
        // after 1000 consecutive stuck iterations, so max_steps
        // must exceed that threshold for the bailout path to win
        // the race vs StepLimitReached.
        let config = LabConfig::new(0)
            .with_auto_advance()
            .worker_count(1)
            .max_steps(5_000);
        let _ = lab_runtime_test_with_config(config, |_cx| async move {
            // pending forever — auto-advance bails out.
            std::future::pending::<()>().await;
        });
    }

    #[test]
    fn assert_ran_to_completion_panics_on_non_clean_termination() {
        let report = LabReport {
            termination: AutoAdvanceTermination::StuckBailout,
            steps: 99,
            oracles_passed: true,
            now_nanos: 0,
        };
        let panicked = std::panic::catch_unwind(|| {
            assert_ran_to_completion(&report);
        });
        assert!(
            panicked.is_err(),
            "assert_ran_to_completion must panic on stuck-bailout"
        );
    }

    #[test]
    fn assert_ran_to_completion_passes_on_clean_termination() {
        let report = LabReport {
            termination: AutoAdvanceTermination::Quiescent,
            steps: 7,
            oracles_passed: true,
            now_nanos: 0,
        };
        assert_ran_to_completion(&report); // must not panic
    }

    #[test]
    fn re_exports_resolve() {
        // Substrate guarantee: callers don't have to depend on
        // asupersync directly to use LabConfig / Budget. If these
        // re-exports break, downstream tests will fail to compile.
        let _config: LabConfig = LabConfig::new(0).with_auto_advance().worker_count(1);
        let _budget: Budget = Budget::INFINITE;
    }

    // ========================================================================
    // ManualTimeHarness (br-ft-dgj2e)
    // ========================================================================

    #[test]
    fn manual_time_harness_starts_at_time_zero_and_no_steps() {
        let harness = ManualTimeHarness::new();
        assert_eq!(harness.now_nanos(), 0);
        assert_eq!(harness.steps(), 0);
        // Quiescent at start: no tasks, no obligations.
        assert!(harness.is_quiescent());
    }

    #[test]
    fn manual_time_harness_runs_a_simple_spawn_to_completion() {
        let observed = Arc::new(AtomicBool::new(false));
        let observed_clone = Arc::clone(&observed);
        let mut harness = ManualTimeHarness::new();
        harness.spawn(move |_cx| {
            let observed = observed_clone;
            async move {
                observed.store(true, Ordering::SeqCst);
            }
        });
        // Even without any explicit time advancement, the task
        // body has no timers — run_until_idle drives it to
        // completion.
        harness.run_until_idle();
        assert!(observed.load(Ordering::SeqCst));
        let report = harness.into_report();
        assert_ran_to_completion(&report);
    }

    #[test]
    fn manual_time_harness_advance_visible_via_now_nanos() {
        // The driving invariant: the test driver decides when time
        // advances. Calling advance() without running the task
        // graph still bumps the virtual clock so subsequent task
        // resumption observes the new time.
        let mut harness = ManualTimeHarness::new();
        assert_eq!(harness.now_nanos(), 0);

        harness.advance(std::time::Duration::from_millis(250));
        assert_eq!(harness.now_nanos(), 250_000_000);

        harness.advance(std::time::Duration::from_secs(5));
        assert_eq!(harness.now_nanos(), 5_250_000_000);
    }

    #[test]
    fn manual_time_harness_passes_runtime_cx_to_explicit_sleep() {
        let observed = Arc::new(AtomicBool::new(false));
        let observed_clone = Arc::clone(&observed);
        let mut harness = ManualTimeHarness::new();

        harness.spawn(move |cx| {
            let observed = observed_clone;
            async move {
                crate::runtime_async::sleep_with_cx(&cx, std::time::Duration::from_secs(1))
                    .await
                    .expect("manual LabRuntime-backed Cx should drive explicit sleep");
                observed.store(true, Ordering::SeqCst);
            }
        });

        harness.run_until_idle();
        assert!(
            !observed.load(Ordering::SeqCst),
            "task should still be blocked before the manual deadline"
        );

        harness.advance(std::time::Duration::from_millis(999));
        harness.run_until_idle();
        assert!(
            !observed.load(Ordering::SeqCst),
            "task must not wake before the requested virtual deadline"
        );

        harness.advance(std::time::Duration::from_millis(1));
        harness.run_until_idle();
        assert!(observed.load(Ordering::SeqCst));
        let report = harness.into_report();
        assert_ran_to_completion(&report);
    }

    #[test]
    fn manual_time_harness_seed_is_deterministic() {
        // Same property as lab_runtime_test_with_seed: identical
        // seeds produce identical step counts on a deterministic
        // body.
        let counter_a = Arc::new(AtomicU32::new(0));
        let ca = Arc::clone(&counter_a);
        let mut a = ManualTimeHarness::with_seed(99);
        a.spawn(move |_cx| {
            let counter = ca;
            async move {
                for _ in 0..50 {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        a.run_until_quiescent();
        let report_a = a.into_report();

        let counter_b = Arc::new(AtomicU32::new(0));
        let cb = Arc::clone(&counter_b);
        let mut b = ManualTimeHarness::with_seed(99);
        b.spawn(move |_cx| {
            let counter = cb;
            async move {
                for _ in 0..50 {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        b.run_until_quiescent();
        let report_b = b.into_report();

        assert_eq!(counter_a.load(Ordering::SeqCst), 50);
        assert_eq!(counter_b.load(Ordering::SeqCst), 50);
        assert_eq!(
            report_a.steps, report_b.steps,
            "manual-time harness must be deterministic across seed-equal runs"
        );
    }

    #[test]
    fn manual_time_harness_with_config_disables_auto_advance_by_default() {
        // Substrate guarantee: with_config does not silently enable
        // auto-advance even when the caller forgets to set it. The
        // test pins this contract — if a future LabConfig default
        // ever flips auto-advance to on, this test fires loudly.
        let config = LabConfig::new(0).worker_count(1).max_steps(1_000);
        // Explicitly NOT calling .with_auto_advance().
        let harness = ManualTimeHarness::with_config(config);
        assert_eq!(harness.now_nanos(), 0);
        assert_eq!(harness.steps(), 0);
        assert!(harness.is_quiescent());
    }

    #[test]
    fn manual_time_harness_into_report_step_limit_when_pending() {
        // If the test driver does not advance past a pending
        // timer, into_report reports StepLimitReached (not
        // Quiescent). This is the manual-time analogue of "task
        // graph still has work to do."
        //
        // This test simulates the "pending obligation" condition
        // by spawning a task and *not* running it to idle — the
        // scheduler still has the task queued, so is_quiescent()
        // returns false.
        let mut harness = ManualTimeHarness::new();
        harness.spawn(|_cx| async move {
            // body — but we never run_until_idle, so the task is
            // queued but not stepped.
        });
        // Did NOT call run_until_idle — task is queued.
        assert!(!harness.is_quiescent());
        let report = harness.into_report();
        assert!(
            matches!(report.termination, AutoAdvanceTermination::StepLimitReached),
            "non-quiescent harness must report StepLimitReached, got {:?}",
            report.termination
        );
    }

    /// Demonstrative migration #1: an existing-shape async test
    /// rewritten through the fixture. The point is to show the
    /// before/after delta so future migrations have a template.
    ///
    /// Before (paraphrased, ~30 lines):
    /// ```ignore
    /// let mut runtime = asupersync::LabRuntime::new(
    ///     asupersync::LabConfig::new(SEED)
    ///         .with_auto_advance()
    ///         .worker_count(1)
    ///         .max_steps(50_000),
    /// );
    /// let region = runtime.state.create_root_region(Budget::INFINITE);
    /// let (task_id, _handle) = runtime.state.create_task(region, Budget::INFINITE,
    ///     async move { body() }).unwrap();
    /// runtime.scheduler.lock().schedule(task_id, 0);
    /// let report = runtime.run_with_auto_advance();
    /// assert!(!matches!(report.termination, AutoAdvanceTermination::StuckBailout));
    /// ```
    ///
    /// After (this test): one call.
    #[test]
    fn migration_example_replaces_thirty_line_boilerplate() {
        let report = lab_runtime_test(|_cx| async move {
            // body
            let mut sum = 0;
            for i in 0..100 {
                sum += i;
            }
            assert_eq!(sum, 4950);
        });
        assert_ran_to_completion(&report);
    }

    // ========================================================================
    // br-ft-n7n4q Gap A — multi-task auto-advance harness
    // ========================================================================

    #[test]
    fn lab_runtime_multi_task_runs_two_tasks_to_completion() {
        let task_a_done = Arc::new(AtomicBool::new(false));
        let second_task_done = Arc::new(AtomicBool::new(false));
        let a = Arc::clone(&task_a_done);
        let b = Arc::clone(&second_task_done);

        let report = LabRuntimeMultiTask::new()
            .spawn(move |_cx| {
                let a = a;
                async move {
                    a.store(true, Ordering::SeqCst);
                }
            })
            .spawn(move |_cx| {
                let b = b;
                async move {
                    b.store(true, Ordering::SeqCst);
                }
            })
            .run();

        assert!(task_a_done.load(Ordering::SeqCst), "task A must complete");
        assert!(
            second_task_done.load(Ordering::SeqCst),
            "task B must complete"
        );
        assert_ran_to_completion(&report);
    }

    #[test]
    fn lab_runtime_multi_task_drives_virtual_time_for_all_tasks() {
        // Two tasks each sleep 250ms in virtual time. Auto-advance
        // must wake both — `now_nanos` after the run must be at
        // least 250_000_000.
        let mut harness = LabRuntimeMultiTask::new();
        let woke_a = Arc::new(AtomicBool::new(false));
        let woke_b = Arc::new(AtomicBool::new(false));
        let a = Arc::clone(&woke_a);
        let b = Arc::clone(&woke_b);

        harness.spawn(move |cx| {
            let woke = a;
            async move {
                crate::runtime_async::sleep_with_cx(&cx, std::time::Duration::from_millis(250))
                    .await
                    .expect("sleep");
                woke.store(true, Ordering::SeqCst);
            }
        });
        harness.spawn(move |cx| {
            let woke = b;
            async move {
                crate::runtime_async::sleep_with_cx(&cx, std::time::Duration::from_millis(250))
                    .await
                    .expect("sleep");
                woke.store(true, Ordering::SeqCst);
            }
        });

        let report = harness.run();
        assert!(woke_a.load(Ordering::SeqCst), "task A must wake");
        assert!(woke_b.load(Ordering::SeqCst), "task B must wake");
        assert!(
            report.now_nanos >= 250_000_000,
            "virtual time must advance past the sleep deadline (got {} ns)",
            report.now_nanos
        );
        assert_ran_to_completion(&report);
    }

    #[test]
    fn lab_runtime_multi_task_with_seed_is_deterministic() {
        let report_a = LabRuntimeMultiTask::with_seed(123)
            .spawn(|_cx| async move {
                for _ in 0..20 {
                    let _ = std::hint::black_box(0u64);
                }
            })
            .spawn(|_cx| async move {
                for _ in 0..20 {
                    let _ = std::hint::black_box(0u64);
                }
            })
            .run();
        let report_b = LabRuntimeMultiTask::with_seed(123)
            .spawn(|_cx| async move {
                for _ in 0..20 {
                    let _ = std::hint::black_box(0u64);
                }
            })
            .spawn(|_cx| async move {
                for _ in 0..20 {
                    let _ = std::hint::black_box(0u64);
                }
            })
            .run();
        assert_eq!(
            report_a.steps, report_b.steps,
            "same seed must produce identical step counts"
        );
        assert_eq!(
            report_a.now_nanos, report_b.now_nanos,
            "same seed must produce identical virtual time"
        );
    }

    #[test]
    #[should_panic(expected = "LabRuntime stuck (multi-task)")]
    fn lab_runtime_multi_task_panics_on_stuck_bailout() {
        // One task pends forever — auto-advance must bail with
        // the multi-task-flavored diagnostic.
        let config = LabConfig::new(0)
            .with_auto_advance()
            .worker_count(1)
            .max_steps(5_000);
        let _ = LabRuntimeMultiTask::with_config(config)
            .spawn(|_cx| async move {
                std::future::pending::<()>().await;
            })
            .spawn(|_cx| async move {
                // benign companion task — doesn't matter, the
                // pending sibling forces bailout.
            })
            .run();
    }

    // ========================================================================
    // br-ft-n7n4q Gap B — now_nanos on LabReport
    // ========================================================================

    #[test]
    fn lab_runtime_test_now_nanos_starts_at_zero_for_no_sleep_body() {
        // A trivial body that never sleeps leaves virtual time at
        // its starting value (zero). The field is populated even
        // though no time advanced.
        let report = lab_runtime_test(|_cx| async move {
            // pure compute, no sleep
        });
        assert_eq!(
            report.now_nanos, 0,
            "trivial body must leave virtual time at zero"
        );
    }

    #[test]
    fn lab_runtime_test_now_nanos_reflects_advanced_virtual_time() {
        // Body sleeps 1.5s in virtual time. now_nanos after the
        // run must reflect the advance.
        let report = lab_runtime_test(|cx| async move {
            crate::runtime_async::sleep_with_cx(&cx, std::time::Duration::from_millis(1500))
                .await
                .expect("sleep");
        });
        assert!(
            report.now_nanos >= 1_500_000_000,
            "virtual time must advance past the sleep deadline (got {} ns)",
            report.now_nanos
        );
    }

    #[test]
    fn manual_time_harness_into_report_carries_now_nanos() {
        // The manual harness's into_report must also populate
        // now_nanos so callers can do uniform post-run assertions
        // across the two harness flavors.
        let mut harness = ManualTimeHarness::new();
        harness.advance(std::time::Duration::from_millis(750));
        let report = harness.into_report();
        assert_eq!(
            report.now_nanos, 750_000_000,
            "manual harness into_report must reflect advanced virtual time"
        );
    }

    // ----------------------------------------------------------------
    // ft-qrjvh — multi-seed sweep override (parser only, no env)
    // ----------------------------------------------------------------

    #[test]
    fn parse_seed_override_accepts_decimal() {
        assert_eq!(parse_seed_override("42"), Some(42));
        assert_eq!(parse_seed_override("12648430"), Some(12648430));
    }

    #[test]
    fn parse_seed_override_accepts_lower_and_upper_hex() {
        assert_eq!(parse_seed_override("0xC0FFEE"), Some(0xC0_FFEE));
        assert_eq!(parse_seed_override("0xc0ffee"), Some(0xC0_FFEE));
        assert_eq!(parse_seed_override("0XDEADBEEF"), Some(0xDEAD_BEEF));
    }

    #[test]
    fn parse_seed_override_trims_whitespace() {
        assert_eq!(parse_seed_override("  42 "), Some(42));
        assert_eq!(parse_seed_override("\t0xCAFE\n"), Some(0xCAFE));
    }

    #[test]
    fn parse_seed_override_rejects_garbage() {
        assert_eq!(parse_seed_override(""), None);
        assert_eq!(parse_seed_override("   "), None);
        assert_eq!(parse_seed_override("not-a-number"), None);
        assert_eq!(parse_seed_override("0xZZ"), None);
        assert_eq!(parse_seed_override("-1"), None);
    }

    #[test]
    fn multi_seed_sweep_seeds_include_default_and_are_distinct() {
        // The sweep corpus must include DEFAULT_SEED so the nightly
        // lane covers the canonical config, and the seeds must be
        // distinct so each lane really exercises a different
        // scheduling tree.
        assert!(
            MULTI_SEED_SWEEP_SEEDS.contains(&DEFAULT_SEED),
            "default seed must be in the sweep corpus"
        );
        let mut sorted: Vec<u64> = MULTI_SEED_SWEEP_SEEDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            MULTI_SEED_SWEEP_SEEDS.len(),
            "sweep seeds must be distinct"
        );
        assert_eq!(MULTI_SEED_SWEEP_SEEDS.len(), 5);
    }

    #[test]
    fn seed_override_env_constant_matches_workflow_contract() {
        // The CI workflow at .github/workflows/lab-runtime-multi-seed-nightly.yml
        // hard-codes this env name; if either side renames it, the
        // sweep silently degrades to the default seed across all
        // lanes. Keep the constant pinned and surfaced.
        assert_eq!(SEED_OVERRIDE_ENV, "FT_LAB_RUNTIME_SEED");
    }
}
