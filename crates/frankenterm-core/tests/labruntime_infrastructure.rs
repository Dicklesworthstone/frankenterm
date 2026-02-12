//! Smoke tests for the LabRuntime test infrastructure.
//!
//! Validates that all test helpers, fixtures, and wrappers work correctly
//! before they are used in module-specific tests.

#![cfg(feature = "asupersync-runtime")]

mod common;

use asupersync::{CancelKind, Cx};
use common::fixtures::{
    MockMuxClient, MockPool, RuntimeFixture, TestPaneData, cancelled_cx, healthy_cx, timeout_cx,
    user_cancelled_cx,
};
use common::lab::{
    ChaosTestConfig, ExplorationTestConfig, LabTestConfig, run_chaos_test, run_exploration_test,
    run_lab_test, run_lab_test_simple, run_multi_seed_test,
};

// ---------------------------------------------------------------------------
// LabTestConfig builder tests
// ---------------------------------------------------------------------------

#[test]
fn lab_test_config_defaults() {
    let config = LabTestConfig::new(42, "test");
    assert_eq!(config.seed, 42);
    assert_eq!(config.test_name, "test");
    assert_eq!(config.worker_count, 2);
    assert_eq!(config.max_steps, 100_000);
    assert!(config.panic_on_leak);
}

#[test]
fn lab_test_config_builder() {
    let config = LabTestConfig::new(7, "custom")
        .worker_count(4)
        .max_steps(50_000)
        .panic_on_leak(false);

    assert_eq!(config.seed, 7);
    assert_eq!(config.worker_count, 4);
    assert_eq!(config.max_steps, 50_000);
    assert!(!config.panic_on_leak);
}

#[test]
fn lab_test_config_to_lab_config() {
    let config = LabTestConfig::new(99, "convert").worker_count(3);
    let lab_config = config.to_lab_config();
    assert_eq!(lab_config.seed, 99);
    assert_eq!(lab_config.worker_count, 3);
}

// ---------------------------------------------------------------------------
// run_lab_test smoke tests
// ---------------------------------------------------------------------------

#[test]
fn run_lab_test_empty_runtime_passes() {
    let report = run_lab_test(LabTestConfig::new(42, "empty_runtime"), |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
    assert_eq!(report.seed, 42);
}

#[test]
fn run_lab_test_simple_passes() {
    let report = run_lab_test_simple(7, "simple_empty", |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

#[test]
fn run_lab_test_with_virtual_time() {
    let report = run_lab_test(LabTestConfig::new(42, "virtual_time"), |runtime| {
        assert_eq!(runtime.now(), asupersync::Time::ZERO);
        runtime.advance_time(1_000_000_000); // 1 second in nanos
        assert!(runtime.now() > asupersync::Time::ZERO);
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

#[test]
fn run_lab_test_multi_worker() {
    let config = LabTestConfig::new(42, "multi_worker").worker_count(4);
    let report = run_lab_test(config, |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
}

// ---------------------------------------------------------------------------
// run_chaos_test smoke tests
// ---------------------------------------------------------------------------

#[test]
fn run_chaos_test_light_passes() {
    let config = ChaosTestConfig::light(42, "chaos_light");
    let report = run_chaos_test(config, |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
    assert!(report.chaos_active);
}

#[test]
fn run_chaos_test_heavy_passes() {
    let config = ChaosTestConfig::heavy(42, "chaos_heavy");
    let report = run_chaos_test(config, |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
    assert!(report.chaos_active);
}

// ---------------------------------------------------------------------------
// run_exploration_test smoke tests
// ---------------------------------------------------------------------------

#[test]
fn run_exploration_test_empty_passes() {
    let config = ExplorationTestConfig::new("explore_empty", 5).base_seed(0);
    let report = run_exploration_test(config, |runtime| {
        runtime.run_until_quiescent();
    });
    assert!(report.passed());
    assert_eq!(report.total_runs, 5);
}

// ---------------------------------------------------------------------------
// run_multi_seed_test smoke tests
// ---------------------------------------------------------------------------

#[test]
fn multi_seed_test_passes() {
    let reports = run_multi_seed_test("multi_seed", &[0, 1, 42, 99, 12345], |runtime| {
        runtime.run_until_quiescent();
    });
    assert_eq!(reports.len(), 5);
    assert!(reports.iter().all(|r| r.passed()));
}

// ---------------------------------------------------------------------------
// Fixture tests: MockPool
// ---------------------------------------------------------------------------

#[test]
fn mock_pool_creation() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let pool = MockPool::new(3);
        assert_eq!(pool.capacity(), 3);
        assert_eq!(pool.available_permits(), 3);
        assert_eq!(pool.total_acquired(), 0);
    });
}

#[test]
fn mock_pool_acquire_release() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let pool = MockPool::new(2);

        let conn1 = pool.acquire(&cx).await.expect("acquire 1");
        assert_eq!(pool.total_acquired(), 1);

        let conn2 = pool.acquire(&cx).await.expect("acquire 2");
        assert_eq!(pool.total_acquired(), 2);
        assert_eq!(pool.available_permits(), 0);

        pool.release(&cx, conn1).await.expect("release 1");
        pool.release(&cx, conn2).await.expect("release 2");
    });
}

// ---------------------------------------------------------------------------
// Fixture tests: MockMuxClient
// ---------------------------------------------------------------------------

#[test]
fn mock_mux_client_set_and_get() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let client = MockMuxClient::new();

        client
            .set_pane_content(&cx, 1, "hello world".to_string())
            .await
            .expect("set content");

        let content = client.get_pane_text(&cx, 1).await.expect("get content");
        assert_eq!(content, "hello world");
        assert_eq!(client.request_count(), 1);
    });
}

#[test]
fn mock_mux_client_missing_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = healthy_cx();
        let client = MockMuxClient::new();
        let result = client.get_pane_text(&cx, 999).await;
        assert!(result.is_err());
    });
}

// ---------------------------------------------------------------------------
// Fixture tests: TestPaneData
// ---------------------------------------------------------------------------

#[test]
fn test_pane_data_generation() {
    let ids = TestPaneData::pane_ids(5);
    assert_eq!(ids, vec![0, 1, 2, 3, 4]);

    let content = TestPaneData::pane_content(3, 2);
    assert!(content.contains("[pane-3]"));
    assert!(content.contains("line 0"));
    assert!(content.contains("line 1"));

    let multi = TestPaneData::multi_pane_content(3, 5);
    assert_eq!(multi.len(), 3);
    assert!(multi.contains_key(&0));
    assert!(multi.contains_key(&2));
}

// ---------------------------------------------------------------------------
// Fixture tests: Cancellation helpers
// ---------------------------------------------------------------------------

#[test]
fn cancelled_cx_helpers() {
    let _cx = timeout_cx();
    let _cx = user_cancelled_cx();
    let _cx = cancelled_cx(CancelKind::Shutdown, "shutting down");
}

#[test]
fn healthy_cx_works() {
    let cx = healthy_cx();
    cx.checkpoint().expect("checkpoint should succeed");
}

// ---------------------------------------------------------------------------
// RuntimeFixture tests
// ---------------------------------------------------------------------------

#[test]
fn runtime_fixture_current_thread() {
    let rt = RuntimeFixture::current_thread();
    let result = rt.block_on(async { 42 });
    assert_eq!(result, 42);
}

#[test]
fn runtime_fixture_with_cx() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let cx = Cx::for_testing();
        let scope = cx.scope();
        assert_eq!(scope.region_id(), cx.region_id());
    });
}
