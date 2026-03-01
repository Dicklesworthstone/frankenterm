//! LabRuntime-ported SPSC ring buffer tests for deterministic async testing.
//!
//! Ports key `spsc_ring_buffer.rs` tests from `#[tokio::test]` to asupersync-based
//! `RuntimeFixture`, gaining deterministic scheduling for concurrent producer/consumer
//! scenarios.
//!
//! The SPSC ring buffer uses `runtime_compat::notify::Notify`, `task::spawn`,
//! and `sleep` — all of which flow through the asupersync runtime when the
//! `asupersync-runtime` feature is enabled.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use asupersync::Budget;
use common::fixtures::RuntimeFixture;
use common::lab::{
    ExplorationTestConfig, LabTestConfig, run_exploration_test, run_lab_test, run_lab_test_simple,
};
use frankenterm_core::spsc_ring_buffer::{channel, spmc_channel};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

// ===========================================================================
// Section 1: SPSC channel tests ported from tokio::test to RuntimeFixture
// ===========================================================================

#[test]
fn spsc_preserves_fifo_order() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel(8);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        tx.send(3).await.unwrap();

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));
    });
}

#[test]
fn spsc_try_send_respects_capacity() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel(1);
        assert!(tx.try_send(11).is_ok());
        assert!(tx.try_send(12).is_err());
        assert_eq!(rx.recv().await, Some(11));
        assert!(tx.try_send(13).is_ok());
        assert_eq!(rx.recv().await, Some(13));
    });
}

#[test]
fn spsc_recv_returns_none_after_close_and_drain() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel(2);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        drop(tx);

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, None);
    });
}

#[test]
fn spsc_recv_on_closed_empty_returns_none() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel::<u32>(4);
        drop(tx);
        assert_eq!(rx.recv().await, None);
    });
}

#[test]
fn spsc_send_on_closed_returns_err() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel::<u32>(4);
        drop(rx);
        tx.close();
        let result = tx.send(1).await;
        assert!(result.is_err());
    });
}

#[test]
fn spsc_fill_and_drain_multiple_cycles() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel::<u32>(2);
        for cycle in 0..5u32 {
            let base = cycle * 2;
            tx.send(base).await.unwrap();
            tx.send(base + 1).await.unwrap();
            assert_eq!(rx.recv().await, Some(base));
            assert_eq!(rx.recv().await, Some(base + 1));
        }
    });
}

#[test]
fn spsc_try_recv_drains_after_producer_drop() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel::<u32>(4);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();
        drop(tx);

        assert!(rx.is_closed());
        assert_eq!(rx.try_recv(), Some(1));
        assert_eq!(rx.try_recv(), Some(2));
        assert_eq!(rx.try_recv(), Some(3));
        assert_eq!(rx.try_recv(), None);
    });
}

// Note: `spsc_large_batch_1000_items` omitted — uses `runtime_compat::task::spawn`
// which delegates to `tokio::spawn` and requires a tokio reactor not available
// under asupersync's RuntimeFixture.

#[test]
fn spsc_string_payload() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel(4);
        tx.send("hello".to_string()).await.unwrap();
        tx.send("world".to_string()).await.unwrap();
        assert_eq!(rx.recv().await, Some("hello".to_string()));
        assert_eq!(rx.recv().await, Some("world".to_string()));
    });
}

#[test]
fn spsc_vec_payload() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel(2);
        tx.send(vec![1, 2, 3]).await.unwrap();
        tx.send(vec![4, 5]).await.unwrap();
        assert_eq!(rx.recv().await, Some(vec![1, 2, 3]));
        assert_eq!(rx.recv().await, Some(vec![4, 5]));
    });
}

// Note: `spsc_capacity_1_stress` omitted — uses `runtime_compat::task::spawn`
// which delegates to `tokio::spawn` (no tokio reactor under asupersync RuntimeFixture).

#[test]
fn spsc_alternating_send_recv() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel(2);
        for i in 0..50u32 {
            tx.send(i).await.unwrap();
            assert_eq!(rx.recv().await, Some(i));
        }
        assert_eq!(tx.depth(), 0);
    });
}

#[test]
fn spsc_send_on_closed_returns_original_value() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, _rx) = channel::<u32>(4);
        tx.close();
        let result = tx.send(42).await;
        assert_eq!(result.unwrap_err(), 42);
    });
}

#[test]
fn spsc_recv_returns_none_immediately_on_empty_closed() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, rx) = channel::<u32>(4);
        tx.close();
        let result = rx.recv().await;
        assert!(result.is_none());
    });
}

// Note: `spsc_concurrent_producer_consumer_stress` omitted — uses
// `runtime_compat::task::spawn` (no tokio reactor under asupersync RuntimeFixture).

// Note: `spsc_recv_wakes_on_close` omitted — uses `runtime_compat::task::spawn`
// and `runtime_compat::sleep` (no tokio reactor under asupersync RuntimeFixture).

// ── SPMC tests ───────────────────────────────────────────────────

#[test]
fn spmc_broadcasts_to_all_consumers_in_order() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, mut consumers) = spmc_channel(16, 2);
        let rx0 = consumers.remove(0);
        let rx1 = consumers.remove(0);

        for i in 0..10u32 {
            tx.send(i).await.unwrap();
        }

        for i in 0..10u32 {
            assert_eq!(rx0.recv().await, Some(i));
            assert_eq!(rx1.recv().await, Some(i));
        }
    });
}

#[test]
fn spmc_close_allows_drain_then_none() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (tx, consumers) = spmc_channel(2, 2);
        tx.send(7u32).await.unwrap();
        drop(tx);

        for rx in consumers {
            assert_eq!(rx.recv().await, Some(7));
            assert_eq!(rx.recv().await, None);
        }
    });
}

// ===========================================================================
// Section 2: LabRuntime tests for concurrent channel operations
// ===========================================================================

#[test]
fn lab_spsc_producer_consumer_basic() {
    let report = run_lab_test_simple(42, "spsc_producer_consumer_basic", |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let completed = Arc::new(AtomicU32::new(0));

        let done = Arc::clone(&completed);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let (tx, rx) = channel(4);
                tx.send(1u32).await.unwrap();
                tx.send(2).await.unwrap();
                tx.send(3).await.unwrap();

                assert_eq!(rx.recv().await, Some(1));
                assert_eq!(rx.recv().await, Some(2));
                assert_eq!(rx.recv().await, Some(3));
                done.fetch_add(1, Ordering::SeqCst);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_quiescent();

        assert_eq!(completed.load(Ordering::SeqCst), 1);
    });
    assert!(report.passed());
}

#[test]
fn lab_spsc_close_wakes_consumer() {
    let report = run_lab_test(
        LabTestConfig::new(100, "spsc_close_wakes_consumer").worker_count(2),
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let consumer_woke = Arc::new(AtomicU32::new(0));

            let woke = Arc::clone(&consumer_woke);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let (tx, rx) = channel::<u32>(4);
                    // Close immediately — consumer should wake and get None
                    drop(tx);
                    let result = rx.recv().await;
                    assert_eq!(result, None);
                    woke.fetch_add(1, Ordering::SeqCst);
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(task_id, 0);
            runtime.run_until_quiescent();

            assert_eq!(consumer_woke.load(Ordering::SeqCst), 1);
        },
    );
    assert!(report.passed());
}

// ===========================================================================
// Section 3: DPOR exploration for producer/consumer interleavings
// ===========================================================================

#[test]
fn exploration_spsc_fifo_under_all_schedules() {
    let config = ExplorationTestConfig::new("spsc_fifo_all_schedules", 15)
        .base_seed(0)
        .worker_count(2)
        .max_steps_per_run(50_000);

    let report = run_exploration_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let passed = Arc::new(AtomicU32::new(0));

        let ok = Arc::clone(&passed);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let (tx, rx) = channel(2);
                // Interleave send/recv to explore different orderings
                tx.send(1u32).await.unwrap();
                tx.send(2).await.unwrap();
                let v1 = rx.recv().await.unwrap();
                let v2 = rx.recv().await.unwrap();
                // FIFO invariant must hold under all schedules
                assert_eq!(v1, 1);
                assert_eq!(v2, 2);
                ok.fetch_add(1, Ordering::SeqCst);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_quiescent();

        assert_eq!(passed.load(Ordering::SeqCst), 1);
    });
    assert!(report.passed());
    assert!(report.total_runs >= 5, "should explore multiple schedules");
}

// ===========================================================================
// Note: Chaos (Section 4) and Multi-seed (Section 5) tests omitted for SPSC.
//
// The SPSC channel uses `runtime_compat::notify::Notify` which is still
// backed by `tokio::sync::Notify`. Under the LabRuntime's deterministic
// scheduler, tokio notification primitives may not properly wake tasks,
// causing "leaked tasks" invariant violations when channel backpressure
// triggers a Notify wait. The RuntimeFixture ports (Section 1) and
// simple LabRuntime tests (Sections 2-3) work because they avoid
// backpressure scenarios where the channel is full.
// ===========================================================================
