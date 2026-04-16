//! wa-2ojrv — End-to-end integration tests for the full asupersync stack.
//!
//! This suite exercises the complete FrankenTerm async surface running on
//! asupersync via `LabRuntime`: pool lifecycle, concurrent pool operations,
//! IPC loopback, event streaming, mid-operation cancellation, timeout
//! cascades, connection recovery, and chaos. Each scenario corresponds to
//! one of the 8 test bodies named in the bead.
//!
//! All scenarios run under deterministic `LabRuntime` virtual time with
//! seeded configurations (100, 200, 300, 400, 500, 600, 700, 800) so
//! failures reproduce exactly. Each scenario emits a structured
//! event-log entry (via `LabTestConfig::artifact_dir`) that makes the
//! per-step sequence readable from the saved log alone.
//!
//! The suite uses the `common/fixtures.rs` mock stack (MockPool,
//! MockMuxClient, MockUnixStream, SimulatedNetwork) rather than real
//! sockets because the whole point of this bead is LabRuntime
//! determinism; real sockets would defeat the deterministic oracle
//! checks (obligation-leak, task-leak, stuck-bailout) that the bead's
//! acceptance criteria depend on.

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use asupersync::Budget;
use common::fixtures::{
    MockMuxClient, MockPool, RuntimeFixture, SimulatedNetwork, SimulatedNetworkConfig,
    healthy_cx, mock_unix_stream_pair, timeout_cx, user_cancelled_cx,
};
use common::lab::{
    ChaosPreset, ChaosTestConfig, LabTestConfig, run_chaos_test, run_lab_test,
    run_lab_test_simple,
};

/// Helper: build a `LabTestConfig` with the wa-2ojrv bead tag and
/// the bead's canonical worker/step budget for integration work.
fn lab_config(seed: u64, name: &str) -> LabTestConfig {
    LabTestConfig::new(seed, name)
        .component("tests.integration.wa_2ojrv")
        .bead_id("wa-2ojrv")
        .worker_count(4)
        .max_steps(500_000)
}

// ============================================================================
// Scenario 1 — Full lifecycle (seed 100)
//
// Start runtime → create MuxPool → acquire connection → execute mux
// command → release → shutdown. Pins the simplest end-to-end path
// through the stack and asserts counter consistency at every step.
// ============================================================================

#[test]
fn wa_2ojrv_s1_full_lifecycle() {
    let report = run_lab_test(lab_config(100, "s1_full_lifecycle"), |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let pool = Arc::new(MockPool::new(4));
        let client = Arc::new(MockMuxClient::new());

        let pool_c = Arc::clone(&pool);
        let client_c = Arc::clone(&client);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();

                // Seed the pane content so the mux command has something to return.
                client_c
                    .set_pane_content(&cx, 1, "hello".into())
                    .await
                    .expect("set content");

                // Full lifecycle: acquire → command → release.
                let conn = pool_c.acquire(&cx).await.expect("acquire");
                assert!(conn.id < 4, "conn id within pool capacity");
                let text = client_c
                    .get_pane_text(&cx, 1)
                    .await
                    .expect("mux command");
                assert_eq!(text, "hello");
                pool_c.release(&cx, conn).await.expect("release");
            })
            .expect("spawn lifecycle task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_quiescent();

        // Counter invariants: exactly one acquire, one release, one command.
        assert_eq!(pool.total_acquired(), 1, "exactly one acquire");
        assert_eq!(
            pool.available_permits(),
            4,
            "all permits returned at shutdown"
        );
        assert_eq!(client.request_count(), 1, "exactly one mux command");
    });
    assert!(report.passed(), "s1_full_lifecycle did not pass oracle");
}

// ============================================================================
// Scenario 2 — Concurrent pool operations (seed 200)
//
// 20 tasks × MockPool(max_size=5): each acquires, executes a command,
// releases. The core invariant is that in-flight connections never
// exceed max_size under any interleaving the LabRuntime scheduler
// explores.
// ============================================================================

#[test]
fn wa_2ojrv_s2_concurrent_pool_operations() {
    let report = run_lab_test(
        lab_config(200, "s2_concurrent_pool_operations").worker_count(4),
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let pool = Arc::new(MockPool::new(5));
            let client = Arc::new(MockMuxClient::new());
            let in_flight = Arc::new(AtomicU64::new(0));
            let max_in_flight = Arc::new(AtomicU64::new(0));
            let completed = Arc::new(AtomicU64::new(0));

            for i in 0u64..20 {
                let pool_c = Arc::clone(&pool);
                let client_c = Arc::clone(&client);
                let in_flight_c = Arc::clone(&in_flight);
                let max_c = Arc::clone(&max_in_flight);
                let completed_c = Arc::clone(&completed);
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        let cx = healthy_cx();
                        client_c
                            .set_pane_content(&cx, i, format!("pane-{i}"))
                            .await
                            .expect("set content");
                        let conn = pool_c.acquire(&cx).await.expect("acquire");
                        let now = in_flight_c.fetch_add(1, Ordering::SeqCst) + 1;
                        let _ = max_c.fetch_max(now, Ordering::SeqCst);
                        asupersync::runtime::yield_now().await;
                        let _ = client_c
                            .get_pane_text(&cx, i)
                            .await
                            .expect("mux command");
                        in_flight_c.fetch_sub(1, Ordering::SeqCst);
                        pool_c.release(&cx, conn).await.expect("release");
                        completed_c.fetch_add(1, Ordering::SeqCst);
                    })
                    .expect("spawn concurrent task");
                runtime.scheduler.lock().schedule(task_id, 0);
            }
            runtime.run_until_quiescent();

            assert_eq!(completed.load(Ordering::SeqCst), 20, "all tasks completed");
            assert_eq!(pool.total_acquired(), 20, "20 acquires recorded");
            assert!(
                max_in_flight.load(Ordering::SeqCst) <= 5,
                "in-flight never exceeded pool capacity (max={})",
                max_in_flight.load(Ordering::SeqCst)
            );
            assert_eq!(pool.available_permits(), 5, "all permits returned");
            assert_eq!(client.request_count(), 20, "20 mux commands");
        },
    );
    assert!(report.passed(), "s2 did not pass oracle");
}

// ============================================================================
// Scenario 3 — IPC round-trip (seed 300)
//
// MockUnixStream pair: client writes a JSON command, server reads it,
// writes a response, client reads it. Pins the IPC loopback contract
// under LabRuntime virtual time with no filesystem touches.
// ============================================================================

#[test]
fn wa_2ojrv_s3_ipc_round_trip() {
    let report = run_lab_test(lab_config(300, "s3_ipc_round_trip"), |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let (client_end, server_end) = mock_unix_stream_pair();

        // MockUnixStream::read is non-blocking (drains whatever's in the
        // buffer). Under LabRuntime scheduling the producer may not have
        // run yet at the first poll, so we need a small yield-and-retry
        // loop per side to accumulate the expected payload. This models
        // exactly what a real framed-I/O read loop does over a socket.
        async fn read_exact(
            stream: &Arc<common::fixtures::MockUnixStream>,
            cx: &asupersync::Cx,
            expected: usize,
        ) -> Vec<u8> {
            let mut out = Vec::with_capacity(expected);
            while out.len() < expected {
                let chunk = stream
                    .read(cx, expected - out.len())
                    .await
                    .expect("read chunk");
                if chunk.is_empty() {
                    asupersync::runtime::yield_now().await;
                    continue;
                }
                out.extend_from_slice(&chunk);
            }
            out
        }

        // Spawn the server end.
        let server = Arc::clone(&server_end);
        let (server_id, _sh) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();
                let req = read_exact(&server, &cx, 14).await;
                assert_eq!(&req, br#"{"cmd":"ping"}"#);
                server
                    .write(&cx, br#"{"ok":true}"#)
                    .await
                    .expect("server write");
            })
            .expect("spawn server");
        runtime.scheduler.lock().schedule(server_id, 0);

        // Spawn the client end.
        let client = Arc::clone(&client_end);
        let (client_id, _ch) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();
                client
                    .write(&cx, br#"{"cmd":"ping"}"#)
                    .await
                    .expect("client write");
                let resp = read_exact(&client, &cx, 11).await;
                assert_eq!(&resp, br#"{"ok":true}"#);
            })
            .expect("spawn client");
        runtime.scheduler.lock().schedule(client_id, 0);

        runtime.run_until_quiescent();

        // Bytes should match on both sides of the loopback.
        assert_eq!(client_end.bytes_written(), 14); // {"cmd":"ping"}
        assert_eq!(server_end.bytes_read(), 14);
        assert_eq!(server_end.bytes_written(), 11); // {"ok":true}
        assert_eq!(client_end.bytes_read(), 11);
    });
    assert!(report.passed(), "s3 did not pass oracle");
}

// ============================================================================
// Scenario 4 — Event streaming (seed 400)
//
// A producer task emits N events on an mpsc channel; a consumer task
// reads them and asserts in-order, no-loss delivery under LabRuntime
// scheduling.
// ============================================================================

#[test]
fn wa_2ojrv_s4_event_streaming() {
    let report = run_lab_test(lab_config(400, "s4_event_streaming"), |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let (tx, rx) = asupersync::channel::mpsc::channel::<u64>(16);
        let received = Arc::new(AtomicU64::new(0));
        let order_ok = Arc::new(AtomicU64::new(1)); // 1 = in-order, 0 = violated

        // Producer.
        let (prod_id, _ph) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();
                for seq in 0u64..25 {
                    tx.reserve(&cx).await.expect("reserve").send(seq);
                }
                drop(tx);
            })
            .expect("spawn producer");
        runtime.scheduler.lock().schedule(prod_id, 0);

        // Consumer.
        let received_c = Arc::clone(&received);
        let order_c = Arc::clone(&order_ok);
        let (cons_id, _ch) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();
                let mut rx = rx;
                let mut expected = 0u64;
                while let Ok(seq) = rx.recv(&cx).await {
                    if seq != expected {
                        order_c.store(0, Ordering::SeqCst);
                    }
                    expected = seq + 1;
                    received_c.fetch_add(1, Ordering::SeqCst);
                }
            })
            .expect("spawn consumer");
        runtime.scheduler.lock().schedule(cons_id, 0);

        runtime.run_until_quiescent();

        assert_eq!(received.load(Ordering::SeqCst), 25, "all events delivered");
        assert_eq!(
            order_ok.load(Ordering::SeqCst),
            1,
            "FIFO preserved across scheduler interleavings"
        );
    });
    assert!(report.passed(), "s4 did not pass oracle");
}

// ============================================================================
// Scenario 5 — Mid-operation cancellation (seed 500)
//
// A long-running producer emits events forever; a watch-based cancel
// signal tells it to stop after 3 receives. Pins the contract that
// cancellation drains the channel cleanly without orphan tasks.
// ============================================================================

#[test]
fn wa_2ojrv_s5_mid_operation_cancellation() {
    let report = run_lab_test(lab_config(500, "s5_mid_operation_cancellation"), |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let (tx, rx) = asupersync::channel::mpsc::channel::<u64>(4);
        let (cancel_tx, cancel_rx) = asupersync::channel::watch::channel(false);
        let received = Arc::new(AtomicU64::new(0));

        // Producer: emits until cancelled.
        let mut cancel_rx_prod = cancel_rx.clone();
        let (prod_id, _ph) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();
                let mut seq = 0u64;
                loop {
                    if cancel_rx_prod.borrow_and_clone() {
                        break;
                    }
                    if tx.try_reserve().is_err() {
                        asupersync::runtime::yield_now().await;
                        continue;
                    }
                    tx.reserve(&cx).await.expect("reserve").send(seq);
                    seq += 1;
                    asupersync::runtime::yield_now().await;
                }
            })
            .expect("spawn producer");
        runtime.scheduler.lock().schedule(prod_id, 0);

        // Consumer: receives 3 events then signals cancel.
        let received_c = Arc::clone(&received);
        let (cons_id, _ch) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();
                let mut rx = rx;
                for _ in 0..3 {
                    let _ = rx.recv(&cx).await.expect("recv");
                    received_c.fetch_add(1, Ordering::SeqCst);
                }
                cancel_tx.send(true).expect("signal cancel");
                // Drain any already-queued events then exit.
                while rx.recv(&cx).await.is_ok() {}
            })
            .expect("spawn consumer");
        runtime.scheduler.lock().schedule(cons_id, 0);

        runtime.run_until_quiescent();

        assert_eq!(
            received.load(Ordering::SeqCst),
            3,
            "consumer received exactly 3 events before cancelling"
        );
    });
    assert!(report.passed(), "s5 did not pass oracle");
}

// ============================================================================
// Scenario 6 — Timeout cascade (seed 600)
//
// A pre-cancelled Cx passed into pool.acquire must surface Cancelled
// without deadlocking the scheduler. Pins Cancelled-Cx propagation
// through the first scope of the stack.
// ============================================================================

#[test]
fn wa_2ojrv_s6_timeout_cascade() {
    let report = run_lab_test(lab_config(600, "s6_timeout_cascade"), |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let pool = Arc::new(MockPool::new(2));
        let cancelled_count = Arc::new(AtomicU64::new(0));
        let timed_count = Arc::new(AtomicU64::new(0));

        let pool_c = Arc::clone(&pool);
        let cancelled_c = Arc::clone(&cancelled_count);
        let timed_c = Arc::clone(&timed_count);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                // Case A: user-cancelled Cx.
                let user_cx = user_cancelled_cx();
                if pool_c.acquire(&user_cx).await.is_err() {
                    cancelled_c.fetch_add(1, Ordering::SeqCst);
                }
                // Case B: timeout Cx.
                let to_cx = timeout_cx();
                if pool_c.acquire(&to_cx).await.is_err() {
                    timed_c.fetch_add(1, Ordering::SeqCst);
                }
            })
            .expect("spawn cascade task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_quiescent();

        assert_eq!(
            cancelled_count.load(Ordering::SeqCst),
            1,
            "user-cancelled Cx must surface Err from acquire"
        );
        assert_eq!(
            timed_count.load(Ordering::SeqCst),
            1,
            "timeout Cx must surface Err from acquire"
        );
        assert_eq!(
            pool.total_acquired(),
            0,
            "no successful acquires under cancellation"
        );
    });
    assert!(report.passed(), "s6 did not pass oracle");
}

// ============================================================================
// Scenario 7 — Connection recovery (seed 700)
//
// First acquire succeeds and the "connection" is broken (MockUnixStream
// close). Next acquire still succeeds because the pool hands out a
// fresh permit. Pins the transparent-recovery invariant: pool
// bookkeeping doesn't care about the underlying stream's health.
// ============================================================================

#[test]
fn wa_2ojrv_s7_connection_recovery() {
    let report = run_lab_test(lab_config(700, "s7_connection_recovery"), |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let pool = Arc::new(MockPool::new(2));
        let (client_end, server_end) = mock_unix_stream_pair();
        let recovered = Arc::new(AtomicU64::new(0));

        let pool_c = Arc::clone(&pool);
        let client = Arc::clone(&client_end);
        let server = Arc::clone(&server_end);
        let recovered_c = Arc::clone(&recovered);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = healthy_cx();

                // First acquire + one successful exchange.
                let conn_a = pool_c.acquire(&cx).await.expect("first acquire");
                server.write(&cx, b"hello").await.expect("server write");
                let got = client.read(&cx, 64).await.expect("client read");
                assert_eq!(&got, b"hello");
                pool_c.release(&cx, conn_a).await.expect("release");

                // Simulate a broken pipe.
                server.close();
                assert!(server.is_closed());

                // Second acquire: pool still has permits; fresh lease
                // succeeds. This is the bead's "transparent recovery"
                // contract — the pool does not poison the remaining
                // permits because one stream died.
                let conn_b = pool_c.acquire(&cx).await.expect("second acquire");
                assert!(conn_b.id < 2);
                pool_c.release(&cx, conn_b).await.expect("release");

                recovered_c.store(1, Ordering::SeqCst);
            })
            .expect("spawn recovery task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_quiescent();

        assert_eq!(
            recovered.load(Ordering::SeqCst),
            1,
            "recovery path completed"
        );
        assert_eq!(pool.total_acquired(), 2, "2 acquires total");
        assert_eq!(pool.available_permits(), 2, "all permits returned");
    });
    assert!(report.passed(), "s7 did not pass oracle");
}

// ============================================================================
// Scenario 8 — Full system chaos (seed 800, chaos Light preset)
//
// Runs the s1 lifecycle under chaos fault injection. The test must
// survive whatever faults the light preset injects (cancellations,
// small delays). The oracle still has to pass. This is the bead's
// final "stack resilience" gate.
// ============================================================================

#[test]
fn wa_2ojrv_s8_full_system_chaos() {
    let chaos_config = ChaosTestConfig {
        base: lab_config(800, "s8_full_system_chaos").worker_count(4),
        chaos: ChaosPreset::Light,
    };
    let report = run_chaos_test(chaos_config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let pool = Arc::new(MockPool::new(3));
        let client = Arc::new(MockMuxClient::new());
        let successes = Arc::new(AtomicU64::new(0));
        let failures = Arc::new(AtomicU64::new(0));

        for i in 0u64..10 {
            let pool_c = Arc::clone(&pool);
            let client_c = Arc::clone(&client);
            let succ = Arc::clone(&successes);
            let fail = Arc::clone(&failures);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = healthy_cx();
                    let _ = client_c
                        .set_pane_content(&cx, i, format!("chaos-{i}"))
                        .await;
                    match pool_c.acquire(&cx).await {
                        Ok(conn) => {
                            let _ = client_c.get_pane_text(&cx, i).await;
                            let _ = pool_c.release(&cx, conn).await;
                            succ.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(_) => {
                            fail.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
                .expect("spawn chaos task");
            runtime.scheduler.lock().schedule(task_id, 0);
        }
        runtime.run_until_quiescent();

        // Chaos must not leak permits; successes + failures = total.
        let total = successes.load(Ordering::SeqCst) + failures.load(Ordering::SeqCst);
        assert_eq!(total, 10, "every chaos task reached a terminal state");
        assert_eq!(
            pool.available_permits(),
            3,
            "all permits returned even under chaos"
        );
    });
    assert!(
        report.passed(),
        "s8 chaos scenario did not pass oracle"
    );
    assert!(report.chaos_active, "chaos injection must be reported active");
}

// ============================================================================
// Positive-signal: fixtures surface sanity check.
//
// This does not exercise a new scenario — it pins that the test-support
// crate surfaces the bead depends on are still intact. If any of these
// constructors break, the 8 scenarios above would compile but test
// something unrelated. Keeps the acceptance criteria honest.
// ============================================================================

#[test]
fn wa_2ojrv_fixtures_still_intact() {
    // These are constructor-only sanity checks; the real tests above
    // exercise the semantics.
    let _fixture = RuntimeFixture::current_thread();
    let _pool = MockPool::new(2);
    let _client = MockMuxClient::new();
    let _net = SimulatedNetworkConfig::healthy();
    let _pair = mock_unix_stream_pair();
    let _healthy = healthy_cx();
    let _to = timeout_cx();
    let _user = user_cancelled_cx();
    // SimulatedNetwork needs a stream; reuse the pair above.
    let (client_end, _server_end) = mock_unix_stream_pair();
    let _sim = SimulatedNetwork::new(client_end, SimulatedNetworkConfig::lossy(), 42);
}

// Dummy helper so the run_lab_test signature has something to pin —
// keeps the `run_lab_test_simple` import referenced even if we add
// lightweight follow-ups later.
#[allow(dead_code)]
fn _simple_lab_seed_anchor() {
    let _report = run_lab_test_simple(0, "wa_2ojrv_anchor", |_r| {});
}
