//! Property-based tests for pool module.
//!
//! Verifies the connection pool invariants:
//! - PoolConfig serde roundtrip preserves all fields
//! - PoolStats serde roundtrip preserves all fields
//! - PoolError equality and Display
//! - idle_count never exceeds max_size
//! - FIFO ordering preserved for idle connections
//! - Stats counters consistency (acquired counts, returned counts)
//! - clear() drains all idle connections
//! - Excess put() beyond max_size drops connections
//! - try_acquire on empty pool with no slots returns error

use proptest::prelude::*;
use std::time::Duration;

use frankenterm_core::pool::{Pool, PoolConfig, PoolError, PoolStats};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_pool_config() -> impl Strategy<Value = PoolConfig> {
    (
        1usize..=20,           // max_size
        1u64..=600,            // idle_timeout_secs
        1u64..=30,             // acquire_timeout_secs
    )
        .prop_map(|(max_size, idle_secs, acq_secs)| PoolConfig {
            max_size,
            idle_timeout: Duration::from_secs(idle_secs),
            acquire_timeout: Duration::from_secs(acq_secs),
        })
}

fn arb_pool_stats() -> impl Strategy<Value = PoolStats> {
    (
        1usize..=100,          // max_size
        0usize..=100,          // idle_count
        0usize..=100,          // active_count
        0u64..=10_000,         // total_acquired
        0u64..=10_000,         // total_returned
        0u64..=10_000,         // total_evicted
        0u64..=10_000,         // total_timeouts
    )
        .prop_map(
            |(max_size, idle_count, active_count, total_acquired, total_returned, total_evicted, total_timeouts)| {
                PoolStats {
                    max_size,
                    idle_count,
                    active_count,
                    total_acquired,
                    total_returned,
                    total_evicted,
                    total_timeouts,
                }
            },
        )
}

fn arb_pool_error() -> impl Strategy<Value = PoolError> {
    prop_oneof![
        Just(PoolError::AcquireTimeout),
        Just(PoolError::Closed),
    ]
}

// ────────────────────────────────────────────────────────────────────
// PoolConfig: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// PoolConfig JSON roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(c in arb_pool_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: PoolConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_size, c.max_size);
        prop_assert_eq!(back.idle_timeout, c.idle_timeout);
        prop_assert_eq!(back.acquire_timeout, c.acquire_timeout);
    }

    /// PoolConfig fields are valid.
    #[test]
    fn prop_config_fields_valid(c in arb_pool_config()) {
        prop_assert!(c.max_size >= 1);
        prop_assert!(c.idle_timeout.as_millis() > 0);
        prop_assert!(c.acquire_timeout.as_millis() > 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// PoolStats: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// PoolStats JSON roundtrip preserves all fields.
    #[test]
    fn prop_stats_serde_roundtrip(s in arb_pool_stats()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: PoolStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_size, s.max_size);
        prop_assert_eq!(back.idle_count, s.idle_count);
        prop_assert_eq!(back.active_count, s.active_count);
        prop_assert_eq!(back.total_acquired, s.total_acquired);
        prop_assert_eq!(back.total_returned, s.total_returned);
        prop_assert_eq!(back.total_evicted, s.total_evicted);
        prop_assert_eq!(back.total_timeouts, s.total_timeouts);
    }
}

// ────────────────────────────────────────────────────────────────────
// PoolError: equality and Display
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// PoolError is self-equal.
    #[test]
    fn prop_error_self_equal(e in arb_pool_error()) {
        prop_assert_eq!(e.clone(), e);
    }

    /// PoolError Display is non-empty.
    #[test]
    fn prop_error_display_nonempty(e in arb_pool_error()) {
        let s = e.to_string();
        prop_assert!(!s.is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// Pool: idle_count bounded by max_size
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// After putting N connections, idle_count <= max_size.
    #[test]
    fn prop_idle_bounded_by_max_size(
        max_size in 1usize..=10,
        n_puts in 1usize..=20,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_secs(1),
            };
            let pool: Pool<u32> = Pool::new(config);
            for i in 0..n_puts {
                pool.put(i as u32).await;
            }
            let stats = pool.stats().await;
            prop_assert!(
                stats.idle_count <= max_size,
                "idle {} > max_size {}", stats.idle_count, max_size
            );
            Ok(())
        })?;
    }

    /// Excess puts beyond max_size are silently dropped.
    #[test]
    fn prop_excess_puts_dropped(
        max_size in 1usize..=5,
        excess in 1usize..=10,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_secs(1),
            };
            let pool: Pool<u32> = Pool::new(config);
            for i in 0..(max_size + excess) {
                pool.put(i as u32).await;
            }
            let stats = pool.stats().await;
            // total_returned counts only successful puts
            prop_assert_eq!(stats.total_returned, max_size as u64);
            prop_assert_eq!(stats.idle_count, max_size);
            Ok(())
        })?;
    }
}

// ────────────────────────────────────────────────────────────────────
// Pool: FIFO ordering
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Connections are returned in FIFO order.
    #[test]
    fn prop_fifo_ordering(
        max_size in 2usize..=10,
        n_items in 2usize..=10,
    ) {
        let n = n_items.min(max_size);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_secs(1),
            };
            let pool: Pool<u32> = Pool::new(config);

            // Put items 0..n
            for i in 0..n as u32 {
                pool.put(i).await;
            }

            // Acquire them back — should be FIFO
            for expected in 0..n as u32 {
                let result = pool.acquire().await.unwrap();
                if let Some(got) = result.conn {
                    prop_assert_eq!(got, expected, "FIFO violation");
                }
                // Drop result to release permit
            }
            Ok(())
        })?;
    }
}

// ────────────────────────────────────────────────────────────────────
// Pool: clear drains all
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// clear() empties the idle queue.
    #[test]
    fn prop_clear_empties(
        max_size in 1usize..=10,
        n_puts in 1usize..=10,
    ) {
        let n = n_puts.min(max_size);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_secs(1),
            };
            let pool: Pool<u32> = Pool::new(config);
            for i in 0..n {
                pool.put(i as u32).await;
            }

            pool.clear().await;

            let stats = pool.stats().await;
            prop_assert_eq!(stats.idle_count, 0);
            prop_assert_eq!(stats.total_evicted, n as u64);
            Ok(())
        })?;
    }
}

// ────────────────────────────────────────────────────────────────────
// Pool: stats consistency after acquire/put cycles
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// After N puts and M acquires, stats reflect operations accurately.
    /// Note: try_acquire can succeed even without idle connections (conn=None,
    /// permit still held), so acquired_count can exceed the number of puts.
    #[test]
    fn prop_stats_after_put_acquire(
        max_size in 2usize..=8,
        n_puts in 1usize..=8,
        n_acquires in 1usize..=8,
    ) {
        let effective_puts = n_puts.min(max_size);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_millis(50),
            };
            let pool: Pool<u32> = Pool::new(config);

            // Put connections
            for i in 0..n_puts {
                pool.put(i as u32).await;
            }

            // Acquire connections (up to what's available)
            let mut held = Vec::new();
            for _ in 0..n_acquires {
                match pool.try_acquire().await {
                    Ok(result) => held.push(result),
                    Err(_) => break,
                }
            }
            let acquired_count = held.len();

            let stats = pool.stats().await;
            prop_assert_eq!(
                stats.total_acquired, acquired_count as u64,
                "acquired mismatch"
            );
            prop_assert_eq!(
                stats.total_returned, effective_puts as u64,
                "returned mismatch"
            );
            // Idle should be max(0, effective_puts - acquired_count)
            // since try_acquire can take permits without idle connections
            let expected_idle = effective_puts.saturating_sub(acquired_count);
            prop_assert_eq!(
                stats.idle_count, expected_idle,
                "idle mismatch"
            );
            // acquired_count <= max_size (bounded by semaphore permits)
            prop_assert!(
                acquired_count <= max_size,
                "acquired {} > max_size {}", acquired_count, max_size
            );

            // Drop held results to release permits
            drop(held);

            Ok(())
        })?;
    }

    /// After acquiring and dropping all, active_count returns to 0.
    #[test]
    fn prop_acquire_drop_restores_slots(
        max_size in 1usize..=5,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_millis(50),
            };
            let pool: Pool<u32> = Pool::new(config);

            // Acquire all slots
            let mut held = Vec::new();
            for _ in 0..max_size {
                held.push(pool.acquire().await.unwrap());
            }

            let stats = pool.stats().await;
            prop_assert_eq!(stats.active_count, max_size);

            // Drop all
            drop(held);

            let stats = pool.stats().await;
            prop_assert_eq!(stats.active_count, 0);

            Ok(())
        })?;
    }
}

// ────────────────────────────────────────────────────────────────────
// Pool: try_acquire on full pool
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// try_acquire fails when all slots are held.
    #[test]
    fn prop_try_acquire_full_pool_fails(
        max_size in 1usize..=5,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_millis(50),
            };
            let pool: Pool<u32> = Pool::new(config);

            // Acquire all slots
            let mut held = Vec::new();
            for _ in 0..max_size {
                held.push(pool.acquire().await.unwrap());
            }

            // Next try_acquire should fail
            let err = pool.try_acquire().await.unwrap_err();
            prop_assert_eq!(err, PoolError::AcquireTimeout);

            drop(held);
            Ok(())
        })?;
    }
}

// ────────────────────────────────────────────────────────────────────
// Pool: into_parts transfers permit
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// into_parts separates connection and permit; dropping guard releases slot.
    #[test]
    fn prop_into_parts_lifecycle(
        max_size in 1usize..=5,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_millis(50),
            };
            let pool: Pool<u32> = Pool::new(config);
            pool.put(42u32).await;

            let result = pool.acquire().await.unwrap();
            let (conn, guard) = result.into_parts();

            // Connection should be available
            prop_assert_eq!(conn, Some(42));

            // Slot still held
            let stats = pool.stats().await;
            prop_assert_eq!(stats.active_count, 1);

            // Release guard
            drop(guard);
            let stats = pool.stats().await;
            prop_assert_eq!(stats.active_count, 0);

            Ok(())
        })?;
    }
}

// ────────────────────────────────────────────────────────────────────
// Pool: empty pool acquire returns None conn
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Acquiring from empty pool (no idle connections) gives conn=None.
    #[test]
    fn prop_empty_pool_acquire_none_conn(
        max_size in 1usize..=10,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_millis(50),
            };
            let pool: Pool<u32> = Pool::new(config);

            let result = pool.acquire().await.unwrap();
            prop_assert!(result.conn.is_none(), "Empty pool should give None conn");
            prop_assert!(!result.has_connection());

            Ok(())
        })?;
    }
}

// ────────────────────────────────────────────────────────────────────
// Pool: acquire after put returns the connection
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Put then acquire returns the same connection.
    #[test]
    fn prop_put_then_acquire_returns_conn(
        max_size in 1usize..=10,
        value in any::<u32>(),
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(300),
                acquire_timeout: Duration::from_millis(50),
            };
            let pool: Pool<u32> = Pool::new(config);
            pool.put(value).await;

            let result = pool.acquire().await.unwrap();
            prop_assert_eq!(result.conn, Some(value));

            Ok(())
        })?;
    }
}
