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

struct HandleContextFuture<F> {
    handle: RuntimeHandle,
    future: Pin<Box<F>>,
}

impl<F: Future> Future for HandleContextFuture<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        install_runtime_handle_for_poll(self.handle.clone());
        self.future.as_mut().poll(cx)
    }
}

#[cfg(feature = "asupersync-runtime")]
fn install_runtime_handle_for_poll(handle: RuntimeHandle) {
    crate::runtime_compat::install_runtime_handle(handle);
}

#[cfg(not(feature = "asupersync-runtime"))]
fn install_runtime_handle_for_poll(_handle: RuntimeHandle) {}

/// Spawn a runtime task after cloning and threading a `Cx` into the task body.
pub fn spawn_with_cx<F, Fut, T>(handle: &RuntimeHandle, cx: &Cx, task: F) -> JoinHandle<T>
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let child_cx = cx.clone();
    let wrapped = HandleContextFuture {
        handle: handle.clone(),
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
        handle: handle.clone(),
        future: Box::pin(async move { task(child_cx).await }),
    };
    handle.try_spawn(wrapped)
}

/// Spawn a batch of child tasks with explicit `Cx` threading and bounded
/// concurrency.
///
/// This helper keeps spawn fan-out deterministic and avoids unbounded
/// task bursts while preserving input order in the collected outputs.
pub async fn spawn_bounded_with_cx<F, Fut, T>(
    handle: &RuntimeHandle,
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
            .map(|task| spawn_with_cx(handle, cx, task)),
    )
    .buffered(limit)
    .collect::<Vec<_>>()
    .await
}

/// Spawn a child task with explicit `Cx` threading and wait for it with a timeout.
///
/// Returns an error string when the timeout elapses before completion.
pub async fn spawn_with_timeout<F, Fut, T>(
    handle: &RuntimeHandle,
    cx: &Cx,
    timeout: Duration,
    task: F,
) -> Result<T, String>
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    crate::runtime_compat::timeout(timeout, spawn_with_cx(handle, cx, task)).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = spawn_with_cx(&handle, &cx, |child_cx| async move {
                child_cx.checkpoint().is_ok()
            });
            join.await
        });
        assert!(result);
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

    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn spawn_with_cx_installs_runtime_handle_for_child_polls() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = spawn_with_cx(&handle, &cx, |_cx| async move {
                crate::runtime_compat::current_runtime_handle().is_some()
            });
            join.await
        });
        assert!(result);
    }

    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn spawn_with_cx_supports_nested_runtime_compat_spawn() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = spawn_with_cx(&handle, &cx, |_cx| async move {
                crate::runtime_compat::task::spawn(async { 42 })
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

    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn try_spawn_with_cx_preserves_runtime_handle_for_nested_spawn() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            let join = try_spawn_with_cx(&handle, &cx, |_cx| async move {
                crate::runtime_compat::task::spawn(async { "nested" })
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
        let handle = runtime.handle();

        let tasks: Vec<
            Box<dyn FnOnce(Cx) -> std::pin::Pin<Box<dyn Future<Output = i32> + Send>> + Send>,
        > = Vec::new();
        let results: Vec<i32> =
            runtime.block_on(async { spawn_bounded_with_cx(&handle, &cx, 4, tasks).await });
        assert!(results.is_empty());
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_preserves_order() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

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
            runtime.block_on(async { spawn_bounded_with_cx(&handle, &cx, 2, tasks).await });
        assert_eq!(results, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn spawn_bounded_concurrency_limit_1() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

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
            runtime.block_on(async { spawn_bounded_with_cx(&handle, &cx, 1, tasks).await });
        assert_eq!(results.len(), 3);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    // -----------------------------------------------------------------------
    // spawn_with_timeout tests
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_with_timeout_completes_in_time() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            spawn_with_timeout(&handle, &cx, Duration::from_secs(5), |_cx| async { "fast" }).await
        });
        assert_eq!(result.unwrap(), "fast");
    }

    #[test]
    fn spawn_with_timeout_returns_error_on_timeout() {
        let runtime = CxRuntimeBuilder::current_thread().build().expect("runtime");
        let cx = for_testing();
        let handle = runtime.handle();

        let result = runtime.block_on(async {
            spawn_with_timeout(&handle, &cx, Duration::from_millis(1), |_cx| async {
                crate::runtime_compat::sleep(Duration::from_secs(10)).await;
                "slow"
            })
            .await
        });
        assert!(result.is_err());
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
