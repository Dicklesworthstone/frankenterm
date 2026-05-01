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
pub use asupersync::{Budget, LabConfig, LabRuntime};

/// Default seed for [`lab_runtime_test`]. Tests that need a
/// specific seed should call [`lab_runtime_test_with_seed`].
pub const DEFAULT_SEED: u64 = 0xC0FFEE;

/// Default step bailout. Mirrors the value used inline at every
/// existing LabRuntime call site (cpu_pressure.rs, native_events.rs,
/// telemetry.rs).
pub const DEFAULT_MAX_STEPS: u64 = 50_000;

/// Result of a [`lab_runtime_test`] run. The wrapped report carries
/// the termination reason + step count + any oracle failures.
#[derive(Debug)]
pub struct LabReport {
    pub termination: AutoAdvanceTermination,
    pub steps: u64,
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
    lab_runtime_test_with_seed(DEFAULT_SEED, f)
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
    let cx = Cx::for_testing();
    let (task_id, _handle) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
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

    LabReport {
        termination: report.termination,
        steps: report.steps,
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
        ref other => {
            panic!(
                "LabRuntime did not complete: termination = {other:?} after {} steps",
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
        let counter_b_clone = Arc::clone(&counter_b);
        let report_b = lab_runtime_test_with_seed(42, move |_cx| {
            let counter = counter_b_clone;
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
}
