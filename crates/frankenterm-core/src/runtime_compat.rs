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

/// Receives one message from an mpsc receiver, normalized to Option semantics.
///
/// Returns:
/// - `Some(value)` when a message was received.
/// - `None` when the channel is closed.
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

#[cfg(test)]
mod tests {
    use super::*;

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
        #[cfg(feature = "asupersync-runtime")]
        let (tx, rx) = mpsc::channel(10);
        #[cfg(not(feature = "asupersync-runtime"))]
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
        #[cfg(feature = "asupersync-runtime")]
        let (tx, rx) = mpsc::channel(10);
        #[cfg(not(feature = "asupersync-runtime"))]
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
}
