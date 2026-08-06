//! LabRuntime port of all `#[tokio::test]` async tests from `restore_scrollback.rs`.
//!
//! Feature-gated behind `asupersync-runtime`.
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::fixtures::RuntimeFixture;
use frankenterm_core::restore_scrollback::{InjectionGuard, ScrollbackData, ScrollbackInjector};
use frankenterm_core::wezterm::{MockWezterm, WeztermInterface};

fn make_injector() -> ScrollbackInjector {
    ScrollbackInjector::new()
}

fn mock_scrollback(lines: Vec<&str>) -> ScrollbackData {
    ScrollbackData::from_terminal_lines(lines.into_iter().map(String::from).collect())
}

// ===========================================================================
// 1. inject_single_pane
// ===========================================================================

#[test]
fn inject_single_pane_fails_closed() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector();

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["line1", "line2", "line3"]));

        injector
            .inject(&pane_id_map, &scrollbacks)
            .await
            .expect_err("mapped replay must report the unsupported safe-output channel");

        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert!(text.is_empty());
    });
}

// ===========================================================================
// 2. inject_multiple_panes
// ===========================================================================

#[test]
fn inject_multiple_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        mock.add_default_pane(11).await;
        let injector = make_injector();

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);
        pane_id_map.insert(2_u64, 11_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["pane1-output"]));
        scrollbacks.insert(2, mock_scrollback(vec!["pane2-output"]));

        injector
            .inject(&pane_id_map, &scrollbacks)
            .await
            .expect_err("mapped replay must report the unsupported safe-output channel");
    });
}

// ===========================================================================
// 3. inject_skips_unmapped_panes
// ===========================================================================

#[test]
fn inject_skips_unmapped_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let injector = make_injector();

        let pane_id_map = HashMap::new();

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["data"]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await.unwrap();

        assert_eq!(report.success_count(), 0);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.skipped_sample(), &[1]);
    });
}

// ===========================================================================
// 4. inject_empty_scrollback
// ===========================================================================

#[test]
fn inject_empty_scrollback() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector();

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, ScrollbackData::from_terminal_lines(vec![]));

        injector
            .inject(&pane_id_map, &scrollbacks)
            .await
            .expect_err("mapped empty replay still requires a safe output channel");
    });
}

// ===========================================================================
// 5. large mapped scrollback fails before replay allocation or output
// ===========================================================================

#[test]
fn inject_large_scrollback_does_not_write() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = ScrollbackInjector::new();

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let lines: Vec<String> = (0..100).map(|i| format!("line-{i}")).collect();
        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, ScrollbackData::from_terminal_lines(lines));

        injector
            .inject(&pane_id_map, &scrollbacks)
            .await
            .expect_err("large mapped replay must fail before allocating replay content");

        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert!(text.is_empty());
    });
}

// ===========================================================================
// 6. inject_no_scrollbacks
// ===========================================================================

#[test]
fn inject_no_scrollbacks() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let injector = make_injector();

        let pane_id_map = HashMap::new();
        let scrollbacks = HashMap::new();

        let report = injector.inject(&pane_id_map, &scrollbacks).await.unwrap();

        assert_eq!(report.success_count(), 0);
        assert_eq!(report.failure_count(), 0);
        assert_eq!(report.skipped_count(), 0);
        assert!(report.skipped_sample().is_empty());
    });
}

// ===========================================================================
// 7. injection_guard_active_during_inject
// ===========================================================================

#[test]
fn unsupported_injection_does_not_change_suppression_state() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector();
        let suppressed = injector.suppressed_panes().clone();

        assert!(!InjectionGuard::is_suppressed(&suppressed, 10));

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["test"]));

        injector
            .inject(&pane_id_map, &scrollbacks)
            .await
            .expect_err("mapped replay must fail closed");
        assert!(!InjectionGuard::is_suppressed(&suppressed, 10));
    });
}
