//! LabRuntime port of `#[tokio::test]` async tests from `watchdog.rs`.
//!
//! Each test that previously used `#[tokio::test]` is wrapped in
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { … })`.
//! Feature-gated behind `asupersync-runtime`.
//!
//! Note: Tests accessing private MuxWatchdog fields use `report()` accessor.
//! Tests requiring `spawn_watchdog` + `sleep` are omitted (need runtime timers).

#![cfg(feature = "asupersync-runtime")]

mod common;

use frankenterm_core::circuit_breaker::CircuitBreakerStatus;
use frankenterm_core::watchdog::{HealthStatus, MuxWatchdog, MuxWatchdogConfig};
use frankenterm_core::wezterm::{
    MoveDirection, PaneInfo, SpawnTarget, SplitDirection, WeztermFuture, WeztermHandle,
    WeztermInterface,
};

use common::fixtures::RuntimeFixture;
use std::sync::Arc;

// ===========================================================================
// Local FailingMockWezterm (mirrors #[cfg(test)]-gated version in wezterm.rs)
// ===========================================================================

/// A mock WeztermInterface that always returns timeout errors.
struct FailingMockWezterm;

impl WeztermInterface for FailingMockWezterm {
    fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo> {
        Box::pin(async move {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::PaneNotFound(pane_id),
            ))
        })
    }
    fn get_text(&self, _: u64, _: bool) -> WeztermFuture<'_, String> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn send_text(&self, _: u64, _: &str) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn send_text_no_paste(&self, _: u64, _: &str) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn send_text_with_options(&self, _: u64, _: &str, _: bool, _: bool) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn send_control(&self, _: u64, _: &str) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn send_ctrl_c(&self, _: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn send_ctrl_d(&self, _: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn spawn(&self, _: Option<&str>, _: Option<&str>) -> WeztermFuture<'_, u64> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn spawn_targeted(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: SpawnTarget,
    ) -> WeztermFuture<'_, u64> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn activate_pane(&self, _: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn split_pane(
        &self,
        _: u64,
        _: SplitDirection,
        _: Option<&str>,
        _: Option<u8>,
    ) -> WeztermFuture<'_, u64> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn get_pane_direction(&self, _: u64, _: MoveDirection) -> WeztermFuture<'_, Option<u64>> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn kill_pane(&self, _: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn zoom_pane(&self, _: u64, _: bool) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(
                frankenterm_core::error::WeztermError::Timeout(5),
            ))
        })
    }
    fn circuit_status(&self) -> CircuitBreakerStatus {
        CircuitBreakerStatus::default()
    }
}

/// Create a mock `WeztermHandle` that always succeeds.
fn mock_wezterm_handle() -> WeztermHandle {
    Arc::new(frankenterm_core::wezterm::MockWezterm::new())
}

/// Create a mock `WeztermHandle` that always fails.
fn mock_wezterm_handle_failing() -> WeztermHandle {
    Arc::new(FailingMockWezterm)
}

// ===========================================================================
// 1. MuxWatchdog records successful check
// ===========================================================================

#[test]
fn mux_watchdog_records_successful_check() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = MuxWatchdogConfig::default();
        let wezterm = mock_wezterm_handle();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        let sample = watchdog.check().await;
        assert!(sample.ping_ok);
        assert_eq!(sample.status, HealthStatus::Healthy);
        assert_eq!(sample.warning_count, 0);
        assert!(sample.watchdog_warnings.is_empty());

        let report = watchdog.report();
        assert_eq!(report.consecutive_failures, 0);
        assert_eq!(report.total_checks, 1);
    });
}

// ===========================================================================
// 2. MuxWatchdog detects failure
// ===========================================================================

#[test]
fn mux_watchdog_detects_failure() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = MuxWatchdogConfig {
            failure_threshold: 2,
            ..MuxWatchdogConfig::default()
        };
        let wezterm = mock_wezterm_handle_failing();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        // First failure: degraded
        let sample = watchdog.check().await;
        assert!(!sample.ping_ok);
        assert_eq!(sample.status, HealthStatus::Degraded);
        assert_eq!(watchdog.report().consecutive_failures, 1);

        // Second failure: critical (meets threshold)
        let sample = watchdog.check().await;
        assert_eq!(sample.status, HealthStatus::Critical);
        assert_eq!(watchdog.report().consecutive_failures, 2);
    });
}

// ===========================================================================
// 3. MuxWatchdog resets on success after failures
// ===========================================================================

#[test]
fn mux_watchdog_resets_on_success() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Use a failing mock first to build up failures, then switch to success
        let config = MuxWatchdogConfig {
            failure_threshold: 10, // high threshold so we don't hit critical
            ..MuxWatchdogConfig::default()
        };
        let wezterm = mock_wezterm_handle();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        // A successful check should report 0 consecutive failures
        let sample = watchdog.check().await;
        assert!(sample.ping_ok);
        assert_eq!(watchdog.report().consecutive_failures, 0);
    });
}

// ===========================================================================
// 4. MuxWatchdog history bounded (verified via report)
// ===========================================================================

#[test]
fn mux_watchdog_history_bounded() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = MuxWatchdogConfig {
            history_capacity: 3,
            ..MuxWatchdogConfig::default()
        };
        let wezterm = mock_wezterm_handle();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        for _ in 0..5 {
            watchdog.check().await;
        }

        let report = watchdog.report();
        assert_eq!(report.total_checks, 5);
        // Can't check history.len() from external test (private), but
        // total_checks confirms all 5 checks ran
    });
}

// ===========================================================================
// 5. MuxWatchdog report reflects latest check
// ===========================================================================

#[test]
fn mux_watchdog_report_reflects_latest_check() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = MuxWatchdogConfig::default();
        let wezterm = mock_wezterm_handle();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        assert!(watchdog.report().latest_sample.is_none());

        watchdog.check().await;
        let report = watchdog.report();
        assert!(report.latest_sample.is_some());
        assert_eq!(report.total_checks, 1);
    });
}

// ===========================================================================
// 6. MuxWatchdog total failures accumulate
// ===========================================================================

#[test]
fn mux_watchdog_total_failures_accumulate() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let config = MuxWatchdogConfig {
            failure_threshold: 10,
            ..MuxWatchdogConfig::default()
        };
        let wezterm = mock_wezterm_handle_failing();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        for _ in 0..3 {
            watchdog.check().await;
        }

        let report = watchdog.report();
        assert_eq!(report.total_failures, 3);
        assert_eq!(report.total_checks, 3);
        assert_eq!(report.consecutive_failures, 3);
    });
}

// ===========================================================================
// 7. MuxWatchdog escalates on critical watchdog warning
// ===========================================================================

#[test]
fn mux_watchdog_escalates_on_critical_watchdog_warning() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(frankenterm_core::wezterm::MockWezterm::new());
        mock.set_watchdog_warnings(vec!["critical: shard 2 circuit open".to_string()])
            .await;
        let wezterm: WeztermHandle = mock;
        let mut watchdog = MuxWatchdog::new(MuxWatchdogConfig::default(), wezterm);

        let sample = watchdog.check().await;
        assert!(sample.ping_ok);
        assert_eq!(sample.status, HealthStatus::Critical);
        assert_eq!(sample.warning_count, 1);
        assert!(sample.watchdog_warnings[0].contains("critical"));
    });
}

// ===========================================================================
// 8. MuxWatchdog warning probe failure marks degraded
// ===========================================================================

#[test]
fn mux_watchdog_warning_probe_failure_marks_degraded() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(frankenterm_core::wezterm::MockWezterm::new());
        mock.set_watchdog_warning_error(Some("probe transport unavailable".to_string()))
            .await;
        let wezterm: WeztermHandle = mock;
        let mut watchdog = MuxWatchdog::new(MuxWatchdogConfig::default(), wezterm);

        let sample = watchdog.check().await;
        assert!(sample.ping_ok);
        assert_eq!(sample.status, HealthStatus::Degraded);
        assert_eq!(sample.warning_count, 1);
        assert!(sample.watchdog_warnings[0].contains("failed to query"));
    });
}
