//! Connection pool for WezTerm mux connections.
//!
//! Reduces overhead by reusing persistent connections to the WezTerm mux
//! server (vendored mode) or limiting concurrent CLI process spawns.
//!
//! # Design
//!
//! The pool manages a fixed set of connection slots. Each slot holds either
//! an idle connection or is empty (available for a new connection). Callers
//! acquire a `PoolGuard` which provides access to a connection and
//! automatically returns it to the pool on drop.
//!
//! For CLI mode, pooling acts as a concurrency limiter — the underlying
//! `WeztermClient` is stateless but spawning too many processes at once
//! causes resource contention.
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(feature = "asupersync-runtime")]
use crate::cx::{self, Cx};
use crate::runtime_compat::{Mutex, Semaphore, TryAcquireError};
use serde::{Deserialize, Serialize};

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
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AcquireTimeout => write!(f, "connection pool acquire timeout"),
            Self::Closed => write!(f, "connection pool is closed"),
        }
    }
}

impl std::error::Error for PoolError {}

/// A generic async connection pool.
///
/// `C` is the connection type (e.g., a WezTerm mux client handle).
/// Connections are created externally and added via [`Pool::put`]; the pool
/// itself does not create connections — it manages their lifecycle.
pub struct Pool<C> {
    config: PoolConfig,
    idle: Arc<Mutex<VecDeque<PooledEntry<C>>>>,
    semaphore: Arc<Semaphore>,
    stats_acquired: AtomicU64,
    stats_returned: AtomicU64,
    stats_evicted: AtomicU64,
    stats_timeouts: AtomicU64,
}

impl<C: Send + 'static> Pool<C> {
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
        }
    }

    /// Try to acquire a connection from the pool without waiting.
    ///
    /// Returns `Ok(result)` with an optional idle connection if a slot is
    /// available, or `Err` if no slots are free. If `result.conn` is `None`,
    /// the caller should create a new connection.
    pub async fn try_acquire(&self) -> Result<PoolAcquireResult<C>, PoolError> {
        #[cfg(feature = "asupersync-runtime")]
        {
            let cx = cx::for_request();
            return self.try_acquire_with_cx(&cx).await;
        }
        #[cfg(not(feature = "asupersync-runtime"))]
        {
            self.try_acquire_inner().await
        }
    }

    /// Try to acquire a connection using an explicit capability context.
    ///
    /// This is the migration-safe entry point for call paths that already
    /// thread `Cx` explicitly.
    #[cfg(feature = "asupersync-runtime")]
    pub async fn try_acquire_with_cx(&self, cx: &Cx) -> Result<PoolAcquireResult<C>, PoolError> {
        cx::with_cx(cx, |_| ());
        self.try_acquire_inner().await
    }

    /// Inner implementation shared by both cx and non-cx paths.
    async fn try_acquire_inner(&self) -> Result<PoolAcquireResult<C>, PoolError> {
        match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => {
                let conn = {
                    let mut idle = self.idle.lock().await;
                    self.evict_expired(&mut idle);
                    idle.pop_front().map(|e| e.conn)
                };
                self.stats_acquired.fetch_add(1, Ordering::Relaxed);
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
        #[cfg(feature = "asupersync-runtime")]
        {
            let cx = cx::for_request();
            return self.acquire_with_cx(&cx).await;
        }
        #[cfg(not(feature = "asupersync-runtime"))]
        {
            self.acquire_inner().await
        }
    }

    /// Acquire a connection using an explicit capability context.
    ///
    /// This preserves existing timeout behavior while allowing upstream
    /// call graphs to carry `Cx` explicitly.
    #[cfg(feature = "asupersync-runtime")]
    pub async fn acquire_with_cx(&self, cx: &Cx) -> Result<PoolAcquireResult<C>, PoolError> {
        let acquire_result = cx::with_cx_async(cx, |_| async {
            crate::runtime_compat::timeout(
                self.config.acquire_timeout,
                self.semaphore.clone().acquire_owned(),
            )
            .await
        })
        .await;

        let permit = match acquire_result {
            Ok(Ok(permit)) => permit,
            Ok(Err(_closed)) => return Err(PoolError::Closed),
            Err(_timeout_err) => {
                self.stats_timeouts.fetch_add(1, Ordering::Relaxed);
                return Err(PoolError::AcquireTimeout);
            }
        };

        let conn = {
            let mut idle = self.idle.lock().await;
            self.evict_expired(&mut idle);
            idle.pop_front().map(|e| e.conn)
        };
        self.stats_acquired.fetch_add(1, Ordering::Relaxed);
        Ok(PoolAcquireResult {
            conn,
            permit: Some(permit),
        })
    }

    /// Inner implementation for acquire without cx.
    #[cfg(not(feature = "asupersync-runtime"))]
    async fn acquire_inner(&self) -> Result<PoolAcquireResult<C>, PoolError> {
        let acquire_result = crate::runtime_compat::timeout(
            self.config.acquire_timeout,
            self.semaphore.clone().acquire_owned(),
        )
        .await;

        let permit = match acquire_result {
            Ok(Ok(permit)) => permit,
            Ok(Err(_closed)) => return Err(PoolError::Closed),
            Err(_timeout_err) => {
                self.stats_timeouts.fetch_add(1, Ordering::Relaxed);
                return Err(PoolError::AcquireTimeout);
            }
        };

        let conn = {
            let mut idle = self.idle.lock().await;
            self.evict_expired(&mut idle);
            idle.pop_front().map(|e| e.conn)
        };
        self.stats_acquired.fetch_add(1, Ordering::Relaxed);
        Ok(PoolAcquireResult {
            conn,
            permit: Some(permit),
        })
    }

    /// Return a connection to the pool for reuse.
    ///
    /// If the pool's idle queue is already at capacity, the connection is
    /// dropped instead.
    pub async fn put(&self, conn: C) {
        let mut idle = self.idle.lock().await;
        self.evict_expired(&mut idle);
        if idle.len() < self.config.max_size {
            idle.push_back(PooledEntry {
                conn,
                returned_at: Instant::now(),
            });
            self.stats_returned.fetch_add(1, Ordering::Relaxed);
        }
        // If queue is at max_size, connection is dropped (not returned).
    }

    /// Evict idle connections that have exceeded the idle timeout.
    pub async fn evict_idle(&self) -> usize {
        let mut idle = self.idle.lock().await;
        self.evict_expired(&mut idle)
    }

    /// Get current pool statistics.
    pub async fn stats(&self) -> PoolStats {
        let idle_count = self.idle.lock().await.len();
        let acquired = self.stats_acquired.load(Ordering::Relaxed);
        let returned = self.stats_returned.load(Ordering::Relaxed);
        PoolStats {
            max_size: self.config.max_size,
            idle_count,
            active_count: self.config.max_size - self.semaphore.available_permits(),
            total_acquired: acquired,
            total_returned: returned,
            total_evicted: self.stats_evicted.load(Ordering::Relaxed),
            total_timeouts: self.stats_timeouts.load(Ordering::Relaxed),
        }
    }

    /// Drain all idle connections from the pool.
    pub async fn clear(&self) {
        let mut idle = self.idle.lock().await;
        let count = idle.len() as u64;
        idle.clear();
        self.stats_evicted.fetch_add(count, Ordering::Relaxed);
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
            self.stats_evicted
                .fetch_add(evicted as u64, Ordering::Relaxed);
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
    permit: Option<crate::runtime_compat::OwnedSemaphorePermit>,
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
    _permit: crate::runtime_compat::OwnedSemaphorePermit,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
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
            crate::runtime_compat::clear_runtime_handle();
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

    #[test]
    fn pool_acquire_returns_none_when_empty() {
        run_async_test(async {
            let pool: Pool<String> = Pool::new(test_config(2));
            let result = pool.acquire().await.expect("should acquire");
            assert!(result.conn.is_none());
            assert!(!result.has_connection());
        });
    }

    #[cfg(feature = "asupersync-runtime")]
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
            crate::runtime_compat::sleep(Duration::from_millis(20)).await;

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

    #[cfg(feature = "asupersync-runtime")]
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

            let h1 = crate::runtime_compat::task::spawn(async move {
                let _r = pool2.acquire().await.expect("acquire 1");
                crate::runtime_compat::sleep(Duration::from_millis(50)).await;
            });

            let h2 = crate::runtime_compat::task::spawn(async move {
                let _r = pool3.acquire().await.expect("acquire 2");
                crate::runtime_compat::sleep(Duration::from_millis(50)).await;
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

            crate::runtime_compat::sleep(Duration::from_millis(20)).await;
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
            crate::runtime_compat::sleep(Duration::from_millis(300)).await;
            pool.put("new".to_string()).await;
            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 2);
            // old age: ~700ms (> 500ms, stale), new age: ~400ms (< 500ms, fresh)
            crate::runtime_compat::sleep(Duration::from_millis(400)).await;

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
                handles.push(crate::runtime_compat::task::spawn(async move {
                    let r = p.acquire().await.expect("acquire");
                    crate::runtime_compat::sleep(Duration::from_millis(10)).await;
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
            crate::runtime_compat::sleep(Duration::from_millis(5)).await;

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
