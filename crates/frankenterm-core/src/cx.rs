//! Asupersync `Cx` capability-context adapters for FrankenTerm.
//!
//! This module provides a narrow FrankenTerm-facing API for threading
//! `asupersync::Cx` through async call graphs during the migration away from
//! ambient runtime access.
//!
//! # Threading Pattern
//!
//! Functions that need runtime effects should accept `&Cx` explicitly and pass
//! it downward:
//!
//! ```ignore
//! use frankenterm_core::cx::{Cx, with_cx};
//!
//! fn parse_layer(cx: &Cx, input: &str) -> usize {
//!     with_cx(cx, |inner| execute_layer(inner, input))
//! }
//!
//! fn execute_layer(cx: &Cx, input: &str) -> usize {
//!     cx.checkpoint().expect("checkpoint");
//!     input.len()
//! }
//! ```
//!
//! This keeps capability flow explicit and makes cancellation/budget handling
//! visible at every layer.
//!
//! # Structured Concurrency Guidance
//!
//! Use the helpers in this module as the shared migration pattern for
//! task-oriented code:
//!
//! - [`spawn_with_cx`] for child work that must inherit an explicit `Cx`.
//! - [`spawn_bounded_with_cx`] for `JoinSet`-style fanout with an explicit
//!   concurrency cap and stable result ordering.
//! - [`spawn_with_timeout`] when a compatibility seam still needs an awaitable
//!   child-task timeout boundary.
//!
//! New code should avoid detached background work. Long-lived loops should be
//! attached to an application-owned scope, while legacy detached spawns remain
//! quarantined behind `runtime_async` until the owning module is migrated.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

pub use asupersync::runtime::{JoinHandle, Runtime, RuntimeConfig, RuntimeHandle, SpawnError};
pub use asupersync::{Budget, Cx, Scope};

/// Runtime presets used by FrankenTerm during dual-runtime migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePreset {
    /// Single-threaded execution (deterministic tests, narrow integration work).
    CurrentThread,
    /// Multi-threaded execution (production-like behavior).
    MultiThread,
}

/// Runtime tuning knobs for FrankenTerm's asupersync integration path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeTuning {
    /// Number of async worker threads.
    pub worker_threads: usize,
    /// Cooperative poll budget.
    pub poll_budget: u32,
    /// Minimum number of blocking pool threads.
    pub blocking_min_threads: usize,
    /// Maximum number of blocking pool threads.
    pub blocking_max_threads: usize,
}

impl Default for RuntimeTuning {
    fn default() -> Self {
        let defaults = RuntimeConfig::default();
        Self {
            worker_threads: defaults.worker_threads,
            poll_budget: defaults.poll_budget,
            blocking_min_threads: defaults.blocking.min_threads,
            blocking_max_threads: defaults.blocking.max_threads,
        }
    }
}

/// FrankenTerm wrapper around `asupersync::runtime::RuntimeBuilder`.
///
/// This provides an intentionally small, stable surface while the codebase
/// migrates to explicit capability-context threading.
pub struct CxRuntimeBuilder {
    inner: asupersync::runtime::RuntimeBuilder,
}

impl std::fmt::Debug for CxRuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CxRuntimeBuilder")
            .field("inner", &"<RuntimeBuilder>")
            .finish()
    }
}

impl CxRuntimeBuilder {
    /// Create a builder using the requested preset.
    #[must_use]
    pub fn from_preset(preset: RuntimePreset) -> Self {
        let inner = match preset {
            RuntimePreset::CurrentThread => asupersync::runtime::RuntimeBuilder::current_thread(),
            RuntimePreset::MultiThread => asupersync::runtime::RuntimeBuilder::multi_thread(),
        };
        Self { inner }
    }

    /// Single-threaded runtime preset.
    #[must_use]
    pub fn current_thread() -> Self {
        Self::from_preset(RuntimePreset::CurrentThread)
    }

    /// Multi-threaded runtime preset.
    #[must_use]
    pub fn multi_thread() -> Self {
        Self::from_preset(RuntimePreset::MultiThread)
    }

    /// Apply a complete tuning profile.
    #[must_use]
    pub fn with_tuning(self, tuning: RuntimeTuning) -> Self {
        self.worker_threads(tuning.worker_threads)
            .poll_budget(tuning.poll_budget)
            .blocking_threads(tuning.blocking_min_threads, tuning.blocking_max_threads)
    }

    /// Override worker thread count.
    #[must_use]
    pub fn worker_threads(mut self, workers: usize) -> Self {
        self.inner = self.inner.worker_threads(workers);
        self
    }

    /// Override cooperative poll budget.
    #[must_use]
    pub fn poll_budget(mut self, poll_budget: u32) -> Self {
        self.inner = self.inner.poll_budget(poll_budget);
        self
    }

    /// Override blocking thread pool sizing.
    #[must_use]
    pub fn blocking_threads(mut self, min_threads: usize, max_threads: usize) -> Self {
        self.inner = self.inner.blocking_threads(min_threads, max_threads);
        self
    }

    /// Build the configured runtime.
    #[allow(clippy::result_large_err)] // asupersync::Error is externally defined
    pub fn build(self) -> Result<Runtime, asupersync::Error> {
        self.inner.build()
    }
}

/// Construct a test-only capability context.
#[must_use]
pub fn for_testing() -> Cx {
    Cx::for_testing()
}

/// Construct a request-scoped capability context for production helper paths.
#[must_use]
pub fn for_request() -> Cx {
    Cx::for_request()
}

/// Execute a closure while explicitly threading the same `Cx`.
#[inline]
pub fn with_cx<T>(cx: &Cx, f: impl FnOnce(&Cx) -> T) -> T {
    f(cx)
}

/// Async version of [`with_cx`].
pub async fn with_cx_async<T, Fut>(cx: &Cx, f: impl FnOnce(&Cx) -> Fut) -> T
where
    Fut: Future<Output = T>,
{
    f(cx).await
}

/// Reconstruct the effective runtime capability mask visible through `cx`.
///
/// asupersync intentionally keeps the raw mask field private. Its public
/// snapshot plus monotone `CapMask::intersect` API is sufficient to rebuild
/// the same mask without forging authority: start from all capabilities and
/// intersect one type-level "all except X" mask for every denied bit.
pub(crate) fn effective_cap_mask(cx: &Cx) -> asupersync::cx::CapMask {
    use asupersync::cx::{CapMask, CapSet, CapSetRuntimeMask};

    let effective = cx.capabilities().effective;
    let mut mask = CapMask::all();
    if !effective.spawn {
        mask = mask.intersect(<CapSet<false, true, true, true, true> as CapSetRuntimeMask>::MASK);
    }
    if !effective.time {
        mask = mask.intersect(<CapSet<true, false, true, true, true> as CapSetRuntimeMask>::MASK);
    }
    if !effective.entropy {
        mask = mask.intersect(<CapSet<true, true, false, true, true> as CapSetRuntimeMask>::MASK);
    }
    if !effective.io {
        mask = mask.intersect(<CapSet<true, true, true, false, true> as CapSetRuntimeMask>::MASK);
    }
    if !effective.remote {
        mask = mask.intersect(<CapSet<true, true, true, true, false> as CapSetRuntimeMask>::MASK);
    }
    mask
}

#[cfg(test)]
pub(crate) fn effective_capability_bits(cx: &Cx) -> [bool; 5] {
    let effective = cx.capabilities().effective;
    [
        effective.spawn,
        effective.time,
        effective.entropy,
        effective.io,
        effective.remote,
    ]
}

/// Exhaustive bit-position cases for explicit-Cx spawn adapter regressions.
///
/// The five single-denial cases are intentionally separate: one mixed mask
/// cannot catch an implementation that swaps two capability positions with
/// equal boolean values. All-enabled and all-disabled pin the endpoints.
#[cfg(test)]
pub(crate) fn capability_mask_test_cases() -> [(asupersync::cx::CapMask, [bool; 5]); 7] {
    use asupersync::cx::{CapSet, CapSetRuntimeMask};

    [
        (
            <CapSet<false, true, true, true, true> as CapSetRuntimeMask>::MASK,
            [false, true, true, true, true],
        ),
        (
            <CapSet<true, false, true, true, true> as CapSetRuntimeMask>::MASK,
            [true, false, true, true, true],
        ),
        (
            <CapSet<true, true, false, true, true> as CapSetRuntimeMask>::MASK,
            [true, true, false, true, true],
        ),
        (
            <CapSet<true, true, true, false, true> as CapSetRuntimeMask>::MASK,
            [true, true, true, false, true],
        ),
        (
            <CapSet<true, true, true, true, false> as CapSetRuntimeMask>::MASK,
            [true, true, true, true, false],
        ),
        (
            <CapSet<true, true, true, true, true> as CapSetRuntimeMask>::MASK,
            [true, true, true, true, true],
        ),
        (
            <CapSet<false, false, false, false, false> as CapSetRuntimeMask>::MASK,
            [false, false, false, false, false],
        ),
    ]
}

struct HandleContextFuture<F> {
    /// Runtime-instance shutdown authority. This token does not own the
    /// runtime, so retaining it while Pending cannot create an ownership cycle.
    runtime_shutdown: Option<crate::runtime_async::RuntimeShutdownToken>,
    task_cx: Cx,
    task_cap_mask: asupersync::cx::CapMask,
    future: Pin<Box<F>>,
}

impl<F: Future> Future for HandleContextFuture<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Acquire the scheduler's ambient handle only for this poll. This
        // wrapper can be nested inside another runtime-owned future, so even an
        // apparently caller-owned strong handle stored here could form an
        // ownership cycle and prevent runtime shutdown.
        let runtime_handle = Runtime::current_handle()
            .expect("runtime task polled without scheduler handle");
        let _runtime_handle_guard = install_runtime_handle_for_poll(runtime_handle);
        let _runtime_shutdown_guard = crate::runtime_async::install_runtime_shutdown_token_scoped(
            self.runtime_shutdown.clone(),
        );
        // `RuntimeHandle::spawn` installs its own scheduler context. The
        // adapter's contract is stronger: ambient Cx-aware helpers inside the
        // child must observe the exact explicitly threaded context. Scope the
        // installation to one poll so Pending never leaks it to sibling work.
        let _task_cx_guard = Cx::set_current(Some(self.task_cx.clone()));
        let _task_capability_guard = Cx::push_restriction(self.task_cap_mask);
        self.future.as_mut().poll(cx)
    }
}

fn install_runtime_handle_for_poll(
    handle: RuntimeHandle,
) -> crate::runtime_async::ScopedRuntimeHandle {
    crate::runtime_async::install_runtime_handle_scoped(handle)
}

/// Spawn a runtime task after cloning and threading a `Cx` into the task body.
pub fn spawn_with_cx<F, Fut, T>(handle: &RuntimeHandle, cx: &Cx, task: F) -> JoinHandle<T>
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let child_cx = cx.clone();
    let wrapped = HandleContextFuture {
        runtime_shutdown: crate::runtime_async::current_runtime_shutdown_token(),
        task_cx: child_cx.clone(),
        task_cap_mask: effective_cap_mask(&child_cx),
        future: Box::pin(async move { task(child_cx).await }),
    };
    handle.spawn(wrapped)
}

/// Fallible variant of [`spawn_with_cx`] that exposes admission errors.
pub fn try_spawn_with_cx<F, Fut, T>(
    handle: &RuntimeHandle,
    cx: &Cx,
    task: F,
) -> Result<JoinHandle<T>, SpawnError>
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let child_cx = cx.clone();
    let wrapped = HandleContextFuture {
        runtime_shutdown: crate::runtime_async::current_runtime_shutdown_token(),
        task_cx: child_cx.clone(),
        task_cap_mask: effective_cap_mask(&child_cx),
        future: Box::pin(async move { task(child_cx).await }),
    };
    handle.try_spawn(wrapped)
}

/// Spawn a batch of child tasks with explicit `Cx` threading and bounded
/// concurrency.
///
/// This helper keeps spawn fan-out deterministic and avoids unbounded
/// task bursts while preserving input order in the collected outputs.
///
/// Prefer this helper at migration boundaries that previously used a
/// `JoinSet` or ad-hoc task vector for bounded fanout.
///
/// # Panics
///
/// Panics if polled without an active asupersync scheduler. The helper acquires
/// only the scheduler's per-poll handle so a pending batch cannot retain its
/// runtime through a strong-handle ownership cycle.
pub async fn spawn_bounded_with_cx<F, Fut, T>(
    cx: &Cx,
    max_concurrency: usize,
    tasks: Vec<F>,
) -> Vec<T>
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    use asupersync::stream::{StreamExt, iter};

    let limit = max_concurrency.max(1);

    iter(
        tasks
            .into_iter()
            .map(|task| {
                let handle = Runtime::current_handle()
                    .expect("bounded Cx task spawned without scheduler handle");
                spawn_with_cx(&handle, cx, task)
            }),
    )
    .buffered(limit)
    .collect::<Vec<_>>()
    .await
}

/// Finite failure class for [`spawn_with_timeout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnWithTimeoutErrorKind {
    /// The helper's explicit timeout elapsed.
    TimeoutElapsed,
    /// The caller's capability context was cancelled.
    ContextCancelled,
    /// The caller's capability deadline elapsed first.
    DeadlineExceeded,
    /// The caller exhausted its cooperative poll quota.
    PollQuotaExhausted,
    /// The caller exhausted its cost budget.
    CostBudgetExhausted,
    /// The timeout boundary failed without an attributable root cause.
    ContextFailure,
}

/// Content-free error returned by [`spawn_with_timeout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnWithTimeoutError {
    kind: SpawnWithTimeoutErrorKind,
}

impl SpawnWithTimeoutError {
    fn from_timeout_failure(cx: &Cx) -> Self {
        use crate::outcome::CancelKind;

        // budget_timeout may be the operation that first observes an expired
        // budget. Checkpoint once to latch its structural root cause before
        // falling back to the helper's own elapsed-time class.
        let context_failed = cx.checkpoint().is_err();
        let kind = match cx.root_cancel_cause().map(|reason| reason.kind) {
            Some(CancelKind::Deadline | CancelKind::Timeout) => {
                SpawnWithTimeoutErrorKind::DeadlineExceeded
            }
            Some(CancelKind::PollQuota) => SpawnWithTimeoutErrorKind::PollQuotaExhausted,
            Some(CancelKind::CostBudget) => SpawnWithTimeoutErrorKind::CostBudgetExhausted,
            Some(
                CancelKind::User
                | CancelKind::FailFast
                | CancelKind::RaceLost
                | CancelKind::ParentCancelled
                | CancelKind::ResourceUnavailable
                | CancelKind::Shutdown
                | CancelKind::LinkedExit,
            ) => SpawnWithTimeoutErrorKind::ContextCancelled,
            None if context_failed => SpawnWithTimeoutErrorKind::ContextFailure,
            None => SpawnWithTimeoutErrorKind::TimeoutElapsed,
        };
        Self { kind }
    }

    /// Return the finite structural failure class.
    #[must_use]
    pub const fn kind(self) -> SpawnWithTimeoutErrorKind {
        self.kind
    }

    /// Return true only for caller-context cancellation, not timeout or budget
    /// exhaustion.
    #[must_use]
    pub const fn is_cancelled(self) -> bool {
        matches!(self.kind, SpawnWithTimeoutErrorKind::ContextCancelled)
    }
}

impl std::fmt::Display for SpawnWithTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self.kind {
            SpawnWithTimeoutErrorKind::TimeoutElapsed => "child task timeout elapsed",
            SpawnWithTimeoutErrorKind::ContextCancelled => {
                "child task cancelled by capability context"
            }
            SpawnWithTimeoutErrorKind::DeadlineExceeded => {
                "child task capability deadline exceeded"
            }
            SpawnWithTimeoutErrorKind::PollQuotaExhausted => {
                "child task capability poll quota exhausted"
            }
            SpawnWithTimeoutErrorKind::CostBudgetExhausted => {
                "child task capability cost budget exhausted"
            }
            SpawnWithTimeoutErrorKind::ContextFailure => "child task capability context failed",
        };
        write!(f, "SpawnWithTimeoutError: {message}")
    }
}

impl std::error::Error for SpawnWithTimeoutError {}

/// Spawn a child task with explicit `Cx` threading and wait for it with a timeout.
///
/// The task future is driven directly under the timeout — a cancel-on-drop
/// task — rather than eagerly scheduled as a detached runtime task. asupersync's
/// `JoinHandle` has no abort-on-`Drop` (and `abort()` is only a soft signal), so
/// eagerly `spawn`-ing the task and letting `budget_timeout` drop the
/// `JoinHandle` on the timeout branch would DETACH the task: it would keep
/// running unbounded past the deadline while the caller already saw a timeout
/// error. Driving the future inline means a timeout drops (cooperatively
/// cancels) the direct outer task future, so that future cannot outlive the
/// timeout. The
/// `HandleContextFuture` still installs the scheduler's current runtime handle
/// for each poll, so nested `task::spawn` / `spawn_*_with_cx` calls inside the
/// task keep working without retaining strong runtime ownership while Pending.
/// Those nested spawns are independently scheduled: if their JoinHandles are
/// dropped before the outer future times out, they are detached and may
/// outlive this helper. The timeout owns and drops only the direct `task`
/// future. Callers requiring a no-descendant-survival guarantee must not spawn
/// detached nested work; they must retain and settle an explicit structured
/// owner before the direct future can return or be dropped.
///
/// # Panics
///
/// Panics if polled without an active asupersync scheduler. The direct future
/// intentionally retains no strong runtime handle while Pending.
pub async fn spawn_with_timeout<F, Fut, T>(
    cx: &Cx,
    timeout: Duration,
    task: F,
) -> Result<T, SpawnWithTimeoutError>
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    if cx.checkpoint().is_err() {
        return Err(SpawnWithTimeoutError::from_timeout_failure(cx));
    }

    let child_cx = cx.clone();
    let wrapped = HandleContextFuture {
        runtime_shutdown: crate::runtime_async::current_runtime_shutdown_token(),
        task_cx: child_cx.clone(),
        task_cap_mask: effective_cap_mask(&child_cx),
        future: Box::pin(async move { task(child_cx).await }),
    };
    crate::runtime_async::timeout_with_cx(cx, timeout, wrapped)
        .await
        .map_err(|_| SpawnWithTimeoutError::from_timeout_failure(cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_peak(peak: &std::sync::atomic::AtomicUsize, current: usize) {
        let mut observed = peak.load(std::sync::atomic::Ordering::SeqCst);
        while current > observed {
            match peak.compare_exchange(
                observed,
                current,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => observed = actual,
            }
        }
    }

    // ── DarkBadger wa-1u90p.7.1 ──────────────────────────────────────

    #[test]
    fn runtime_preset_equality() {
        assert_eq!(RuntimePreset::CurrentThread, RuntimePreset::CurrentThread);
        assert_eq!(RuntimePreset::MultiThread, RuntimePreset::MultiThread);
        assert_ne!(RuntimePreset::CurrentThread, RuntimePreset::MultiThread);
    }

    #[test]
    #[allow(clippy::clone_on_copy)]
    fn runtime_preset_clone_copy() {
        let preset = RuntimePreset::CurrentThread;
        let cloned = preset.clone();
        let copied = preset;
        assert_eq!(preset, cloned);
        assert_eq!(preset, copied);
    }

    #[test]
    fn runtime_preset_debug_format() {
        let ct = format!("{:?}", RuntimePreset::CurrentThread);
        let mt = format!("{:?}", RuntimePreset::MultiThread);
        assert!(ct.contains("CurrentThread"));
        assert!(mt.contains("MultiThread"));
    }

    #[test]
    fn runtime_tuning_default_has_positive_values() {
        let tuning = RuntimeTuning::default();
        assert!(
            tuning.worker_threads > 0,
            "worker_threads should be positive"
        );
        assert!(tuning.poll_budget > 0, "poll_budget should be positive");
        assert!(
            tuning.blocking_max_threads >= tuning.blocking_min_threads,
            "max_threads should be >= min_threads"
        );
    }

    #[test]
    #[allow(clippy::clone_on_copy)]
    fn runtime_tuning_clone_eq() {
        let t1 = RuntimeTuning {
            worker_threads: 4,
            poll_budget: 128,
            blocking_min_threads: 2,
            blocking_max_threads: 16,
        };
        let t2 = t1.clone();
        assert_eq!(t1, t2);
    }

    #[test]
    fn runtime_tuning_ne_when_different() {
        let t1 = RuntimeTuning::default();
        let t2 = RuntimeTuning {
            worker_threads: t1.worker_threads + 1,
            ..t1
        };
        assert_ne!(t1, t2);
    }

    #[test]
    fn runtime_tuning_debug_format() {
        let tuning = RuntimeTuning::default();
        let dbg = format!("{:?}", tuning);
        assert!(dbg.contains("RuntimeTuning"));
        assert!(dbg.contains("worker_threads"));
        assert!(dbg.contains("poll_budget"));
    }

    #[test]
    fn cx_runtime_builder_debug_format() {
        let builder = CxRuntimeBuilder::current_thread();
        let dbg = format!("{:?}", builder);
        assert!(dbg.contains("CxRuntimeBuilder"));
    }

    #[test]
    fn cx_runtime_builder_from_preset_current_thread() {
        let builder = CxRuntimeBuilder::from_preset(RuntimePreset::CurrentThread);
        let dbg = format!("{:?}", builder);
        assert!(dbg.contains("CxRuntimeBuilder"));
    }

    #[test]
    fn cx_runtime_builder_from_preset_multi_thread() {
        let builder = CxRuntimeBuilder::from_preset(RuntimePreset::MultiThread);
        let dbg = format!("{:?}", builder);
        assert!(dbg.contains("CxRuntimeBuilder"));
    }

    #[test]
    fn cx_runtime_builder_chain_methods() {
        // Verify builder chain methods compile and don't panic
        let _builder = CxRuntimeBuilder::multi_thread()
            .worker_threads(2)
            .poll_budget(64)
            .blocking_threads(1, 8);
    }

    #[test]
    fn cx_runtime_builder_with_tuning() {
        let tuning = RuntimeTuning {
            worker_threads: 2,
            poll_budget: 32,
            blocking_min_threads: 1,
            blocking_max_threads: 4,
        };
        let _builder = CxRuntimeBuilder::current_thread().with_tuning(tuning);
    }

    #[test]
    fn for_testing_creates_cx() {
        let _cx = for_testing();
    }

    #[test]
    fn with_cx_passes_through() {
        let cx = for_testing();
        let result = with_cx(&cx, |_inner| 42);
        assert_eq!(result, 42);
    }

    #[test]
    fn with_cx_preserves_identity() {
        let cx = for_testing();
        with_cx(&cx, |inner| {
            // inner is the same Cx reference
            let _ = inner;
        });
    }

    // ── DarkMill test expansion ──────────────────────────────────────

    // -----------------------------------------------------------------------
    // CxRuntimeBuilder::build tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_current_thread_runtime() {
        let runtime = CxRuntimeBuilder::current_thread()
            .build()
            .expect("build current_thread runtime");
        let result = runtime.block_on(async { 42 });
        assert_eq!(result, 42);
    }

    #[test]
    fn build_multi_thread_runtime() {
        let runtime = CxRuntimeBuilder::multi_thread()
            .worker_threads(2)
            .build()
            .expect("build multi_thread runtime");
        let result = runtime.block_on(async { "hello" });
        assert_eq!(result, "hello");
    }

    #[test]
    fn build_runtime_with_full_tuning() {
        let tuning = RuntimeTuning {
            worker_threads: 1,
            poll_budget: 64,
            blocking_min_threads: 1,
            blocking_max_threads: 4,
        };
        let runtime = CxRuntimeBuilder::current_thread()
            .with_tuning(tuning)
            .build()
            .expect("build tuned runtime");
        let result = runtime.block_on(async { true });
        assert!(result);
    }

    // -----------------------------------------------------------------------
    // with_cx_async tests
    // -----------------------------------------------------------------------

    #[test]
    fn with_cx_async_passes_through() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let result = runtime.block_on(async { with_cx_async(&cx, |_inner| async { 99 }).await });
        assert_eq!(result, 99);
    }

    #[test]
    fn with_cx_async_can_await_futures() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let result = runtime.block_on(async {
            with_cx_async(&cx, |_inner| async {
                let a = async { 10 }.await;
                let b = async { 20 }.await;
                a + b
            })
            .await
        });
        assert_eq!(result, 30);
    }

    // -----------------------------------------------------------------------
    // spawn_with_cx tests
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_with_cx_runs_task() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = spawn_with_cx(&handle, &cx, |_cx| async { 42 });
            join.await
        });
        assert_eq!(result, 42);
    }

    #[test]
    fn spawn_with_cx_receives_cloned_cx() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let expected_region = cx.region_id();
        let expected_task = cx.task_id();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = spawn_with_cx(&handle, &cx, move |child_cx| async move {
                let active_before = Cx::current().expect("explicit cx installed for child poll");
                assert_eq!(active_before.region_id(), expected_region);
                assert_eq!(active_before.task_id(), expected_task);
                assert_eq!(child_cx.region_id(), expected_region);
                assert_eq!(child_cx.task_id(), expected_task);

                crate::runtime_async::task::yield_now().await;
                let active_after = Cx::current().expect("explicit cx reinstalled after yield");
                active_after.region_id() == expected_region
                    && active_after.task_id() == expected_task
                    && child_cx.checkpoint().is_ok()
            });
            join.await
        });
        assert!(result);
    }

    #[test]
    fn spawn_with_cx_ambient_context_inherits_cancellation() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::ParentCancelled,
            Some("explicit cx parent cancelled"),
        );
        let handle = runtime.handle();

        let cancelled = runtime.block_on(async {
            spawn_with_cx(&handle, &cx, |_child_cx| async move {
                Cx::current()
                    .expect("explicit cx installed for child poll")
                    .checkpoint()
                    .is_err()
            })
            .await
        });
        assert!(cancelled);
    }

    #[test]
    fn spawn_with_cx_preserves_all_effective_capability_bits_across_yield() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let handle = runtime.handle();

        for (mask, expected) in capability_mask_test_cases() {
            let restricted = {
                let _base_guard = Cx::set_current(Some(for_testing()));
                let _restriction = Cx::push_restriction(mask);
                Cx::current().expect("restricted explicit cx")
            };
            assert_eq!(effective_capability_bits(&restricted), expected);

            let observed = runtime.block_on(async {
                spawn_with_cx(&handle, &restricted, move |child_cx| async move {
                    assert_eq!(effective_capability_bits(&child_cx), expected);
                    let before =
                        effective_capability_bits(&Cx::current().expect("installed restricted cx"));
                    crate::runtime_async::task::yield_now().await;
                    let after = effective_capability_bits(
                        &Cx::current().expect("reinstalled restricted cx"),
                    );
                    (before, after)
                })
                .await
            });
            assert_eq!(observed, (expected, expected));
        }
    }

    #[test]
    fn spawn_with_cx_multiple_tasks() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let results = runtime.block_on(async {
            let mut joins = Vec::new();
            for i in 0..5u32 {
                joins.push(spawn_with_cx(&handle, &cx, move |_cx| async move { i * 2 }));
            }
            let mut out = Vec::new();
            for join in joins {
                out.push(join.await);
            }
            out
        });
        assert_eq!(results, vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn spawn_with_cx_installs_runtime_handle_for_child_polls() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = spawn_with_cx(&handle, &cx, |_cx| async move {
                crate::runtime_async::current_runtime_handle().is_some()
            });
            join.await
        });
        assert!(result);
    }

    #[test]
    fn pending_spawn_with_cx_task_does_not_keep_runtime_alive() {
        struct PendingTaskDropProbe(std::sync::Arc<std::sync::atomic::AtomicBool>);

        impl Drop for PendingTaskDropProbe {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }

        let runtime = CxRuntimeBuilder::multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime-cycle fixture");
        let handle = runtime.handle();
        let cx = for_testing();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = PendingTaskDropProbe(std::sync::Arc::clone(&dropped));
        let join = spawn_with_cx(&handle, &cx, move |_child_cx| async move {
            let _probe = probe;
            std::future::pending::<()>().await;
        });

        drop(join);
        drop(handle);
        drop(runtime);
        assert!(
            dropped.load(std::sync::atomic::Ordering::Acquire),
            "the Cx adapter must not retain its spawning runtime"
        );
    }

    #[test]
    fn spawn_with_cx_supports_nested_runtime_async_spawn() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = spawn_with_cx(&handle, &cx, |_cx| async move {
                crate::runtime_async::task::spawn(async { 42 })
                    .await
                    .expect("nested spawn should succeed")
            });
            join.await
        });
        assert_eq!(result, 42);
    }

    // -----------------------------------------------------------------------
    // try_spawn_with_cx tests
    // -----------------------------------------------------------------------

    #[test]
    fn try_spawn_with_cx_success() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = try_spawn_with_cx(&handle, &cx, |_cx| async { "spawned" })
                .expect("try_spawn should succeed");
            join.await
        });
        assert_eq!(result, "spawned");
    }

    #[test]
    fn try_spawn_with_cx_preserves_runtime_handle_for_nested_spawn() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = try_spawn_with_cx(&handle, &cx, |_cx| async move {
                crate::runtime_async::task::spawn(async { "nested" })
                    .await
                    .expect("nested spawn should succeed")
            })
            .expect("try_spawn should succeed");
            join.await
        });
        assert_eq!(result, "nested");
    }

    // -----------------------------------------------------------------------
    // spawn_bounded_with_cx tests
    // -----------------------------------------------------------------------

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_empty_tasks() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let tasks: Vec<
            Box<dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = i32> + Send>> + Send>,
        > = Vec::new();
        let results: Vec<i32> =
            runtime.block_on(async { spawn_bounded_with_cx(&cx, 4, tasks).await });
        assert!(results.is_empty());
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_preserves_order() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let tasks: Vec<
            Box<dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> + Send>,
        > = (0..5u32)
            .map(|i| {
                let closure: Box<
                    dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> + Send,
                > = Box::new(move |_cx| Box::pin(async move { i }));
                closure
            })
            .collect();

        let results =
            runtime.block_on(async { spawn_bounded_with_cx(&cx, 2, tasks).await });
        assert_eq!(results, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_preserves_order_when_tasks_finish_out_of_order() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let tasks: Vec<
            Box<dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> + Send>,
        > = [(0_u32, 30_u64), (1, 0), (2, 10), (3, 20)]
            .into_iter()
            .map(|(value, delay_ms)| {
                let closure: Box<
                    dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> + Send,
                > = Box::new(move |_cx| {
                    Box::pin(async move {
                        if delay_ms > 0 {
                            crate::runtime_async::sleep(Duration::from_millis(delay_ms)).await;
                        }
                        value
                    })
                });
                closure
            })
            .collect();

        let results = runtime
            .block_on(async { spawn_bounded_with_cx(&cx, tasks.len(), tasks).await });
        assert_eq!(results, vec![0, 1, 2, 3]);
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_concurrency_limit_1() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let counter = Arc::new(AtomicU32::new(0));
        let tasks: Vec<
            Box<dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> + Send>,
        > = (0..3u32)
            .map(|i| {
                let counter = Arc::clone(&counter);
                let closure: Box<
                    dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> + Send,
                > = Box::new(move |_cx| {
                    Box::pin(async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        i
                    })
                });
                closure
            })
            .collect();

        let results =
            runtime.block_on(async { spawn_bounded_with_cx(&cx, 1, tasks).await });
        assert_eq!(results.len(), 3);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_waits_for_all_children_before_returning() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let completed = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<
            Box<dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = usize> + Send>> + Send>,
        > = (0..4_usize)
            .map(|i| {
                let completed = Arc::clone(&completed);
                let closure: Box<
                    dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = usize> + Send>> + Send,
                > = Box::new(move |_cx| {
                    Box::pin(async move {
                        crate::runtime_async::sleep(Duration::from_millis(5 * (4 - i) as u64))
                            .await;
                        completed.fetch_add(1, Ordering::SeqCst);
                        i
                    })
                });
                closure
            })
            .collect();

        let results =
            runtime.block_on(async { spawn_bounded_with_cx(&cx, 2, tasks).await });
        assert_eq!(results, vec![0, 1, 2, 3]);
        assert_eq!(
            completed.load(Ordering::SeqCst),
            4,
            "spawn_bounded_with_cx should not return until every child has completed"
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_respects_peak_concurrency_cap() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<
            Box<dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = usize> + Send>> + Send>,
        > = (0..6_usize)
            .map(|i| {
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                let closure: Box<
                    dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = usize> + Send>> + Send,
                > = Box::new(move |_cx| {
                    Box::pin(async move {
                        let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                        record_peak(&peak, now_active);
                        crate::runtime_async::sleep(Duration::from_millis(10)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        i
                    })
                });
                closure
            })
            .collect();

        let results =
            runtime.block_on(async { spawn_bounded_with_cx(&cx, 2, tasks).await });

        assert_eq!(results, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "spawn_bounded_with_cx should never run more than the requested number of children concurrently"
        );
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "all child tasks should have drained before spawn_bounded_with_cx returns"
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_zero_concurrency_is_treated_as_single_slot() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<
            Box<dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = usize> + Send>> + Send>,
        > = (0..4_usize)
            .map(|i| {
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                let closure: Box<
                    dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = usize> + Send>> + Send,
                > = Box::new(move |_cx| {
                    Box::pin(async move {
                        let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                        record_peak(&peak, now_active);
                        crate::runtime_async::sleep(Duration::from_millis(10)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        i
                    })
                });
                closure
            })
            .collect();

        let results =
            runtime.block_on(async { spawn_bounded_with_cx(&cx, 0, tasks).await });

        assert_eq!(results, vec![0, 1, 2, 3]);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "max_concurrency=0 should be coerced to a single active child rather than allowing unbounded fanout"
        );
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "all child tasks should have drained before spawn_bounded_with_cx returns"
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_supports_nested_runtime_async_spawn() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let tasks: Vec<
            Box<dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> + Send>,
        > = (0..4_u32)
            .map(|value| {
                let closure: Box<
                    dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> + Send,
                > = Box::new(move |_cx| {
                    Box::pin(async move {
                        crate::runtime_async::task::spawn(async move { value + 10 })
                            .await
                            .expect("nested spawn should succeed")
                    })
                });
                closure
            })
            .collect();

        let results =
            runtime.block_on(async { spawn_bounded_with_cx(&cx, 2, tasks).await });
        assert_eq!(results, vec![10, 11, 12, 13]);
    }

    // -----------------------------------------------------------------------
    // spawn_with_timeout tests
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_with_timeout_completes_in_time() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let result = runtime.block_on(async {
            spawn_with_timeout(&cx, Duration::from_secs(5), |_cx| async { "fast" }).await
        });
        assert_eq!(result.unwrap(), "fast");
    }

    #[test]
    fn spawn_with_timeout_returns_error_on_timeout() {
        struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped_by_child = std::sync::Arc::clone(&dropped);

        let result = runtime.block_on(async {
            spawn_with_timeout(
                &cx,
                Duration::from_millis(1),
                move |_cx| async move {
                    let _drop_flag = DropFlag(dropped_by_child);
                    std::future::pending::<&'static str>().await
                },
            )
            .await
        });
        let error = result.expect_err("direct child must time out");
        assert_eq!(error.kind(), SpawnWithTimeoutErrorKind::TimeoutElapsed);
        assert!(!error.is_cancelled());
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "timeout result must follow destruction of the direct child future"
        );
    }

    #[test]
    fn pending_spawn_with_timeout_task_does_not_keep_runtime_alive() {
        struct PendingTaskDropProbe(std::sync::Arc<std::sync::atomic::AtomicBool>);

        impl Drop for PendingTaskDropProbe {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }

        let runtime = CxRuntimeBuilder::multi_thread()
            .worker_threads(1)
            .build()
            .expect("timeout runtime-cycle fixture");
        let cx = for_testing();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = PendingTaskDropProbe(std::sync::Arc::clone(&dropped));
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let join = runtime.handle().spawn(async move {
            let _ = spawn_with_timeout(&cx, Duration::from_secs(60), move |_child_cx| async move {
                let _probe = probe;
                entered_tx
                    .send(())
                    .expect("timeout cycle probe receiver must remain live");
                std::future::pending::<()>().await;
            })
            .await;
        });

        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("timeout child must enter Pending before runtime drop");
        drop(join);
        drop(runtime);
        assert!(
            dropped.load(std::sync::atomic::Ordering::Acquire),
            "the inline timeout adapter must not retain its polling runtime"
        );
    }

    #[test]
    fn spawn_with_timeout_precancel_is_prompt_and_never_polls_child() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("SECRET spawn_with_timeout pre-cancel"),
        );
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let polls_by_child = std::sync::Arc::clone(&polls);

        let started = std::time::Instant::now();
        let result = runtime.block_on(async {
            spawn_with_timeout(
                &cx,
                Duration::from_secs(30),
                move |_cx| async move {
                    polls_by_child.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::future::pending::<()>().await;
                },
            )
            .await
        });
        let elapsed = started.elapsed();

        let error = result.expect_err("pre-cancelled Cx must reject before polling");
        assert_eq!(error.kind(), SpawnWithTimeoutErrorKind::ContextCancelled);
        assert!(error.is_cancelled());
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(elapsed < Duration::from_secs(1));
        assert!(!error.to_string().contains("SECRET"));
        assert!(!format!("{error:?}").contains("SECRET"));
    }

    #[test]
    fn spawn_with_timeout_uses_tighter_cx_budget() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let seed_cx = for_testing();
        let seed_now = seed_cx
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |driver| driver.now());
        let cx = Cx::for_testing_with_budget(
            Budget::new().with_deadline(seed_now + Duration::from_millis(20)),
        );

        let result = runtime.block_on(async {
            spawn_with_timeout(&cx, Duration::from_secs(5), |_cx| async {
                std::future::pending::<()>().await;
            })
            .await
        });

        assert!(
            matches!(
                result,
                Err(SpawnWithTimeoutError {
                    kind: SpawnWithTimeoutErrorKind::DeadlineExceeded
                })
            ),
            "explicit Cx deadline should win over a looser timeout argument"
        );
    }

    #[test]
    fn spawn_with_timeout_does_not_claim_ownership_of_detached_nested_spawns() {
        struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let direct_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let nested_completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let direct_dropped_by_child = std::sync::Arc::clone(&direct_dropped);
        let nested_completed_by_task = std::sync::Arc::clone(&nested_completed);
        let (release_tx, release_rx) = crate::runtime_async::oneshot::channel::<()>();
        let (completed_tx, completed_rx) = crate::runtime_async::oneshot::channel::<()>();

        runtime.block_on(async {
            let result = spawn_with_timeout(
                &cx,
                Duration::from_millis(1),
                move |_cx| async move {
                    let _drop_flag = DropFlag(direct_dropped_by_child);
                    // Deliberately discard the nested JoinHandle. This is the
                    // unsupported structured-ownership case documented on the
                    // helper: timeout drops this direct future, not the already
                    // scheduled descendant.
                    drop(crate::runtime_async::task::spawn(async move {
                        crate::runtime_async::oneshot_recv(release_rx)
                            .await
                            .expect("detached nested release sender");
                        nested_completed_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = completed_tx.send(());
                    }));
                    std::future::pending::<()>().await;
                },
            )
            .await;

            assert!(matches!(
                result,
                Err(SpawnWithTimeoutError {
                    kind: SpawnWithTimeoutErrorKind::TimeoutElapsed
                })
            ));
            assert!(direct_dropped.load(std::sync::atomic::Ordering::SeqCst));
            assert!(!nested_completed.load(std::sync::atomic::Ordering::SeqCst));

            release_tx
                .send(())
                .expect("detached nested receiver must still be alive after outer timeout");
            crate::runtime_async::timeout(
                Duration::from_secs(5),
                crate::runtime_async::oneshot_recv(completed_rx),
            )
            .await
            .expect("detached nested task did not settle within 5s")
            .expect("detached nested task dropped completion sender");
            assert!(
                nested_completed.load(std::sync::atomic::Ordering::SeqCst),
                "detached nested work may outlive the direct timeout boundary"
            );
        });
    }

    #[test]
    fn spawn_with_timeout_supports_nested_runtime_async_spawn() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let result = runtime.block_on(async {
            spawn_with_timeout(&cx, Duration::from_secs(1), |_cx| async move {
                crate::runtime_async::task::spawn(async { 41_u32 })
                    .await
                    .expect("nested spawn should succeed")
            })
            .await
        });

        assert_eq!(result.unwrap(), 41);
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_with_timeout_supports_nested_spawn_bounded_with_cx() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();

        let result = runtime.block_on(async {
            spawn_with_timeout(&cx, Duration::from_secs(1), move |nested_cx| {
                async move {
                    let tasks: Vec<
                        Box<
                            dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>>
                                + Send,
                        >,
                    > = [(0_u32, 10_u64), (1, 0), (2, 5)]
                        .into_iter()
                        .map(|(value, delay_ms)| {
                            let closure: Box<
                                dyn FnOnce(
                                        Cx,
                                    )
                                        -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>>
                                    + Send,
                            > = Box::new(move |_cx| {
                                Box::pin(async move {
                                    if delay_ms > 0 {
                                        crate::runtime_async::sleep(Duration::from_millis(
                                            delay_ms,
                                        ))
                                        .await;
                                    }
                                    value
                                })
                            });
                            closure
                        })
                        .collect();

                    spawn_bounded_with_cx(&nested_cx, 2, tasks).await
                }
            })
            .await
        });

        assert_eq!(result.unwrap(), vec![0, 1, 2]);
    }

    // -----------------------------------------------------------------------
    // Cx interaction tests
    // -----------------------------------------------------------------------

    #[test]
    fn for_testing_cx_supports_checkpoint() {
        let cx = for_testing();
        assert!(cx.checkpoint().is_ok());
    }

    #[test]
    fn with_cx_nested_calls() {
        let cx = for_testing();
        let result = with_cx(&cx, |inner1| {
            with_cx(inner1, |inner2| with_cx(inner2, |_inner3| 7))
        });
        assert_eq!(result, 7);
    }

    #[test]
    fn with_cx_returns_complex_type() {
        let cx = for_testing();
        let result: Vec<String> = with_cx(&cx, |_| vec!["a".to_string(), "b".to_string()]);
        assert_eq!(result.len(), 2);
    }

    // -----------------------------------------------------------------------
    // RuntimeTuning additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn runtime_tuning_copy_trait() {
        let t1 = RuntimeTuning::default();
        let t2 = t1; // Copy
        let t3 = t1; // Still accessible, so Copy works
        assert_eq!(t2, t3);
    }

    #[test]
    fn runtime_tuning_custom_values() {
        let tuning = RuntimeTuning {
            worker_threads: 8,
            poll_budget: 256,
            blocking_min_threads: 4,
            blocking_max_threads: 32,
        };
        assert_eq!(tuning.worker_threads, 8);
        assert_eq!(tuning.poll_budget, 256);
        assert_eq!(tuning.blocking_min_threads, 4);
        assert_eq!(tuning.blocking_max_threads, 32);
    }
}
