//! LabRuntime port of all `#[tokio::test]` async tests from `restore_scrollback.rs`.
//!
//! Feature-gated behind `asupersync-runtime`.
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::fixtures::RuntimeFixture;
use frankenterm_core::restore_scrollback::{
    InjectionConfig, InjectionGuard, ScrollbackData, ScrollbackInjector,
};
use frankenterm_core::wezterm::{MockWezterm, WeztermInterface};

fn make_injector(mock: Arc<MockWezterm>) -> ScrollbackInjector {
    ScrollbackInjector::new(mock, InjectionConfig::default())
}

fn mock_scrollback(lines: Vec<&str>) -> ScrollbackData {
    ScrollbackData::from_segments(lines.into_iter().map(String::from).collect())
}

// ===========================================================================
// 1. inject_single_pane
// ===========================================================================

#[test]
fn inject_single_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector(mock.clone());

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["line1", "line2", "line3"]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);
        assert_eq!(report.failure_count(), 0);
        assert_eq!(report.successes[0].lines_injected, 3);
        assert!(report.successes[0].bytes_written > 0);

        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert!(text.contains("line1"));
        assert!(text.contains("line3"));
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
        let injector = make_injector(mock.clone());

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);
        pane_id_map.insert(2_u64, 11_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["pane1-output"]));
        scrollbacks.insert(2, mock_scrollback(vec!["pane2-output"]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 2);
        assert_eq!(report.failure_count(), 0);
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
        let injector = make_injector(mock.clone());

        let pane_id_map = HashMap::new();

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["data"]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 0);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0], 1);
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
        let injector = make_injector(mock.clone());

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, ScrollbackData::from_segments(vec![]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);
        assert_eq!(report.successes[0].lines_injected, 0);
        assert_eq!(report.successes[0].bytes_written, 0);
    });
}

// ===========================================================================
// 5. inject_truncates_large_scrollback
// ===========================================================================

#[test]
fn inject_truncates_large_scrollback() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let config = InjectionConfig {
            max_lines: 3,
            ..Default::default()
        };
        let injector = ScrollbackInjector::new(mock.clone(), config);

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let lines: Vec<String> = (0..100).map(|i| format!("line-{i}")).collect();
        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, ScrollbackData::from_segments(lines));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);
        assert_eq!(report.successes[0].lines_injected, 3);

        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert!(text.contains("line-99"));
        assert!(text.contains("line-97"));
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
        let injector = make_injector(mock.clone());

        let pane_id_map = HashMap::new();
        let scrollbacks = HashMap::new();

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 0);
        assert_eq!(report.failure_count(), 0);
        assert_eq!(report.skipped.len(), 0);
    });
}

// ===========================================================================
// 7. injection_guard_active_during_inject
// ===========================================================================

#[test]
fn injection_guard_active_during_inject() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector(mock.clone());
        let suppressed = injector.suppressed_panes().clone();

        assert!(!InjectionGuard::is_suppressed(&suppressed, 10));

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["test"]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);
        assert!(!InjectionGuard::is_suppressed(&suppressed, 10));
    });
}

// ===========================================================================
// 8. inject_with_small_chunks
// ===========================================================================

#[test]
fn inject_with_small_chunks() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let config = InjectionConfig {
            chunk_size: 16,
            inter_chunk_delay_ms: 0,
            ..Default::default()
        };
        let injector = ScrollbackInjector::new(mock.clone(), config);

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(
            1,
            mock_scrollback(vec![
                "this is a longer line that will require multiple chunks",
            ]),
        );

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);
        assert!(report.successes[0].chunks_sent > 1);

        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert!(text.contains("multiple chunks"));
    });
}
