//! LabRuntime-ported telemetry tests for deterministic async testing.
//!
//! Bead: ft-22x4r
//!
//! Converts `#[tokio::test]` async tests from `telemetry.rs` into
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { … })` form.
//! Feature-gated behind `asupersync-runtime`.

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;

use frankenterm_core::runtime_compat;
use frankenterm_core::telemetry::{TelemetryCollector, TelemetryConfig};

use std::sync::Arc;
use std::time::Duration;

// ===========================================================================
// 1. collector_run_and_shutdown — spawns the collector loop, sleeps, then
//    shuts it down and verifies at least one sample was collected.
// ===========================================================================

#[test]
fn collector_run_and_shutdown() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let collector = Arc::new(TelemetryCollector::new(TelemetryConfig {
            sample_interval: Duration::from_millis(50),
            mux_server_pid: 0,
            ..Default::default()
        }));

        let c = Arc::clone(&collector);
        let handle = runtime_compat::task::spawn(async move {
            c.run().await;
        });

        // Let it collect a few samples (macOS subprocess sampling is slow)
        runtime_compat::sleep(Duration::from_millis(500)).await;
        collector.shutdown();
        handle.await.unwrap();

        // Should have collected at least 1 sample (first tick is immediate)
        assert!(
            collector.sample_count() >= 1,
            "sample_count={}, expected >= 1",
            collector.sample_count()
        );
    });
}
