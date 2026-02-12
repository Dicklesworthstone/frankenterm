//! Temporary dual-runtime compatibility surface for the tokio -> asupersync migration.
//!
//! This module intentionally keeps the API small and explicit:
//! - sync primitive type aliases (`Mutex`, `RwLock`, `Semaphore`, ...)
//! - channel module aliases (`mpsc`, `watch`)
//! - runtime lifecycle wrappers (`RuntimeBuilder`, `Runtime`, `CompatRuntime`)
//! - time helpers (`sleep`, `timeout`)
//!
//! The scaffold is expected to be removed once migration is complete.

use std::future::Future;
use std::time::Duration;

#[cfg(feature = "asupersync-runtime")]
use std::ops::{Deref, DerefMut};
#[cfg(feature = "asupersync-runtime")]
use std::sync::Arc;

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

    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        let cx = asupersync::Cx::for_testing();
        let guard = self
            .inner
            .read(&cx)
            .await
            .expect("runtime_compat rwlock read failed");
        RwLockReadGuard { inner: guard }
    }

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
}

/// MPSC channel aliases for the active runtime.
#[cfg(not(feature = "asupersync-runtime"))]
pub mod mpsc {
    pub use tokio::sync::mpsc::{
        Receiver, Sender, channel,
        error::{SendError, TryRecvError, TrySendError},
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
        self.inner.block_on(future)
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
