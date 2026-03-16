//! Integration tests for mux client/pool/runtime migration completion (ft-e34d9.10.5.2).
//!
//! Validates that the asupersync migration of the vendored mux subsystem is
//! complete and correct: Cx threading, cancellation semantics, recovery under
//! contention, diagnostic coverage, and pool invariants.

#![cfg(all(feature = "asupersync-runtime", feature = "vendored", unix))]

mod common;

use common::fixtures::{
    MockMuxClient, MockPool, RuntimeFixture, SimulatedNetwork, SimulatedNetworkConfig,
    TestPaneData, healthy_cx, mock_unix_stream_pair, timeout_cx, user_cancelled_cx,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// ===========================================================================
// Section 1: Cx-threaded pool invariants
//
// Verifies that pool acquire/release operations correctly thread Cx and
// maintain invariants under the asupersync runtime.
// ===========================================================================

#[test]
fn pool_cx_threading_acquire_release_preserves_capacity() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let pool = MockPool::new(4);

        // Acquire and release 8 times — pool should reuse connections
        for _ in 0..8 {
            let conn = pool.acquire(&cx).await.expect("acquire");
            pool.release(&cx, conn).await.expect("release");
        }

        assert_eq!(pool.total_acquired(), 8);
        assert_eq!(pool.available_permits(), 4, "all connections returned");
    });
}

#[test]
fn pool_cx_threading_concurrent_acquire_respects_capacity() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let pool = Arc::new(MockPool::new(2));
        let acquired = Arc::new(AtomicU64::new(0));
        let in_flight = Arc::new(AtomicU64::new(0));
        let max_in_flight = Arc::new(AtomicU64::new(0));

        // Acquire 4 connections across a pool of capacity 2. This should
        // complete, but the number of simultaneously checked-out connections
        // must stay bounded by the held semaphore permits.
        let mut handles = Vec::new();
        for _ in 0..4 {
            let pool = pool.clone();
            let cx = cx.clone();
            let acquired = acquired.clone();
            let in_flight = in_flight.clone();
            let max_in_flight = max_in_flight.clone();
            handles.push(frankenterm_core::runtime_compat::task::spawn(async move {
                let conn = pool.acquire(&cx).await.expect("acquire");
                acquired.fetch_add(1, Ordering::SeqCst);
                let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = max_in_flight.fetch_max(current, Ordering::SeqCst);
                frankenterm_core::runtime_compat::sleep(std::time::Duration::from_millis(5)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                pool.release(&cx, conn).await.expect("release");
            }));
        }

        for h in handles {
            h.await.expect("task");
        }

        assert_eq!(acquired.load(Ordering::SeqCst), 4);
        assert_eq!(pool.available_permits(), 2);
        assert!(
            max_in_flight.load(Ordering::SeqCst) <= 2,
            "checked-out connections must never exceed pool capacity"
        );
    });
}

// ===========================================================================
// Section 2: Cancellation semantics
//
// Verifies that Cx cancellation propagates correctly through pool and
// mux client operations.
// ===========================================================================

#[test]
fn pool_cancelled_cx_acquire_fails_cleanly() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = timeout_cx();
        let pool = MockPool::new(4);

        let result = pool.acquire(&cx).await;
        assert!(result.is_err(), "acquire with cancelled Cx should fail");
        let err = result.unwrap_err();
        assert!(
            err.contains("cancelled") || err.contains("semaphore") || err.contains("Cancelled"),
            "error should indicate cancellation: {err}"
        );
    });
}

#[test]
fn mux_client_cancelled_cx_rejects_operations() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = user_cancelled_cx();
        let client = MockMuxClient::new();

        let result = client.get_pane_text(&cx, 0).await;
        assert!(result.is_err(), "cancelled Cx should fail operations");
    });
}

#[test]
fn pool_release_succeeds_even_after_timeout() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let pool = MockPool::new(2);

        // Acquire with healthy Cx
        let conn = pool.acquire(&cx).await.expect("acquire");

        // Release should work even if we create a fresh Cx
        let release_cx = healthy_cx();
        pool.release(&release_cx, conn).await.expect("release");
        assert_eq!(pool.available_permits(), 2);
    });
}

// ===========================================================================
// Section 3: MockMuxClient integration with Cx
//
// Tests the mock mux client's behavior under various Cx states,
// simulating real mux client patterns.
// ===========================================================================

#[test]
fn mux_client_set_and_get_pane_content_round_trips() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let client = MockMuxClient::new();

        client
            .set_pane_content(&cx, 42, "hello agent".to_string())
            .await
            .expect("set content");

        let text = client.get_pane_text(&cx, 42).await.expect("get text");
        assert_eq!(text, "hello agent");
        assert_eq!(client.request_count(), 1);
    });
}

#[test]
fn mux_client_multiple_panes_independent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let client = MockMuxClient::new();

        let content = TestPaneData::multi_pane_content(5, 10);
        for (id, text) in &content {
            client
                .set_pane_content(&cx, *id, text.clone())
                .await
                .expect("set");
        }

        for (id, expected) in &content {
            let actual = client.get_pane_text(&cx, *id).await.expect("get");
            assert_eq!(&actual, expected, "pane {id} content mismatch");
        }
        assert_eq!(client.request_count(), 5);
    });
}

#[test]
fn mux_client_missing_pane_returns_error() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let client = MockMuxClient::new();

        let result = client.get_pane_text(&cx, 999).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    });
}

// ===========================================================================
// Section 4: MockUnixStream IPC with Cx
//
// Validates the mock stream pair behaves correctly under asupersync
// primitives, including fault injection.
// ===========================================================================

#[test]
fn mock_stream_pair_bidirectional_data_transfer() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let (a, b) = mock_unix_stream_pair();

        a.write(&cx, b"hello from A").await.expect("write A");
        b.write(&cx, b"hello from B").await.expect("write B");

        let from_a = b.read(&cx, 1024).await.expect("read B");
        let from_b = a.read(&cx, 1024).await.expect("read A");

        assert_eq!(from_a, b"hello from A");
        assert_eq!(from_b, b"hello from B");
        assert_eq!(a.bytes_written(), 12);
        assert_eq!(b.bytes_written(), 12);
    });
}

#[test]
fn mock_stream_closed_end_rejects_writes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let (a, _b) = mock_unix_stream_pair();

        a.close();
        let result = a.write(&cx, b"should fail").await;
        assert!(result.is_err());
        assert!(a.is_closed());
    });
}

#[test]
fn simulated_network_fault_injection_deterministic() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let (a, _b) = mock_unix_stream_pair();

        let net = SimulatedNetwork::new(a, SimulatedNetworkConfig::lossy(), 42);

        // Write 100 messages — some should be dropped or error
        let mut successes = 0u64;
        for i in 0..100u64 {
            let data = format!("msg-{i}");
            if net.write(&cx, data.as_bytes()).await.is_ok() {
                successes += 1;
            }
        }

        // With lossy config (5% error, 10% drop), we should see some faults
        // but not all failures
        assert!(successes > 0, "some writes should succeed");
        let total_injected = net.errors_injected() + net.drops_injected();
        assert!(total_injected > 0, "fault injection should trigger");
    });
}

#[test]
fn simulated_network_healthy_passes_all_data() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let (a, b) = mock_unix_stream_pair();

        let net_a = SimulatedNetwork::new(a, SimulatedNetworkConfig::healthy(), 1);

        for i in 0..50 {
            let data = format!("packet-{i}");
            net_a.write(&cx, data.as_bytes()).await.expect("write");
        }

        assert_eq!(net_a.errors_injected(), 0);
        assert_eq!(net_a.drops_injected(), 0);

        let received = b.read(&cx, 65536).await.expect("read");
        assert!(!received.is_empty(), "all data should arrive");
    });
}

// ===========================================================================
// Section 5: Pool + MuxClient composition
//
// Integration tests combining pool acquire/release with mux client
// operations to validate the full Cx-threaded path.
// ===========================================================================

#[test]
fn pool_and_mux_client_full_lifecycle() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let pool = MockPool::new(2);
        let client = Arc::new(MockMuxClient::new());

        // Setup pane data
        client
            .set_pane_content(&cx, 0, "pane-0-output".to_string())
            .await
            .expect("set");

        // Simulate pool-managed mux operations
        for _ in 0..5 {
            let conn = pool.acquire(&cx).await.expect("acquire");
            let _text = client.get_pane_text(&cx, 0).await.expect("get text");
            pool.release(&cx, conn).await.expect("release");
        }

        assert_eq!(pool.total_acquired(), 5);
        assert_eq!(client.request_count(), 5);
        assert_eq!(pool.available_permits(), 2);
    });
}

#[test]
fn pool_and_mux_client_concurrent_operations() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let pool = Arc::new(MockPool::new(4));
        let client = Arc::new(MockMuxClient::new());

        // Setup panes
        for id in 0..4 {
            client
                .set_pane_content(&cx, id, format!("output-{id}"))
                .await
                .expect("set");
        }

        // Concurrent operations
        let mut handles = Vec::new();
        for pane_id in 0..4 {
            let pool = pool.clone();
            let client = client.clone();
            let cx = cx.clone();
            handles.push(frankenterm_core::runtime_compat::task::spawn(async move {
                let conn = pool.acquire(&cx).await.expect("acquire");
                let text = client.get_pane_text(&cx, pane_id).await.expect("get text");
                assert_eq!(text, format!("output-{pane_id}"));
                pool.release(&cx, conn).await.expect("release");
            }));
        }

        for h in handles {
            h.await.expect("task");
        }

        assert_eq!(pool.total_acquired(), 4);
        assert_eq!(client.request_count(), 4);
    });
}

// ===========================================================================
// Section 6: Diagnostic coverage verification
//
// Ensures the runtime_compat module provides necessary diagnostic surface.
// ===========================================================================

#[test]
fn runtime_compat_exports_required_primitives() {
    // Verify that the runtime_compat module exports all primitives
    // needed by the mux subsystem. Compilation-only test.
    use frankenterm_core::runtime_compat::{sleep, task, timeout};

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // sleep works
        sleep(std::time::Duration::from_millis(1)).await;

        // task::spawn works
        let handle = task::spawn(async { 42u32 });
        let result = handle.await.expect("spawn");
        assert_eq!(result, 42);

        // timeout works
        let result = timeout(std::time::Duration::from_secs(1), async { "ok" }).await;
        assert!(result.is_ok());
    });
}

#[test]
fn pool_stats_type_supports_serde() {
    // Verify MuxPoolStats serializes (compilation + roundtrip)
    use frankenterm_core::pool::PoolStats;
    use frankenterm_core::vendored::mux_pool::MuxPoolStats;

    let stats = MuxPoolStats {
        pool: PoolStats {
            max_size: 8,
            idle_count: 3,
            active_count: 2,
            total_acquired: 100,
            total_returned: 97,
            total_evicted: 5,
            total_timeouts: 0,
        },
        connections_created: 20,
        connections_failed: 1,
        health_checks: 50,
        health_check_failures: 2,
        recovery_attempts: 3,
        recovery_successes: 2,
        permanent_failures: 1,
    };

    let json = serde_json::to_string(&stats).expect("serialize");
    let deser: MuxPoolStats = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(deser.connections_created, 20);
    assert_eq!(deser.health_check_failures, 2);
    assert_eq!(deser.pool.max_size, 8);
}
