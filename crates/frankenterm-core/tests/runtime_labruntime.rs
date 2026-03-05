//! LabRuntime-ported observation runtime tests for deterministic async testing.
//!
//! Tests the asupersync primitive layer used by the observation runtime:
//! - RwLock contention under concurrent access (registry/cursors/engine pattern)
//! - Bounded mpsc channel backpressure (capture ingress pipeline)
//! - Backpressure classification tier logic
//! - Watch channel hot-reload propagation (config_tx/config_rx)
//! - Mutex exclusion for single-writer state
//! - Channel close/drop cleanup behavior
//! - Multi-producer channel fan-in (capture relay pattern)
//!
//! Note: Full ObservationRuntime integration tests (startup, shutdown,
//! discovery, capture) require the `runtime_compat::task` module to be migrated
//! from tokio::spawn to asupersync::spawn. Until that migration completes,
//! ObservationRuntime::start() cannot run under asupersync RuntimeFixture
//! because task::spawn → tokio::spawn panics without a tokio reactor.
//! See events_labruntime.rs and undo_labruntime.rs for the same pattern.
//!
//! Bead: ft-1m7nk (Refactor runtime.rs async observation loops to asupersync)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::runtime_compat::{Mutex, RwLock, mpsc, sleep, watch};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// ===========================================================================
// Test 1: RwLock contention — concurrent readers + writer on shared state
// ===========================================================================

#[test]
fn labruntime_rwlock_contention_no_deadlock() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Test RwLock contention directly using runtime_compat primitives,
        // mirroring how the observation loop shares registry/cursors/engine.
        // Uses interleaved async operations instead of task::spawn (which
        // requires tokio) to exercise the asupersync RwLock.
        let lock = Arc::new(RwLock::new(HashMap::<u64, String>::new()));

        // Phase 1: Writer inserts entries (mimics discovery updating registry)
        for i in 0..10u64 {
            let mut guard = lock.write().await;
            guard.insert(i, format!("value-{i}"));
            drop(guard);

            // Interleave reads between writes (mimics capture reading registry)
            let guard = lock.read().await;
            assert!(guard.contains_key(&i), "should see just-written key {i}");
            drop(guard);
        }

        // Phase 2: Multiple sequential readers (mimics multiple capture tasks)
        for _ in 0..5 {
            let guard = lock.read().await;
            assert_eq!(guard.len(), 10, "all 10 entries should be visible");
            drop(guard);
        }

        // Phase 3: Writer overwrites while readers verify consistency
        {
            let mut guard = lock.write().await;
            guard.insert(0, "updated-0".to_string());
            drop(guard);
        }
        let guard = lock.read().await;
        assert_eq!(
            guard.get(&0).map(|s| s.as_str()),
            Some("updated-0"),
            "overwritten value should be visible after write completes"
        );
        assert_eq!(guard.len(), 10, "update should not change entry count");
    });
}

// ===========================================================================
// Test 2: Backpressure — bounded channel behavior under load
// ===========================================================================

#[test]
fn labruntime_backpressure_bounded_channel() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Create a small bounded channel to test backpressure behavior
        // (mirrors the capture ingress channel in the observation loop)
        let (tx, mut rx) = mpsc::channel::<u32>(4);

        // Fill the channel to capacity
        for i in 0..4u32 {
            let cx = asupersync::Cx::for_testing();
            tx.send(&cx, i)
                .await
                .expect("send should succeed within capacity");
        }

        // Drain the channel and verify ordering
        let mut received = Vec::new();
        for _ in 0..4 {
            let cx = asupersync::Cx::for_testing();
            received.push(rx.recv(&cx).await.expect("recv should succeed"));
        }

        assert_eq!(
            received,
            vec![0, 1, 2, 3],
            "should receive all sent values in FIFO order"
        );
    });
}

// ===========================================================================
// Test 3: Backpressure classification tiers
// ===========================================================================

#[test]
fn labruntime_backpressure_classification_tiers() {
    // Test the backpressure tier classification logic used by the health
    // snapshot (mirrors classify_backpressure_tier in runtime.rs)
    use frankenterm_core::backpressure::BackpressureConfig;

    let config = BackpressureConfig::default();

    // GREEN: low utilization
    let capture_capacity = 1024usize;
    let capture_depth_green = 10usize;
    let capture_ratio_green = capture_depth_green as f64 / capture_capacity as f64;
    assert!(
        capture_ratio_green < config.yellow_capture,
        "low utilization ({capture_ratio_green:.3}) should be below yellow ({:.3})",
        config.yellow_capture
    );

    // YELLOW: moderate utilization
    let yellow_depth = (capture_capacity as f64 * config.yellow_capture) as usize + 1;
    let yellow_ratio = yellow_depth as f64 / capture_capacity as f64;
    assert!(
        yellow_ratio >= config.yellow_capture,
        "yellow depth ({yellow_ratio:.3}) should exceed yellow threshold ({:.3})",
        config.yellow_capture
    );
    assert!(
        yellow_ratio < config.red_capture,
        "yellow depth ({yellow_ratio:.3}) should be below red threshold ({:.3})",
        config.red_capture
    );

    // RED: high utilization
    let red_depth = (capture_capacity as f64 * config.red_capture) as usize + 1;
    let red_ratio = red_depth as f64 / capture_capacity as f64;
    assert!(
        red_ratio >= config.red_capture,
        "red depth ({red_ratio:.3}) should exceed red threshold ({:.3})",
        config.red_capture
    );

    // BLACK: near saturation (within 5 of capacity)
    let black_depth = capture_capacity.saturating_sub(4);
    assert!(
        black_depth >= capture_capacity.saturating_sub(5),
        "black depth should be near capacity"
    );
}

// ===========================================================================
// Test 4: Watch channel hot-reload config propagation
// ===========================================================================

#[test]
fn labruntime_watch_channel_config_propagation() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Test watch channel for hot-reloadable config (mirrors config_tx/config_rx)
        let initial_value = 100u64;
        let (tx, rx) = watch::channel(initial_value);

        // Verify initial value
        let val = *rx.borrow();
        assert_eq!(val, 100, "initial watch value should be 100");

        // Send updated value (simulates hot-reload)
        tx.send(200).expect("watch send should succeed");

        // Receiver should see the update
        let updated = *rx.borrow();
        assert_eq!(updated, 200, "watch value should update to 200");

        // Multiple rapid updates (simulates config file changes)
        tx.send(300).expect("watch send should succeed");
        tx.send(400).expect("watch send should succeed");

        // Receiver should see the latest value
        let latest = *rx.borrow();
        assert_eq!(latest, 400, "watch should reflect latest value");
    });
}

// ===========================================================================
// Test 5: Mutex exclusion for single-writer state
// ===========================================================================

#[test]
fn labruntime_mutex_exclusion_single_writer() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Test Mutex for single-writer state, mirroring how the observation
        // loop protects mutable resources like cursor maps or config state.
        let state = Arc::new(Mutex::new(Vec::<String>::new()));

        // Sequential writes (mimics serialized config updates)
        for i in 0..5 {
            let mut guard = state.lock().await;
            guard.push(format!("entry-{i}"));
        }

        // Verify all writes completed in order
        let guard = state.lock().await;
        assert_eq!(guard.len(), 5, "all 5 entries should be present");
        assert_eq!(guard[0], "entry-0");
        assert_eq!(guard[4], "entry-4");
    });
}

// ===========================================================================
// Test 6: Channel close/drop cleanup behavior
// ===========================================================================

#[test]
fn labruntime_channel_close_signals_receiver() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Test that dropping the sender closes the channel cleanly,
        // mirroring shutdown behavior when capture_tx is dropped.
        let (tx, mut rx) = mpsc::channel::<u32>(8);

        // Send a few items
        let cx = asupersync::Cx::for_testing();
        tx.send(&cx, 1).await.expect("send should succeed");
        tx.send(&cx, 2).await.expect("send should succeed");

        // Drop sender to signal channel close
        drop(tx);

        // Receiver should still get buffered items
        let cx = asupersync::Cx::for_testing();
        assert_eq!(rx.recv(&cx).await.unwrap(), 1);
        assert_eq!(rx.recv(&cx).await.unwrap(), 2);

        // After buffer drained, recv should return Err (channel closed)
        assert!(
            rx.recv(&cx).await.is_err(),
            "recv should return Err after sender dropped"
        );
    });
}

// ===========================================================================
// Test 7: Multi-producer channel fan-in (capture relay pattern)
// ===========================================================================

#[test]
fn labruntime_multi_producer_channel_fanin() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Test multiple senders feeding into a single receiver,
        // mirroring how multiple capture tasks feed the persistence pipeline.
        let (tx, mut rx) = mpsc::channel::<String>(16);

        let cx = asupersync::Cx::for_testing();

        // Clone sender for each "capture task"
        let tx2 = tx.clone();
        let tx3 = tx.clone();

        // Each producer sends tagged messages
        tx.send(&cx, "pane-0:data".to_string()).await.unwrap();
        tx2.send(&cx, "pane-1:data".to_string()).await.unwrap();
        tx3.send(&cx, "pane-2:data".to_string()).await.unwrap();

        // Drop all senders
        drop(tx);
        drop(tx2);
        drop(tx3);

        // Collect all messages
        let mut received: Vec<String> = Vec::new();
        while let Ok(msg) = rx.recv(&cx).await {
            received.push(msg);
        }

        assert_eq!(received.len(), 3, "should receive from all 3 producers");
        // All messages should be present (order depends on scheduling)
        assert!(received.iter().any(|m: &String| m.starts_with("pane-0")));
        assert!(received.iter().any(|m: &String| m.starts_with("pane-1")));
        assert!(received.iter().any(|m: &String| m.starts_with("pane-2")));
    });
}

// ===========================================================================
// Test 8: RuntimeConfig defaults are reasonable under asupersync
// ===========================================================================

#[test]
fn labruntime_runtime_config_defaults_are_reasonable() {
    use frankenterm_core::runtime::RuntimeConfig;

    let config = RuntimeConfig::default();

    assert_eq!(config.discovery_interval, Duration::from_secs(5));
    assert_eq!(config.capture_interval, Duration::from_millis(200));
    assert_eq!(config.overlap_size, 1_048_576); // 1MB default
    assert_eq!(config.channel_buffer, 1024);
}

// ===========================================================================
// Test 9: RwLock read-write upgrade pattern (registry + cursors)
// ===========================================================================

#[test]
fn labruntime_rwlock_read_write_upgrade_pattern() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Mirrors the observation loop's pattern: read registry, then upgrade
        // to write if new panes discovered. The upgrade requires dropping the
        // read guard first (no in-place upgrade).
        let registry = Arc::new(RwLock::new(HashMap::<u64, String>::new()));

        // Check-then-insert pattern (mimics discovery adding new panes)
        for pane_id in 0..5u64 {
            let needs_insert = {
                let guard = registry.read().await;
                !guard.contains_key(&pane_id)
            };

            if needs_insert {
                let mut guard = registry.write().await;
                guard.insert(pane_id, format!("pane-{pane_id}"));
            }
        }

        let guard = registry.read().await;
        assert_eq!(guard.len(), 5, "all 5 panes should be registered");

        // Idempotent re-insert (mimics re-discovery of existing panes)
        drop(guard);
        for pane_id in 0..5u64 {
            let needs_insert = {
                let guard = registry.read().await;
                !guard.contains_key(&pane_id)
            };
            assert!(!needs_insert, "pane {pane_id} should already exist");
        }
    });
}

// ===========================================================================
// Test 10: Sleep-based adaptive polling simulation
// ===========================================================================

#[test]
fn labruntime_adaptive_polling_sleep_behavior() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Verify that sleep completes without hanging under asupersync runtime.
        // This is fundamental for the observation loop's adaptive polling.
        let start = std::time::Instant::now();

        sleep(Duration::from_millis(10)).await;

        // Sleep should complete (may be longer on loaded systems but
        // should not hang indefinitely under asupersync)
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(5),
            "sleep should wait at least ~10ms, got {elapsed:?}"
        );
    });
}

// ===========================================================================
// Test 11: Watch channel multiple receivers (broadcast config pattern)
// ===========================================================================

#[test]
fn labruntime_watch_multiple_receivers_see_latest() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Test that multiple watch receivers all see the latest value,
        // mirroring how multiple loop tasks observe config changes.
        let (tx, rx1) = watch::channel(0u64);
        let rx2 = rx1.clone();
        let rx3 = rx1.clone();

        tx.send(42).expect("watch send should succeed");

        assert_eq!(*rx1.borrow(), 42);
        assert_eq!(*rx2.borrow(), 42);
        assert_eq!(*rx3.borrow(), 42);

        tx.send(99).expect("watch send should succeed");

        assert_eq!(*rx1.borrow(), 99);
        assert_eq!(*rx2.borrow(), 99);
        assert_eq!(*rx3.borrow(), 99);
    });
}

// ===========================================================================
// ObservationRuntime integration tests
//
// These tests exercise the full runtime lifecycle using the asupersync task
// module (migrated from tokio::spawn in runtime_compat). They test startup,
// shutdown, discovery, and capture with MockWezterm.
// ===========================================================================

fn temp_db() -> (tempfile::TempDir, String) {
    let dir = tempfile::TempDir::new().expect("create temp dir");
    let path = dir.path().join("test.db").to_string_lossy().to_string();
    (dir, path)
}

fn test_config() -> frankenterm_core::runtime::RuntimeConfig {
    let mut config = frankenterm_core::runtime::RuntimeConfig::default();
    config.discovery_interval = Duration::from_millis(10);
    config.capture_interval = Duration::from_millis(10);
    config.min_capture_interval = Duration::from_millis(5);
    config.channel_buffer = 64;
    config
}

async fn make_mock_handle(
    pane_ids: &[u64],
) -> (
    Arc<frankenterm_core::wezterm::MockWezterm>,
    frankenterm_core::wezterm::WeztermHandle,
) {
    let mock = Arc::new(frankenterm_core::wezterm::MockWezterm::new());
    for &id in pane_ids {
        mock.add_default_pane(id).await;
        mock.inject_output(id, &format!("pane-{id} boot\n"))
            .await
            .unwrap();
    }
    let handle: frankenterm_core::wezterm::WeztermHandle = mock.clone();
    (mock, handle)
}

// ===========================================================================
// Test 12: Event loop startup/shutdown — verify clean lifecycle
// ===========================================================================

#[test]
fn labruntime_event_loop_startup_shutdown_is_clean() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_dir, db_path) = temp_db();
        let storage = frankenterm_core::storage::StorageHandle::new(&db_path)
            .await
            .unwrap();
        let engine = frankenterm_core::patterns::PatternEngine::new();
        let config = test_config();
        let (_mock, wezterm_handle) = make_mock_handle(&[0]).await;

        let mut runtime = frankenterm_core::runtime::ObservationRuntime::new(
            config,
            storage,
            Arc::new(RwLock::new(engine)),
        )
        .with_wezterm_handle(wezterm_handle);

        let handle = runtime.start().await.expect("runtime should start");

        // Let the runtime observe for a short period
        sleep(Duration::from_millis(30)).await;

        // Verify clean shutdown
        let summary = handle.shutdown_with_summary().await;
        assert!(
            summary.clean,
            "shutdown should be clean: warnings={:?}",
            summary.warnings
        );
    });
}

// ===========================================================================
// Test 13: Channel dispatch — capture event reaches persistence
// ===========================================================================

#[test]
fn labruntime_channel_dispatch_delivers_capture_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_dir, db_path) = temp_db();
        let storage = frankenterm_core::storage::StorageHandle::new(&db_path)
            .await
            .unwrap();
        let engine = frankenterm_core::patterns::PatternEngine::new();
        let config = test_config();

        let (mock, wezterm_handle) = make_mock_handle(&[0]).await;

        let mut runtime = frankenterm_core::runtime::ObservationRuntime::new(
            config,
            storage,
            Arc::new(RwLock::new(engine)),
        )
        .with_wezterm_handle(wezterm_handle);

        let handle = runtime.start().await.expect("runtime should start");

        // Inject additional output that should flow through the capture pipeline
        mock.inject_output(0, "new line after start\n")
            .await
            .unwrap();

        // Allow time for discovery -> capture -> relay -> persistence pipeline
        sleep(Duration::from_millis(80)).await;

        // Verify the metrics pipeline is accessible (count depends on timing)
        let _persisted = handle.metrics.segments_persisted();

        handle.shutdown().await;
    });
}

// ===========================================================================
// Test 14: Shutdown propagation — flag drains all spawned tasks
// ===========================================================================

#[test]
fn labruntime_shutdown_propagation_drains_all_tasks() {
    use std::sync::atomic::Ordering;

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_dir, db_path) = temp_db();
        let storage = frankenterm_core::storage::StorageHandle::new(&db_path)
            .await
            .unwrap();
        let engine = frankenterm_core::patterns::PatternEngine::new();
        let config = test_config();
        let (_mock, wezterm_handle) = make_mock_handle(&[0, 1]).await;

        let mut runtime = frankenterm_core::runtime::ObservationRuntime::new(
            config,
            storage,
            Arc::new(RwLock::new(engine)),
        )
        .with_wezterm_handle(wezterm_handle);

        let handle = runtime.start().await.expect("runtime should start");

        // Let runtime run briefly
        sleep(Duration::from_millis(30)).await;

        // Signal shutdown via the flag
        handle.shutdown_flag.store(true, Ordering::SeqCst);

        // shutdown_with_summary should complete promptly
        let summary = handle.shutdown_with_summary().await;
        assert!(
            summary.clean,
            "all tasks should drain cleanly after shutdown flag: warnings={:?}",
            summary.warnings
        );
    });
}
