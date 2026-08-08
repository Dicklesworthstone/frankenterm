//! LabRuntime-ported orphan reaper tests for deterministic async testing.
//!
//! Bead: ft-22x4r
//!
//! Each `#[tokio::test]` from `orphan_reaper.rs` is converted to
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { ... })`,
//! feature-gated behind `asupersync-runtime`.
//!
//! The production surface is deliberately inert until cleanup can be backed by
//! an owned child-handle registry. These tests prove that both zero and non-zero
//! legacy configuration remain process-table- and signal-free.

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;

use frankenterm_core::config::CliConfig;
use frankenterm_core::orphan_reaper::{ReapReport, reap_orphans, run_orphan_reaper};
use frankenterm_core::runtime_async;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// reap_orphans fail-closed report
// ---------------------------------------------------------------------------

#[test]
fn reap_orphans_async_returns_report() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let report: ReapReport = reap_orphans(999_999).await;
        assert_eq!(report.scanned, 0);
        assert_eq!(report.killed, 0);
        assert!(report.killed_pids.is_empty());
        assert_eq!(
            report.errors,
            vec!["reap disabled: no handle-owned child identity".to_string()]
        );
    });
}

#[test]
fn reap_orphans_async_zero_max_age() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let report: ReapReport = reap_orphans(0).await;
        assert_eq!(report.scanned, 0);
        assert_eq!(report.killed, 0);
        assert!(report.killed_pids.is_empty());
    });
}

// ---------------------------------------------------------------------------
// run_orphan_reaper — disabled (interval = 0)
// ---------------------------------------------------------------------------

#[test]
fn run_orphan_reaper_disabled_returns_immediately() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = CliConfig {
            orphan_reap_interval_seconds: 0, // disabled
            ..CliConfig::default()
        };
        let shutdown = Arc::new(AtomicBool::new(false));

        // Should return immediately when interval is 0
        let handle = runtime_async::task::spawn(run_orphan_reaper(config, shutdown));
        let result = runtime_async::timeout(Duration::from_millis(100), handle).await;
        assert!(result.is_ok(), "disabled reaper should return immediately");
    });
}

// ---------------------------------------------------------------------------
// run_orphan_reaper — non-zero legacy setting remains inert
// ---------------------------------------------------------------------------

#[test]
fn run_orphan_reaper_nonzero_setting_returns_without_waiting() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = CliConfig {
            orphan_reap_interval_seconds: 1, // 1 second interval
            ..CliConfig::default()
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let handle = runtime_async::task::spawn(run_orphan_reaper(config, shutdown_clone));
        let result = runtime_async::timeout(Duration::from_millis(100), handle).await;
        assert!(
            result.is_ok(),
            "inert cleanup surface should return immediately"
        );
        assert!(!shutdown.load(Ordering::Relaxed));
    });
}
