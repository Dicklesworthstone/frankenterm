//! Temporary dual-runtime compatibility surface for the tokio -> asupersync migration.
//!
//! This module intentionally keeps the API small and explicit:
//! - sync primitive type aliases (`Mutex`, `RwLock`, `Semaphore`, ...)
//! - channel module aliases (`mpsc`, `watch`, `broadcast`)
//! - runtime lifecycle wrappers (`RuntimeBuilder`, `Runtime`, `CompatRuntime`)
//! - time helpers (`sleep`, `timeout`)
//!
//! The scaffold is expected to be removed once migration is complete.

use std::future::Future;
use std::time::Duration;

/// Migration policy classification for `runtime_compat` APIs.
///
/// Bead: `ft-e34d9.10.2.3`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceDisposition {
    /// Intentional compatibility seam that remains part of the target surface.
    Keep,
    /// Transitional helper that should be replaced by a more explicit API.
    Replace,
    /// Transitional helper that should be removed after replacement lands.
    Retire,
}

/// One contract entry describing an exported compatibility API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceContractEntry {
    /// Fully-qualified API path (within this module).
    pub api: &'static str,
    /// Keep/replace/retire policy.
    pub disposition: SurfaceDisposition,
    /// Why the API has this policy.
    pub rationale: &'static str,
    /// Explicit replacement path for replace/retire entries.
    pub replacement: Option<&'static str>,
}

/// Runtime-compat surface contract (v1).
///
/// This catalog keeps the migration seam auditable and intentionally shrinking.
pub const SURFACE_CONTRACT_V1: &[SurfaceContractEntry] = &[
    SurfaceContractEntry {
        api: "RuntimeBuilder",
        disposition: SurfaceDisposition::Keep,
        rationale: "Canonical runtime bootstrap seam shared by CLI/watch/test harnesses.",
        replacement: None,
    },
    SurfaceContractEntry {
        api: "Runtime",
        disposition: SurfaceDisposition::Keep,
        rationale: "Owns active runtime instance behind migration boundary.",
        replacement: None,
    },
    SurfaceContractEntry {
        api: "CompatRuntime::block_on",
        disposition: SurfaceDisposition::Keep,
        rationale: "Used by deterministic tests and bridge code while call-graph migration continues.",
        replacement: None,
    },
    SurfaceContractEntry {
        api: "CompatRuntime::spawn_detached",
        disposition: SurfaceDisposition::Replace,
        rationale: "Detached execution masks scope ownership semantics in target asupersync state.",
        replacement: Some("cx::spawn_with_cx / explicit scope-owned spawn"),
    },
    SurfaceContractEntry {
        api: "sleep",
        disposition: SurfaceDisposition::Keep,
        rationale: "Cross-runtime time seam with stable call-site behavior.",
        replacement: None,
    },
    SurfaceContractEntry {
        api: "timeout",
        disposition: SurfaceDisposition::Keep,
        rationale: "Shared timeout boundary used by IPC/web/watchdog call paths.",
        replacement: None,
    },
    SurfaceContractEntry {
        api: "spawn_blocking",
        disposition: SurfaceDisposition::Keep,
        rationale: "Canonical blocking-work seam with normalized error mapping.",
        replacement: None,
    },
    SurfaceContractEntry {
        api: "task::spawn_blocking",
        disposition: SurfaceDisposition::Replace,
        rationale: "JoinHandle-centric blocking helper should be reserved for explicit abortable workflows only.",
        replacement: Some(
            "spawn_blocking (use task::spawn_blocking only when JoinHandle control is required)",
        ),
    },
    SurfaceContractEntry {
        api: "mpsc_recv_option",
        disposition: SurfaceDisposition::Replace,
        rationale: "Option-normalized receive can hide cancellation semantics in asupersync mode.",
        replacement: Some("mpsc::Receiver::recv with explicit cx/cancellation handling"),
    },
    SurfaceContractEntry {
        api: "mpsc_send",
        disposition: SurfaceDisposition::Replace,
        rationale: "Send helper abstracts over reserve/commit vs direct send semantics.",
        replacement: Some("cx-aware channel send path (reserve/commit where required)"),
    },
    SurfaceContractEntry {
        api: "process::Command",
        disposition: SurfaceDisposition::Retire,
        rationale: "Tokio process shim remains temporary and should be replaced by asupersync-native process layer.",
        replacement: Some("asupersync process abstraction"),
    },
    SurfaceContractEntry {
        api: "signal",
        disposition: SurfaceDisposition::Retire,
        rationale: "Tokio-only signal shim is transitional and should be removed after native runtime integration.",
        replacement: Some("asupersync-native signal handling"),
    },
];

#[cfg(feature = "asupersync-runtime")]
use std::ops::{Deref, DerefMut};
#[cfg(feature = "asupersync-runtime")]
use std::sync::Arc;

// Thread-local storage for the asupersync `RuntimeHandle`, installed by
// `Runtime::block_on` and consumed by `task::spawn` to provide ambient
// runtime context (analogous to tokio's internal CONTEXT thread-local).
#[cfg(feature = "asupersync-runtime")]
thread_local! {
    static ASUPERSYNC_HANDLE: std::cell::RefCell<Option<asupersync::runtime::RuntimeHandle>> =
        const { std::cell::RefCell::new(None) };
}

/// Install an asupersync `RuntimeHandle` into thread-local storage for
/// ambient `task::spawn` access.
///
/// The `runtime_compat::Runtime::block_on` wrapper calls this automatically.
/// Test fixtures using the raw asupersync runtime should call this manually.
#[cfg(feature = "asupersync-runtime")]
pub fn install_runtime_handle(handle: asupersync::runtime::RuntimeHandle) {
    ASUPERSYNC_HANDLE.with(|cell| cell.replace(Some(handle)));
}

/// Remove the asupersync `RuntimeHandle` from thread-local storage.
#[cfg(feature = "asupersync-runtime")]
pub fn clear_runtime_handle() {
    ASUPERSYNC_HANDLE.with(|cell| cell.replace(None));
}

#[cfg(feature = "asupersync-runtime")]
#[derive(Debug)]
pub struct Mutex<T> {
    inner: asupersync::sync::Mutex<T>,
}

#[cfg(feature = "asupersync-runtime")]
impl<T> Mutex<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            inner: asupersync::sync::Mutex::new(value),
        }
    }

    pub async fn lock(&self) -> MutexGuard<'_, T> {
        let cx = asupersync::Cx::for_testing();
        let guard = self
            .inner
            .lock(&cx)
            .await
            .expect("runtime_compat mutex lock failed");
        MutexGuard { inner: guard }
    }
}

#[cfg(feature = "asupersync-runtime")]
pub struct MutexGuard<'a, T> {
    inner: asupersync::sync::MutexGuard<'a, T>,
}

#[cfg(feature = "asupersync-runtime")]
impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

#[cfg(feature = "asupersync-runtime")]
impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.deref_mut()
    }
}

#[cfg(feature = "asupersync-runtime")]
#[derive(Debug)]
pub struct RwLock<T> {
    inner: asupersync::sync::RwLock<T>,
}

#[cfg(feature = "asupersync-runtime")]
impl<T> RwLock<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            inner: asupersync::sync::RwLock::new(value),
        }
    }

    #[allow(clippy::future_not_send)] // asupersync RwLock is !Sync by design
    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        let cx = asupersync::Cx::for_testing();
        let guard = self
            .inner
            .read(&cx)
            .await
            .expect("runtime_compat rwlock read failed");
        RwLockReadGuard { inner: guard }
    }

    #[allow(clippy::future_not_send)] // asupersync RwLock is !Sync by design
    pub async fn write(&self) -> RwLockWriteGuard<'_, T> {
        let cx = asupersync::Cx::for_testing();
        let guard = self
            .inner
            .write(&cx)
            .await
            .expect("runtime_compat rwlock write failed");
        RwLockWriteGuard { inner: guard }
    }
}

#[cfg(feature = "asupersync-runtime")]
pub struct RwLockReadGuard<'a, T> {
    inner: asupersync::sync::RwLockReadGuard<'a, T>,
}

#[cfg(feature = "asupersync-runtime")]
impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

#[cfg(feature = "asupersync-runtime")]
pub struct RwLockWriteGuard<'a, T> {
    inner: asupersync::sync::RwLockWriteGuard<'a, T>,
}

#[cfg(feature = "asupersync-runtime")]
impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

#[cfg(feature = "asupersync-runtime")]
impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.deref_mut()
    }
}

#[cfg(feature = "asupersync-runtime")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryAcquireError {
    NoPermits,
    Closed,
}

#[cfg(feature = "asupersync-runtime")]
impl std::fmt::Display for TryAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPermits => write!(f, "no semaphore permits available"),
            Self::Closed => write!(f, "semaphore closed"),
        }
    }
}

#[cfg(feature = "asupersync-runtime")]
impl std::error::Error for TryAcquireError {}

#[cfg(feature = "asupersync-runtime")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquireError;

#[cfg(feature = "asupersync-runtime")]
impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "semaphore acquire failed")
    }
}

#[cfg(feature = "asupersync-runtime")]
impl std::error::Error for AcquireError {}

#[cfg(feature = "asupersync-runtime")]
#[derive(Debug)]
pub struct Semaphore {
    inner: Arc<asupersync::sync::Semaphore>,
}

#[cfg(feature = "asupersync-runtime")]
impl Semaphore {
    #[must_use]
    pub fn new(permits: usize) -> Self {
        Self {
            inner: Arc::new(asupersync::sync::Semaphore::new(permits)),
        }
    }

    #[must_use]
    pub fn available_permits(&self) -> usize {
        self.inner.available_permits()
    }

    pub fn close(&self) {
        self.inner.close();
    }

    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        if self.inner.is_closed() {
            return Err(TryAcquireError::Closed);
        }

        self.inner
            .try_acquire(1)
            .map(|inner| SemaphorePermit { inner })
            .map_err(|_| {
                if self.inner.is_closed() {
                    TryAcquireError::Closed
                } else {
                    TryAcquireError::NoPermits
                }
            })
    }

    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        let cx = asupersync::Cx::for_testing();
        self.inner
            .acquire(&cx, 1)
            .await
            .map(|inner| SemaphorePermit { inner })
            .map_err(|_| AcquireError)
    }

    pub fn try_acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        if self.inner.is_closed() {
            return Err(TryAcquireError::Closed);
        }

        asupersync::sync::OwnedSemaphorePermit::try_acquire(self.inner.clone(), 1)
            .map(|inner| OwnedSemaphorePermit { inner })
            .map_err(|_| {
                if self.inner.is_closed() {
                    TryAcquireError::Closed
                } else {
                    TryAcquireError::NoPermits
                }
            })
    }

    pub async fn acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, AcquireError> {
        let cx = asupersync::Cx::for_testing();
        asupersync::sync::OwnedSemaphorePermit::acquire(self.inner.clone(), &cx, 1)
            .await
            .map(|inner| OwnedSemaphorePermit { inner })
            .map_err(|_| AcquireError)
    }
}

#[cfg(feature = "asupersync-runtime")]
pub struct SemaphorePermit<'a> {
    inner: asupersync::sync::SemaphorePermit<'a>,
}

#[cfg(feature = "asupersync-runtime")]
impl SemaphorePermit<'_> {
    #[must_use]
    pub fn count(&self) -> usize {
        self.inner.count()
    }
}

#[cfg(feature = "asupersync-runtime")]
impl std::fmt::Debug for SemaphorePermit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemaphorePermit")
            .field("count", &self.count())
            .finish()
    }
}

#[cfg(feature = "asupersync-runtime")]
#[derive(Debug)]
pub struct OwnedSemaphorePermit {
    inner: asupersync::sync::OwnedSemaphorePermit,
}

#[cfg(feature = "asupersync-runtime")]
impl OwnedSemaphorePermit {
    #[must_use]
    pub fn count(&self) -> usize {
        self.inner.count()
    }
}

#[cfg(not(feature = "asupersync-runtime"))]
pub use tokio::sync::{
    AcquireError, Mutex, MutexGuard, OwnedSemaphorePermit, RwLock, RwLockReadGuard,
    RwLockWriteGuard, Semaphore, SemaphorePermit, TryAcquireError,
};

/// MPSC channel aliases for the active runtime.
#[cfg(feature = "asupersync-runtime")]
pub mod mpsc {
    pub use asupersync::channel::mpsc::{
        Receiver, RecvError, SendError, SendPermit, Sender, channel,
    };

    /// Compatibility alias for `try_send` errors, matching the tokio
    /// `TrySendError` API surface (`Full` / `Closed`).
    ///
    /// In asupersync the `try_send` method returns `SendError` which uses
    /// `Disconnected` instead of `Closed`. This wrapper bridges the naming
    /// gap so that call-sites can use `TrySendError::Full` / `Closed` uniformly.
    #[derive(Debug)]
    pub enum TrySendError<T> {
        /// The channel is full.
        Full(T),
        /// The receiver has been dropped.
        Closed(T),
    }

    impl<T> From<SendError<T>> for TrySendError<T> {
        fn from(err: SendError<T>) -> Self {
            match err {
                SendError::Full(v) => Self::Full(v),
                SendError::Disconnected(v) | SendError::Cancelled(v) => Self::Closed(v),
            }
        }
    }
}

/// MPSC channel aliases for the active runtime.
#[cfg(not(feature = "asupersync-runtime"))]
pub mod mpsc {
    pub use tokio::sync::mpsc::{
        Receiver, Sender, channel,
        error::{SendError, TryRecvError, TrySendError},
        unbounded_channel,
    };
}

/// Watch channel aliases for the active runtime.
#[cfg(feature = "asupersync-runtime")]
pub mod watch {
    pub use asupersync::channel::watch::{Receiver, RecvError, SendError, Sender, channel};
}

/// Watch channel aliases for the active runtime.
#[cfg(not(feature = "asupersync-runtime"))]
pub mod watch {
    pub use tokio::sync::watch::{
        Receiver, Sender, channel,
        error::{RecvError, SendError},
    };
}

/// Broadcast channel aliases for the active runtime.
///
/// Note: this remains tokio-backed while the broader broadcast migration is
/// completed; exposing it via runtime_compat centralizes call sites.
pub mod broadcast {
    pub use tokio::sync::broadcast::{
        Receiver, Sender, channel,
        error::{RecvError, SendError, TryRecvError},
    };
}

/// Oneshot channel aliases for the active runtime.
///
/// Note: this remains tokio-backed while the broader oneshot migration is
/// completed; exposing it via runtime_compat centralizes call sites.
pub mod oneshot {
    pub use tokio::sync::oneshot::{Receiver, Sender, channel, error::RecvError};
}

/// Async notification primitive for the active runtime.
///
/// Note: this remains tokio-backed while the broader sync-primitive migration
/// is completed; exposing it via runtime_compat centralizes call sites.
pub mod notify {
    pub use tokio::sync::Notify;
}

/// Task primitives used during runtime migration.
///
/// When `asupersync-runtime` is enabled, spawns on the asupersync runtime
/// via the thread-local handle installed by `Runtime::block_on`. Otherwise,
/// delegates to tokio.
#[cfg(not(feature = "asupersync-runtime"))]
pub mod task {
    pub use tokio::task::{JoinError, JoinHandle, JoinSet};

    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(future)
    }

    /// Spawns blocking work on the runtime's dedicated blocking thread pool,
    /// returning a `JoinHandle` that can be awaited, aborted, or used in
    /// `select!`.
    ///
    /// Use this when callers need direct `JoinHandle` control (e.g. `.abort()`).
    /// For fire-and-forget blocking work, prefer the top-level
    /// [`super::spawn_blocking`] helper which returns `Result<T, String>`.
    pub fn spawn_blocking<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(f)
    }

    /// Yields execution back to the runtime, allowing other tasks to progress.
    pub async fn yield_now() {
        tokio::task::yield_now().await;
    }
}

/// Task primitives for the asupersync runtime backend.
///
/// Provides API-compatible wrappers around asupersync's spawn/join
/// infrastructure, using the thread-local `ASUPERSYNC_HANDLE` installed
/// by `Runtime::block_on` to support ambient spawning.
#[cfg(feature = "asupersync-runtime")]
pub mod task {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Error type returned when a spawned task fails.
    ///
    /// Wraps asupersync's join error to provide a compatible API surface.
    #[derive(Debug)]
    pub struct JoinError {
        msg: String,
    }

    impl std::fmt::Display for JoinError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "JoinError: {}", self.msg)
        }
    }

    impl std::error::Error for JoinError {}

    /// Handle to a spawned task. Awaiting it yields the task's output
    /// wrapped in `Result<T, JoinError>` for API compatibility with tokio.
    ///
    /// Uses `Pin<Box<_>>` internally to avoid unsafe pin projection while
    /// maintaining `#![forbid(unsafe_code)]` compliance.
    pub struct JoinHandle<T> {
        inner: Pin<Box<asupersync::runtime::JoinHandle<T>>>,
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.inner.as_mut().poll(cx) {
                Poll::Ready(value) => Poll::Ready(Ok(value)),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl<T> JoinHandle<T> {
        /// Returns `true` if the task has completed.
        pub fn is_finished(&self) -> bool {
            // is_finished is available on the inner JoinHandle via deref
            // through Pin<Box<_>>
            self.inner.is_finished()
        }

        /// Request cancellation of the task.
        ///
        /// Note: asupersync uses context-based cancellation; this is a
        /// best-effort no-op provided for API compatibility with tokio.
        pub fn abort(&self) {
            // asupersync JoinHandle does not support abort; cancellation
            // is driven through Cx. This is a no-op for compatibility.
        }
    }

    /// Minimal JoinSet implementation backed by a Vec of JoinHandles.
    ///
    /// Provides the subset of tokio::task::JoinSet API used in frankenterm.
    pub struct JoinSet<T> {
        handles: Vec<JoinHandle<T>>,
    }

    impl<T: Send + 'static> JoinSet<T> {
        pub fn new() -> Self {
            Self {
                handles: Vec::new(),
            }
        }

        pub fn spawn<F>(&mut self, future: F)
        where
            F: Future<Output = T> + Send + 'static,
        {
            self.handles.push(super::task::spawn(future));
        }

        pub fn len(&self) -> usize {
            self.handles.len()
        }

        pub fn is_empty(&self) -> bool {
            self.handles.is_empty()
        }

        /// Await the next completed task. Returns `None` if the set is empty.
        pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
            if self.handles.is_empty() {
                return None;
            }
            // Simple approach: await the last handle (LIFO)
            let handle = self.handles.pop().unwrap();
            Some(handle.await)
        }

        /// Non-blocking poll for the next completed task.
        ///
        /// Checks if any handle is finished and returns its result.
        /// Returns `None` if the set is empty or no task has completed.
        pub fn try_join_next(&mut self) -> Option<Result<T, JoinError>> {
            // Find the first finished handle
            let pos = self.handles.iter().position(|h| h.is_finished());
            if let Some(idx) = pos {
                let handle = self.handles.swap_remove(idx);
                // Task is finished, so we can poll it synchronously via a noop waker
                let waker = futures::task::noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                let mut pinned = std::pin::pin!(handle);
                match pinned.as_mut().poll(&mut cx) {
                    std::task::Poll::Ready(result) => Some(result),
                    std::task::Poll::Pending => None, // shouldn't happen for finished task
                }
            } else {
                None
            }
        }

        /// Cancel all tasks in the set.
        ///
        /// Under asupersync, abort is a no-op (context-based cancellation).
        /// This clears the handle set to release resources.
        pub fn abort_all(&mut self) {
            // asupersync doesn't support handle-based abort.
            // Best effort: drop all handles (tasks continue to completion).
            self.handles.clear();
        }
    }

    /// Spawn a future on the current asupersync runtime.
    ///
    /// Uses the thread-local `ASUPERSYNC_HANDLE` installed by
    /// `Runtime::block_on`. Panics if called outside a runtime context.
    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let inner = super::ASUPERSYNC_HANDLE.with(|cell| {
            let borrow = cell.borrow();
            let handle = borrow
                .as_ref()
                .expect("task::spawn called outside of Runtime::block_on context");
            handle.spawn(future)
        });
        JoinHandle {
            inner: Box::pin(inner),
        }
    }

    /// Spawns blocking work on the runtime's blocking thread pool.
    ///
    /// Returns a `JoinHandle` for API compatibility. Under asupersync,
    /// this delegates to `asupersync::runtime::spawn_blocking`.
    pub fn spawn_blocking<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // For spawn_blocking we need to wrap it differently since
        // asupersync's spawn_blocking is async and returns T directly.
        // We spawn a task that calls spawn_blocking internally.
        spawn(async move { asupersync::runtime::spawn_blocking(f).await })
    }

    /// Yields execution back to the runtime, allowing other tasks to progress.
    pub async fn yield_now() {
        asupersync::runtime::yield_now().await;
    }
}

/// Re-export `join!` macro for concurrent future evaluation.
///
/// Uses `futures::join!` under asupersync-runtime (runtime-agnostic),
/// and `tokio::join!` otherwise.
#[cfg(feature = "asupersync-runtime")]
pub use futures::join;
#[cfg(not(feature = "asupersync-runtime"))]
pub use tokio::join;

/// Re-export `select!` macro for multiplexing futures.
///
/// Uses `tokio::select!` in both paths during migration. The asupersync
/// path will be replaced with a runtime-agnostic implementation once all
/// select call-sites are audited.
// TODO(asupersync): replace with asupersync-native select when available
#[cfg(feature = "asupersync-runtime")]
pub use tokio::select;
#[cfg(not(feature = "asupersync-runtime"))]
pub use tokio::select;

/// Time-control primitives for deterministic test scheduling.
///
/// These are primarily used in `#[tokio::test(start_paused = true)]` tests
/// to drive time manually. Requires tokio's `test-util` feature, which is
/// only available in test builds.
#[cfg(test)]
#[cfg(not(feature = "asupersync-runtime"))]
pub mod time {
    use std::time::Duration;

    /// Pauses the runtime's time driver so that `sleep` and `timeout`
    /// only resolve when time is manually advanced.
    pub fn pause() {
        tokio::time::pause();
    }

    /// Advances the runtime clock by the given duration.
    ///
    /// Only effective after `pause()` has been called.
    pub async fn advance(duration: Duration) {
        tokio::time::advance(duration).await;
    }
}

/// Unix socket aliases/helpers for the active runtime.
#[cfg(feature = "asupersync-runtime")]
pub mod unix {
    use std::io;
    use std::path::Path;

    pub use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
    pub use asupersync::net::{UnixListener, UnixStream};

    pub type LineReader<T> = asupersync::io::Lines<BufReader<T>>;

    pub async fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
        let path = path.as_ref();
        let _ = std::fs::remove_file(path);
        UnixListener::bind(path).await
    }

    pub async fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
        UnixStream::connect(path).await
    }

    #[must_use]
    pub fn buffered<T: AsyncRead>(stream: T) -> BufReader<T> {
        BufReader::new(stream)
    }

    #[must_use]
    pub fn lines<T>(reader: BufReader<T>) -> LineReader<T>
    where
        T: AsyncRead + Unpin,
    {
        asupersync::io::Lines::new(reader)
    }

    pub async fn next_line<T>(lines: &mut LineReader<T>) -> io::Result<Option<String>>
    where
        T: AsyncRead + Unpin,
    {
        use asupersync::stream::StreamExt;

        match lines.next().await {
            Some(Ok(line)) => Ok(Some(line)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }
}

/// Unix socket aliases/helpers for the active runtime.
#[cfg(not(feature = "asupersync-runtime"))]
pub mod unix {
    use std::io;
    use std::path::Path;

    pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
    pub use tokio::net::{UnixListener, UnixStream};

    pub type LineReader<T> = tokio::io::Lines<BufReader<T>>;

    pub async fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
        let path = path.as_ref();
        let _ = std::fs::remove_file(path);
        UnixListener::bind(path)
    }

    pub async fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
        UnixStream::connect(path).await
    }

    #[must_use]
    pub fn buffered<T: AsyncRead>(stream: T) -> BufReader<T> {
        BufReader::new(stream)
    }

    pub fn lines<T>(reader: BufReader<T>) -> LineReader<T>
    where
        T: AsyncRead + Unpin,
    {
        use tokio::io::AsyncBufReadExt;
        reader.lines()
    }

    pub async fn next_line<T>(lines: &mut LineReader<T>) -> io::Result<Option<String>>
    where
        T: AsyncRead + Unpin,
    {
        lines.next_line().await
    }
}

/// Async process primitives for the active runtime.
///
/// Centralizes tokio::process usage so asupersync can swap it later.
pub mod process {
    pub use tokio::process::Command;
}

/// Async I/O traits for the active runtime.
///
/// Re-exports the extension traits needed for TCP stream I/O.
/// For Unix-specific I/O (BufReader, lines, etc.) see the `unix` module.
#[cfg(feature = "asupersync-runtime")]
pub mod io {
    pub use asupersync::io::{AsyncReadExt, AsyncWriteExt};

    /// Read some bytes from an async reader into `buf`, returning how many
    /// bytes were read. Polyfill for tokio's `AsyncReadExt::read` which
    /// asupersync does not yet provide.
    pub async fn read<R: asupersync::io::AsyncRead + Unpin>(
        reader: &mut R,
        buf: &mut [u8],
    ) -> std::io::Result<usize> {
        std::future::poll_fn(|cx| {
            let mut read_buf = asupersync::io::ReadBuf::new(buf);
            match std::pin::Pin::new(&mut *reader).poll_read(cx, &mut read_buf) {
                std::task::Poll::Ready(Ok(())) => {
                    std::task::Poll::Ready(Ok(read_buf.filled().len()))
                }
                std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        })
        .await
    }
}

/// Async I/O traits for the active runtime.
///
/// Re-exports the extension traits needed for TCP stream I/O.
/// For Unix-specific I/O (BufReader, lines, etc.) see the `unix` module.
#[cfg(not(feature = "asupersync-runtime"))]
pub mod io {
    pub use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Read some bytes from an async reader into `buf`, returning how many
    /// bytes were read. Delegates to `AsyncReadExt::read`.
    pub async fn read<R: tokio::io::AsyncRead + Unpin>(
        reader: &mut R,
        buf: &mut [u8],
    ) -> std::io::Result<usize> {
        <R as tokio::io::AsyncReadExt>::read(reader, buf).await
    }
}

/// Async networking primitives for the active runtime.
///
/// For Unix sockets, see the `unix` module.
#[cfg(feature = "asupersync-runtime")]
pub mod net {
    pub use asupersync::net::{TcpListener, TcpStream};
}

/// Async networking primitives for the active runtime.
///
/// For Unix sockets, see the `unix` module.
#[cfg(not(feature = "asupersync-runtime"))]
pub mod net {
    pub use tokio::net::{TcpListener, TcpStream};
}

/// Signal handling primitives for graceful shutdown.
///
/// Wraps `asupersync::signal` for the asupersync runtime.
#[cfg(feature = "asupersync-runtime")]
pub mod signal {
    /// Completes when a Ctrl+C (SIGINT) signal is received.
    ///
    /// # Errors
    ///
    /// Returns an error if the signal handler could not be registered.
    pub async fn ctrl_c() -> std::io::Result<()> {
        asupersync::signal::ctrl_c().await
    }

    /// Unix-specific signal handling.
    #[cfg(unix)]
    pub mod unix {
        /// Signal kinds for Unix signal handling.
        pub struct SignalKind(asupersync::signal::SignalKind);

        impl SignalKind {
            /// Returns the `SIGTERM` signal kind.
            pub fn terminate() -> Self {
                Self(asupersync::signal::SignalKind::terminate())
            }
        }

        /// A stream of signals of a specific kind.
        pub struct Signal {
            inner: asupersync::signal::Signal,
        }

        impl Signal {
            /// Receives the next signal notification.
            ///
            /// Returns `None` if the signal stream is terminated.
            pub async fn recv(&mut self) -> Option<()> {
                self.inner.recv().await
            }
        }

        /// Creates a new listener for the given signal kind.
        ///
        /// # Errors
        ///
        /// Returns an error if the signal handler could not be registered.
        pub fn signal(kind: SignalKind) -> std::io::Result<Signal> {
            asupersync::signal::signal(kind.0).map(|inner| Signal { inner })
        }
    }
}

/// Signal handling primitives for graceful shutdown.
///
/// Wraps `tokio::signal` in the default build for eventual asupersync swap.
#[cfg(not(feature = "asupersync-runtime"))]
pub mod signal {
    /// Completes when a Ctrl+C (SIGINT) signal is received.
    ///
    /// # Errors
    ///
    /// Returns an error if the signal handler could not be registered.
    pub async fn ctrl_c() -> std::io::Result<()> {
        tokio::signal::ctrl_c().await
    }

    /// Unix-specific signal handling.
    #[cfg(unix)]
    pub mod unix {
        pub use tokio::signal::unix::SignalKind;

        /// A stream of signals of a specific kind.
        pub struct Signal {
            inner: tokio::signal::unix::Signal,
        }

        impl Signal {
            /// Receives the next signal notification.
            ///
            /// Returns `None` if the signal stream is terminated.
            pub async fn recv(&mut self) -> Option<()> {
                self.inner.recv().await
            }
        }

        /// Creates a new listener for the given signal kind.
        ///
        /// # Errors
        ///
        /// Returns an error if the signal handler could not be registered.
        pub fn signal(kind: SignalKind) -> std::io::Result<Signal> {
            tokio::signal::unix::signal(kind).map(|inner| Signal { inner })
        }
    }
}

/// Re-export of `tokio::task::JoinError` for task join handle error handling.
#[cfg(not(feature = "asupersync-runtime"))]
pub use tokio::task::JoinError;

/// Unified runtime trait used during migration.
pub trait CompatRuntime {
    /// Runs a future to completion.
    fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future;

    /// Spawns a detached task.
    fn spawn_detached<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static;
}

/// Runtime wrapper for the active runtime backend.
#[cfg(feature = "asupersync-runtime")]
pub struct Runtime {
    inner: asupersync::runtime::Runtime,
}

/// Runtime wrapper for the active runtime backend.
#[cfg(not(feature = "asupersync-runtime"))]
pub struct Runtime {
    inner: tokio::runtime::Runtime,
}

#[cfg(feature = "asupersync-runtime")]
impl CompatRuntime for Runtime {
    fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        // Install the RuntimeHandle into thread-local storage so that
        // task::spawn can find it without requiring callers to pass it
        // explicitly. This mirrors tokio's ambient runtime context.
        let handle = self.inner.handle();
        ASUPERSYNC_HANDLE.with(|cell| cell.replace(Some(handle)));
        let result = self.inner.block_on(future);
        ASUPERSYNC_HANDLE.with(|cell| cell.replace(None));
        result
    }

    fn spawn_detached<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = self.inner.handle();
        std::mem::drop(handle.spawn(future));
    }
}

#[cfg(not(feature = "asupersync-runtime"))]
impl CompatRuntime for Runtime {
    fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        self.inner.block_on(future)
    }

    fn spawn_detached<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        std::mem::drop(self.inner.spawn(future));
    }
}

/// Runtime builder wrapper for the active backend.
#[cfg(feature = "asupersync-runtime")]
pub struct RuntimeBuilder {
    inner: asupersync::runtime::RuntimeBuilder,
}

#[cfg(feature = "asupersync-runtime")]
impl RuntimeBuilder {
    #[must_use]
    pub fn current_thread() -> Self {
        Self {
            inner: asupersync::runtime::RuntimeBuilder::current_thread(),
        }
    }

    #[must_use]
    pub fn multi_thread() -> Self {
        Self {
            inner: asupersync::runtime::RuntimeBuilder::new(),
        }
    }

    #[must_use]
    pub fn worker_threads(self, n: usize) -> Self {
        Self {
            inner: self.inner.worker_threads(n),
        }
    }

    /// No-op: asupersync handles I/O and timers automatically.
    #[must_use]
    pub fn enable_all(self) -> Self {
        self
    }

    /// No-op: thread naming is not exposed in asupersync.
    #[must_use]
    pub fn thread_name(self, _name: &str) -> Self {
        self
    }

    pub fn build(self) -> Result<Runtime, String> {
        self.inner
            .build()
            .map(|inner| Runtime { inner })
            .map_err(|err| err.to_string())
    }
}

/// Runtime builder wrapper for the active backend.
#[cfg(not(feature = "asupersync-runtime"))]
pub struct RuntimeBuilder {
    inner: tokio::runtime::Builder,
    supports_worker_threads: bool,
}

#[cfg(not(feature = "asupersync-runtime"))]
impl RuntimeBuilder {
    #[must_use]
    pub fn current_thread() -> Self {
        let mut inner = tokio::runtime::Builder::new_current_thread();
        inner.enable_all();
        Self {
            inner,
            supports_worker_threads: false,
        }
    }

    #[must_use]
    pub fn multi_thread() -> Self {
        let mut inner = tokio::runtime::Builder::new_multi_thread();
        inner.enable_all();
        Self {
            inner,
            supports_worker_threads: true,
        }
    }

    #[must_use]
    pub fn worker_threads(mut self, n: usize) -> Self {
        if self.supports_worker_threads {
            self.inner.worker_threads(n);
        }
        self
    }

    /// No-op: `enable_all()` is already called in the constructors.
    #[must_use]
    pub fn enable_all(self) -> Self {
        self
    }

    /// Sets the thread name for spawned worker threads.
    #[must_use]
    pub fn thread_name(mut self, name: &str) -> Self {
        self.inner.thread_name(name);
        self
    }

    pub fn build(mut self) -> Result<Runtime, String> {
        self.inner
            .build()
            .map(|inner| Runtime { inner })
            .map_err(|err| err.to_string())
    }
}

/// Sleep for the specified duration using the active runtime backend.
#[cfg(feature = "asupersync-runtime")]
pub async fn sleep(duration: Duration) {
    asupersync::time::sleep(asupersync::time::wall_now(), duration).await;
}

/// Sleep for the specified duration using the active runtime backend.
#[cfg(not(feature = "asupersync-runtime"))]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// Runs `future` with a timeout using the active runtime backend.
#[cfg(feature = "asupersync-runtime")]
pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, String>
where
    F: Future,
{
    asupersync::time::timeout(asupersync::time::wall_now(), duration, Box::pin(future))
        .await
        .map_err(|err| err.to_string())
}

/// Runs `future` with a timeout using the active runtime backend.
#[cfg(not(feature = "asupersync-runtime"))]
pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, String>
where
    F: Future,
{
    tokio::time::timeout(duration, future)
        .await
        .map_err(|err| err.to_string())
}

/// Runs blocking work on the active runtime's blocking executor.
///
/// Returns the closure output when successful, or a stringified join/runtime
/// error when the blocking task could not complete.
pub async fn spawn_blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    #[cfg(feature = "asupersync-runtime")]
    {
        Ok(asupersync::runtime::spawn_blocking(work).await)
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|err| err.to_string())
    }
}

/// Receives one message from an mpsc receiver, normalized to Option semantics.
///
/// Returns:
/// - `Some(value)` when a message was received.
/// - `None` when the channel is closed.
///
/// Transitional helper retained for migration-era tests. New production
/// call-sites should prefer explicit receive semantics.
pub async fn mpsc_recv_option<T>(rx: &mut mpsc::Receiver<T>) -> Option<T> {
    #[cfg(feature = "asupersync-runtime")]
    {
        let cx = crate::cx::for_testing();
        rx.recv(&cx).await.ok()
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        rx.recv().await
    }
}

/// Sends one message through an mpsc sender using the active runtime semantics.
///
/// Transitional helper retained for migration-era tests. New production
/// call-sites should prefer explicit send semantics.
pub async fn mpsc_send<T>(tx: &mpsc::Sender<T>, value: T) -> Result<(), mpsc::SendError<T>> {
    #[cfg(feature = "asupersync-runtime")]
    {
        let cx = crate::cx::for_testing();
        tx.send(&cx, value).await
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        tx.send(value).await
    }
}

/// Checks whether a watch receiver has observed a new value.
///
/// Returns `false` if the channel has closed.
pub fn watch_has_changed<T>(rx: &watch::Receiver<T>) -> bool {
    #[cfg(feature = "asupersync-runtime")]
    {
        rx.has_changed()
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        rx.has_changed().unwrap_or(false)
    }
}

/// Borrows the latest watch value and clones it while marking the update as
/// consumed where required by the active runtime backend.
pub fn watch_borrow_and_update_clone<T: Clone>(rx: &mut watch::Receiver<T>) -> T {
    #[cfg(feature = "asupersync-runtime")]
    {
        rx.borrow_and_clone()
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        rx.borrow_and_update().clone()
    }
}

/// Waits until the watch receiver observes a change, abstracting the
/// `&Cx` parameter required by asupersync.
///
/// Returns `Ok(())` on change, `Err(RecvError)` if the sender was dropped.
pub async fn watch_changed<T: Send + Sync>(
    rx: &mut watch::Receiver<T>,
) -> Result<(), watch::RecvError> {
    #[cfg(feature = "asupersync-runtime")]
    {
        let cx = crate::cx::for_testing();
        rx.changed(&cx).await
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        rx.changed().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    #[test]
    fn surface_contract_entries_are_unique() {
        let mut seen = HashSet::new();
        for entry in SURFACE_CONTRACT_V1 {
            assert!(
                seen.insert(entry.api),
                "duplicate surface contract entry: {}",
                entry.api
            );
        }
    }

    #[test]
    fn surface_contract_replacements_are_explicit() {
        for entry in SURFACE_CONTRACT_V1 {
            if matches!(
                entry.disposition,
                SurfaceDisposition::Replace | SurfaceDisposition::Retire
            ) {
                assert!(
                    entry.replacement.is_some(),
                    "missing replacement for {}",
                    entry.api
                );
            }
        }
    }

    #[test]
    fn surface_contract_marks_task_spawn_blocking_as_replace() {
        let entry = SURFACE_CONTRACT_V1
            .iter()
            .find(|entry| entry.api == "task::spawn_blocking")
            .expect("task::spawn_blocking entry must exist");
        assert_eq!(entry.disposition, SurfaceDisposition::Replace);
    }

    #[test]
    fn runtime_builder_current_thread_builds() {
        let rt = RuntimeBuilder::current_thread().build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_multi_thread_builds() {
        let rt = RuntimeBuilder::multi_thread().build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_worker_threads_chainable() {
        let rt = RuntimeBuilder::multi_thread().worker_threads(2).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_current_thread_ignores_worker_threads() {
        // current_thread doesn't support worker_threads; should not panic
        let rt = RuntimeBuilder::current_thread().worker_threads(4).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn compat_runtime_block_on_runs_future() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async { 42 });
        assert_eq!(result, 42);
    }

    #[test]
    fn compat_runtime_spawn_detached_does_not_panic() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {
            // Can't directly test the spawned task completes, but ensure no panic
        });
        rt.spawn_detached(async {});
    }

    #[tokio::test]
    async fn sleep_completes() {
        let start = std::time::Instant::now();
        sleep(Duration::from_millis(10)).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(5));
    }

    #[tokio::test]
    async fn timeout_succeeds_before_deadline() {
        let result = timeout(Duration::from_secs(1), async { 99 }).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 99);
    }

    #[tokio::test]
    async fn timeout_expires_returns_error() {
        let result = timeout(Duration::from_millis(10), async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            42
        })
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn block_on_with_async_value() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let value = rt.block_on(async {
            let a = 10;
            let b = 20;
            a + b
        });
        assert_eq!(value, 30);
    }

    #[test]
    fn multi_thread_runtime_block_on() {
        let rt = RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let value = rt.block_on(async { "hello" });
        assert_eq!(value, "hello");
    }

    // ========================================================================
    // Mutex tests
    // ========================================================================

    #[tokio::test]
    async fn mutex_lock_and_read() {
        let m = Mutex::new(42);
        let guard = m.lock().await;
        assert_eq!(*guard, 42);
    }

    #[tokio::test]
    async fn mutex_lock_and_mutate() {
        let m = Mutex::new(0);
        {
            let mut guard = m.lock().await;
            *guard = 99;
        }
        let guard = m.lock().await;
        assert_eq!(*guard, 99);
    }

    #[tokio::test]
    async fn mutex_sequential_locks() {
        let m = Mutex::new(vec![1, 2, 3]);
        {
            let mut guard = m.lock().await;
            guard.push(4);
        }
        let guard = m.lock().await;
        assert_eq!(*guard, vec![1, 2, 3, 4]);
    }

    // ========================================================================
    // RwLock tests
    // ========================================================================

    #[tokio::test]
    async fn rwlock_read() {
        let rw = RwLock::new("hello".to_string());
        let guard = rw.read().await;
        assert_eq!(&*guard, "hello");
    }

    #[tokio::test]
    async fn rwlock_write() {
        let rw = RwLock::new(0);
        {
            let mut guard = rw.write().await;
            *guard = 42;
        }
        let guard = rw.read().await;
        assert_eq!(*guard, 42);
    }

    #[tokio::test]
    async fn rwlock_multiple_sequential_readers() {
        let rw = RwLock::new(100);
        let r1 = rw.read().await;
        assert_eq!(*r1, 100);
        drop(r1);
        let r2 = rw.read().await;
        assert_eq!(*r2, 100);
    }

    // ========================================================================
    // Semaphore tests
    // ========================================================================

    #[tokio::test]
    async fn semaphore_available_permits() {
        let sem = Semaphore::new(3);
        assert_eq!(sem.available_permits(), 3);
    }

    #[tokio::test]
    async fn semaphore_acquire_decrements_permits() {
        let sem = Semaphore::new(2);
        let _p1 = sem.acquire().await.expect("acquire 1");
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn semaphore_release_on_drop() {
        let sem = Semaphore::new(1);
        {
            let _p = sem.acquire().await.expect("acquire");
            assert_eq!(sem.available_permits(), 0);
        }
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn semaphore_try_acquire_success() {
        let sem = Semaphore::new(1);
        let p = sem.try_acquire();
        assert!(p.is_ok());
    }

    #[tokio::test]
    async fn semaphore_try_acquire_no_permits() {
        let sem = Semaphore::new(1);
        let _held = sem.acquire().await.expect("acquire");
        let err = sem.try_acquire();
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn semaphore_try_acquire_owned_success() {
        let sem = std::sync::Arc::new(Semaphore::new(2));
        let p = sem.clone().try_acquire_owned();
        assert!(p.is_ok());
    }

    #[tokio::test]
    async fn semaphore_try_acquire_owned_no_permits() {
        let sem = std::sync::Arc::new(Semaphore::new(1));
        let _held = sem.clone().acquire_owned().await.expect("acquire");
        let err = sem.clone().try_acquire_owned();
        assert!(err.is_err());
    }

    // ========================================================================
    // MPSC channel tests
    // ========================================================================

    #[tokio::test]
    async fn mpsc_send_recv() {
        let (tx, mut rx) = mpsc::channel(10);
        #[cfg(feature = "asupersync-runtime")]
        {
            let cx = asupersync::Cx::for_testing();
            tx.send(&cx, 42).await.expect("send");
            let val = rx.recv(&cx).await.expect("recv");
            assert_eq!(val, 42);
        }
        #[cfg(not(feature = "asupersync-runtime"))]
        {
            tx.send(42).await.expect("send");
            let val = rx.recv().await.expect("recv");
            assert_eq!(val, 42);
        }
    }

    #[tokio::test]
    async fn mpsc_multiple_messages_fifo() {
        let (tx, mut rx) = mpsc::channel(10);
        #[cfg(feature = "asupersync-runtime")]
        {
            let cx = asupersync::Cx::for_testing();
            for i in 0..5 {
                tx.send(&cx, i).await.expect("send");
            }
        }
        #[cfg(not(feature = "asupersync-runtime"))]
        {
            for i in 0..5 {
                tx.send(i).await.expect("send");
            }
        }
        for i in 0..5 {
            #[cfg(feature = "asupersync-runtime")]
            {
                let cx = asupersync::Cx::for_testing();
                let val = rx.recv(&cx).await.expect("recv");
                assert_eq!(val, i);
            }
            #[cfg(not(feature = "asupersync-runtime"))]
            {
                let val = rx.recv().await.expect("recv");
                assert_eq!(val, i);
            }
        }
    }

    #[tokio::test]
    async fn mpsc_send_and_recv_option_helpers_roundtrip() {
        let (tx, mut rx) = mpsc::channel(4);
        mpsc_send(&tx, 7).await.expect("send helper");
        let got = mpsc_recv_option(&mut rx).await;
        assert_eq!(got, Some(7));
    }

    #[tokio::test]
    async fn mpsc_recv_option_helper_returns_none_when_closed() {
        let (tx, mut rx) = mpsc::channel::<u8>(1);
        drop(tx);
        let got = mpsc_recv_option(&mut rx).await;
        assert_eq!(got, None);
    }

    // ========================================================================
    // Watch channel tests
    // ========================================================================

    #[tokio::test]
    async fn watch_initial_value() {
        let (_, rx) = watch::channel(42);
        assert_eq!(*rx.borrow(), 42);
    }

    #[tokio::test]
    async fn watch_send_updates_value() {
        let (tx, rx) = watch::channel(0);
        tx.send(99).expect("send");
        assert_eq!(*rx.borrow(), 99);
    }

    #[tokio::test]
    async fn watch_has_changed_detects_new_value() {
        let (tx, mut rx) = watch::channel(0u32);
        assert!(!watch_has_changed(&rx));
        tx.send(5).expect("send");
        assert!(watch_has_changed(&rx));
        let latest = watch_borrow_and_update_clone(&mut rx);
        assert_eq!(latest, 5);
    }

    #[tokio::test]
    async fn watch_has_changed_handles_closed_channel() {
        let (tx, rx) = watch::channel(42u32);
        drop(tx);
        assert!(!watch_has_changed(&rx));
    }

    #[tokio::test]
    async fn watch_borrow_and_update_clone_returns_latest_value() {
        let (tx, mut rx) = watch::channel(vec![1u8, 2u8]);
        tx.send(vec![3u8, 4u8]).expect("send");
        let latest = watch_borrow_and_update_clone(&mut rx);
        assert_eq!(latest, vec![3u8, 4u8]);
    }

    // ========================================================================
    // Broadcast channel tests
    // ========================================================================

    #[tokio::test]
    async fn broadcast_send_recv() {
        let (tx, mut rx) = broadcast::channel(16);
        tx.send(42).expect("send");
        let val = rx.recv().await.expect("recv");
        assert_eq!(val, 42);
    }

    #[tokio::test]
    async fn broadcast_multiple_receivers() {
        let (tx, mut rx1) = broadcast::channel(16);
        let mut rx2 = tx.subscribe();
        tx.send(7).expect("send");
        assert_eq!(rx1.recv().await.expect("r1"), 7);
        assert_eq!(rx2.recv().await.expect("r2"), 7);
    }

    // ========================================================================
    // Sleep and timeout edge cases
    // ========================================================================

    #[tokio::test]
    async fn sleep_zero_duration_completes_immediately() {
        let start = std::time::Instant::now();
        sleep(Duration::ZERO).await;
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn timeout_with_immediate_future() {
        let result = timeout(Duration::from_millis(100), async { "fast" }).await;
        assert_eq!(result.unwrap(), "fast");
    }

    #[tokio::test]
    async fn timeout_error_is_string() {
        let result = timeout(Duration::from_millis(1), async {
            tokio::time::sleep(Duration::from_secs(10)).await;
        })
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(!err.is_empty());
    }

    // ========================================================================
    // CompatRuntime trait tests
    // ========================================================================

    #[test]
    fn block_on_returns_complex_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Vec<i32> = rt.block_on(async { vec![1, 2, 3] });
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[test]
    fn spawn_detached_accepts_send_future() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        rt.block_on(async {});
        rt.spawn_detached(async {});
    }

    // ========================================================================
    // NEW TESTS: RuntimeBuilder edge cases
    // ========================================================================

    #[test]
    fn runtime_builder_worker_threads_one() {
        // Minimum meaningful worker thread count
        let rt = RuntimeBuilder::multi_thread().worker_threads(1).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_multi_thread_without_worker_threads_uses_default() {
        // multi_thread without explicit worker_threads should use system default
        let rt = RuntimeBuilder::multi_thread().build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_current_thread_ignores_worker_threads_one() {
        // current_thread silently ignores worker_threads(1)
        let rt = RuntimeBuilder::current_thread().worker_threads(1).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_current_thread_worker_threads_large() {
        // current_thread should silently ignore even large worker_threads values
        let rt = RuntimeBuilder::current_thread().worker_threads(128).build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_build_returns_result() {
        // Verify the build() return type is Result<Runtime, String>
        let result: Result<Runtime, String> = RuntimeBuilder::current_thread().build();
        assert!(result.is_ok());
    }

    // ========================================================================
    // NEW TESTS: CompatRuntime block_on edge cases
    // ========================================================================

    #[test]
    fn block_on_returns_unit() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: () = rt.block_on(async {});
        assert_eq!(result, ());
    }

    #[test]
    fn block_on_returns_result_ok() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Result<i32, String> = rt.block_on(async { Ok(42) });
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn block_on_returns_result_err() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Result<i32, String> = rt.block_on(async { Err("oops".to_string()) });
        assert_eq!(result.unwrap_err(), "oops");
    }

    #[test]
    fn block_on_returns_option_some() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Option<u64> = rt.block_on(async { Some(100) });
        assert_eq!(result, Some(100));
    }

    #[test]
    fn block_on_returns_option_none() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result: Option<u64> = rt.block_on(async { None });
        assert_eq!(result, None);
    }

    #[test]
    fn block_on_with_string_type() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async { String::from("async string") });
        assert_eq!(result, "async string");
    }

    #[test]
    fn block_on_with_nested_async_computation() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async {
            let a = async { 10 }.await;
            let b = async { 20 }.await;
            a + b
        });
        assert_eq!(result, 30);
    }

    #[test]
    fn multi_thread_block_on_returns_tuple() {
        let rt = RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let (a, b) = rt.block_on(async { (1, "two") });
        assert_eq!(a, 1);
        assert_eq!(b, "two");
    }

    #[test]
    fn spawn_detached_from_multi_thread_runtime() {
        let rt = RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        // Should not panic even from multi-threaded runtime
        rt.spawn_detached(async {});
    }

    // ========================================================================
    // NEW TESTS: Mutex edge cases
    // ========================================================================

    #[tokio::test]
    async fn mutex_with_string_type() {
        let m = Mutex::new(String::from("initial"));
        {
            let mut guard = m.lock().await;
            guard.push_str(" modified");
        }
        let guard = m.lock().await;
        assert_eq!(&*guard, "initial modified");
    }

    #[tokio::test]
    async fn mutex_with_hashmap() {
        use std::collections::HashMap;
        let m = Mutex::new(HashMap::new());
        {
            let mut guard = m.lock().await;
            guard.insert("key", 42);
        }
        let guard = m.lock().await;
        assert_eq!(guard.get("key"), Some(&42));
    }

    #[tokio::test]
    async fn mutex_with_option_type() {
        let m = Mutex::new(None::<u32>);
        {
            let mut guard = m.lock().await;
            *guard = Some(7);
        }
        let guard = m.lock().await;
        assert_eq!(*guard, Some(7));
    }

    #[tokio::test]
    async fn mutex_multiple_lock_unlock_cycles() {
        let m = Mutex::new(0u64);
        for i in 0..10 {
            let mut guard = m.lock().await;
            *guard = i;
        }
        let guard = m.lock().await;
        assert_eq!(*guard, 9);
    }

    #[tokio::test]
    async fn mutex_deref_read_access() {
        let m = Mutex::new(vec![10, 20, 30]);
        let guard = m.lock().await;
        // Test Deref: can call Vec methods via guard
        assert_eq!(guard.len(), 3);
        assert!(guard.contains(&20));
    }

    // ========================================================================
    // NEW TESTS: RwLock edge cases
    // ========================================================================

    #[tokio::test]
    async fn rwlock_write_then_write() {
        let rw = RwLock::new(0);
        {
            let mut guard = rw.write().await;
            *guard = 10;
        }
        {
            let mut guard = rw.write().await;
            *guard += 5;
        }
        let guard = rw.read().await;
        assert_eq!(*guard, 15);
    }

    #[tokio::test]
    async fn rwlock_with_string_type() {
        let rw = RwLock::new(String::new());
        {
            let mut guard = rw.write().await;
            guard.push_str("hello");
        }
        let guard = rw.read().await;
        assert_eq!(&*guard, "hello");
    }

    #[tokio::test]
    async fn rwlock_with_vec_type() {
        let rw = RwLock::new(Vec::<i32>::new());
        {
            let mut guard = rw.write().await;
            guard.extend_from_slice(&[1, 2, 3]);
        }
        let guard = rw.read().await;
        assert_eq!(guard.len(), 3);
        assert_eq!(&*guard, &[1, 2, 3]);
    }

    #[tokio::test]
    async fn rwlock_read_does_not_mutate() {
        let rw = RwLock::new(42);
        {
            let guard = rw.read().await;
            assert_eq!(*guard, 42);
        }
        // Value unchanged
        let guard = rw.read().await;
        assert_eq!(*guard, 42);
    }

    #[tokio::test]
    async fn rwlock_multiple_write_cycles() {
        let rw = RwLock::new(0i64);
        for i in 0..5 {
            let mut guard = rw.write().await;
            *guard += i;
        }
        // Sum of 0..5 = 0+1+2+3+4 = 10
        let guard = rw.read().await;
        assert_eq!(*guard, 10);
    }

    // ========================================================================
    // NEW TESTS: Semaphore edge cases
    // ========================================================================

    #[tokio::test]
    async fn semaphore_zero_permits() {
        let sem = Semaphore::new(0);
        assert_eq!(sem.available_permits(), 0);
        // try_acquire should fail immediately with zero permits
        let result = sem.try_acquire();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn semaphore_close_then_try_acquire() {
        let sem = Semaphore::new(5);
        sem.close();
        let result = sem.try_acquire();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn semaphore_close_then_try_acquire_owned() {
        let sem = std::sync::Arc::new(Semaphore::new(5));
        sem.close();
        let result = sem.clone().try_acquire_owned();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn semaphore_acquire_all_permits_then_release() {
        let sem = Semaphore::new(3);
        let p1 = sem.acquire().await.expect("acquire 1");
        let p2 = sem.acquire().await.expect("acquire 2");
        let p3 = sem.acquire().await.expect("acquire 3");
        assert_eq!(sem.available_permits(), 0);

        drop(p1);
        assert_eq!(sem.available_permits(), 1);
        drop(p2);
        assert_eq!(sem.available_permits(), 2);
        drop(p3);
        assert_eq!(sem.available_permits(), 3);
    }

    #[tokio::test]
    async fn semaphore_large_permit_count() {
        let sem = Semaphore::new(10000);
        assert_eq!(sem.available_permits(), 10000);
        let _p = sem.try_acquire().expect("should acquire from large pool");
        assert_eq!(sem.available_permits(), 9999);
    }

    #[tokio::test]
    async fn semaphore_owned_acquire_and_release() {
        let sem = std::sync::Arc::new(Semaphore::new(2));
        let p1 = sem.clone().acquire_owned().await.expect("acquire 1");
        assert_eq!(sem.available_permits(), 1);
        let p2 = sem.clone().acquire_owned().await.expect("acquire 2");
        assert_eq!(sem.available_permits(), 0);
        drop(p1);
        assert_eq!(sem.available_permits(), 1);
        drop(p2);
        assert_eq!(sem.available_permits(), 2);
    }

    #[tokio::test]
    async fn semaphore_try_acquire_returns_permit_on_success() {
        let sem = Semaphore::new(1);
        let permit = sem.try_acquire();
        assert!(permit.is_ok());
        assert_eq!(sem.available_permits(), 0);
        drop(permit);
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn semaphore_close_preserves_held_permits() {
        let sem = Semaphore::new(2);
        let _p = sem.acquire().await.expect("acquire");
        assert_eq!(sem.available_permits(), 1);
        sem.close();
        // After close, available permits may still be reported
        // but new acquires should fail
        let result = sem.try_acquire();
        assert!(result.is_err());
    }

    // ========================================================================
    // NEW TESTS: MPSC channel edge cases
    // ========================================================================

    #[tokio::test]
    async fn mpsc_send_helper_to_closed_receiver_returns_error() {
        let (tx, rx) = mpsc::channel::<i32>(1);
        drop(rx);
        let result = mpsc_send(&tx, 42).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mpsc_send_recv_string_type() {
        let (tx, mut rx) = mpsc::channel(4);
        mpsc_send(&tx, String::from("hello")).await.expect("send");
        let got = mpsc_recv_option(&mut rx).await;
        assert_eq!(got, Some(String::from("hello")));
    }

    #[tokio::test]
    async fn mpsc_multiple_messages_via_helpers() {
        let (tx, mut rx) = mpsc::channel(8);
        for i in 0..5u32 {
            mpsc_send(&tx, i).await.expect("send");
        }
        for i in 0..5u32 {
            let got = mpsc_recv_option(&mut rx).await;
            assert_eq!(got, Some(i));
        }
    }

    #[tokio::test]
    async fn mpsc_channel_capacity_one() {
        let (tx, mut rx) = mpsc::channel(1);
        mpsc_send(&tx, 99u8).await.expect("send");
        let got = mpsc_recv_option(&mut rx).await;
        assert_eq!(got, Some(99u8));
    }

    #[tokio::test]
    async fn mpsc_send_error_contains_value() {
        let (tx, rx) = mpsc::channel::<String>(1);
        drop(rx);
        let err = mpsc_send(&tx, String::from("lost")).await;
        assert!(err.is_err());
        // The SendError should contain the value that could not be sent
        let send_err = err.unwrap_err();
        #[cfg(feature = "asupersync-runtime")]
        assert!(
            matches!(
                send_err,
                mpsc::SendError::Disconnected(value) if value == "lost"
            ),
            "expected disconnected send error carrying original value",
        );

        #[cfg(not(feature = "asupersync-runtime"))]
        assert_eq!(send_err.0, "lost");
    }

    #[tokio::test]
    async fn mpsc_recv_option_multiple_then_close() {
        let (tx, mut rx) = mpsc::channel(4);
        mpsc_send(&tx, 1).await.expect("send 1");
        mpsc_send(&tx, 2).await.expect("send 2");
        drop(tx);
        assert_eq!(mpsc_recv_option(&mut rx).await, Some(1));
        assert_eq!(mpsc_recv_option(&mut rx).await, Some(2));
        assert_eq!(mpsc_recv_option(&mut rx).await, None);
    }

    // ========================================================================
    // NEW TESTS: Watch channel edge cases
    // ========================================================================

    #[tokio::test]
    async fn watch_multiple_sends_receiver_sees_latest() {
        let (tx, rx) = watch::channel(0);
        tx.send(1).expect("send 1");
        tx.send(2).expect("send 2");
        tx.send(3).expect("send 3");
        // Watch channels only retain the latest value
        assert_eq!(*rx.borrow(), 3);
    }

    #[tokio::test]
    async fn watch_send_after_drop_receiver_fails() {
        let (tx, rx) = watch::channel(0);
        drop(rx);
        // With no receivers, send should fail
        let result = tx.send(42);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn watch_initial_value_string() {
        let (_, rx) = watch::channel(String::from("init"));
        assert_eq!(&*rx.borrow(), "init");
    }

    #[tokio::test]
    async fn watch_borrow_returns_ref_to_current_value() {
        let (tx, rx) = watch::channel(vec![1, 2, 3]);
        assert_eq!(*rx.borrow(), vec![1, 2, 3]);
        tx.send(vec![4, 5]).expect("send");
        assert_eq!(*rx.borrow(), vec![4, 5]);
    }

    #[tokio::test]
    async fn watch_multiple_receivers_see_same_value() {
        let (tx, rx1) = watch::channel(0);
        let rx2 = rx1.clone();
        tx.send(42).expect("send");
        assert_eq!(*rx1.borrow(), 42);
        assert_eq!(*rx2.borrow(), 42);
    }

    // ========================================================================
    // NEW TESTS: Broadcast channel edge cases
    // ========================================================================

    #[tokio::test]
    async fn broadcast_multiple_messages_fifo() {
        let (tx, mut rx) = broadcast::channel(16);
        tx.send(1).expect("send 1");
        tx.send(2).expect("send 2");
        tx.send(3).expect("send 3");
        assert_eq!(rx.recv().await.expect("recv 1"), 1);
        assert_eq!(rx.recv().await.expect("recv 2"), 2);
        assert_eq!(rx.recv().await.expect("recv 3"), 3);
    }

    #[tokio::test]
    async fn broadcast_receiver_lagged_returns_error() {
        // Create a tiny capacity channel
        let (tx, mut rx) = broadcast::channel(2);
        // Send more messages than the channel can hold
        tx.send(1).expect("send 1");
        tx.send(2).expect("send 2");
        tx.send(3).expect("send 3");
        // First recv should return Lagged error
        let result = rx.recv().await;
        match result {
            Err(broadcast::RecvError::Lagged(_)) => {} // expected
            other => panic!("expected Lagged error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn broadcast_send_with_no_receivers_returns_error() {
        let (tx, rx) = broadcast::channel::<i32>(16);
        drop(rx);
        // send should return error when there are no receivers
        let result = tx.send(42);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn broadcast_subscribe_after_send_misses_prior_messages() {
        let (tx, _rx) = broadcast::channel(16);
        tx.send(1).expect("send");
        let mut rx2 = tx.subscribe();
        tx.send(2).expect("send 2");
        // rx2 subscribed after message 1, should only see message 2
        let val = rx2.recv().await.expect("recv");
        assert_eq!(val, 2);
    }

    #[tokio::test]
    async fn broadcast_try_recv_empty_channel() {
        let (_tx, mut rx) = broadcast::channel::<i32>(16);
        let result = rx.try_recv();
        assert!(result.is_err());
        match result {
            Err(broadcast::TryRecvError::Empty) => {} // expected
            other => panic!("expected Empty, got {:?}", other),
        }
    }

    // ========================================================================
    // NEW TESTS: Timeout edge cases
    // ========================================================================

    #[tokio::test]
    async fn timeout_zero_duration_with_immediate_future_succeeds() {
        // Zero timeout but future completes immediately: should succeed
        let result = timeout(Duration::ZERO, async { 42 }).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn timeout_returns_complex_type() {
        let result = timeout(Duration::from_secs(1), async { vec![1, 2, 3] }).await;
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn timeout_returns_result_type() {
        let result = timeout(Duration::from_secs(1), async { Ok::<_, String>(42) }).await;
        let inner = result.expect("should not timeout");
        assert_eq!(inner.unwrap(), 42);
    }

    #[tokio::test]
    async fn timeout_preserves_string_value() {
        let result = timeout(Duration::from_secs(1), async { String::from("survived") }).await;
        assert_eq!(result.unwrap(), "survived");
    }

    // ========================================================================
    // NEW TESTS: Sleep edge cases
    // ========================================================================

    #[tokio::test]
    async fn sleep_very_short_duration() {
        let start = std::time::Instant::now();
        sleep(Duration::from_nanos(1)).await;
        // Should complete quickly (nanos might round up to ~1ms)
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn sleep_one_millisecond() {
        let start = std::time::Instant::now();
        sleep(Duration::from_millis(1)).await;
        // Should complete in reasonable time
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    // ========================================================================
    // NEW TESTS: CompatRuntime with spawn_detached edge cases
    // ========================================================================

    #[test]
    fn spawn_detached_multiple_tasks() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        // Spawning multiple detached tasks should not panic
        for _ in 0..10 {
            rt.spawn_detached(async {});
        }
    }

    #[test]
    fn block_on_with_tokio_sync_inside() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async {
            let (tx, rx) = watch::channel(0);
            tx.send(42).expect("send");
            *rx.borrow()
        });
        assert_eq!(result, 42);
    }

    #[test]
    fn block_on_with_mutex_inside() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async {
            let m = Mutex::new(99);
            let guard = m.lock().await;
            *guard
        });
        assert_eq!(result, 99);
    }

    #[test]
    fn block_on_with_rwlock_inside() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async {
            let rw = RwLock::new(77);
            let guard = rw.read().await;
            *guard
        });
        assert_eq!(result, 77);
    }

    #[test]
    fn block_on_with_mpsc_inside() {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let result = rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(1);
            mpsc_send(&tx, 123).await.expect("send");
            mpsc_recv_option(&mut rx).await
        });
        assert_eq!(result, Some(123));
    }

    // ========================================================================
    // NEW TESTS: Type assertions and trait bounds
    // ========================================================================

    #[test]
    fn runtime_builder_build_error_type_is_string() {
        // The build() method returns Result<Runtime, String>
        let result = RuntimeBuilder::current_thread().build();
        let _rt: Runtime = result.expect("build should succeed");
    }

    #[tokio::test]
    async fn semaphore_is_send_sync() {
        // Verify Semaphore can be shared across tasks
        let sem = std::sync::Arc::new(Semaphore::new(1));
        let sem2 = sem.clone();
        let handle = task::spawn(async move {
            let _p = sem2.acquire().await.expect("acquire in spawned task");
        });
        handle.await.expect("spawned task should complete");
    }

    #[tokio::test]
    async fn mutex_is_send_sync() {
        // Verify Mutex can be shared across tasks
        let m = std::sync::Arc::new(Mutex::new(0));
        let m2 = m.clone();
        let handle = task::spawn(async move {
            let mut guard = m2.lock().await;
            *guard = 42;
        });
        handle.await.expect("spawned task should complete");
        let guard = m.lock().await;
        assert_eq!(*guard, 42);
    }

    #[tokio::test]
    async fn rwlock_is_send_sync() {
        // Verify RwLock can be shared across tasks
        let rw = std::sync::Arc::new(RwLock::new(0));
        let rw2 = rw.clone();
        let handle = task::spawn(async move {
            let mut guard = rw2.write().await;
            *guard = 99;
        });
        handle.await.expect("spawned task should complete");
        let guard = rw.read().await;
        assert_eq!(*guard, 99);
    }

    // ========================================================================
    // Property-based tests
    // ========================================================================

    proptest! {
        #[test]
        fn proptest_mpsc_preserves_fifo(values in proptest::collection::vec(any::<i16>(), 0..64)) {
            let expected = values.clone();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let received = rt.block_on(async move {
                let (tx, mut rx) = mpsc::channel(expected.len().max(1));
                for value in &expected {
                    mpsc_send(&tx, *value).await.expect("send should succeed");
                }
                drop(tx);

                let mut out = Vec::with_capacity(expected.len());
                while let Some(value) = mpsc_recv_option(&mut rx).await {
                    out.push(value);
                }
                out
            });

            prop_assert_eq!(received, values);
        }

        #[test]
        fn proptest_watch_receiver_sees_latest(values in proptest::collection::vec(any::<u32>(), 1..64)) {
            let expected_latest = *values.last().expect("non-empty");
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed_latest = rt.block_on(async move {
                let (tx, rx) = watch::channel(values[0]);
                for value in values.iter().skip(1) {
                    tx.send(*value).expect("watch send should succeed");
                }
                *rx.borrow()
            });

            prop_assert_eq!(observed_latest, expected_latest);
        }

        #[test]
        fn proptest_semaphore_permit_accounting(
            permits in 1usize..16,
            acquire_count in 0usize..16,
        ) {
            prop_assume!(acquire_count <= permits);

            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let (during, after) = rt.block_on(async move {
                let sem = Semaphore::new(permits);
                let mut held = Vec::with_capacity(acquire_count);
                for _ in 0..acquire_count {
                    held.push(sem.acquire().await.expect("acquire should succeed"));
                }

                let during = sem.available_permits();
                drop(held);
                let after = sem.available_permits();
                (during, after)
            });

            prop_assert_eq!(during, permits - acquire_count);
            prop_assert_eq!(after, permits);
        }

        #[test]
        fn proptest_mutex_preserves_write_sequence(values in proptest::collection::vec(any::<i32>(), 0..128)) {
            let expected = values.clone();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed = rt.block_on(async move {
                let mutex = Mutex::new(Vec::<i32>::new());
                for value in &expected {
                    let mut guard = mutex.lock().await;
                    guard.push(*value);
                }
                let guard = mutex.lock().await;
                guard.clone()
            });

            prop_assert_eq!(observed, values);
        }

        #[test]
        fn proptest_rwlock_accumulates_deltas(
            initial in any::<i64>(),
            deltas in proptest::collection::vec(-1000i64..1000i64, 0..64),
        ) {
            let expected = initial + deltas.iter().copied().sum::<i64>();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed = rt.block_on(async move {
                let lock = RwLock::new(initial);
                for delta in &deltas {
                    let mut guard = lock.write().await;
                    *guard += *delta;
                }
                let guard = lock.read().await;
                *guard
            });

            prop_assert_eq!(observed, expected);
        }

        #[test]
        fn proptest_timeout_ready_future_returns_value(value in any::<i64>()) {
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed = rt.block_on(async move {
                timeout(Duration::from_millis(1), async move { value })
                    .await
                    .expect("ready future should not timeout")
            });

            prop_assert_eq!(observed, value);
        }

        #[test]
        fn proptest_spawn_blocking_returns_computed_result(values in proptest::collection::vec(any::<i32>(), 0..64)) {
            let expected: i64 = values.iter().map(|v| i64::from(*v)).sum();
            let rt = RuntimeBuilder::current_thread()
                .build()
                .expect("runtime should build");

            let observed = rt.block_on(async move {
                spawn_blocking(move || values.iter().map(|v| i64::from(*v)).sum::<i64>())
                    .await
                    .expect("spawn_blocking should succeed")
            });

            prop_assert_eq!(observed, expected);
        }
    }

    // =========================================================================
    // Batch: DarkBadger wa-1u90p.7.1 — trait impls and edge cases
    // =========================================================================

    // -- TryAcquireError --

    #[tokio::test]
    async fn try_acquire_error_debug_no_permits() {
        let sem = Semaphore::new(0);
        let err = sem.try_acquire().unwrap_err();
        let dbg = format!("{:?}", err);
        assert!(!dbg.is_empty());
    }

    #[tokio::test]
    async fn try_acquire_error_debug_closed() {
        let sem = Semaphore::new(5);
        sem.close();
        let err = sem.try_acquire().unwrap_err();
        let dbg = format!("{:?}", err);
        assert!(!dbg.is_empty());
    }

    #[tokio::test]
    async fn try_acquire_error_display_no_permits() {
        let sem = Semaphore::new(0);
        let err = sem.try_acquire().unwrap_err();
        let display = format!("{}", err);
        assert!(!display.is_empty());
    }

    #[tokio::test]
    async fn try_acquire_error_display_closed() {
        let sem = Semaphore::new(5);
        sem.close();
        let err = sem.try_acquire().unwrap_err();
        let display = format!("{}", err);
        assert!(!display.is_empty());
    }

    #[tokio::test]
    async fn try_acquire_error_is_std_error() {
        let sem = Semaphore::new(0);
        let err = sem.try_acquire().unwrap_err();
        // Verify it implements std::error::Error
        let _: &dyn std::error::Error = &err;
    }

    // -- AcquireError --

    #[tokio::test]
    async fn acquire_error_debug() {
        let sem = Semaphore::new(1);
        sem.close();
        let err = sem.acquire().await.unwrap_err();
        let dbg = format!("{:?}", err);
        assert!(!dbg.is_empty());
    }

    #[tokio::test]
    async fn acquire_error_display() {
        let sem = Semaphore::new(1);
        sem.close();
        let err = sem.acquire().await.unwrap_err();
        let display = format!("{}", err);
        assert!(!display.is_empty());
    }

    #[tokio::test]
    async fn acquire_error_is_std_error() {
        let sem = Semaphore::new(1);
        sem.close();
        let err = sem.acquire().await.unwrap_err();
        let _: &dyn std::error::Error = &err;
    }

    // -- MutexGuard DerefMut edge cases --

    #[tokio::test]
    async fn mutex_guard_deref_mut_vec_indexing() {
        let m = Mutex::new(vec![1, 2, 3]);
        {
            let mut guard = m.lock().await;
            guard[0] = 99;
            guard[2] = 77;
        }
        let guard = m.lock().await;
        assert_eq!(*guard, vec![99, 2, 77]);
    }

    // -- RwLockWriteGuard DerefMut edge cases --

    #[tokio::test]
    async fn rwlock_write_guard_deref_mut_vec_indexing() {
        let rw = RwLock::new(vec![10, 20, 30]);
        {
            let mut guard = rw.write().await;
            guard[1] = 99;
        }
        let guard = rw.read().await;
        assert_eq!(*guard, vec![10, 99, 30]);
    }

    // -- spawn_blocking --

    #[tokio::test]
    async fn spawn_blocking_basic_computation() {
        let result = spawn_blocking(|| 2 + 2).await;
        assert_eq!(result.unwrap(), 4);
    }

    #[tokio::test]
    async fn spawn_blocking_string_computation() {
        let result = spawn_blocking(|| {
            let mut s = String::new();
            for i in 0..5 {
                s.push_str(&i.to_string());
            }
            s
        })
        .await;
        assert_eq!(result.unwrap(), "01234");
    }

    #[tokio::test]
    async fn spawn_blocking_heavy_computation() {
        let result = spawn_blocking(|| {
            let mut sum: u64 = 0;
            for i in 0..1000 {
                sum += i;
            }
            sum
        })
        .await;
        assert_eq!(result.unwrap(), 499_500);
    }

    // -- task::spawn --

    #[tokio::test]
    async fn task_spawn_returns_value() {
        let handle = task::spawn(async { 42 });
        let result = handle.await.expect("task should complete");
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn task_spawn_string_value() {
        let handle = task::spawn(async { String::from("from task") });
        let result = handle.await.expect("task should complete");
        assert_eq!(result, "from task");
    }

    // -- Semaphore permit count verification --

    #[tokio::test]
    async fn semaphore_multiple_try_acquire_exhaust_permits() {
        let sem = Semaphore::new(3);
        let _p1 = sem.try_acquire().expect("1st acquire");
        let _p2 = sem.try_acquire().expect("2nd acquire");
        let _p3 = sem.try_acquire().expect("3rd acquire");
        assert_eq!(sem.available_permits(), 0);
        assert!(sem.try_acquire().is_err());
    }

    // -- Channel edge cases --

    #[tokio::test]
    async fn watch_channel_drop_sender_borrow_still_works() {
        let (tx, rx) = watch::channel(42);
        tx.send(100).expect("send");
        drop(tx);
        // After sender dropped, receiver should still see last value
        assert_eq!(*rx.borrow(), 100);
    }

    #[tokio::test]
    async fn broadcast_receiver_clone_both_receive() {
        let (tx, mut rx1) = broadcast::channel(16);
        let mut rx2 = tx.subscribe();
        tx.send(7).expect("send");
        assert_eq!(rx1.recv().await.expect("r1"), 7);
        assert_eq!(rx2.recv().await.expect("r2"), 7);
    }

    // ========================================================================
    // Notify tests
    // ========================================================================

    #[tokio::test]
    async fn notify_one_wakes_waiter() {
        let n = notify::Notify::new();
        let n2 = std::sync::Arc::new(n);
        let n3 = n2.clone();

        let handle = task::spawn(async move {
            n3.notified().await;
            42
        });

        sleep(Duration::from_millis(5)).await;
        n2.notify_one();

        let result = handle.await.expect("task");
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn notify_waiters_wakes_all() {
        let n = std::sync::Arc::new(notify::Notify::new());
        let n1 = n.clone();
        let n2 = n.clone();

        let h1 = task::spawn(async move {
            n1.notified().await;
            1
        });
        let h2 = task::spawn(async move {
            n2.notified().await;
            2
        });

        sleep(Duration::from_millis(5)).await;
        n.notify_waiters();

        let r1 = h1.await.expect("h1");
        let r2 = h2.await.expect("h2");
        assert_eq!(r1 + r2, 3);
    }

    #[tokio::test]
    async fn notify_before_notified_does_not_block() {
        let n = notify::Notify::new();
        n.notify_one();
        // Should complete immediately since notification is stored
        n.notified().await;
    }

    #[test]
    fn notify_new_does_not_panic() {
        let _n = notify::Notify::new();
    }

    // ========================================================================
    // Oneshot channel tests
    // ========================================================================

    #[tokio::test]
    async fn oneshot_send_recv() {
        let (tx, rx) = oneshot::channel();
        tx.send(42).expect("send");
        let val = rx.await.expect("recv");
        assert_eq!(val, 42);
    }

    #[tokio::test]
    async fn oneshot_recv_after_drop_sender_returns_err() {
        let (tx, rx) = oneshot::channel::<u32>();
        drop(tx);
        let result = rx.await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn oneshot_send_after_drop_receiver_returns_err() {
        let (tx, rx) = oneshot::channel::<u32>();
        drop(rx);
        let result = tx.send(42);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn oneshot_with_string_payload() {
        let (tx, rx) = oneshot::channel();
        tx.send("hello".to_string()).expect("send");
        let val = rx.await.expect("recv");
        assert_eq!(val, "hello");
    }

    #[tokio::test]
    async fn oneshot_with_result_payload() {
        let (tx, rx) = oneshot::channel::<Result<i32, String>>();
        tx.send(Ok(99)).expect("send");
        let val = rx.await.expect("recv");
        assert_eq!(val.unwrap(), 99);
    }

    #[tokio::test]
    async fn oneshot_with_result_err_payload() {
        let (tx, rx) = oneshot::channel::<Result<i32, String>>();
        tx.send(Err("fail".to_string())).expect("send");
        let val = rx.await.expect("recv");
        assert_eq!(val.unwrap_err(), "fail");
    }

    #[tokio::test]
    async fn oneshot_with_vec_payload() {
        let (tx, rx) = oneshot::channel();
        tx.send(vec![1, 2, 3]).expect("send");
        let val = rx.await.expect("recv");
        assert_eq!(val, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn oneshot_with_option_payload() {
        let (tx, rx) = oneshot::channel::<Option<u32>>();
        tx.send(Some(7)).expect("send");
        assert_eq!(rx.await.expect("recv"), Some(7));

        let (tx2, rx2) = oneshot::channel::<Option<u32>>();
        tx2.send(None).expect("send none");
        assert_eq!(rx2.await.expect("recv none"), None);
    }

    #[tokio::test]
    async fn oneshot_recv_error_is_recv_error() {
        let (tx, rx) = oneshot::channel::<u32>();
        drop(tx);
        let err = rx.await.unwrap_err();
        // RecvError should display something meaningful
        let display = format!("{err}");
        assert!(!display.is_empty());
    }

    #[tokio::test]
    async fn oneshot_send_returns_value_on_closed_receiver() {
        let (tx, rx) = oneshot::channel::<u32>();
        drop(rx);
        // send() returns the value when receiver is dropped
        let returned = tx.send(42).unwrap_err();
        assert_eq!(returned, 42);
    }

    // ========================================================================
    // Process module tests
    // ========================================================================

    #[tokio::test]
    async fn process_command_echo() {
        let output = process::Command::new("echo")
            .arg("hello")
            .output()
            .await
            .expect("echo should succeed");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello"));
    }

    #[tokio::test]
    async fn process_command_false_returns_non_zero() {
        let output = process::Command::new("false")
            .output()
            .await
            .expect("false should execute");
        assert!(!output.status.success());
    }

    #[tokio::test]
    async fn process_command_with_env() {
        let output = process::Command::new("env")
            .env("TEST_RC_VAR", "42")
            .output()
            .await
            .expect("env should succeed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("TEST_RC_VAR=42"));
    }

    #[tokio::test]
    async fn process_command_stdin_piped() {
        use std::process::Stdio;
        let child = process::Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        assert!(child.is_ok());
    }

    #[tokio::test]
    async fn process_command_nonexistent_binary() {
        let result = process::Command::new("nonexistent_binary_xyz_123")
            .output()
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn process_command_args_multiple() {
        let output = process::Command::new("echo")
            .args(["a", "b", "c"])
            .output()
            .await
            .expect("echo should succeed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("a b c"));
    }

    // ========================================================================
    // IO module tests
    // ========================================================================

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn io_async_read_ext_available() {
        use io::AsyncReadExt;
        let data: &[u8] = b"hello world";
        let mut cursor = std::io::Cursor::new(data);
        let mut buf = [0u8; 5];
        let n = cursor.read(&mut buf).await.expect("read should succeed");
        assert_eq!(n, 5);
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn io_async_write_ext_available() {
        use io::AsyncWriteExt;
        let mut buf = Vec::new();
        buf.write_all(b"test").await.expect("write should succeed");
        assert_eq!(&buf, b"test");
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn io_read_to_end_via_ext() {
        use io::AsyncReadExt;
        let data: &[u8] = b"abcdef";
        let mut cursor = std::io::Cursor::new(data);
        let mut buf = Vec::new();
        cursor
            .read_to_end(&mut buf)
            .await
            .expect("read_to_end should succeed");
        assert_eq!(&buf, b"abcdef");
    }

    // ========================================================================
    // Net module tests
    // ========================================================================

    #[tokio::test]
    async fn net_tcp_listener_bind() {
        let listener = net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("should have local addr");
        assert!(addr.port() > 0);
    }

    #[tokio::test]
    async fn net_tcp_stream_connect_to_listener() {
        let listener = net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let stream = net::TcpStream::connect(addr).await;
        assert!(stream.is_ok());
    }

    #[tokio::test]
    async fn net_tcp_roundtrip() {
        use io::{AsyncReadExt, AsyncWriteExt};

        let listener = net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = task::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await.expect("read");
            buf
        });

        let mut client = net::TcpStream::connect(addr).await.expect("connect");
        client.write_all(b"ping").await.expect("write");

        let received = server.await.expect("server task");
        assert_eq!(&received, b"ping");
    }

    // ========================================================================
    // RuntimeBuilder enable_all and thread_name tests
    // ========================================================================

    #[test]
    fn runtime_builder_enable_all_is_chainable() {
        let rt = RuntimeBuilder::multi_thread().enable_all().build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_thread_name_is_chainable() {
        let rt = RuntimeBuilder::multi_thread()
            .thread_name("test-worker")
            .build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_full_chain() {
        let rt = RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("full-chain-test")
            .build();
        assert!(rt.is_ok());
    }

    #[test]
    fn runtime_builder_current_thread_with_enable_all_and_thread_name() {
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .thread_name("ct-test")
            .build();
        assert!(rt.is_ok());
    }

    // ========================================================================
    // task::spawn_blocking tests
    // ========================================================================

    #[tokio::test]
    async fn task_spawn_blocking_returns_value() {
        let handle = task::spawn_blocking(|| 42);
        let result = handle.await.expect("join");
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn task_spawn_blocking_runs_closure() {
        let handle = task::spawn_blocking(|| {
            let mut sum = 0;
            for i in 0..100 {
                sum += i;
            }
            sum
        });
        assert_eq!(handle.await.expect("join"), 4950);
    }

    #[tokio::test]
    async fn task_spawn_blocking_abort_cancels() {
        let handle = task::spawn_blocking(|| {
            std::thread::sleep(Duration::from_secs(60));
            "never"
        });
        handle.abort();
        let result = handle.await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn task_spawn_blocking_returns_join_handle() {
        let handle: task::JoinHandle<String> = task::spawn_blocking(|| "hello".to_string());
        let val = handle.await.expect("join");
        assert_eq!(val, "hello");
    }

    // ========================================================================
    // join! macro tests
    // ========================================================================

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn join_two_futures() {
        let (a, b) = join!(async { 1 }, async { 2 });
        assert_eq!(a, 1);
        assert_eq!(b, 2);
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn join_three_futures() {
        let (a, b, c) = join!(async { "x" }, async { "y" }, async { "z" });
        assert_eq!(a, "x");
        assert_eq!(b, "y");
        assert_eq!(c, "z");
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn join_with_sleep() {
        let (a, b) = join!(
            async {
                sleep(Duration::from_millis(1)).await;
                10
            },
            async {
                sleep(Duration::from_millis(1)).await;
                20
            }
        );
        assert_eq!(a + b, 30);
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn join_single_future() {
        let (result,) = join!(async { 99 });
        assert_eq!(result, 99);
    }

    // ========================================================================
    // select! macro tests
    // ========================================================================

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn select_first_branch_ready() {
        let result = select! {
            val = async { 1 } => val,
            () = sleep(Duration::from_secs(10)) => 0,
        };
        assert_eq!(result, 1);
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn select_sleep_branch() {
        let result = select! {
            () = sleep(Duration::from_millis(1)) => "timer",
            () = sleep(Duration::from_secs(60)) => "never",
        };
        assert_eq!(result, "timer");
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn select_with_channel() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(42).await.expect("send");
        let result = select! {
            val = rx.recv() => val.unwrap_or(0),
            () = sleep(Duration::from_secs(10)) => 0,
        };
        assert_eq!(result, 42);
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn select_biased_picks_first_ready() {
        let result = select! {
            biased;
            val = async { "first" } => val,
            val = async { "second" } => val,
        };
        assert_eq!(result, "first");
    }

    // ========================================================================
    // task::yield_now tests
    // ========================================================================

    #[tokio::test]
    async fn yield_now_does_not_panic() {
        task::yield_now().await;
    }

    #[tokio::test]
    async fn yield_now_multiple_times() {
        for _ in 0..5 {
            task::yield_now().await;
        }
    }

    // ========================================================================
    // time module tests
    // ========================================================================

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test(start_paused = true)]
    async fn time_advance_moves_clock() {
        let start = std::time::Instant::now();
        time::advance(Duration::from_secs(60)).await;
        // In paused mode, wall-clock barely moves but tokio's clock advances.
        // We just verify no panic.
        let _ = start.elapsed();
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn time_pause_enables_deterministic_sleep() {
        time::pause();
        // After pausing, sleeps resolve as time is auto-advanced in single-threaded
        // runtime. Verify a long sleep completes quickly.
        let start = std::time::Instant::now();
        sleep(Duration::from_secs(300)).await;
        let wall_elapsed = start.elapsed();
        // Wall-clock should be well under 300 seconds.
        assert!(wall_elapsed < Duration::from_secs(5));
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test(start_paused = true)]
    async fn time_advance_then_sleep_resolves() {
        let (tx, mut rx) = mpsc::channel(1);
        task::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            let _ = tx.send(42).await;
        });
        time::advance(Duration::from_millis(200)).await;
        task::yield_now().await;
        let val = rx.recv().await;
        assert_eq!(val, Some(42));
    }

    // ── Signal module tests ──────────────────────────────────────────────

    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn signal_ctrl_c_is_constructible() {
        // Verify ctrl_c() returns a future that can be selected against.
        // We cannot actually send SIGINT in a test, so we verify it compiles
        // and that the select! with an immediate timeout works.
        let result = timeout(Duration::from_millis(1), signal::ctrl_c()).await;
        // Should timeout since no SIGINT is sent.
        assert!(result.is_err(), "ctrl_c should not resolve without signal");
    }

    #[cfg(unix)]
    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn signal_unix_terminate_is_constructible() {
        // Verify we can create a SIGTERM listener via the compat layer.
        let listener = signal::unix::signal(signal::unix::SignalKind::terminate());
        assert!(listener.is_ok(), "SIGTERM listener creation should succeed");
    }

    #[cfg(unix)]
    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn signal_unix_hangup_is_constructible() {
        let listener = signal::unix::signal(signal::unix::SignalKind::hangup());
        assert!(listener.is_ok(), "SIGHUP listener creation should succeed");
    }

    #[cfg(unix)]
    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn signal_unix_recv_times_out_without_signal() {
        let mut sig = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("create SIGTERM listener");
        let result = timeout(Duration::from_millis(5), sig.recv()).await;
        assert!(result.is_err(), "recv should timeout without actual signal");
    }

    #[cfg(unix)]
    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn signal_unix_usr1_is_constructible() {
        let listener = signal::unix::signal(signal::unix::SignalKind::user_defined1());
        assert!(listener.is_ok(), "SIGUSR1 listener creation should succeed");
    }

    #[cfg(unix)]
    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn signal_unix_usr2_is_constructible() {
        let listener = signal::unix::signal(signal::unix::SignalKind::user_defined2());
        assert!(listener.is_ok(), "SIGUSR2 listener creation should succeed");
    }

    #[cfg(unix)]
    #[cfg(not(feature = "asupersync-runtime"))]
    #[tokio::test]
    async fn signal_unix_recv_delivers_sent_signal() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut sig = signal::unix::signal(signal::unix::SignalKind::user_defined1())
            .expect("create SIGUSR1 listener");

        let received = Arc::new(AtomicBool::new(false));
        let received_clone = received.clone();

        // Spawn a task that waits for the signal.
        let handle = task::spawn(async move {
            if sig.recv().await == Some(()) {
                received_clone.store(true, Ordering::SeqCst);
            }
        });

        // Give the listener a moment to register.
        task::yield_now().await;
        sleep(Duration::from_millis(10)).await;

        // Send SIGUSR1 to ourselves via Command (no unsafe).
        let pid = std::process::id();
        let _ = std::process::Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .status();

        // Wait for delivery.
        let _ = timeout(Duration::from_secs(2), handle).await;
        assert!(
            received.load(Ordering::SeqCst),
            "SIGUSR1 should have been received"
        );
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    #[test]
    fn join_error_type_is_reexported() {
        // Verify the JoinError re-export compiles and is usable as a type.
        fn _accept_join_error(_e: JoinError) {}
    }
}
