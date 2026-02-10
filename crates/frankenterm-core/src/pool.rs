//! Connection pool for WezTerm mux connections.
//!
//! Reduces overhead by reusing persistent connections to the WezTerm mux
//! server (vendored mode) or limiting concurrent CLI process spawns.
//!
//! # Design
//!
//! The pool manages a fixed set of connection slots. Each slot holds either
//! an idle connection or is empty (available for a new connection). Callers
//! acquire a [`PoolGuard`] which provides access to a connection and
//! automatically returns it to the pool on drop.
//!
//! For CLI mode, pooling acts as a concurrency limiter — the underlying
//! `WeztermClient` is stateless but spawning too many processes at once
//! causes resource contention.
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore, TryAcquireError};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        let permit = match tokio::time::timeout(
            self.config.acquire_timeout,
            self.semaphore.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_closed)) => return Err(PoolError::Closed),
            Err(_elapsed) => {
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
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
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
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(max_size: usize) -> PoolConfig {
        PoolConfig {
            max_size,
            idle_timeout: Duration::from_secs(60),
            acquire_timeout: Duration::from_millis(100),
        }
    }

    #[tokio::test]
    async fn pool_acquire_returns_none_when_empty() {
        let pool: Pool<String> = Pool::new(test_config(2));
        let result = pool.acquire().await.expect("should acquire");
        assert!(result.conn.is_none());
        assert!(!result.has_connection());
    }

    #[tokio::test]
    async fn pool_put_and_acquire_returns_idle_connection() {
        let pool: Pool<String> = Pool::new(test_config(2));
        pool.put("conn-1".to_string()).await;
        // Release the implicit semaphore hold — put doesn't hold a permit
        let result = pool.acquire().await.expect("should acquire");
        assert_eq!(result.conn.as_deref(), Some("conn-1"));
    }

    #[tokio::test]
    async fn pool_fifo_ordering() {
        let pool: Pool<String> = Pool::new(test_config(4));
        pool.put("first".to_string()).await;
        pool.put("second".to_string()).await;

        let r1 = pool.acquire().await.expect("acquire 1");
        assert_eq!(r1.conn.as_deref(), Some("first"));
        let r2 = pool.acquire().await.expect("acquire 2");
        assert_eq!(r2.conn.as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn pool_respects_max_size() {
        let pool: Pool<String> = Pool::new(test_config(1));

        // Acquire the only slot
        let _held = pool.acquire().await.expect("acquire 1");

        // Second acquire should timeout
        let err = pool.acquire().await.expect_err("should timeout");
        assert_eq!(err, PoolError::AcquireTimeout);
    }

    #[tokio::test]
    async fn pool_releases_slot_on_drop() {
        let pool: Pool<String> = Pool::new(test_config(1));

        {
            let _held = pool.acquire().await.expect("acquire 1");
            // _held dropped here
        }

        // Should succeed now
        let result = pool.acquire().await.expect("acquire after drop");
        assert!(result.conn.is_none());
    }

    #[tokio::test]
    async fn pool_idle_timeout_eviction() {
        let config = PoolConfig {
            max_size: 2,
            idle_timeout: Duration::from_millis(10),
            acquire_timeout: Duration::from_millis(100),
        };
        let pool: Pool<String> = Pool::new(config);
        pool.put("stale".to_string()).await;

        // Wait for it to expire
        tokio::time::sleep(Duration::from_millis(20)).await;

        let result = pool.acquire().await.expect("acquire");
        assert!(
            result.conn.is_none(),
            "stale connection should have been evicted"
        );
    }

    #[tokio::test]
    async fn pool_clear_drains_all() {
        let pool: Pool<String> = Pool::new(test_config(4));
        pool.put("a".to_string()).await;
        pool.put("b".to_string()).await;
        pool.put("c".to_string()).await;

        pool.clear().await;

        let stats = pool.stats().await;
        assert_eq!(stats.idle_count, 0);
        assert_eq!(stats.total_evicted, 3);
    }

    #[tokio::test]
    async fn pool_stats_are_accurate() {
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
    }

    #[tokio::test]
    async fn pool_try_acquire_when_full() {
        let pool: Pool<String> = Pool::new(test_config(1));
        let _held = pool.acquire().await.expect("acquire");

        let err = pool.try_acquire().await.expect_err("should fail");
        assert_eq!(err, PoolError::AcquireTimeout);
    }

    #[tokio::test]
    async fn pool_try_acquire_returns_idle() {
        let pool: Pool<String> = Pool::new(test_config(2));
        pool.put("idle-conn".to_string()).await;

        let result = pool.try_acquire().await.expect("should succeed");
        assert_eq!(result.conn.as_deref(), Some("idle-conn"));
    }

    #[tokio::test]
    async fn pool_concurrent_acquire_respects_limit() {
        let pool = Arc::new(Pool::<u64>::new(test_config(2)));
        let pool2 = pool.clone();
        let pool3 = pool.clone();

        let h1 = tokio::spawn(async move {
            let _r = pool2.acquire().await.expect("acquire 1");
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let h2 = tokio::spawn(async move {
            let _r = pool3.acquire().await.expect("acquire 2");
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        // Both should succeed with pool size 2
        h1.await.expect("h1");
        h2.await.expect("h2");
    }

    #[tokio::test]
    async fn pool_evict_idle_returns_count() {
        let config = PoolConfig {
            max_size: 4,
            idle_timeout: Duration::from_millis(10),
            acquire_timeout: Duration::from_millis(100),
        };
        let pool: Pool<String> = Pool::new(config);
        pool.put("a".to_string()).await;
        pool.put("b".to_string()).await;

        tokio::time::sleep(Duration::from_millis(20)).await;
        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, 2);
    }

    #[test]
    fn pool_config_default() {
        let config = PoolConfig::default();
        assert_eq!(config.max_size, 4);
        assert_eq!(config.idle_timeout, Duration::from_secs(300));
        assert_eq!(config.acquire_timeout, Duration::from_secs(5));
    }

    #[test]
    fn pool_config_serde_roundtrip() {
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
    fn pool_stats_serde_roundtrip() {
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

    #[tokio::test]
    async fn pool_into_parts_transfers_permit() {
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
    }

    #[tokio::test]
    async fn pool_put_excess_connections_dropped() {
        let pool: Pool<String> = Pool::new(test_config(2));
        pool.put("a".to_string()).await;
        pool.put("b".to_string()).await;
        pool.put("c".to_string()).await; // Exceeds max_size, should be dropped

        let stats = pool.stats().await;
        assert_eq!(stats.idle_count, 2);
        assert_eq!(stats.total_returned, 2);
    }
}
