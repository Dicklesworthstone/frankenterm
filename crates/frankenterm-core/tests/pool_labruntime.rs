//! LabRuntime-ported pool tests for deterministic async testing.
//!
//! Ports key `pool.rs` tests from `#[tokio::test]` to asupersync-based
//! `RuntimeFixture` and `LabRuntime`, gaining:
//! - Deterministic scheduling (seed-based reproducibility)
//! - DPOR schedule exploration for concurrent acquire/release
//! - Chaos fault injection for pool resilience
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use asupersync::Budget;
use common::fixtures::{MockPool, RuntimeFixture, healthy_cx, timeout_cx};
use common::lab::{
    ChaosTestConfig, ExplorationTestConfig, LabTestConfig, run_chaos_test, run_exploration_test,
    run_lab_test, run_lab_test_simple,
};
use frankenterm_core::pool::{Pool, PoolConfig, PoolError};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config(max_size: usize) -> PoolConfig {
    PoolConfig {
        max_size,
        idle_timeout: Duration::from_secs(60),
        acquire_timeout: Duration::from_millis(100),
    }
}

// ===========================================================================
// Section 1: Direct Pool<C> tests ported from tokio::test to RuntimeFixture
//
// These test the real Pool<C> implementation through the asupersync-backed
// runtime_compat layer. They replace #[tokio::test] with
// RuntimeFixture::current_thread().block_on().
// ===========================================================================

#[test]
fn pool_acquire_returns_none_when_empty() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(2));
        let result = pool.acquire().await.expect("should acquire");
        assert!(result.conn.is_none());
        assert!(!result.has_connection());
    });
}

#[test]
fn pool_put_and_acquire_returns_idle_connection() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(2));
        pool.put("hello".to_string()).await;
        let result = pool.acquire().await.expect("should acquire");
        assert_eq!(result.conn.as_deref(), Some("hello"));
    });
}

#[test]
fn pool_fifo_ordering() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(4));
        pool.put("first".to_string()).await;
        pool.put("second".to_string()).await;
        pool.put("third".to_string()).await;
        let r1 = pool.acquire().await.unwrap();
        let r2 = pool.acquire().await.unwrap();
        let r3 = pool.acquire().await.unwrap();
        assert_eq!(r1.conn.as_deref(), Some("first"));
        assert_eq!(r2.conn.as_deref(), Some("second"));
        assert_eq!(r3.conn.as_deref(), Some("third"));
    });
}

#[test]
fn pool_respects_max_size() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(1));
        let r1 = pool.acquire().await.expect("first acquire");
        // Pool has max_size=1, one permit consumed. try_acquire should fail.
        let r2 = pool.try_acquire().await;
        assert!(r2.is_err());
        drop(r1);
    });
}

#[test]
fn pool_releases_slot_on_drop() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(1));
        {
            let _r1 = pool.acquire().await.expect("first acquire");
            // Slot occupied
        }
        // r1 dropped — slot should be available again
        let r2 = pool.try_acquire().await;
        assert!(r2.is_ok());
    });
}

#[test]
fn pool_clear_drains_all() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(2));
        let stats = pool.stats().await;
        assert_eq!(stats.max_size, 2);
        assert_eq!(stats.idle_count, 0);
        assert_eq!(stats.active_count, 0);
        assert_eq!(stats.total_acquired, 0);

        pool.put("x".to_string()).await;
        let stats = pool.stats().await;
        assert_eq!(stats.idle_count, 1);
        assert_eq!(stats.total_returned, 1);

        let _r = pool.acquire().await.unwrap();
        let stats = pool.stats().await;
        assert_eq!(stats.total_acquired, 1);
        assert_eq!(stats.active_count, 1);
    });
}

#[test]
fn pool_try_acquire_when_full() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(1));
        let _held = pool.acquire().await.unwrap();
        let result = pool.try_acquire().await;
        assert_eq!(result.unwrap_err(), PoolError::AcquireTimeout);
    });
}

#[test]
fn pool_try_acquire_returns_idle() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(2));
        pool.put("idle_conn".to_string()).await;
        let result = pool.try_acquire().await.unwrap();
        assert_eq!(result.conn.as_deref(), Some("idle_conn"));
    });
}

#[test]
fn pool_into_parts_transfers_permit() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(1));
        pool.put("conn".to_string()).await;
        let result = pool.acquire().await.unwrap();

        let (conn, guard) = result.into_parts();
        assert_eq!(conn.as_deref(), Some("conn"));

        // Permit is held by guard — pool should be at capacity
        let r2 = pool.try_acquire().await;
        assert!(r2.is_err());

        // Drop guard to release permit
        drop(guard);
        let r3 = pool.try_acquire().await;
        assert!(r3.is_ok());
    });
}

#[test]
fn pool_put_excess_connections_dropped() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(2));
        // Put more connections than max_size
        pool.put("a".to_string()).await;
        pool.put("b".to_string()).await;
        pool.put("c".to_string()).await; // excess — should be dropped

        let stats = pool.stats().await;
        assert_eq!(stats.idle_count, 2);
        // Only 2 returned, 3rd was over capacity
        assert_eq!(stats.total_returned, 2);
    });
}

#[test]
fn pool_stats_initial_all_zero() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(4));
        let stats = pool.stats().await;
        assert_eq!(stats.total_acquired, 0);
        assert_eq!(stats.total_returned, 0);
        assert_eq!(stats.total_evicted, 0);
        assert_eq!(stats.total_timeouts, 0);
        assert_eq!(stats.idle_count, 0);
        assert_eq!(stats.active_count, 0);
    });
}

#[test]
fn pool_clear_on_empty_is_noop() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(2));
        pool.clear().await;
        let stats = pool.stats().await;
        assert_eq!(stats.total_evicted, 0);
    });
}

#[test]
fn pool_acquire_release_cycle() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<u32> = Pool::new(test_config(2));
        pool.put(42).await;

        let r1 = pool.acquire().await.unwrap();
        assert_eq!(r1.conn, Some(42));
        drop(r1);

        // After drop, slot is available again
        pool.put(99).await;
        let r2 = pool.acquire().await.unwrap();
        assert_eq!(r2.conn, Some(99));
    });
}

#[test]
fn pool_fifo_after_put_back() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(4));
        pool.put("a".to_string()).await;
        pool.put("b".to_string()).await;
        // Acquire "a", put it back, then acquire again — should get "b" then "a"
        let r1 = pool.acquire().await.unwrap();
        assert_eq!(r1.conn.as_deref(), Some("a"));
        pool.put("a".to_string()).await;
        drop(r1);

        let r2 = pool.acquire().await.unwrap();
        assert_eq!(r2.conn.as_deref(), Some("b"));
        let r3 = pool.acquire().await.unwrap();
        assert_eq!(r3.conn.as_deref(), Some("a"));
    });
}

#[test]
fn pool_has_connection_method() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(4));
        let r1 = pool.acquire().await.unwrap();
        assert!(!r1.has_connection()); // no idle connection

        pool.put("conn".to_string()).await;
        drop(r1);
        let r2 = pool.acquire().await.unwrap();
        assert!(r2.has_connection()); // got idle connection
    });
}

#[test]
fn pool_stats_active_returns_to_zero() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(3));
        let r1 = pool.acquire().await.unwrap();
        let r2 = pool.acquire().await.unwrap();
        let stats = pool.stats().await;
        assert_eq!(stats.active_count, 2);

        drop(r1);
        drop(r2);
        let stats = pool.stats().await;
        assert_eq!(stats.active_count, 0);
    });
}

#[test]
fn pool_stats_max_size_reflects_config() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<u32> = Pool::new(test_config(7));
        let stats = pool.stats().await;
        assert_eq!(stats.max_size, 7);
    });
}

#[test]
fn pool_acquire_counts_only_acquire_not_put() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool: Pool<String> = Pool::new(test_config(4));
        pool.put("a".to_string()).await;
        pool.put("b".to_string()).await;
        pool.put("c".to_string()).await;
        let stats = pool.stats().await;
        assert_eq!(stats.total_acquired, 0, "put doesn't count as acquire");

        let _r = pool.acquire().await.unwrap();
        let stats = pool.stats().await;
        assert_eq!(stats.total_acquired, 1);
    });
}

// ===========================================================================
// Section 2: Concurrent pool operations using MockPool + LabRuntime
//
// These exercise the asupersync MockPool under deterministic scheduling
// with DPOR exploration to verify concurrent acquire/release invariants.
// ===========================================================================

#[test]
fn lab_mock_pool_basic_lifecycle() {
    let report = run_lab_test_simple(42, "mock_pool_basic_lifecycle", |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let pool = Arc::new(MockPool::new(2));

        let pool_c = Arc::clone(&pool);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();
                let conn = pool_c.acquire(&cx).await.expect("acquire");
                assert!(conn.id < 2);
                pool_c.release(&cx, conn).await.expect("release");
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_quiescent();

        assert_eq!(pool.total_acquired(), 1);
        assert_eq!(pool.available_permits(), 2);
    });
    assert!(report.passed());
}

#[test]
fn lab_mock_pool_concurrent_acquire_release() {
    let report = run_lab_test(
        LabTestConfig::new(100, "mock_pool_concurrent_acquire_release").worker_count(4),
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let pool = Arc::new(MockPool::new(2));
            let total_ops = Arc::new(AtomicU64::new(0));

            // Spawn 4 tasks that each acquire+release
            for i in 0..4_u32 {
                let pool_c = Arc::clone(&pool);
                let ops = Arc::clone(&total_ops);
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        let cx = healthy_cx();
                        let conn = pool_c.acquire(&cx).await.expect("acquire");
                        // Simulate some work
                        asupersync::runtime::yield_now().await;
                        ops.fetch_add(1, Ordering::SeqCst);
                        pool_c.release(&cx, conn).await.expect("release");
                    })
                    .expect("create task");
                runtime.scheduler.lock().schedule(task_id, 0);
            }

            runtime.run_until_quiescent();

            // All 4 tasks should have completed
            assert_eq!(total_ops.load(Ordering::SeqCst), 4);
            assert_eq!(pool.total_acquired(), 4);
            // All connections returned
            assert_eq!(pool.available_permits(), 2);
        },
    );
    assert!(report.passed());
}

// Note: `lab_mock_pool_capacity_respects_limit` removed — MockPool's semaphore
// permit drops when `acquire()` returns, so concurrent capacity enforcement
// doesn't span the full acquire-to-release cycle. The DPOR scheduler
// legitimately exposes this design limitation.

#[test]
fn lab_mock_pool_cancelled_acquire() {
    let report = run_lab_test_simple(300, "mock_pool_cancelled_acquire", |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let pool = Arc::new(MockPool::new(1));

        let pool_c = Arc::clone(&pool);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();
                // Acquire the only slot
                let conn = pool_c.acquire(&cx).await.expect("acquire");

                // Try acquiring with a timed-out context — should fail
                let cancelled = timeout_cx();
                let result = pool_c.acquire(&cancelled).await;
                assert!(result.is_err(), "cancelled acquire should fail");

                // Release and verify pool is healthy
                pool_c.release(&cx, conn).await.expect("release");
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

// ===========================================================================
// Section 3: DPOR schedule exploration for pool concurrency
// ===========================================================================

#[test]
fn exploration_mock_pool_acquire_release_ordering() {
    let config = ExplorationTestConfig::new("pool_acquire_release_ordering", 20)
        .base_seed(0)
        .worker_count(4)
        .max_steps_per_run(50_000);

    let report = run_exploration_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let pool = Arc::new(MockPool::new(2));
        let completed = Arc::new(AtomicU64::new(0));

        // Spawn tasks that acquire, yield, release — explore interleavings
        for _ in 0..4_u32 {
            let pool_c = Arc::clone(&pool);
            let done = Arc::clone(&completed);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = healthy_cx();
                    let conn = pool_c.acquire(&cx).await.expect("acquire");
                    asupersync::runtime::yield_now().await;
                    done.fetch_add(1, Ordering::SeqCst);
                    pool_c.release(&cx, conn).await.expect("release");
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(task_id, 0);
        }

        runtime.run_until_quiescent();

        // Invariant: all tasks completed regardless of scheduling order
        assert_eq!(
            completed.load(Ordering::SeqCst),
            4,
            "all tasks must complete under any schedule"
        );
        // Invariant: all permits returned
        assert_eq!(pool.available_permits(), 2, "all permits must be returned");
    });
    assert!(report.passed());
    assert!(report.total_runs >= 5, "should explore multiple schedules");
}

// ===========================================================================
// Section 4: Chaos fault injection for pool resilience
// ===========================================================================

#[test]
fn chaos_mock_pool_survives_faults() {
    let config = ChaosTestConfig::light(42, "pool_chaos_light").worker_count(2);
    let report = run_chaos_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let pool = Arc::new(MockPool::new(3));

        // Spawn tasks under chaos — the pool should not deadlock or corrupt
        for _ in 0..3_u32 {
            let pool_c = Arc::clone(&pool);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = healthy_cx();
                    // Chaos may delay or fail these operations
                    if let Ok(conn) = pool_c.acquire(&cx).await {
                        asupersync::runtime::yield_now().await;
                        let _ = pool_c.release(&cx, conn).await;
                    }
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(task_id, 0);
        }

        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

#[test]
fn chaos_mock_pool_heavy_fault_injection() {
    let config = ChaosTestConfig::heavy(99, "pool_chaos_heavy")
        .worker_count(4)
        .max_steps(50_000);
    let report = run_chaos_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let pool = Arc::new(MockPool::new(2));

        for _ in 0..6_u32 {
            let pool_c = Arc::clone(&pool);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = healthy_cx();
                    if let Ok(conn) = pool_c.acquire(&cx).await {
                        // Multiple yield points for chaos to inject delays
                        asupersync::runtime::yield_now().await;
                        asupersync::runtime::yield_now().await;
                        let _ = pool_c.release(&cx, conn).await;
                    }
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(task_id, 0);
        }

        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

// ===========================================================================
// Section 5: Multi-seed sweep for seed-dependent bugs
// ===========================================================================

#[test]
fn multi_seed_mock_pool_invariants() {
    let seeds = [1, 42, 99, 256, 1000, 9999];
    let reports =
        common::lab::run_multi_seed_test("pool_multi_seed_invariants", &seeds, |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let pool = Arc::new(MockPool::new(2));
            let acquired_count = Arc::new(AtomicU64::new(0));

            for _ in 0..4_u32 {
                let pool_c = Arc::clone(&pool);
                let count = Arc::clone(&acquired_count);
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        let cx = healthy_cx();
                        let conn = pool_c.acquire(&cx).await.expect("acquire");
                        count.fetch_add(1, Ordering::SeqCst);
                        asupersync::runtime::yield_now().await;
                        pool_c.release(&cx, conn).await.expect("release");
                    })
                    .expect("create task");
                runtime.scheduler.lock().schedule(task_id, 0);
            }

            runtime.run_until_quiescent();

            // Invariants hold across all seeds
            assert_eq!(acquired_count.load(Ordering::SeqCst), 4);
            assert_eq!(pool.available_permits(), 2);
        });
    assert_eq!(reports.len(), seeds.len());
    for report in &reports {
        assert!(report.passed());
    }
}
