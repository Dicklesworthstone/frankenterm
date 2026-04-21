//! Minimal `crate::cx` compatibility surface for builds without the
//! `asupersync-runtime` feature.
//!
//! This keeps `_with_cx` call sites compiling under `--no-default-features`
//! by providing the same type names and a tiny cancellation API, while the
//! actual work continues to route through the legacy non-Cx code paths.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Budget;

impl Budget {
    pub const INFINITE: Self = Self;

    #[must_use]
    pub fn new() -> Self {
        Self
    }

    #[must_use]
    pub fn with_poll_quota(self, _quota: u32) -> Self {
        self
    }

    #[must_use]
    pub fn with_deadline<T>(self, _deadline: T) -> Self {
        self
    }
}

#[derive(Debug, Default)]
struct CxState {
    cancelled: AtomicBool,
    reason: Mutex<Option<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct Cx {
    state: Arc<CxState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scope;

#[derive(Debug, Clone)]
pub struct Cancelled {
    reason: Option<String>,
}

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reason {
            Some(reason) => write!(f, "{reason}"),
            None => write!(f, "capability context cancelled"),
        }
    }
}

impl std::error::Error for Cancelled {}

impl Cx {
    #[must_use]
    pub fn for_request() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn for_testing() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn for_testing_with_budget(_budget: Budget) -> Self {
        Self::default()
    }

    #[must_use]
    pub fn current() -> Option<Self> {
        None
    }

    pub fn checkpoint(&self) -> Result<(), Cancelled> {
        if self.is_cancel_requested() {
            let reason = self
                .state
                .reason
                .lock()
                .ok()
                .and_then(|guard| guard.clone());
            Err(Cancelled { reason })
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub fn is_cancel_requested(&self) -> bool {
        self.state.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel_with<K>(&self, _kind: K, reason: Option<&str>) {
        self.state.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.state.reason.lock() {
            *guard = reason.map(ToOwned::to_owned);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePreset {
    CurrentThread,
    MultiThread,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeTuning {
    pub worker_threads: usize,
    pub poll_budget: u32,
    pub blocking_min_threads: usize,
    pub blocking_max_threads: usize,
}

impl Default for RuntimeTuning {
    fn default() -> Self {
        Self {
            worker_threads: 1,
            poll_budget: 128,
            blocking_min_threads: 1,
            blocking_max_threads: 1,
        }
    }
}

pub struct CxRuntimeBuilder {
    inner: crate::runtime_compat::RuntimeBuilder,
}

impl std::fmt::Debug for CxRuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CxRuntimeBuilder")
            .field("inner", &"<RuntimeBuilder>")
            .finish()
    }
}

impl CxRuntimeBuilder {
    #[must_use]
    pub fn from_preset(preset: RuntimePreset) -> Self {
        let inner = match preset {
            RuntimePreset::CurrentThread => crate::runtime_compat::RuntimeBuilder::current_thread(),
            RuntimePreset::MultiThread => crate::runtime_compat::RuntimeBuilder::multi_thread(),
        };
        Self { inner }
    }

    #[must_use]
    pub fn current_thread() -> Self {
        Self::from_preset(RuntimePreset::CurrentThread)
    }

    #[must_use]
    pub fn multi_thread() -> Self {
        Self::from_preset(RuntimePreset::MultiThread)
    }

    #[must_use]
    pub fn with_tuning(self, tuning: RuntimeTuning) -> Self {
        self.worker_threads(tuning.worker_threads)
            .poll_budget(tuning.poll_budget)
            .blocking_threads(tuning.blocking_min_threads, tuning.blocking_max_threads)
    }

    #[must_use]
    pub fn worker_threads(mut self, workers: usize) -> Self {
        self.inner = self.inner.worker_threads(workers);
        self
    }

    #[must_use]
    pub fn poll_budget(self, _poll_budget: u32) -> Self {
        self
    }

    #[must_use]
    pub fn blocking_threads(self, _min_threads: usize, _max_threads: usize) -> Self {
        self
    }

    pub fn build(self) -> Result<crate::runtime_compat::Runtime, String> {
        self.inner.build()
    }
}

#[must_use]
pub fn for_testing() -> Cx {
    Cx::for_testing()
}

#[must_use]
pub fn for_request() -> Cx {
    Cx::for_request()
}

#[inline]
pub fn with_cx<T>(cx: &Cx, f: impl FnOnce(&Cx) -> T) -> T {
    f(cx)
}

pub async fn with_cx_async<T, Fut>(cx: &Cx, f: impl FnOnce(&Cx) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    f(cx).await
}
