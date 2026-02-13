//! Smoke tests for the LabRuntime test infrastructure.
//!
//! Exercises all helpers from `common::lab` and `common::fixtures` to verify
//! the infrastructure compiles and runs correctly under the asupersync-runtime
//! feature flag.

#![cfg(feature = "asupersync-runtime")]

mod common;

use asupersync::{Budget, CancelKind, Cx, LabConfig, LabRuntime};
use common::fixtures::{
    self, MockMuxClient, MockPool, RuntimeFixture, TestPaneData, healthy_cx, timeout_cx,
    user_cancelled_cx,
};
use common::lab::{
    ChaosTestConfig, ExplorationTestConfig, LabTestConfig, run_chaos_test, run_exploration_test,
    run_lab_test, run_lab_test_simple, run_multi_seed_test,
};

// ---------------------------------------------------------------------------
// run_lab_test smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_run_lab_test_empty_runtime() {
    let report = run_lab_test_simple(42, "smoke_empty", |_runtime| {
        // No tasks — just verify the harness works with an empty runtime.
    });
    assert!(report.passed());
    assert_eq!(report.seed, 42);
}

#[test]
fn smoke_run_lab_test_with_config() {
    let config = LabTestConfig::new(7, "smoke_configured")
        .worker_count(1)
        .max_steps(10_000);
    let report = run_lab_test(config, |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

#[test]
fn smoke_run_lab_test_with_tasks() {
    let report = run_lab_test_simple(99, "smoke_with_tasks", |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async { 42_u32 })
            .expect("create task");
        runtime
            .scheduler
            .lock()
            .expect("lock scheduler")
            .schedule(task_id, 0);
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
    assert!(report.steps > 0, "should have executed at least one step");
}

#[test]
fn smoke_run_lab_test_concurrent_tasks() {
    let report = run_lab_test(
        LabTestConfig::new(123, "smoke_concurrent").worker_count(4),
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for i in 0..4_u32 {
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move { i * 2 })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        },
    );
    assert!(report.passed());
}

// ---------------------------------------------------------------------------
// run_chaos_test smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_chaos_test_light() {
    let config = ChaosTestConfig::light(42, "smoke_chaos_light");
    let report = run_chaos_test(config, |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
    assert!(report.chaos_active);
}

#[test]
fn smoke_chaos_test_heavy() {
    let config = ChaosTestConfig::heavy(42, "smoke_chaos_heavy").max_steps(10_000);
    let report = run_chaos_test(config, |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
    assert!(report.chaos_active);
}

#[test]
fn smoke_chaos_test_with_tasks() {
    let config = ChaosTestConfig::light(7, "smoke_chaos_tasks").worker_count(2);
    let report = run_chaos_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async { "survived chaos" })
            .expect("create task");
        runtime
            .scheduler
            .lock()
            .expect("lock scheduler")
            .schedule(task_id, 0);
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

// ---------------------------------------------------------------------------
// run_exploration_test smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_exploration_test_empty() {
    let config = ExplorationTestConfig::new("smoke_explore_empty", 5)
        .base_seed(0)
        .worker_count(1);
    let report = run_exploration_test(config, |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
    assert_eq!(report.total_runs, 5);
}

#[test]
fn smoke_exploration_test_with_tasks() {
    let config = ExplorationTestConfig::new("smoke_explore_tasks", 10)
        .base_seed(42)
        .worker_count(2)
        .max_steps_per_run(50_000);
    let report = run_exploration_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        for _ in 0..2 {
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async { 1_u32 })
                .expect("create task");
            runtime
                .scheduler
                .lock()
                .expect("lock scheduler")
                .schedule(task_id, 0);
        }
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
    assert!(
        report.total_runs >= 5,
        "should have explored multiple seeds"
    );
}

// ---------------------------------------------------------------------------
// run_multi_seed_test smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_multi_seed_test() {
    let reports = run_multi_seed_test("smoke_multi_seed", &[1, 42, 99, 1000], |runtime| {
        runtime.run_until_quiescent();
    });
    assert_eq!(reports.len(), 4);
    for report in &reports {
        assert!(report.passed());
    }
}

// ---------------------------------------------------------------------------
// MockPool fixture smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_mock_pool_acquire_release() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let pool = MockPool::new(2);
        assert_eq!(pool.capacity(), 2);

        // MockPool's semaphore permit is scoped to the acquire() call,
        // so available_permits recovers after each acquire completes.
        let conn1 = pool.acquire(&cx).await.expect("acquire 1");
        assert_eq!(pool.total_acquired(), 1);

        let conn2 = pool.acquire(&cx).await.expect("acquire 2");
        assert_eq!(pool.total_acquired(), 2);

        // Release returns connections to the available pool.
        pool.release(&cx, conn1).await.expect("release 1");
        pool.release(&cx, conn2).await.expect("release 2");

        // After release, we can re-acquire.
        let conn3 = pool.acquire(&cx).await.expect("acquire 3");
        assert_eq!(pool.total_acquired(), 3);
        pool.release(&cx, conn3).await.expect("release 3");
    });
}

#[test]
fn smoke_mock_pool_cancelled_acquire() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let pool = MockPool::new(1);
        let _conn = pool.acquire(&cx).await.expect("acquire");

        // Pool is exhausted — try to acquire with a cancelled context.
        let cancelled = timeout_cx();
        let result = pool.acquire(&cancelled).await;
        assert!(result.is_err(), "acquire with cancelled cx should fail");
    });
}

// ---------------------------------------------------------------------------
// MockMuxClient fixture smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_mock_mux_client() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let client = MockMuxClient::new();

        client
            .set_pane_content(&cx, 1, "hello world".to_string())
            .await
            .expect("set content");
        let text = client.get_pane_text(&cx, 1).await.expect("get text");
        assert_eq!(text, "hello world");
        assert_eq!(client.request_count(), 1);

        let err = client.get_pane_text(&cx, 999).await;
        assert!(err.is_err(), "missing pane should error");
        assert_eq!(client.request_count(), 2);
    });
}

// ---------------------------------------------------------------------------
// TestPaneData smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_test_pane_data() {
    let ids = TestPaneData::pane_ids(5);
    assert_eq!(ids, vec![0, 1, 2, 3, 4]);

    let content = TestPaneData::pane_content(3, 2);
    assert!(content.contains("[pane-3]"));
    assert!(content.contains("line 0"));
    assert!(content.contains("line 1"));

    let multi = TestPaneData::multi_pane_content(3, 1);
    assert_eq!(multi.len(), 3);
    assert!(multi.contains_key(&0));
    assert!(multi.contains_key(&2));
}

// ---------------------------------------------------------------------------
// Cancellation helper smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_cancellation_helpers() {
    let hcx = healthy_cx();
    assert!(!hcx.is_cancel_requested());

    let tcx = timeout_cx();
    assert!(tcx.is_cancel_requested());

    let ucx = user_cancelled_cx();
    assert!(ucx.is_cancel_requested());
}

// ---------------------------------------------------------------------------
// MockUnixStream smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_mock_unix_stream_pair_write_read() {
    use common::fixtures::mock_unix_stream_pair;

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let (stream_a, stream_b) = mock_unix_stream_pair();

        // A writes, B reads
        let written = stream_a.write(&cx, b"hello from A").await.expect("write A");
        assert_eq!(written, 12);
        assert_eq!(stream_a.bytes_written(), 12);

        let data = stream_b.read(&cx, 1024).await.expect("read B");
        assert_eq!(data, b"hello from A");
        assert_eq!(stream_b.bytes_read(), 12);

        // B writes, A reads
        stream_b.write(&cx, b"reply from B").await.expect("write B");
        let reply = stream_a.read(&cx, 1024).await.expect("read A");
        assert_eq!(reply, b"reply from B");
    });
}

#[test]
fn smoke_mock_unix_stream_close() {
    use common::fixtures::mock_unix_stream_pair;

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let (stream_a, _stream_b) = mock_unix_stream_pair();

        assert!(!stream_a.is_closed());
        stream_a.close();
        assert!(stream_a.is_closed());

        let result = stream_a.write(&cx, b"after close").await;
        assert!(result.is_err(), "write after close should fail");
    });
}

#[test]
fn smoke_mock_unix_stream_empty_read() {
    use common::fixtures::mock_unix_stream_pair;

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let (_stream_a, stream_b) = mock_unix_stream_pair();

        // Reading from empty buffer returns empty vec
        let data = stream_b.read(&cx, 1024).await.expect("read empty");
        assert!(data.is_empty());
    });
}

// ---------------------------------------------------------------------------
// SimulatedNetwork smoke tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_simulated_network_healthy() {
    use common::fixtures::{SimulatedNetwork, SimulatedNetworkConfig, mock_unix_stream_pair};

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let (stream_a, stream_b) = mock_unix_stream_pair();

        let net_a = SimulatedNetwork::new(stream_a, SimulatedNetworkConfig::healthy(), 42);

        // Healthy network should pass all data through
        net_a.write(&cx, b"test data").await.expect("write");
        let data = stream_b.read(&cx, 1024).await.expect("read");
        assert_eq!(data, b"test data");
        assert_eq!(net_a.errors_injected(), 0);
        assert_eq!(net_a.drops_injected(), 0);
    });
}

#[test]
fn smoke_simulated_network_hostile() {
    use common::fixtures::{SimulatedNetwork, SimulatedNetworkConfig, mock_unix_stream_pair};

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let (stream_a, stream_b) = mock_unix_stream_pair();

        let net_a = SimulatedNetwork::new(stream_a, SimulatedNetworkConfig::hostile(), 42);

        // With 20% error + 30% drop rate, some operations will fail.
        // Run many writes and verify at least one fault is injected.
        let mut total_errors = 0_u32;
        for i in 0..50_u32 {
            let msg = format!("msg-{i}");
            if net_a.write(&cx, msg.as_bytes()).await.is_err() {
                total_errors += 1;
            }
        }

        // With 50% combined fault rate over 50 attempts, we should see faults.
        let faults = net_a.errors_injected() + net_a.drops_injected();
        assert!(
            faults > 0,
            "hostile network should inject at least one fault over 50 writes"
        );

        // The underlying stream should have received some data
        let data = stream_b.read(&cx, 1024 * 1024).await.expect("read");
        // Some data got through (not all was errored/dropped)
        assert!(
            !data.is_empty() || total_errors == 50,
            "either some data arrived or all writes errored"
        );
    });
}
