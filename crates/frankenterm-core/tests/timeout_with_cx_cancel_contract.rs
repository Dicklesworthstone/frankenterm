// =============================================================================
// Contract tests for `runtime_async::timeout_with_cx` cancel/timeout semantics.
//
// `timeout_with_cx` is a public Cx-first timeout seam (ft-xbnl0.2.2) whose
// documented cancellation contract (ft-xbnl0.2.4 tick 328) was previously
// unguarded by any test:
//
//   - it bounds the wait by the cx BUDGET deadline (via budget_timeout), but
//   - it does NOT itself check `cx.is_cancel_requested()`, so a pre-cancelled
//     cx with an infinite budget waits the full requested `duration` before
//     returning `Err` — short-circuit on cancel is the caller's job
//     (`cx.checkpoint()?` before the call).
//
// These tests pin both the ordinary timeout behavior and that subtle
// cancel-does-not-short-circuit contract so it cannot silently regress.
// =============================================================================

#![cfg(feature = "asupersync-runtime")]

use asupersync::CancelKind;
use frankenterm_core::cx::{CxRuntimeBuilder, RuntimeTuning, for_testing};
use frankenterm_core::runtime_async;
use std::time::Duration;

fn current_thread_runtime() -> frankenterm_core::cx::Runtime {
    CxRuntimeBuilder::current_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 1,
            poll_budget: 64,
            blocking_min_threads: 0,
            blocking_max_threads: 0,
        })
        .build()
        .expect("build current-thread runtime")
}

/// A future that resolves before the deadline yields its value as `Ok`.
#[test]
fn timeout_with_cx_returns_inner_value_before_deadline() {
    let runtime = current_thread_runtime();
    let cx = for_testing();
    let result = runtime.block_on(runtime_async::timeout_with_cx(
        &cx,
        Duration::from_millis(500),
        async move { 42usize },
    ));
    assert_eq!(result.expect("should resolve before deadline"), 42);
}

/// A future that outlives the deadline returns `Err` (the timeout fires).
#[test]
fn timeout_with_cx_errors_when_future_exceeds_deadline() {
    let runtime = current_thread_runtime();
    let cx = for_testing();
    let result: Result<usize, String> = runtime.block_on(runtime_async::timeout_with_cx(
        &cx,
        Duration::from_millis(5),
        async move {
            runtime_async::sleep(Duration::from_millis(200)).await;
            7usize
        },
    ));
    assert!(result.is_err(), "future exceeding the deadline must time out");
}

/// Documented contract (ft-xbnl0.2.4): `timeout_with_cx` does NOT check
/// `cx.is_cancel_requested()`, so a future that completes promptly still
/// yields `Ok(value)` even when the cx was cancelled before the call. If this
/// ever returns `Err`, the function started short-circuiting on cancel and the
/// documented "caller must `checkpoint()?` first" contract changed.
#[test]
fn timeout_with_cx_does_not_short_circuit_on_precancelled_cx() {
    let runtime = current_thread_runtime();
    let cx = for_testing();
    cx.cancel_with(CancelKind::User, Some("p2 timeout_with_cx contract"));

    let result = runtime.block_on(runtime_async::timeout_with_cx(
        &cx,
        Duration::from_millis(500),
        async move { 99usize },
    ));
    assert_eq!(
        result.expect("cancel must not pre-empt a future that completes in time"),
        99,
        "timeout_with_cx must not short-circuit on a cancelled cx"
    );
}

/// A pre-cancelled cx must still let the timeout fire (it must not hang waiting
/// for a cancel signal it never observes). A long-running future under a short
/// deadline returns `Err` promptly via the budget/timeout path.
#[test]
fn timeout_with_cx_precancel_still_times_out_rather_than_hangs() {
    let runtime = current_thread_runtime();
    let cx = for_testing();
    cx.cancel_with(CancelKind::User, Some("p2 timeout_with_cx no-hang"));

    let result: Result<usize, String> = runtime.block_on(runtime_async::timeout_with_cx(
        &cx,
        Duration::from_millis(20),
        async move {
            runtime_async::sleep(Duration::from_secs(3600)).await;
            123usize
        },
    ));
    assert!(
        result.is_err(),
        "a long future under a short deadline must time out, not hang, on a cancelled cx"
    );
}

/// `sleep_with_cx` resolves `Ok(())` for a short sleep on a live cx — a basic
/// happy-path guard for the Cx-aware sleep seam used throughout the crate.
#[test]
fn sleep_with_cx_completes_on_live_cx() {
    let runtime = current_thread_runtime();
    let cx = for_testing();
    let result = runtime.block_on(runtime_async::sleep_with_cx(&cx, Duration::from_millis(5)));
    assert!(result.is_ok(), "a short sleep on a live cx must complete Ok");
}

// =============================================================================
// Metamorphic relations
// =============================================================================

/// Timeout monotonicity: a future that completes in ~30ms times out under a
/// 5ms deadline but succeeds under a 500ms deadline. Enlarging the deadline
/// flips `Err -> Ok` and never the reverse — the success region is monotone in
/// the timeout. (Wide 5/30/500 margins keep this off the real-clock edge.)
#[test]
fn timeout_with_cx_success_region_monotone_in_deadline() {
    let runtime = current_thread_runtime();

    let tight: Result<usize, String> = runtime.block_on(runtime_async::timeout_with_cx(
        &for_testing(),
        Duration::from_millis(5),
        async move {
            runtime_async::sleep(Duration::from_millis(30)).await;
            1usize
        },
    ));
    assert!(tight.is_err(), "30ms future under a 5ms deadline must time out");

    let generous: Result<usize, String> = runtime.block_on(runtime_async::timeout_with_cx(
        &for_testing(),
        Duration::from_millis(500),
        async move {
            runtime_async::sleep(Duration::from_millis(30)).await;
            1usize
        },
    ));
    assert_eq!(
        generous.expect("30ms future under a 500ms deadline must succeed"),
        1,
        "enlarging the deadline must not flip Ok back to Err"
    );
}

/// Value invariance: when a future resolves before the deadline, the returned
/// value does not depend on the (sufficiently large) timeout duration.
#[test]
fn timeout_with_cx_value_invariant_across_sufficient_deadlines() {
    let runtime = current_thread_runtime();
    for ms in [50u64, 200, 500, 2000] {
        let out = runtime.block_on(runtime_async::timeout_with_cx(
            &for_testing(),
            Duration::from_millis(ms),
            async move { 7usize },
        ));
        assert_eq!(
            out.expect("fast future resolves under every sufficient deadline"),
            7,
            "the resolved value must be invariant across deadlines (ms={ms})"
        );
    }
}

/// Cancel-before-poll ≡ cancel-mid-flight observable equivalence at a Cx
/// checkpoint: a checkpoint observes cancellation identically whether the cx
/// was cancelled before the future was ever polled or while it was running.
/// Both observations are `Err`; a checkpoint reached before any cancel is `Ok`.
#[test]
fn checkpoint_observes_cancel_equivalently_before_poll_and_mid_flight() {
    let runtime = current_thread_runtime();

    // Cancel-before-poll: cx cancelled prior to entering the future.
    let before = for_testing();
    before.cancel_with(CancelKind::User, Some("p2 cancel-before-poll"));
    let observed_before = runtime.block_on(async move { before.checkpoint().is_err() });
    assert!(observed_before, "a pre-cancelled cx must fail its first checkpoint");

    // Cancel-mid-flight: first checkpoint passes, then cancel, then the next
    // checkpoint observes it — the same Err outcome as cancel-before-poll.
    let during = for_testing();
    let (pre_cancel_ok, post_cancel_err) = runtime.block_on(async move {
        let pre = during.checkpoint().is_ok();
        during.cancel_with(CancelKind::User, Some("p2 cancel-mid-flight"));
        let post = during.checkpoint().is_err();
        (pre, post)
    });
    assert!(pre_cancel_ok, "a live cx must pass a checkpoint before cancellation");
    assert!(
        post_cancel_err,
        "a checkpoint after mid-flight cancel must observe it, like cancel-before-poll"
    );
}
