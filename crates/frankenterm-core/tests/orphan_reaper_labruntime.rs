//! LabRuntime-ported orphan reaper tests for deterministic async testing.
//!
//! Bead: ft-22x4r
//!
//! Each `#[tokio::test]` from `orphan_reaper.rs` is converted to
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { ... })`,
//! feature-gated behind `asupersync-runtime`.
//!
//! Plain `#[test]` functions (parse_etime, split_first_token, ReapReport
//! unit tests, etc.) are NOT ported here — they do not require a runtime.

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;

use frankenterm_core::config::CliConfig;
use frankenterm_core::orphan_reaper::{ReapReport, reap_orphans, run_orphan_reaper};
use frankenterm_core::runtime_compat;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// reap_orphans (async wrapper around spawn_blocking)
// ---------------------------------------------------------------------------

#[test]
fn reap_orphans_async_returns_report() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let report: ReapReport = reap_orphans(999_999).await;
        assert!(report.killed_pids.len() == report.killed);
    });
}

#[test]
fn reap_orphans_async_zero_max_age() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let report: ReapReport = reap_orphans(0).await;
        assert!(report.killed_pids.len() == report.killed);
    });
}

// ---------------------------------------------------------------------------
// run_orphan_reaper — disabled (interval = 0)
// ---------------------------------------------------------------------------

#[test]
fn run_orphan_reaper_disabled_returns_immediately() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut config = CliConfig::default();
        config.orphan_reap_interval_seconds = 0; // disabled
        let shutdown = Arc::new(AtomicBool::new(false));

        // Should return immediately when interval is 0
        let handle = runtime_compat::task::spawn(run_orphan_reaper(config, shutdown));
        let result = runtime_compat::timeout(Duration::from_millis(100), handle).await;
        assert!(result.is_ok(), "disabled reaper should return immediately");
    });
}

// ---------------------------------------------------------------------------
// run_orphan_reaper — shutdown signal
// ---------------------------------------------------------------------------

#[test]
fn run_orphan_reaper_responds_to_shutdown() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut config = CliConfig::default();
        config.orphan_reap_interval_seconds = 1; // 1 second interval
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = runtime_compat::task::spawn(run_orphan_reaper(config, shutdown_clone));

        // Signal shutdown after a short delay
        runtime_compat::sleep(Duration::from_millis(50)).await;
        shutdown.store(true, Ordering::Relaxed);

        // Should exit within a reasonable time (after current sleep)
        let result = runtime_compat::timeout(Duration::from_secs(3), handle).await;
        assert!(result.is_ok(), "reaper should respond to shutdown signal");
    });
}
