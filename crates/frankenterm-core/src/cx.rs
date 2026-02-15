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
    pub fn build(self) -> Result<Runtime, asupersync::Error> {
        self.inner.build()
    }
}

/// Construct a test-only capability context.
#[must_use]
pub fn for_testing() -> Cx {
    Cx::for_testing()
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

/// Spawn a runtime task after cloning and threading a `Cx` into the task body.
pub fn spawn_with_cx<F, Fut, T>(handle: &RuntimeHandle, cx: &Cx, task: F) -> JoinHandle<T>
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let child_cx = cx.clone();
    handle.spawn(async move { task(child_cx).await })
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
    handle.try_spawn(async move { task(child_cx).await })
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
}
