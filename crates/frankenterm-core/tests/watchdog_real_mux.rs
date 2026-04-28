//! No-mocks watchdog tests built on the `WeztermSubprocessFixture`
//! (ft-dvgzi). Mirrors the per-test scenarios from
//! `tests/watchdog_labruntime.rs` against a real wezterm-mux-server
//! subprocess instead of MockWezterm.
//!
//! Bead: ft-2funa.
//!
//! Gated on `FT_REAL_WEZTERM_TESTS=1`. Default `cargo test` runs skip
//! cleanly when the wezterm-mux-server binary is absent, so this file
//! does NOT replace `watchdog_labruntime.rs` — it runs alongside.
//!
//! ## Coverage
//! - mux_watchdog_records_successful_check_real_mux: real list_panes
//!   on healthy fixture → ping_ok=true, status=Healthy
//! - mux_watchdog_history_bounded_real_mux: 5 successful checks,
//!   history capacity = 3
//! - mux_watchdog_report_reflects_latest_check_real_mux: latest_sample
//!   populates after first check
//! - mux_watchdog_detects_failure_real_mux: kill_mux mid-flight →
//!   real fault injection — strict-socket guard returns CommandFailed,
//!   watchdog records consecutive_failures
//! - mux_watchdog_total_failures_accumulate_real_mux: kill_mux + 3
//!   checks → total_failures=3
//!
//! Tests 7 & 8 from watchdog_labruntime.rs (set_watchdog_warnings /
//! set_watchdog_warning_error) are NOT migrated: those depend on the
//! mock's external warning-injection channel that has no real-mux
//! equivalent. Real WeztermClient derives `watchdog_warnings` from
//! its circuit-breaker state (wezterm.rs:2950), which the test
//! cannot drive externally without going through real failures.

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use common::wezterm_subprocess::{WeztermSubprocessFixture, should_run};
use frankenterm_core::watchdog::{HealthStatus, MuxWatchdog, MuxWatchdogConfig};

/// Emit a structured JSON-line trace per the no-mocks skill.
fn log(test: &str, phase: &str, body: serde_json::Value) {
    let line = serde_json::json!({
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "suite": "watchdog_real_mux",
        "test": test,
        "phase": phase,
        "data": body,
    });
    eprintln!("{line}");
}

// ── 1. records successful check on healthy fixture ──────────────────────────

#[test]
fn mux_watchdog_records_successful_check_real_mux() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    log("succ_check", "spawn", serde_json::json!({"pid": fixture.pid()}));
    let handle = fixture.handle();

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async move {
        let mut watchdog = MuxWatchdog::new(MuxWatchdogConfig::default(), handle);
        let sample = watchdog.check().await;
        log(
            "succ_check",
            "checked",
            serde_json::json!({
                "ping_ok": sample.ping_ok,
                "status": format!("{:?}", sample.status),
                "warning_count": sample.warning_count,
            }),
        );
        assert!(sample.ping_ok, "real fixture should ping_ok");
        assert_eq!(sample.status, HealthStatus::Healthy);
        // The real WeztermClient.watchdog_warnings derives from circuit-
        // breaker state (wezterm.rs:2950). On a healthy fixture with no
        // prior failures the circuit is Closed → empty warnings.
        assert_eq!(sample.warning_count, 0);
        assert!(sample.watchdog_warnings.is_empty());

        let report = watchdog.report();
        assert_eq!(report.consecutive_failures, 0);
        assert_eq!(report.total_checks, 1);
    });
}

// ── 2. history bounded ──────────────────────────────────────────────────────

#[test]
fn mux_watchdog_history_bounded_real_mux() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    let handle = fixture.handle();
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async move {
        let config = MuxWatchdogConfig {
            history_capacity: 3,
            ..MuxWatchdogConfig::default()
        };
        let mut watchdog = MuxWatchdog::new(config, handle);
        for _ in 0..5 {
            watchdog.check().await;
        }
        let report = watchdog.report();
        log(
            "history",
            "after_5_checks",
            serde_json::json!({
                "total_checks": report.total_checks,
                "consecutive_failures": report.consecutive_failures,
            }),
        );
        // MuxHealthReport doesn't expose internal history length; the
        // history bound is enforced internally and tested in the
        // labruntime suite via private-field access. Here we just
        // confirm total_checks ticks through history-capacity boundaries
        // without panic and that the latest sample is still recorded.
        assert_eq!(report.total_checks, 5);
        assert!(report.latest_sample.is_some());
    });
}

// ── 3. report reflects latest check ─────────────────────────────────────────

#[test]
fn mux_watchdog_report_reflects_latest_check_real_mux() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    let handle = fixture.handle();
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async move {
        let mut watchdog = MuxWatchdog::new(MuxWatchdogConfig::default(), handle);
        assert!(watchdog.report().latest_sample.is_none());
        watchdog.check().await;
        let report = watchdog.report();
        assert!(report.latest_sample.is_some());
        assert_eq!(report.total_checks, 1);
    });
}

// ── 4. detects failure via real fault injection (kill_mux) ──────────────────

#[test]
fn mux_watchdog_detects_failure_real_mux() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let mut fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    let handle = fixture.handle();
    log("detect_fail", "before_kill", serde_json::json!({"pid": fixture.pid()}));

    // FAULT INJECTION: kill the mux subprocess. The strict-socket
    // guard from ft-dvgzi.1.1 ensures the wezterm CLI does NOT fall
    // back to the user's interactive mux — it returns a real
    // connection failure that the watchdog must register as ping_ok=false.
    fixture.kill_mux();
    log("detect_fail", "killed", serde_json::json!({}));

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async move {
        let config = MuxWatchdogConfig {
            failure_threshold: 2,
            ..MuxWatchdogConfig::default()
        };
        let mut watchdog = MuxWatchdog::new(config, handle);

        // First failure: degraded
        let sample = watchdog.check().await;
        log(
            "detect_fail",
            "first_check",
            serde_json::json!({
                "ping_ok": sample.ping_ok,
                "status": format!("{:?}", sample.status),
            }),
        );
        assert!(!sample.ping_ok, "killed mux must produce ping_ok=false");
        assert_eq!(sample.status, HealthStatus::Degraded);
        assert_eq!(watchdog.report().consecutive_failures, 1);

        // Second failure: critical (meets threshold=2)
        let sample = watchdog.check().await;
        assert_eq!(sample.status, HealthStatus::Critical);
        assert_eq!(watchdog.report().consecutive_failures, 2);
    });
}

// ── 5. total_failures accumulate across N checks (real fault) ───────────────

#[test]
fn mux_watchdog_total_failures_accumulate_real_mux() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let mut fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    let handle = fixture.handle();
    fixture.kill_mux();

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async move {
        let config = MuxWatchdogConfig {
            failure_threshold: 10,
            ..MuxWatchdogConfig::default()
        };
        let mut watchdog = MuxWatchdog::new(config, handle);
        for _ in 0..3 {
            watchdog.check().await;
        }
        let report = watchdog.report();
        log(
            "total_fail",
            "after_3_checks",
            serde_json::json!({
                "total_failures": report.total_failures,
                "total_checks": report.total_checks,
                "consecutive_failures": report.consecutive_failures,
            }),
        );
        assert_eq!(report.total_failures, 3);
        assert_eq!(report.total_checks, 3);
        assert_eq!(report.consecutive_failures, 3);
    });
}
