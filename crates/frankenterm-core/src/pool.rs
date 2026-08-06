//! Connection pool for WezTerm mux connections.
//!
//! Reduces overhead by reusing persistent connections to the WezTerm mux
//! server (vendored mode) or limiting concurrent CLI process spawns.
//!
//! # Design
//!
//! The pool manages a fixed set of connection slots. Each slot holds either
//! an idle connection or is empty (available for a new connection). Callers
//! acquire a `PoolAcquireResult` that owns the concurrency permit and may
//! contain an idle connection. Callers explicitly return reusable connections;
//! dropping the result or its transferred guard always releases the slot.
//!
//! For CLI mode, pooling acts as a concurrency limiter — the underlying
//! `WeztermClient` is stateless but spawning too many processes at once
//! causes resource contention.
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::cx::{self, Cx};
use crate::runtime_async::{LockAcquireError, Mutex, Semaphore, TryAcquireError};
use serde::{Deserialize, Serialize};

/// Add to a telemetry counter without allowing an ancient/high-volume process
/// to make the cumulative value appear to move backwards after `u64::MAX`.
/// Returns the value published by this update.
fn saturating_atomic_add(counter: &AtomicU64, delta: u64) -> u64 {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(delta);
        match counter.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

/// Configuration for the connection pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Maximum number of concurrent connections (pool size).
    pub max_size: usize,
    /// How long an idle connection can stay in the pool before eviction.
    pub idle_timeout: Duration,
    /// How long to wait to acquire a connection before giving up.
    pub acquire_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: 4,
            idle_timeout: Duration::from_secs(300),
            acquire_timeout: Duration::from_secs(5),
        }
    }
}

/// A pooled connection wrapper that tracks idle time.
#[derive(Debug)]
struct PooledEntry<C> {
    conn: C,
    returned_at: Instant,
}

/// Statistics about the pool's current state and historical usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolStats {
    /// Maximum pool capacity.
    pub max_size: usize,
    /// Number of idle connections currently in the pool.
    pub idle_count: usize,
    /// Number of connections currently checked out.
    pub active_count: usize,
    /// Total number of successful acquisitions.
    pub total_acquired: u64,
    /// Total number of connections returned to the pool.
    pub total_returned: u64,
    /// Total number of connections evicted due to idle timeout.
    pub total_evicted: u64,
    /// Total number of acquire attempts that timed out.
    pub total_timeouts: u64,
}

/// Error returned when pool operations fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolError {
    /// No connection available within the acquire timeout.
    AcquireTimeout,
    /// Pool has been shut down.
    Closed,
    /// The caller's explicit capability context was cancelled during a pool
    /// operation.
    Cancelled,
    /// The caller's capability-context deadline elapsed.
    DeadlineExceeded,
    /// The caller's cooperative poll quota was exhausted.
    PollQuotaExhausted,
    /// The caller's cost budget was exhausted.
    CostBudgetExhausted,
    /// A capability checkpoint failed without a stable typed cause.
    ContextFailure,
    /// The lock's own logical acquisition deadline elapsed.
    LockTimedOut { deadline_nanos: u64 },
    /// The semaphore acquire future was polled after it had completed.
    PolledAfterCompletion,
    /// The idle-queue lock failed for a non-cancellation reason.
    LockAcquire(LockAcquireError),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AcquireTimeout => write!(f, "connection pool acquire timeout"),
            Self::Closed => write!(f, "connection pool is closed"),
            Self::Cancelled => write!(f, "connection pool operation cancelled"),
            Self::DeadlineExceeded => write!(f, "connection pool capability deadline exceeded"),
            Self::PollQuotaExhausted => {
                write!(f, "connection pool capability poll quota exhausted")
            }
            Self::CostBudgetExhausted => {
                write!(f, "connection pool capability cost budget exhausted")
            }
            Self::ContextFailure => write!(f, "connection pool capability context failed"),
            Self::LockTimedOut { deadline_nanos } => {
                write!(f, "connection pool lock timed out at {deadline_nanos}ns")
            }
            Self::PolledAfterCompletion => {
                write!(f, "connection pool acquire future polled after completion")
            }
            Self::LockAcquire(error) => {
                write!(f, "connection pool lock acquisition failed: {error}")
            }
        }
    }
}

impl std::error::Error for PoolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LockAcquire(error) => Some(error),
            Self::AcquireTimeout
            | Self::Closed
            | Self::Cancelled
            | Self::DeadlineExceeded
            | Self::PollQuotaExhausted
            | Self::CostBudgetExhausted
            | Self::ContextFailure
            | Self::LockTimedOut { .. }
            | Self::PolledAfterCompletion => None,
        }
    }
}

impl From<LockAcquireError> for PoolError {
    fn from(error: LockAcquireError) -> Self {
        match error {
            LockAcquireError::Cancelled => Self::Cancelled,
            LockAcquireError::DeadlineExceeded => Self::DeadlineExceeded,
            LockAcquireError::PollQuotaExhausted => Self::PollQuotaExhausted,
            LockAcquireError::CostBudgetExhausted => Self::CostBudgetExhausted,
            LockAcquireError::ContextFailure => Self::ContextFailure,
            LockAcquireError::TimedOut { deadline_nanos } => {
                Self::LockTimedOut { deadline_nanos }
            }
            LockAcquireError::Poisoned | LockAcquireError::PolledAfterCompletion => {
                Self::LockAcquire(error)
            }
        }
    }
}

/// A generic async connection pool.
///
/// `C` is the connection type (e.g., a WezTerm mux client handle).
/// Connections are created externally and added via [`Pool::put_with_cx`];
/// the pool itself does not create connections — it manages their lifecycle.
pub struct Pool<C> {
    config: PoolConfig,
    idle: Arc<Mutex<VecDeque<PooledEntry<C>>>>,
    semaphore: Arc<Semaphore>,
    stats_acquired: AtomicU64,
    stats_returned: AtomicU64,
    stats_evicted: AtomicU64,
    stats_timeouts: AtomicU64,
    #[cfg(test)]
    after_idle_lock_acquired: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl<C: Send + 'static> Pool<C> {
    pub(crate) fn classify_cx_failure(cx: &Cx) -> PoolError {
        use crate::outcome::CancelKind;

        match cx.root_cancel_cause().map(|reason| reason.kind) {
            Some(CancelKind::Deadline | CancelKind::Timeout) => PoolError::DeadlineExceeded,
            Some(CancelKind::PollQuota) => PoolError::PollQuotaExhausted,
            Some(CancelKind::CostBudget) => PoolError::CostBudgetExhausted,
            Some(
                CancelKind::User
                | CancelKind::FailFast
                | CancelKind::RaceLost
                | CancelKind::ParentCancelled
                | CancelKind::ResourceUnavailable
                | CancelKind::Shutdown
                | CancelKind::LinkedExit,
            ) => PoolError::Cancelled,
            None => PoolError::ContextFailure,
        }
    }

    fn classify_lock_failure(_cx: &Cx, error: LockAcquireError) -> PoolError {
        error.into()
    }

    fn classify_acquire_failure(
        cx: &Cx,
        error: crate::runtime_async::AcquireError,
    ) -> PoolError {
        match error {
            crate::runtime_async::AcquireError::Closed => PoolError::Closed,
            crate::runtime_async::AcquireError::Cancelled => Self::classify_cx_failure(cx),
            crate::runtime_async::AcquireError::PolledAfterCompletion => {
                PoolError::PolledAfterCompletion
            }
        }
    }

    fn checkpoint_explicit_cx(cx: &Cx) -> Result<(), PoolError> {
        cx.checkpoint().map_err(|_| Self::classify_cx_failure(cx))
    }

    #[cfg(test)]
    fn fire_after_idle_lock_acquired(&self) {
        let hook = self
            .after_idle_lock_acquired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Create a new pool with the given configuration.
    #[must_use]
    pub fn new(config: PoolConfig) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_size));
        Self {
            config,
            idle: Arc::new(Mutex::new(VecDeque::new())),
            semaphore,
            stats_acquired: AtomicU64::new(0),
            stats_returned: AtomicU64::new(0),
            stats_evicted: AtomicU64::new(0),
            stats_timeouts: AtomicU64::new(0),
            #[cfg(test)]
            after_idle_lock_acquired: std::sync::Mutex::new(None),
        }
    }

    /// Try to acquire a connection from the pool without waiting.
    ///
    /// Returns `Ok(result)` with an optional idle connection if a slot is
    /// available, or `Err` if no slots are free. If `result.conn` is `None`,
    /// the caller should create a new connection.
    pub async fn try_acquire(&self) -> Result<PoolAcquireResult<C>, PoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.try_acquire_with_cx(&cx).await
    }

    /// Try to acquire a connection using an explicit capability context.
    ///
    /// The internal idle-pool mutex acquire is
    /// bound to the caller's `Cx` via
    /// `Mutex::lock_with_cx`, so a caller-cancelled wait propagates
    /// through the full acquire path as [`PoolError::Cancelled`] rather than a
    /// panic (ft-xbnl0.2.3).
    pub async fn try_acquire_with_cx(&self, cx: &Cx) -> Result<PoolAcquireResult<C>, PoolError> {
        Self::checkpoint_explicit_cx(cx)?;
        match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => {
                let conn = {
                    let mut idle = self
                        .idle
                        .lock_with_cx(cx)
                        .await
                        .map_err(|error| Self::classify_lock_failure(cx, error))?;
                    #[cfg(test)]
                    self.fire_after_idle_lock_acquired();
                    Self::checkpoint_explicit_cx(cx)?;
                    self.evict_expired(&mut idle);
                    idle.pop_front().map(|e| e.conn)
                };
                saturating_atomic_add(&self.stats_acquired, 1);
                Ok(PoolAcquireResult {
                    conn,
                    permit: Some(permit),
                })
            }
            Err(TryAcquireError::NoPermits) => Err(PoolError::AcquireTimeout),
            Err(TryAcquireError::Closed) => Err(PoolError::Closed),
        }
    }

    /// Acquire a connection from the pool, waiting up to `acquire_timeout`.
    ///
    /// Returns an idle connection if available, or `None` as the connection
    /// value if the caller needs to create a fresh one (a permit is still held).
    pub async fn acquire(&self) -> Result<PoolAcquireResult<C>, PoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.acquire_with_cx(&cx).await
    }

    /// Acquire a connection using an explicit capability context.
    ///
    /// This preserves existing timeout behavior while allowing upstream
    /// call graphs to carry `Cx` explicitly. The acquire timeout is
    /// bound to the caller's `Cx` via
    /// [`crate::runtime_async::timeout_with_cx`] (ft-xbnl0.2.3) so
    /// cancellation on the caller's Cx cuts the semaphore wait
    /// deterministically instead of being pulled from
    /// `Cx::current()` thread-local state.
    pub async fn acquire_with_cx(&self, cx: &Cx) -> Result<PoolAcquireResult<C>, PoolError> {
        Self::checkpoint_explicit_cx(cx)?;

        let acquire_result = crate::runtime_async::timeout_with_cx(
            cx,
            self.config.acquire_timeout,
            self.semaphore.clone().acquire_owned_with_cx(cx),
        )
        .await;

        let permit = match acquire_result {
            Ok(Ok(permit)) => permit,
            Ok(Err(error)) => return Err(Self::classify_acquire_failure(cx, error)),
            Err(_timeout_err) => {
                // A configured acquire timeout does not mutate the caller's
                // Cx. A tighter caller deadline/quota does, and a checkpoint
                // exposes its finite root-cause class without consulting or
                // reflecting free-form cancellation reason text.
                if cx.checkpoint().is_err() {
                    return Err(Self::classify_cx_failure(cx));
                }
                saturating_atomic_add(&self.stats_timeouts, 1);
                return Err(PoolError::AcquireTimeout);
            }
        };

        let conn = {
            // Keep idle-queue cancellation typed. The permit is released
            // automatically if lock acquisition returns early.
            let mut idle = self
                .idle
                .lock_with_cx(cx)
                .await
                .map_err(|error| Self::classify_lock_failure(cx, error))?;
            #[cfg(test)]
            self.fire_after_idle_lock_acquired();
            Self::checkpoint_explicit_cx(cx)?;
            self.evict_expired(&mut idle);
            idle.pop_front().map(|e| e.conn)
        };
        saturating_atomic_add(&self.stats_acquired, 1);
        Ok(PoolAcquireResult {
            conn,
            permit: Some(permit),
        })
    }

    /// Return a connection to the pool for reuse.
    ///
    /// If the pool's idle queue is already at capacity, the connection is
    /// dropped instead.
    ///
    /// The caller must supply the capability context. There is intentionally
    /// no ambient convenience wrapper: returning a connection can wait for the
    /// idle-queue lock, so cancellation must remain an ordinary typed result
    /// rather than becoming a panic or being bypassed with a fresh context.
    /// The internal idle-pool
    /// mutex acquire returns a typed error when the caller cancels. If that
    /// happens, `conn` is dropped instead of being leaked or crossing a panic
    /// boundary.
    ///
    /// # Errors
    ///
    /// Preserves the caller's exact cancellation, deadline, quota, context, or
    /// lock-timeout class. Poisoning and invalid future reuse remain structural
    /// [`PoolError::LockAcquire`] failures.
    pub async fn put_with_cx(&self, cx: &Cx, conn: C) -> Result<(), PoolError> {
        let mut idle = self
            .idle
            .lock_with_cx(cx)
            .await
            .map_err(|error| Self::classify_lock_failure(cx, error))?;
        #[cfg(test)]
        self.fire_after_idle_lock_acquired();
        Self::checkpoint_explicit_cx(cx)?;
        self.evict_expired(&mut idle);
        if idle.len() < self.config.max_size {
            idle.push_back(PooledEntry {
                conn,
                returned_at: Instant::now(),
            });
            saturating_atomic_add(&self.stats_returned, 1);
        }
        // If queue is at max_size, connection is dropped (not returned).
        Ok(())
    }

    /// Evict expired idle connections under an explicit `&Cx`.
    ///
    /// There is intentionally no ambient wrapper because eviction can wait on
    /// the idle-queue lock and therefore must preserve cancellation as data.
    ///
    /// # Errors
    ///
    /// Returns a typed pool error if the idle queue cannot be locked.
    pub async fn evict_idle_with_cx(&self, cx: &Cx) -> Result<usize, PoolError> {
        let mut idle = self
            .idle
            .lock_with_cx(cx)
            .await
            .map_err(|error| Self::classify_lock_failure(cx, error))?;
        #[cfg(test)]
        self.fire_after_idle_lock_acquired();
        Self::checkpoint_explicit_cx(cx)?;
        Ok(self.evict_expired(&mut idle))
    }

    /// Get current pool statistics under an explicit `&Cx`.
    ///
    /// There is intentionally no ambient wrapper because the snapshot takes
    /// the idle-queue lock and therefore has a real cancellation failure mode.
    ///
    /// # Errors
    ///
    /// Returns a typed pool error if the idle queue cannot be locked.
    pub async fn stats_with_cx(&self, cx: &Cx) -> Result<PoolStats, PoolError> {
        let idle = self
            .idle
            .lock_with_cx(cx)
            .await
            .map_err(|error| Self::classify_lock_failure(cx, error))?;
        #[cfg(test)]
        self.fire_after_idle_lock_acquired();
        Self::checkpoint_explicit_cx(cx)?;
        let idle_count = idle.len();
        let acquired = self.stats_acquired.load(Ordering::Relaxed);
        let returned = self.stats_returned.load(Ordering::Relaxed);
        Ok(PoolStats {
            max_size: self.config.max_size,
            idle_count,
            active_count: self.config.max_size - self.semaphore.available_permits(),
            total_acquired: acquired,
            total_returned: returned,
            total_evicted: self.stats_evicted.load(Ordering::Relaxed),
            total_timeouts: self.stats_timeouts.load(Ordering::Relaxed),
        })
    }

    /// Drain all idle connections under an explicit `&Cx`.
    ///
    /// There is intentionally no ambient wrapper because shutdown cleanup must
    /// report cancellation instead of silently skipping work or panicking.
    ///
    /// # Errors
    ///
    /// Returns a typed pool error if the idle queue cannot be locked.
    pub async fn clear_with_cx(&self, cx: &Cx) -> Result<(), PoolError> {
        let mut idle = self
            .idle
            .lock_with_cx(cx)
            .await
            .map_err(|error| Self::classify_lock_failure(cx, error))?;
        #[cfg(test)]
        self.fire_after_idle_lock_acquired();
        Self::checkpoint_explicit_cx(cx)?;
        let count = idle.len() as u64;
        idle.clear();
        saturating_atomic_add(&self.stats_evicted, count);
        Ok(())
    }

    /// Internal: remove expired entries from the idle queue.
    fn evict_expired(&self, idle: &mut VecDeque<PooledEntry<C>>) -> usize {
        let cutoff = self.config.idle_timeout;
        let now = Instant::now();
        let mut evicted = 0;
        while let Some(front) = idle.front() {
            if now.duration_since(front.returned_at) > cutoff {
                idle.pop_front();
                evicted += 1;
            } else {
                break;
            }
        }
        if evicted > 0 {
            saturating_atomic_add(&self.stats_evicted, evicted as u64);
        }
        evicted
    }
}

/// Result of acquiring from the pool.
///
/// Holds a semaphore permit (limiting concurrency) and optionally an idle
/// connection. If `conn` is `None`, the caller should create a new connection.
/// The permit is released when this struct is dropped.
pub struct PoolAcquireResult<C> {
    /// An idle connection, or `None` if the caller needs to create one.
    pub conn: Option<C>,
    /// Semaphore permit — dropped when the acquire result is dropped.
    permit: Option<crate::runtime_async::OwnedSemaphorePermit>,
}

impl<C: std::fmt::Debug> std::fmt::Debug for PoolAcquireResult<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolAcquireResult")
            .field("conn", &self.conn)
            .field("has_permit", &self.permit.is_some())
            .finish()
    }
}

impl<C> PoolAcquireResult<C> {
    /// Whether an idle connection was provided.
    #[must_use]
    pub fn has_connection(&self) -> bool {
        self.conn.is_some()
    }

    /// Decompose into connection and guard, transferring permit ownership.
    ///
    /// The returned [`PoolAcquireGuard`] holds the concurrency slot. Drop it
    /// to release the slot back to the pool.
    pub fn into_parts(mut self) -> (Option<C>, PoolAcquireGuard) {
        let conn = self.conn.take();
        let permit = self
            .permit
            .take()
            .expect("permit already taken — into_parts called twice");
        (conn, PoolAcquireGuard { _permit: permit })
    }
}

impl<C> Drop for PoolAcquireResult<C> {
    fn drop(&mut self) {
        // If permit hasn't been moved out via into_parts, it drops here
        // releasing the semaphore slot automatically.
    }
}

/// Guard that holds a pool permit. Dropping it releases the slot.
pub struct PoolAcquireGuard {
    _permit: crate::runtime_async::OwnedSemaphorePermit,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only ergonomics for legacy assertions in this module. Product code
    /// has no ambient maintenance API; every helper below creates an explicit
    /// test context and asserts the typed result.
    trait TestPoolMaintenance<C> {
        async fn put(&self, conn: C);
        async fn evict_idle(&self) -> usize;
        async fn stats(&self) -> PoolStats;
        async fn clear(&self);
    }

    impl<C: Send + 'static> TestPoolMaintenance<C> for Pool<C> {
        async fn put(&self, conn: C) {
            let cx = Cx::for_testing();
            self.put_with_cx(&cx, conn)
                .await
                .expect("test pool return must succeed");
        }

        async fn evict_idle(&self) -> usize {
            let cx = Cx::for_testing();
            self.evict_idle_with_cx(&cx)
                .await
                .expect("test pool eviction must succeed")
        }

        async fn stats(&self) -> PoolStats {
            let cx = Cx::for_testing();
            self.stats_with_cx(&cx)
                .await
                .expect("test pool stats snapshot must succeed")
        }

        async fn clear(&self) {
            let cx = Cx::for_testing();
            self.clear_with_cx(&cx)
                .await
                .expect("test pool clear must succeed");
        }
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build pool test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn test_config(max_size: usize) -> PoolConfig {
        PoolConfig {
            max_size,
            idle_timeout: Duration::from_secs(60),
            acquire_timeout: Duration::from_millis(100),
        }
    }

    fn cancel_after_next_idle_lock<C: Send + 'static>(pool: &Pool<C>, cx: &Cx) {
        let cancel = cx.clone();
        *pool
            .after_idle_lock_acquired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            cancel.cancel_with(
                crate::outcome::CancelKind::User,
                Some("deterministic cancellation after idle lock grant"),
            );
        }));
    }

    #[test]
    fn pool_telemetry_counter_saturates_instead_of_wrapping() {
        let counter = AtomicU64::new(u64::MAX - 1);

        assert_eq!(saturating_atomic_add(&counter, 1), u64::MAX);
        assert_eq!(saturating_atomic_add(&counter, 1), u64::MAX);
        assert_eq!(saturating_atomic_add(&counter, u64::MAX), u64::MAX);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    async fn wait_for_all_permits_to_be_consumed<C: Send + 'static>(pool: &Pool<C>) {
        const MAX_SCHEDULER_STEPS: usize = 4_096;
        for _ in 0..MAX_SCHEDULER_STEPS {
            if pool.semaphore.available_permits() == 0 {
                return;
            }
            crate::runtime_async::yield_now().await;
        }
        assert_eq!(
            pool.semaphore.available_permits(),
            0,
            "waiter did not consume the pool permit after {MAX_SCHEDULER_STEPS} scheduler steps; configured_max_size={}",
            pool.config.max_size,
        );
    }

    #[test]
    fn pool_acquire_returns_none_when_empty() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            let result = pool.acquire().await.expect("should acquire");
            assert!(result.conn.is_none());
            assert!(!result.has_connection());
        });
    }

    #[test]
    fn pool_acquire_with_cx_returns_none_when_empty() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            let cx = crate::cx::for_testing();
            let result = pool.acquire_with_cx(&cx).await.expect("should acquire");
            assert!(result.conn.is_none());
            assert!(!result.has_connection());
        });
    }

    #[test]
    fn pool_put_and_acquire_returns_idle_connection() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("conn-1".to_string()).await;
            // Release the implicit semaphore hold — put doesn't hold a permit
            let result = pool.acquire().await.expect("should acquire");
            assert_eq!(result.conn.as_deref(), Some("conn-1"));
        });
    }

    #[test]
    fn pool_fifo_ordering() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            pool.put("first".to_string()).await;
            pool.put("second".to_string()).await;

            let r1 = pool.acquire().await.expect("acquire 1");
            assert_eq!(r1.conn.as_deref(), Some("first"));
            let r2 = pool.acquire().await.expect("acquire 2");
            assert_eq!(r2.conn.as_deref(), Some("second"));
        });
    }

    #[test]
    fn pool_respects_max_size() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));

            // Acquire the only slot
            let _held = pool.acquire().await.expect("acquire 1");

            // Second acquire should timeout
            let err = pool.acquire().await.expect_err("should timeout");
            assert_eq!(err, PoolError::AcquireTimeout);
        });
    }

    #[test]
    fn pool_releases_slot_on_drop() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));

            {
                let _held = pool.acquire().await.expect("acquire 1");
                // _held dropped here
            }

            // Should succeed now
            let result = pool.acquire().await.expect("acquire after drop");
            assert!(result.conn.is_none());
        });
    }

    #[test]
    fn pool_idle_timeout_eviction() {
        run_async_test(async {
            let config = PoolConfig {
                max_size: 2,
                idle_timeout: Duration::from_millis(10),
                acquire_timeout: Duration::from_millis(100),
            };
            let pool: Pool<String> = Pool::new(config);
            pool.put("stale".to_string()).await;

            // Wait for it to expire
            crate::runtime_async::sleep(Duration::from_millis(20)).await;

            let result = pool.acquire().await.expect("acquire");
            assert!(
                result.conn.is_none(),
                "stale connection should have been evicted"
            );
        });
    }

    #[test]
    fn pool_clear_drains_all() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;
            pool.put("c".to_string()).await;

            pool.clear().await;

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 0);
            assert_eq!(stats.total_evicted, 3);
        });
    }

    #[test]
    fn pool_stats_are_accurate() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));

            let stats = pool.stats().await;
            assert_eq!(stats.max_size, 2);
            assert_eq!(stats.idle_count, 0);
            assert_eq!(stats.active_count, 0);
            assert_eq!(stats.total_acquired, 0);

            pool.put("conn".to_string()).await;
            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 1);
            assert_eq!(stats.total_returned, 1);

            let _held = pool.acquire().await.expect("acquire");
            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 0);
            assert_eq!(stats.active_count, 1);
            assert_eq!(stats.total_acquired, 1);
        });
    }

    #[test]
    fn pool_try_acquire_when_full_batch2() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));
            let _held = pool.acquire().await.expect("acquire");

            let err = pool.try_acquire().await.expect_err("should fail");
            assert_eq!(err, PoolError::AcquireTimeout);
        });
    }

    #[test]
    fn pool_try_acquire_returns_idle() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("idle-conn".to_string()).await;

            let result = pool.try_acquire().await.expect("should succeed");
            assert_eq!(result.conn.as_deref(), Some("idle-conn"));
        });
    }

    #[test]
    fn pool_try_acquire_with_cx_returns_idle() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("idle-conn".to_string()).await;
            let cx = crate::cx::for_testing();

            let result = pool.try_acquire_with_cx(&cx).await.expect("should succeed");
            assert_eq!(result.conn.as_deref(), Some("idle-conn"));
        });
    }

    #[test]
    fn pool_concurrent_acquire_respects_limit() {
        run_async_test(async {
            let pool = Arc::new(Pool::<u64>::new(test_config(2)));
            let pool2 = pool.clone();
            let pool3 = pool.clone();

            let h1 = crate::runtime_async::task::spawn(async move {
                let _r = pool2.acquire().await.expect("acquire 1");
                crate::runtime_async::sleep(Duration::from_millis(50)).await;
            });

            let h2 = crate::runtime_async::task::spawn(async move {
                let _r = pool3.acquire().await.expect("acquire 2");
                crate::runtime_async::sleep(Duration::from_millis(50)).await;
            });

            // Both should succeed with pool size 2
            h1.await.expect("h1");
            h2.await.expect("h2");
        });
    }

    #[test]
    fn pool_evict_idle_returns_count() {
        run_async_test(async {
            let config = PoolConfig {
                max_size: 4,
                idle_timeout: Duration::from_millis(10),
                acquire_timeout: Duration::from_millis(100),
            };
            let pool: Pool<String> = Pool::new(config);
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;

            crate::runtime_async::sleep(Duration::from_millis(20)).await;
            let evicted = pool.evict_idle().await;
            assert_eq!(evicted, 2);
        });
    }

    #[test]
    fn pool_config_default() {
        let config = PoolConfig::default();
        assert_eq!(config.max_size, 4);
        assert_eq!(config.idle_timeout, Duration::from_secs(300));
        assert_eq!(config.acquire_timeout, Duration::from_secs(5));
    }

    #[test]
    fn pool_config_serde_roundtrip_batch2() {
        let config = PoolConfig {
            max_size: 8,
            idle_timeout: Duration::from_secs(120),
            acquire_timeout: Duration::from_secs(3),
        };
        let json = serde_json::to_string(&config).expect("serialize");
        let deserialized: PoolConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.max_size, 8);
    }

    #[test]
    fn pool_stats_serde_roundtrip_batch2() {
        let stats = PoolStats {
            max_size: 4,
            idle_count: 2,
            active_count: 1,
            total_acquired: 10,
            total_returned: 8,
            total_evicted: 1,
            total_timeouts: 0,
        };
        let json = serde_json::to_string(&stats).expect("serialize");
        let deserialized: PoolStats = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.total_acquired, 10);
        assert_eq!(deserialized.idle_count, 2);
    }

    #[test]
    fn pool_error_display() {
        assert_eq!(
            PoolError::AcquireTimeout.to_string(),
            "connection pool acquire timeout"
        );
        assert_eq!(PoolError::Closed.to_string(), "connection pool is closed");
        assert_eq!(
            PoolError::Cancelled.to_string(),
            "connection pool operation cancelled"
        );
        assert_eq!(
            PoolError::LockAcquire(LockAcquireError::Poisoned).to_string(),
            "connection pool lock acquisition failed: lock is poisoned"
        );
        assert_eq!(
            PoolError::LockTimedOut { deadline_nanos: 17 }.to_string(),
            "connection pool lock timed out at 17ns"
        );
        assert_eq!(
            PoolError::PolledAfterCompletion.to_string(),
            "connection pool acquire future polled after completion"
        );
    }

    #[test]
    fn pool_acquire_invariant_error_is_not_reported_as_cancellation() {
        let cx = Cx::for_testing();
        assert_eq!(
            Pool::<String>::classify_acquire_failure(
                &cx,
                crate::runtime_async::AcquireError::PolledAfterCompletion,
            ),
            PoolError::PolledAfterCompletion
        );
        assert_ne!(PoolError::PolledAfterCompletion, PoolError::Cancelled);
    }

    #[test]
    fn pool_lock_failure_preserves_every_finite_context_and_timeout_class() {
        let cx = Cx::for_testing();
        let cases = [
            (LockAcquireError::Cancelled, PoolError::Cancelled),
            (
                LockAcquireError::DeadlineExceeded,
                PoolError::DeadlineExceeded,
            ),
            (
                LockAcquireError::PollQuotaExhausted,
                PoolError::PollQuotaExhausted,
            ),
            (
                LockAcquireError::CostBudgetExhausted,
                PoolError::CostBudgetExhausted,
            ),
            (
                LockAcquireError::ContextFailure,
                PoolError::ContextFailure,
            ),
            (
                LockAcquireError::TimedOut { deadline_nanos: 23 },
                PoolError::LockTimedOut { deadline_nanos: 23 },
            ),
            (
                LockAcquireError::Poisoned,
                PoolError::LockAcquire(LockAcquireError::Poisoned),
            ),
            (
                LockAcquireError::PolledAfterCompletion,
                PoolError::LockAcquire(LockAcquireError::PolledAfterCompletion),
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(Pool::<String>::classify_lock_failure(&cx, error), expected);
        }
    }

    #[test]
    fn pool_checkpoint_preserves_deadline_and_quota_classes() {
        run_async_test(async {
            let cases = [
                (
                    Cx::for_testing_with_budget(
                        crate::cx::Budget::new()
                            .with_deadline(crate::runtime_async::RuntimeTime::ZERO),
                    ),
                    PoolError::DeadlineExceeded,
                ),
                (
                    Cx::for_testing_with_budget(
                        crate::cx::Budget::new().with_poll_quota(0),
                    ),
                    PoolError::PollQuotaExhausted,
                ),
                (
                    Cx::for_testing_with_budget(
                        crate::cx::Budget::new().with_cost_quota(0),
                    ),
                    PoolError::CostBudgetExhausted,
                ),
            ];

            for (cx, expected) in cases {
                let pool = Pool::<String>::new(test_config(1));
                let error = pool
                    .try_acquire_with_cx(&cx)
                    .await
                    .expect_err("exhausted Cx must reject pool acquisition");
                assert_eq!(error, expected);
            }
        });
    }

    #[test]
    fn pool_context_failure_classification_is_exhaustive_and_content_free() {
        use crate::outcome::CancelKind;

        const SECRET: &str = "SECRET pool cancellation classification";
        let cases = [
            (CancelKind::User, PoolError::Cancelled),
            (CancelKind::Timeout, PoolError::DeadlineExceeded),
            (CancelKind::Deadline, PoolError::DeadlineExceeded),
            (CancelKind::PollQuota, PoolError::PollQuotaExhausted),
            (CancelKind::CostBudget, PoolError::CostBudgetExhausted),
            (CancelKind::FailFast, PoolError::Cancelled),
            (CancelKind::RaceLost, PoolError::Cancelled),
            (CancelKind::ParentCancelled, PoolError::Cancelled),
            (CancelKind::ResourceUnavailable, PoolError::Cancelled),
            (CancelKind::Shutdown, PoolError::Cancelled),
            (CancelKind::LinkedExit, PoolError::Cancelled),
        ];

        for (kind, expected) in cases {
            let cx = Cx::for_testing();
            cx.cancel_with(kind, Some(SECRET));
            let error = Pool::<String>::classify_cx_failure(&cx);
            assert_eq!(error, expected, "unexpected class for {kind:?}");
            assert!(!error.to_string().contains(SECRET));
            assert!(!format!("{error:?}").contains(SECRET));
        }

        assert_eq!(
            Pool::<String>::classify_cx_failure(&Cx::for_testing()),
            PoolError::ContextFailure
        );
    }

    #[test]
    fn pool_into_parts_transfers_permit() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));
            pool.put("conn".to_string()).await;

            let result = pool.acquire().await.expect("acquire");
            let (conn, _guard) = result.into_parts();
            assert_eq!(conn.as_deref(), Some("conn"));

            // Slot is still held by guard
            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 1);

            // Drop guard
            drop(_guard);
            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 0);
        });
    }

    #[test]
    fn pool_put_excess_connections_dropped() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;
            pool.put("c".to_string()).await; // Exceeds max_size, should be dropped

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 2);
            assert_eq!(stats.total_returned, 2);
        });
    }

    // ── Batch: RubyBeaver wa-1u90p.7.1 ──────────────────────────────────

    #[test]
    fn pool_error_is_clone() {
        let err = PoolError::AcquireTimeout;
        let cloned = err.clone();
        assert_eq!(err, cloned);
    }

    #[test]
    fn pool_error_is_std_error() {
        let err: &dyn std::error::Error = &PoolError::AcquireTimeout;
        assert!(err.source().is_none());
    }

    #[test]
    fn pool_error_debug_format() {
        let err = PoolError::AcquireTimeout;
        let dbg = format!("{err:?}");
        assert!(dbg.contains("AcquireTimeout"));
    }

    #[test]
    fn pool_error_closed_debug_format() {
        let err = PoolError::Closed;
        let dbg = format!("{err:?}");
        assert!(dbg.contains("Closed"));
    }

    #[test]
    fn pool_error_cancelled_debug_format() {
        let err = PoolError::Cancelled;
        let dbg = format!("{err:?}");
        assert!(dbg.contains("Cancelled"));
    }

    #[test]
    fn pool_config_debug_batch2() {
        let config = PoolConfig::default();
        let dbg = format!("{config:?}");
        assert!(dbg.contains("max_size"));
        assert!(dbg.contains("idle_timeout"));
    }

    #[test]
    fn pool_config_clone() {
        let config = PoolConfig {
            max_size: 16,
            idle_timeout: Duration::from_secs(999),
            acquire_timeout: Duration::from_millis(42),
        };
        let cloned = config.clone();
        assert_eq!(cloned.max_size, 16);
        assert_eq!(cloned.idle_timeout, Duration::from_secs(999));
        assert_eq!(cloned.acquire_timeout, Duration::from_millis(42));
    }

    #[test]
    fn pool_config_serde_all_fields_preserved() {
        let config = PoolConfig {
            max_size: 32,
            idle_timeout: Duration::from_secs(600),
            acquire_timeout: Duration::from_millis(250),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: PoolConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_size, 32);
        assert_eq!(back.idle_timeout, Duration::from_secs(600));
        assert_eq!(back.acquire_timeout, Duration::from_millis(250));
    }

    #[test]
    fn pool_stats_debug_batch2() {
        let stats = PoolStats {
            max_size: 1,
            idle_count: 0,
            active_count: 0,
            total_acquired: 0,
            total_returned: 0,
            total_evicted: 0,
            total_timeouts: 0,
        };
        let dbg = format!("{stats:?}");
        assert!(dbg.contains("max_size"));
        assert!(dbg.contains("total_timeouts"));
    }

    #[test]
    fn pool_stats_clone_batch2() {
        let stats = PoolStats {
            max_size: 8,
            idle_count: 3,
            active_count: 2,
            total_acquired: 100,
            total_returned: 95,
            total_evicted: 5,
            total_timeouts: 3,
        };
        let cloned = stats.clone();
        assert_eq!(cloned.max_size, 8);
        assert_eq!(cloned.total_acquired, 100);
        assert_eq!(cloned.total_timeouts, 3);
    }

    #[test]
    fn pool_stats_serde_all_fields() {
        let stats = PoolStats {
            max_size: 10,
            idle_count: 4,
            active_count: 3,
            total_acquired: 50,
            total_returned: 45,
            total_evicted: 2,
            total_timeouts: 1,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: PoolStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_size, 10);
        assert_eq!(back.idle_count, 4);
        assert_eq!(back.active_count, 3);
        assert_eq!(back.total_acquired, 50);
        assert_eq!(back.total_returned, 45);
        assert_eq!(back.total_evicted, 2);
        assert_eq!(back.total_timeouts, 1);
    }

    #[test]
    fn pool_stats_timeout_counter() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));
            let _held = pool.acquire().await.expect("acquire slot");

            // This should timeout and increment the counter
            let _ = pool.acquire().await;

            let stats = pool.stats().await;
            assert_eq!(stats.total_timeouts, 1);
        });
    }

    #[test]
    fn pool_stats_initial_all_zero() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 0);
            assert_eq!(stats.active_count, 0);
            assert_eq!(stats.total_acquired, 0);
            assert_eq!(stats.total_returned, 0);
            assert_eq!(stats.total_evicted, 0);
            assert_eq!(stats.total_timeouts, 0);
        });
    }

    #[test]
    fn pool_clear_on_empty_is_noop() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.clear().await;
            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 0);
            assert_eq!(stats.total_evicted, 0);
        });
    }

    #[test]
    fn pool_put_after_clear_works() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("a".to_string()).await;
            pool.clear().await;
            pool.put("b".to_string()).await;

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 1);
            let result = pool.acquire().await.expect("acquire");
            assert_eq!(result.conn.as_deref(), Some("b"));
        });
    }

    #[test]
    fn pool_evict_idle_returns_zero_when_fresh() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            pool.put("fresh".to_string()).await;
            let evicted = pool.evict_idle().await;
            assert_eq!(evicted, 0);
        });
    }

    #[test]
    fn pool_evict_idle_on_empty_returns_zero() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            let evicted = pool.evict_idle().await;
            assert_eq!(evicted, 0);
        });
    }

    #[test]
    fn pool_evict_partial_only_stale() {
        run_async_test(async {
            // put() calls evict_expired internally, so we must ensure
            // old is still within timeout at the time of put("new").
            // Constraints: first_sleep < timeout, first_sleep + second_sleep > timeout,
            // second_sleep < timeout.
            let config = PoolConfig {
                max_size: 4,
                idle_timeout: Duration::from_millis(500),
                acquire_timeout: Duration::from_millis(500),
            };
            let pool: Pool<String> = Pool::new(config);
            pool.put("old".to_string()).await;
            // old age: ~300ms (< 500ms timeout), survives put("new") evict
            crate::runtime_async::sleep(Duration::from_millis(300)).await;
            pool.put("new".to_string()).await;
            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 2);
            // old age: ~700ms (> 500ms, stale), new age: ~400ms (< 500ms, fresh)
            crate::runtime_async::sleep(Duration::from_millis(400)).await;

            let evicted = pool.evict_idle().await;
            assert_eq!(evicted, 1);

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 1);
        });
    }

    #[test]
    fn pool_into_parts_with_none_connection() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            let result = pool.acquire().await.expect("acquire empty slot");
            assert!(!result.has_connection());

            let (conn, guard) = result.into_parts();
            assert!(conn.is_none());

            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 1);
            drop(guard);
            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 0);
        });
    }

    #[test]
    fn pool_try_acquire_no_idle_returns_none_conn() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            let result = pool.try_acquire().await.expect("slot available");
            assert!(result.conn.is_none());
        });
    }

    #[test]
    fn pool_acquire_result_debug_batch2() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("test-conn".to_string()).await;
            let result = pool.acquire().await.expect("acquire");
            let dbg = format!("{result:?}");
            assert!(dbg.contains("PoolAcquireResult"));
            assert!(dbg.contains("test-conn"));
            assert!(dbg.contains("has_permit"));
        });
    }

    #[test]
    fn pool_acquire_release_cycle() {
        run_async_test(async {
            let pool: Pool<u32> = Pool::new(test_config(2));
            for i in 0..10u32 {
                let result = pool.acquire().await.expect("acquire");
                drop(result);
                pool.put(i).await;
            }
            let stats = pool.stats().await;
            assert_eq!(stats.total_acquired, 10);
            assert_eq!(stats.total_returned, 10);
        });
    }

    #[test]
    fn pool_stats_active_returns_to_zero() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(3));
            let r1 = pool.acquire().await.unwrap();
            let r2 = pool.acquire().await.unwrap();
            let r3 = pool.acquire().await.unwrap();

            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 3);

            drop(r1);
            drop(r2);
            drop(r3);

            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 0);
        });
    }

    #[test]
    fn pool_fifo_after_put_back() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            pool.put("first".to_string()).await;
            pool.put("second".to_string()).await;

            // Acquire first, put it back, acquire again
            let r1 = pool.acquire().await.unwrap();
            assert_eq!(r1.conn.as_deref(), Some("first"));
            drop(r1);
            pool.put("first-recycled".to_string()).await;

            let r2 = pool.acquire().await.unwrap();
            assert_eq!(r2.conn.as_deref(), Some("second"));
            let r3 = pool.acquire().await.unwrap();
            assert_eq!(r3.conn.as_deref(), Some("first-recycled"));
        });
    }

    #[test]
    fn pool_multiple_concurrent_three_slots() {
        run_async_test(async {
            let pool = Arc::new(Pool::<u64>::new(test_config(3)));
            let mut handles = Vec::new();

            for i in 0..3u64 {
                let p = pool.clone();
                handles.push(crate::runtime_async::task::spawn(async move {
                    let r = p.acquire().await.expect("acquire");
                    crate::runtime_async::sleep(Duration::from_millis(10)).await;
                    drop(r);
                    p.put(i).await;
                }));
            }

            for h in handles {
                h.await.expect("task");
            }

            let stats = pool.stats().await;
            assert_eq!(stats.total_acquired, 3);
        });
    }

    #[test]
    fn pool_stats_evicted_increments_on_clear() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;
            pool.clear().await;

            pool.put("c".to_string()).await;
            pool.clear().await;

            let stats = pool.stats().await;
            assert_eq!(stats.total_evicted, 3); // 2 + 1
        });
    }

    #[test]
    fn pool_with_large_max_size() {
        run_async_test(async {
            let pool: Pool<u32> = Pool::new(test_config(100));
            for i in 0..50u32 {
                pool.put(i).await;
            }
            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 50);
            assert_eq!(stats.total_returned, 50);
        });
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ──────────────────────────────────

    #[test]
    fn pool_error_variants_not_equal() {
        assert_ne!(PoolError::AcquireTimeout, PoolError::Closed);
    }

    #[test]
    fn pool_error_closed_is_std_error() {
        let err: &dyn std::error::Error = &PoolError::Closed;
        assert!(err.source().is_none());
    }

    #[test]
    fn pool_config_zero_max_size() {
        let config = PoolConfig {
            max_size: 0,
            idle_timeout: Duration::ZERO,
            acquire_timeout: Duration::ZERO,
        };
        assert_eq!(config.max_size, 0);
    }

    #[test]
    fn pool_config_very_large_timeout() {
        let config = PoolConfig {
            max_size: 1,
            idle_timeout: Duration::from_secs(u64::MAX / 2),
            acquire_timeout: Duration::from_secs(1),
        };
        assert!(config.idle_timeout > Duration::from_secs(1_000_000));
    }

    #[test]
    fn pool_stats_serde_json_keys() {
        let stats = PoolStats {
            max_size: 1,
            idle_count: 0,
            active_count: 0,
            total_acquired: 0,
            total_returned: 0,
            total_evicted: 0,
            total_timeouts: 0,
        };
        let json: serde_json::Value = serde_json::to_value(&stats).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("max_size"));
        assert!(obj.contains_key("idle_count"));
        assert!(obj.contains_key("active_count"));
        assert!(obj.contains_key("total_acquired"));
        assert!(obj.contains_key("total_returned"));
        assert!(obj.contains_key("total_evicted"));
        assert!(obj.contains_key("total_timeouts"));
        assert_eq!(obj.len(), 7);
    }

    #[test]
    fn pool_multiple_timeouts_accumulate() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));
            let _held = pool.acquire().await.expect("acquire slot");

            for _ in 0..3 {
                let _ = pool.acquire().await;
            }

            let stats = pool.stats().await;
            assert_eq!(stats.total_timeouts, 3);
        });
    }

    #[test]
    fn pool_put_and_clear_and_stats_consistent() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;
            pool.put("c".to_string()).await;

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 3);
            assert_eq!(stats.total_returned, 3);
            assert_eq!(stats.total_evicted, 0);

            pool.clear().await;

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 0);
            assert_eq!(stats.total_returned, 3);
            assert_eq!(stats.total_evicted, 3);
        });
    }

    #[test]
    fn pool_acquire_counts_only_acquire_not_put() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;

            let stats = pool.stats().await;
            assert_eq!(
                stats.total_acquired, 0,
                "put should not increment total_acquired"
            );

            let _r = pool.acquire().await.unwrap();
            let stats = pool.stats().await;
            assert_eq!(stats.total_acquired, 1);
        });
    }

    #[test]
    fn pool_try_acquire_increments_acquired() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            let _r = pool.try_acquire().await.unwrap();
            let stats = pool.stats().await;
            assert_eq!(stats.total_acquired, 1);
        });
    }

    #[test]
    fn pool_has_connection_method() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            pool.put("conn".to_string()).await;

            let with_conn = pool.acquire().await.unwrap();
            assert!(with_conn.has_connection());
            drop(with_conn);

            // Now pool is empty (connection was taken, not returned)
            let without_conn = pool.acquire().await.unwrap();
            assert!(!without_conn.has_connection());
        });
    }

    // ── Batch 2: DarkBadger wa-1u90p.7.1 ─────────────────────────────────

    #[test]
    fn pool_error_display_acquire_timeout() {
        let err = PoolError::AcquireTimeout;
        let msg = format!("{err}");
        assert_eq!(msg, "connection pool acquire timeout");
    }

    #[test]
    fn pool_error_display_closed() {
        let err = PoolError::Closed;
        let msg = format!("{err}");
        assert_eq!(msg, "connection pool is closed");
    }

    #[test]
    fn pool_error_clone() {
        let err = PoolError::AcquireTimeout;
        let cloned = err.clone();
        assert_eq!(err, cloned);
    }

    #[test]
    fn pool_error_debug() {
        let err = PoolError::Closed;
        let debug = format!("{err:?}");
        assert!(debug.contains("Closed"));
    }

    #[test]
    fn pool_config_default_values() {
        let cfg = PoolConfig::default();
        assert_eq!(cfg.max_size, 4);
        assert_eq!(cfg.idle_timeout, Duration::from_secs(300));
        assert_eq!(cfg.acquire_timeout, Duration::from_secs(5));
    }

    #[test]
    fn pool_config_serde_roundtrip_v2() {
        let cfg = PoolConfig {
            max_size: 8,
            idle_timeout: Duration::from_secs(120),
            acquire_timeout: Duration::from_millis(500),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: PoolConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_size, 8);
        assert_eq!(back.idle_timeout, Duration::from_secs(120));
        assert_eq!(back.acquire_timeout, Duration::from_millis(500));
    }

    #[test]
    fn pool_config_debug_v2() {
        let cfg = PoolConfig::default();
        let debug = format!("{cfg:?}");
        assert!(debug.contains("PoolConfig"));
        assert!(debug.contains("max_size"));
    }

    #[test]
    fn pool_stats_serde_roundtrip_v2() {
        let stats = PoolStats {
            max_size: 4,
            idle_count: 2,
            active_count: 1,
            total_acquired: 10,
            total_returned: 8,
            total_evicted: 3,
            total_timeouts: 1,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: PoolStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats.max_size, back.max_size);
        assert_eq!(stats.idle_count, back.idle_count);
        assert_eq!(stats.active_count, back.active_count);
        assert_eq!(stats.total_acquired, back.total_acquired);
        assert_eq!(stats.total_returned, back.total_returned);
        assert_eq!(stats.total_evicted, back.total_evicted);
        assert_eq!(stats.total_timeouts, back.total_timeouts);
    }

    #[test]
    fn pool_stats_debug_v2() {
        let stats = PoolStats {
            max_size: 2,
            idle_count: 0,
            active_count: 1,
            total_acquired: 5,
            total_returned: 4,
            total_evicted: 0,
            total_timeouts: 0,
        };
        let debug = format!("{stats:?}");
        assert!(debug.contains("PoolStats"));
        assert!(debug.contains("total_acquired"));
    }

    #[test]
    fn pool_stats_clone_v2() {
        let stats = PoolStats {
            max_size: 3,
            idle_count: 1,
            active_count: 2,
            total_acquired: 7,
            total_returned: 5,
            total_evicted: 1,
            total_timeouts: 0,
        };
        let cloned = stats.clone();
        assert_eq!(stats.max_size, cloned.max_size);
        assert_eq!(stats.total_acquired, cloned.total_acquired);
    }

    #[test]
    fn pool_into_parts_decompose() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("hello".to_string()).await;

            let result = pool.acquire().await.unwrap();
            assert!(result.has_connection());

            let (conn, _guard) = result.into_parts();
            assert_eq!(conn, Some("hello".to_string()));

            // Guard holds the permit — pool slot is still occupied
            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 1);
        });
    }

    #[test]
    fn pool_into_parts_releases_on_guard_drop() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));
            let result = pool.acquire().await.unwrap();
            let (conn, guard) = result.into_parts();
            assert!(conn.is_none()); // no idle connection

            // Pool is at capacity (1 slot held by guard)
            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 1);

            // Dropping guard releases the slot
            drop(guard);
            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 0);
        });
    }

    #[test]
    fn pool_into_parts_no_connection() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            let result = pool.acquire().await.unwrap();
            let (conn, _guard) = result.into_parts();
            assert!(conn.is_none());
        });
    }

    #[test]
    fn pool_acquire_result_debug_v2() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("debug-test".to_string()).await;

            let result = pool.acquire().await.unwrap();
            let debug = format!("{result:?}");
            assert!(debug.contains("PoolAcquireResult"));
            assert!(debug.contains("debug-test"));
            assert!(debug.contains("has_permit"));
        });
    }

    #[test]
    fn pool_try_acquire_when_full_v2() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));
            let _held = pool.acquire().await.unwrap();

            let err = pool.try_acquire().await.unwrap_err();
            assert_eq!(err, PoolError::AcquireTimeout);
        });
    }

    #[test]
    fn pool_try_acquire_returns_idle_conn() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("idle".to_string()).await;

            let result = pool.try_acquire().await.unwrap();
            assert!(result.has_connection());
            assert_eq!(result.conn.as_deref(), Some("idle"));
        });
    }

    #[test]
    fn pool_try_acquire_returns_none_when_no_idle() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            let result = pool.try_acquire().await.unwrap();
            assert!(!result.has_connection());
            assert!(result.conn.is_none());
        });
    }

    #[test]
    fn pool_evict_idle_returns_zero_when_none_expired() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;

            let evicted = pool.evict_idle().await;
            assert_eq!(evicted, 0);

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 2);
        });
    }

    #[test]
    fn pool_evict_idle_evicts_expired_entries() {
        run_async_test(async {
            let config = PoolConfig {
                max_size: 4,
                idle_timeout: Duration::ZERO, // everything expires immediately
                acquire_timeout: Duration::from_millis(100),
            };
            let pool: Pool<String> = Pool::new(config);
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;

            // Wait a tiny bit so the entries are past idle_timeout=0
            crate::runtime_async::sleep(Duration::from_millis(5)).await;

            let evicted = pool.evict_idle().await;
            // put() eagerly evicts expired entries, so "a" may already be gone
            // With ZERO timeout, at least some entries are evicted
            assert!(evicted >= 1);

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 0);
            assert!(stats.total_evicted >= 2);
        });
    }

    #[test]
    fn pool_put_at_max_capacity_drops_connection() {
        run_async_test(async {
            // Pool with max_size=2, fill it to capacity, then put one more
            let pool: Pool<String> = Pool::new(test_config(2));
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;

            // This should be silently dropped since idle queue is at max_size
            pool.put("c".to_string()).await;

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 2); // still 2, not 3
            // Only 2 were counted as returned
            assert_eq!(stats.total_returned, 2);

            // Verify which connections are in the pool (FIFO)
            let r1 = pool.acquire().await.unwrap();
            assert_eq!(r1.conn.as_deref(), Some("a"));
            let r2 = pool.acquire().await.unwrap();
            assert_eq!(r2.conn.as_deref(), Some("b"));
        });
    }

    #[test]
    fn pool_clear_then_refill() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(3));
            pool.put("a".to_string()).await;
            pool.put("b".to_string()).await;
            pool.clear().await;

            // Refill after clear
            pool.put("c".to_string()).await;
            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 1);
            assert_eq!(stats.total_evicted, 2);
            assert_eq!(stats.total_returned, 3); // a + b + c

            let r = pool.acquire().await.unwrap();
            assert_eq!(r.conn.as_deref(), Some("c"));
        });
    }

    #[test]
    fn pool_stats_max_size_reflects_config() {
        run_async_test(async {
            let pool: Pool<u32> = Pool::new(test_config(7));
            let stats = pool.stats().await;
            assert_eq!(stats.max_size, 7);
        });
    }

    #[test]
    fn pool_stats_active_count_with_multiple_acquires() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(5));
            let r1 = pool.acquire().await.unwrap();
            let r2 = pool.acquire().await.unwrap();
            let r3 = pool.acquire().await.unwrap();

            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 3);

            drop(r2);
            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 2);

            drop(r1);
            drop(r3);
            let stats = pool.stats().await;
            assert_eq!(stats.active_count, 0);
        });
    }

    #[test]
    fn pool_acquire_after_timeout_still_works() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));
            let held = pool.acquire().await.unwrap();

            // This should timeout
            let err = pool.acquire().await.unwrap_err();
            assert_eq!(err, PoolError::AcquireTimeout);

            // Release and try again
            drop(held);
            let result = pool.acquire().await;
            assert!(result.is_ok());
        });
    }

    #[test]
    fn pool_lock_grant_then_cancel_never_reads_or_mutates_idle_state() {
        run_async_test(async {
            for blocking_acquire in [false, true] {
                let pool: Pool<String> = Pool::new(test_config(1));
                pool.put("idle".to_string()).await;
                let cx = Cx::for_testing();
                cancel_after_next_idle_lock(&pool, &cx);

                let error = if blocking_acquire {
                    pool.acquire_with_cx(&cx)
                        .await
                        .expect_err("post-grant cancellation must reject acquire")
                } else {
                    pool.try_acquire_with_cx(&cx)
                        .await
                        .expect_err("post-grant cancellation must reject try_acquire")
                };
                assert_eq!(error, PoolError::Cancelled);

                let stats_cx = Cx::for_testing();
                let stats = pool
                    .stats_with_cx(&stats_cx)
                    .await
                    .expect("fresh context must read pool stats");
                assert_eq!(stats.idle_count, 1, "cancelled acquire must not pop");
                assert_eq!(stats.active_count, 0, "permit must be released");
                assert_eq!(stats.total_acquired, 0);
            }

            struct DropProbe(Arc<AtomicU64>);
            impl Drop for DropProbe {
                fn drop(&mut self) {
                    saturating_atomic_add(&self.0, 1);
                }
            }

            let put_pool: Pool<DropProbe> = Pool::new(test_config(1));
            let dropped = Arc::new(AtomicU64::new(0));
            let put_cx = Cx::for_testing();
            cancel_after_next_idle_lock(&put_pool, &put_cx);
            assert_eq!(
                put_pool
                    .put_with_cx(&put_cx, DropProbe(Arc::clone(&dropped)))
                    .await
                    .expect_err("post-grant cancellation must reject put"),
                PoolError::Cancelled
            );
            assert_eq!(dropped.load(Ordering::Relaxed), 1);
            let put_stats = put_pool
                .stats_with_cx(&Cx::for_testing())
                .await
                .expect("fresh context must read put stats");
            assert_eq!(put_stats.idle_count, 0);
            assert_eq!(put_stats.total_returned, 0);

            let mut evict_config = test_config(1);
            evict_config.idle_timeout = Duration::ZERO;
            let evict_pool: Pool<String> = Pool::new(evict_config);
            evict_pool.put("idle".to_string()).await;
            let evict_cx = Cx::for_testing();
            cancel_after_next_idle_lock(&evict_pool, &evict_cx);
            assert_eq!(
                evict_pool
                    .evict_idle_with_cx(&evict_cx)
                    .await
                    .expect_err("post-grant cancellation must reject eviction"),
                PoolError::Cancelled
            );
            let evict_stats = evict_pool
                .stats_with_cx(&Cx::for_testing())
                .await
                .expect("fresh context must read eviction stats");
            assert_eq!(evict_stats.idle_count, 1);
            assert_eq!(evict_stats.total_evicted, 0);

            let clear_pool: Pool<String> = Pool::new(test_config(1));
            clear_pool.put("idle".to_string()).await;
            let clear_cx = Cx::for_testing();
            cancel_after_next_idle_lock(&clear_pool, &clear_cx);
            assert_eq!(
                clear_pool
                    .clear_with_cx(&clear_cx)
                    .await
                    .expect_err("post-grant cancellation must reject clear"),
                PoolError::Cancelled
            );
            let clear_stats = clear_pool
                .stats_with_cx(&Cx::for_testing())
                .await
                .expect("fresh context must read clear stats");
            assert_eq!(clear_stats.idle_count, 1);
            assert_eq!(clear_stats.total_evicted, 0);

            let stats_pool: Pool<String> = Pool::new(test_config(1));
            let stats_cx = Cx::for_testing();
            cancel_after_next_idle_lock(&stats_pool, &stats_cx);
            assert_eq!(
                stats_pool
                    .stats_with_cx(&stats_cx)
                    .await
                    .expect_err("post-grant cancellation must reject stats snapshot"),
                PoolError::Cancelled
            );
        });
    }

    #[test]
    fn pool_try_acquire_with_precancelled_cx_returns_cancelled_without_taking_idle() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));
            pool.put("idle".to_string()).await;

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancelled try acquire"),
            );

            let err = pool
                .try_acquire_with_cx(&cx)
                .await
                .expect_err("pre-cancelled try_acquire_with_cx should fail");
            assert_eq!(err, PoolError::Cancelled);

            let stats = pool.stats().await;
            assert_eq!(
                stats.idle_count, 1,
                "cancelled try_acquire should not pop idle state"
            );
            assert_eq!(stats.total_acquired, 0);
        });
    }

    #[test]
    fn pool_try_acquire_cancellation_during_idle_lock_wait_is_typed() {
        run_async_test(async {
            let pool = Arc::new(Pool::<String>::new(test_config(1)));
            let lock_cx = Cx::for_testing();
            let idle_guard = pool
                .idle
                .lock_with_cx(&lock_cx)
                .await
                .expect("live lock context must acquire idle guard");

            let operation_cx = Cx::for_testing();
            let task_cx = operation_cx.clone();
            let task_pool = Arc::clone(&pool);
            let waiter = crate::runtime_async::task::spawn(async move {
                task_pool.try_acquire_with_cx(&task_cx).await
            });

            wait_for_all_permits_to_be_consumed(&pool).await;
            assert_eq!(
                pool.semaphore.available_permits(),
                0,
                "waiter must hold the permit before cancellation exercises the idle lock"
            );
            operation_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel try_acquire during idle lock wait"),
            );
            drop(idle_guard);

            let error = waiter
                .await
                .expect("try_acquire waiter task should join")
                .expect_err("cancelled idle-lock wait must fail");
            assert_eq!(error, PoolError::Cancelled);
            assert_eq!(
                pool.semaphore.available_permits(),
                1,
                "typed lock cancellation must release the acquired permit"
            );
        });
    }

    #[test]
    fn pool_acquire_with_precancelled_cx_returns_cancelled_without_timeout() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancelled acquire"),
            );

            let err = pool
                .acquire_with_cx(&cx)
                .await
                .expect_err("pre-cancelled acquire_with_cx should fail");
            assert_eq!(err, PoolError::Cancelled);

            let stats = pool.stats().await;
            assert_eq!(
                stats.total_timeouts, 0,
                "cancelled acquire must not count as timeout"
            );
            assert_eq!(stats.total_acquired, 0);
            assert_eq!(stats.active_count, 0);
        });
    }

    #[test]
    fn pool_acquire_cancellation_during_idle_lock_wait_is_typed() {
        run_async_test(async {
            let pool = Arc::new(Pool::<String>::new(test_config(1)));
            let lock_cx = Cx::for_testing();
            let idle_guard = pool
                .idle
                .lock_with_cx(&lock_cx)
                .await
                .expect("live lock context must acquire idle guard");

            let operation_cx = Cx::for_testing();
            let task_cx = operation_cx.clone();
            let task_pool = Arc::clone(&pool);
            let waiter = crate::runtime_async::task::spawn(async move {
                task_pool.acquire_with_cx(&task_cx).await
            });

            wait_for_all_permits_to_be_consumed(&pool).await;
            assert_eq!(
                pool.semaphore.available_permits(),
                0,
                "waiter must hold the permit before cancellation exercises the idle lock"
            );
            operation_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel acquire during idle lock wait"),
            );
            drop(idle_guard);

            let error = waiter
                .await
                .expect("acquire waiter task should join")
                .expect_err("cancelled idle-lock wait must fail");
            assert_eq!(error, PoolError::Cancelled);
            assert_eq!(
                pool.semaphore.available_permits(),
                1,
                "typed lock cancellation must release the acquired permit"
            );
        });
    }

    #[test]
    fn pool_acquire_with_cx_cancelled_while_waiting_returns_cancelled() {
        run_async_test(async {
            let pool = Arc::new(Pool::<String>::new(test_config(1)));
            let held = pool.acquire().await.expect("hold only slot");

            let wait_cx = crate::cx::for_testing();
            let task_cx = wait_cx.clone();
            let waiter_pool = pool.clone();
            let waiter = crate::runtime_async::task::spawn(async move {
                waiter_pool.acquire_with_cx(&task_cx).await
            });

            crate::runtime_async::sleep(Duration::from_millis(10)).await;
            wait_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel while waiting for permit"),
            );

            let err = waiter
                .await
                .expect("waiter task should join")
                .expect_err("cancelled wait should fail");
            assert_eq!(err, PoolError::Cancelled);

            let stats = pool.stats().await;
            assert_eq!(
                stats.total_timeouts, 0,
                "cancelled wait must not increment timeout"
            );
            assert_eq!(
                stats.total_acquired, 1,
                "cancelled waiter must not consume a permit"
            );
            assert_eq!(
                stats.active_count, 1,
                "held permit should remain the only active one"
            );

            drop(held);
        });
    }

    /// ft-xbnl0.2.3 Cx-first: pool acquire bound via timeout_with_cx
    /// surfaces Cancelled even if the caller's Cx is cancelled between
    /// the pre-flight `checkpoint_explicit_cx` and the timeout_with_cx
    /// await. This pins the behavior that `timeout_with_cx` returns
    /// Err on Cx cancellation AND the error surface disambiguates
    /// Cancelled from AcquireTimeout via `cx.is_cancel_requested()`.
    ///
    /// Previous code used `runtime_async::timeout` (ambient Cx) which
    /// meant a caller-provided Cx was NOT propagated to the timeout
    /// future — only the inner `acquire_owned_with_cx` saw it. The fix
    /// (commit ???) plumbs caller Cx through the timeout so the
    /// cancellation path is consistent.
    #[test]
    fn pool_acquire_with_cx_timeout_surface_bound_to_caller_cx() {
        run_async_test(async {
            let pool = Arc::new(Pool::<String>::new(test_config(1)));
            let _held = pool.acquire().await.expect("hold only slot");

            // Cancel the caller's Cx before the acquire enters the
            // timeout. The pre-flight checkpoint catches it, producing
            // Cancelled WITHOUT ever entering the timeout path. The
            // previous behavior (ambient timeout) also produced
            // Cancelled but via a different code path — this test
            // ensures we don't regress either.
            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel pre-acquire checkpoint"),
            );

            let err = pool
                .acquire_with_cx(&cx)
                .await
                .expect_err("pre-cancelled acquire must fail");
            assert_eq!(err, PoolError::Cancelled);

            let stats = pool.stats().await;
            assert_eq!(
                stats.total_timeouts, 0,
                "pre-cancelled acquire must NOT register a timeout"
            );
            assert_eq!(
                stats.total_acquired, 1,
                "only the held permit should be in total_acquired"
            );
        });
    }

    /// ft-xbnl0.2.3 Cx-first: put_with_cx / evict_idle_with_cx /
    /// stats_with_cx / clear_with_cx all round-trip a connection
    /// through the idle queue under an explicit caller `&Cx`. Pins
    /// no-regression on the Cx-first variants of the remaining four
    /// pool methods.
    #[test]
    fn pool_non_acquire_methods_with_cx_full_roundtrip() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(4));
            let cx = Cx::for_testing();

            // Acquire to get a permit.
            let result = pool
                .acquire_with_cx(&cx)
                .await
                .expect("acquire_with_cx should succeed");
            let (_, guard) = result.into_parts();

            // Return a connection via put_with_cx.
            pool.put_with_cx(&cx, "conn-a".to_string())
                .await
                .expect("put_with_cx should succeed");
            drop(guard);

            // stats_with_cx should reflect the put.
            let stats = pool
                .stats_with_cx(&cx)
                .await
                .expect("stats_with_cx should succeed");
            assert_eq!(stats.idle_count, 1, "put_with_cx should leave 1 idle");
            assert_eq!(stats.total_returned, 1);

            // evict_idle_with_cx with a fresh entry shouldn't evict (not
            // yet expired under the test config's idle_timeout).
            let evicted = pool
                .evict_idle_with_cx(&cx)
                .await
                .expect("evict_idle_with_cx should succeed");
            assert_eq!(evicted, 0, "fresh idle entry must not be evicted");

            // clear_with_cx drains the idle queue.
            pool.clear_with_cx(&cx)
                .await
                .expect("clear_with_cx should succeed");
            let stats_after_clear = pool
                .stats_with_cx(&cx)
                .await
                .expect("stats_with_cx after clear should succeed");
            assert_eq!(
                stats_after_clear.idle_count, 0,
                "clear_with_cx must drain all idle entries"
            );
            assert!(
                stats_after_clear.total_evicted >= 1,
                "clear_with_cx must increment the evicted counter"
            );
        });
    }

    #[test]
    fn pool_non_acquire_methods_with_cancelled_cx_return_typed_errors() {
        run_async_test(async {
            struct DropProbe(Arc<AtomicU64>);

            impl Drop for DropProbe {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }

            let pool: Pool<DropProbe> = Pool::new(test_config(4));
            let dropped = Arc::new(AtomicU64::new(0));
            let cx = Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel explicit pool maintenance"),
            );

            let put_error = pool
                .put_with_cx(&cx, DropProbe(Arc::clone(&dropped)))
                .await
                .expect_err("cancelled put_with_cx must return a typed error");
            assert_eq!(put_error, PoolError::Cancelled);
            assert_eq!(
                dropped.load(Ordering::Relaxed),
                1,
                "a connection that cannot be returned must be dropped exactly once"
            );

            let stats_error = pool
                .stats_with_cx(&cx)
                .await
                .expect_err("cancelled stats_with_cx must return a typed error");
            assert_eq!(stats_error, PoolError::Cancelled);

            let evict_error = pool
                .evict_idle_with_cx(&cx)
                .await
                .expect_err("cancelled evict_idle_with_cx must return a typed error");
            assert_eq!(evict_error, PoolError::Cancelled);

            let clear_error = pool
                .clear_with_cx(&cx)
                .await
                .expect_err("cancelled clear_with_cx must return a typed error");
            assert_eq!(clear_error, PoolError::Cancelled);

            let post_cancel_stats = pool.stats().await;
            assert_eq!(post_cancel_stats.idle_count, 0);
            assert_eq!(post_cancel_stats.total_returned, 0);
        });
    }

    #[test]
    fn pool_error_boxed_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(PoolError::AcquireTimeout);
        assert!(!err.to_string().is_empty());
        let err2: Box<dyn std::error::Error> = Box::new(PoolError::Closed);
        assert!(!err2.to_string().is_empty());
    }

    #[test]
    fn pool_stats_total_timeouts_increments() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(1));
            let _held = pool.acquire().await.unwrap();

            // This should timeout and increment timeout counter
            let _err = pool.acquire().await.unwrap_err();
            let stats = pool.stats().await;
            assert_eq!(stats.total_timeouts, 1);
        });
    }
}
