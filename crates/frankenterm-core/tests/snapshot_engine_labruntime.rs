//! LabRuntime-ported snapshot_engine tests for deterministic async testing.
//!
//! Ports snapshot_engine.rs tests from `run_async_test()`/`#[tokio::test]` to
//! asupersync-based `RuntimeFixture`. The SnapshotEngine uses direct rusqlite
//! I/O (not StorageHandle), so it works under RuntimeFixture's block_on.
//!
//! Tests requiring private internals (compute_state_hash, generate_session_id,
//! trigger_value, is_immediate_trigger, open_conn) or `runtime_compat::task::spawn`
//! (intelligent_*, periodic_*, emit_trigger_*) are omitted — they remain as
//! unit tests in snapshot_engine.rs.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::config::SnapshotConfig;
use frankenterm_core::snapshot_engine::{SnapshotEngine, SnapshotError, SnapshotTrigger};
use frankenterm_core::storage::StorageHandle;
use frankenterm_core::wezterm::{PaneInfo, PaneSize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// ===========================================================================
// Helpers
// ===========================================================================

fn make_test_pane(id: u64, rows: u32, cols: u32) -> PaneInfo {
    PaneInfo {
        pane_id: id,
        tab_id: 0,
        window_id: 0,
        domain_id: None,
        domain_name: None,
        workspace: None,
        size: Some(PaneSize {
            rows,
            cols,
            pixel_width: None,
            pixel_height: None,
            dpi: None,
        }),
        rows: None,
        cols: None,
        title: Some(format!("pane-{id}")),
        cwd: Some(format!("file:///home/user/project-{id}")),
        tty_name: None,
        cursor_x: Some(5),
        cursor_y: Some(10),
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: id == 0,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

/// Creates a temp SQLite DB with the snapshot schema via StorageHandle.
/// StorageHandle::new() creates all tables including mux_sessions,
/// session_checkpoints, and mux_pane_state that SnapshotEngine expects.
/// Returns the temp dir (to keep it alive), the db path, and the storage handle.
async fn setup_test_db() -> (tempfile::TempDir, Arc<String>, Arc<StorageHandle>) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("snapshot-test.db");
    let db_path_str = db_path.to_string_lossy().to_string();
    let storage = StorageHandle::new(&db_path_str).await.expect("storage");
    (tmp, Arc::new(db_path_str), Arc::new(storage))
}

// ===========================================================================
// Section 1: Capture tests ported from run_async_test to RuntimeFixture
// ===========================================================================

#[test]
fn snapshot_capture_single_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        let result = engine.capture(&panes, SnapshotTrigger::Manual).await;
        assert!(result.is_ok(), "capture should succeed: {result:?}");
        let result = result.unwrap();
        assert_eq!(result.pane_count, 1);
        assert!(result.checkpoint_id > 0);
        assert!(result.session_id.starts_with("sess-"));
    });
}

#[test]
fn snapshot_capture_multiple_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![
            make_test_pane(1, 24, 80),
            make_test_pane(2, 24, 80),
            make_test_pane(3, 30, 120),
        ];

        let result = engine
            .capture(&panes, SnapshotTrigger::Startup)
            .await
            .unwrap();
        assert_eq!(result.pane_count, 3);
    });
}

#[test]
fn snapshot_agent_metadata_detected_from_title() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let mut pane = make_test_pane(1, 24, 80);
        pane.title = Some("claude-code".to_string());

        let result = engine
            .capture(&[pane], SnapshotTrigger::Manual)
            .await
            .unwrap();
        // Verify capture succeeds — agent metadata is persisted in DB
        // (DB verification requires rusqlite which is not a dev-dep;
        // the unit test in snapshot_engine.rs verifies the DB contents)
        assert_eq!(result.pane_count, 1);
    });
}

#[test]
fn snapshot_dedup_skips_unchanged_periodic() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        let r1 = engine.capture(&panes, SnapshotTrigger::Periodic).await;
        assert!(r1.is_ok());

        // Second periodic with same data should be skipped
        let r2 = engine.capture(&panes, SnapshotTrigger::Periodic).await;
        assert!(matches!(r2, Err(SnapshotError::NoChanges)));
    });
}

#[test]
fn snapshot_dedup_does_not_skip_manual() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        let r1 = engine.capture(&panes, SnapshotTrigger::Manual).await;
        assert!(r1.is_ok());

        // Manual should NOT be skipped even with same data
        let r2 = engine.capture(&panes, SnapshotTrigger::Manual).await;
        assert!(r2.is_ok());
    });
}

#[test]
fn snapshot_empty_panes_returns_error() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

        let result = engine.capture(&[], SnapshotTrigger::Manual).await;
        assert!(matches!(result, Err(SnapshotError::NoPanes)));
    });
}

#[test]
fn snapshot_session_reused_across_captures() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

        let panes1 = vec![make_test_pane(1, 24, 80)];
        let panes2 = vec![make_test_pane(1, 30, 120)]; // changed size

        let r1 = engine
            .capture(&panes1, SnapshotTrigger::Startup)
            .await
            .unwrap();
        let r2 = engine
            .capture(&panes2, SnapshotTrigger::Periodic)
            .await
            .unwrap();

        assert_eq!(r1.session_id, r2.session_id);
        assert_ne!(r1.checkpoint_id, r2.checkpoint_id);
    });
}

#[test]
fn snapshot_cleanup_removes_old_checkpoints() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let config = SnapshotConfig {
            retention_count: 2,
            retention_days: 365,
            ..SnapshotConfig::default()
        };
        let engine = SnapshotEngine::new(db_path.clone(), config);

        // Create 4 snapshots with different pane data
        for i in 0..4u64 {
            let panes = vec![make_test_pane(i, 24 + i as u32, 80)];
            engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .unwrap();
        }

        // Cleanup should remove 2 (keep latest 2)
        let deleted = engine.cleanup().await.unwrap();
        assert_eq!(deleted, 2);
    });
}

#[test]
fn snapshot_mark_shutdown_sets_flag() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        engine
            .capture(&panes, SnapshotTrigger::Startup)
            .await
            .unwrap();
        let result = engine.mark_shutdown().await;
        assert!(result.is_ok());
    });
}

// ===========================================================================
// Section 2: Async tests ported from #[tokio::test] to RuntimeFixture
// ===========================================================================

#[test]
fn snapshot_shutdown_checkpoint_with_no_session() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

        let result = engine.mark_shutdown().await;
        assert!(result.is_ok());
    });
}

#[test]
fn snapshot_shutdown_checkpoint_captures_and_marks() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        engine
            .capture(&panes, SnapshotTrigger::Startup)
            .await
            .unwrap();

        // Shutdown checkpoint with different panes to avoid NoChanges
        let panes2 = vec![make_test_pane(1, 30, 100)];
        let result = engine
            .shutdown_checkpoint(&panes2, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_some());

        let snap = result.unwrap();
        assert_eq!(snap.trigger, SnapshotTrigger::Shutdown);
    });
}

#[test]
fn snapshot_dedup_skips_periodic_fallback_too() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        engine
            .capture(&panes, SnapshotTrigger::PeriodicFallback)
            .await
            .unwrap();

        let r2 = engine
            .capture(&panes, SnapshotTrigger::PeriodicFallback)
            .await;
        assert!(matches!(r2, Err(SnapshotError::NoChanges)));
    });
}

#[test]
fn snapshot_dedup_does_not_skip_event_triggers() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        engine
            .capture(&panes, SnapshotTrigger::Startup)
            .await
            .unwrap();

        let r2 = engine.capture(&panes, SnapshotTrigger::Event).await;
        assert!(r2.is_ok());
    });
}

#[test]
fn snapshot_capture_with_minimal_pane_info() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let pane = PaneInfo {
            pane_id: 1,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: HashMap::new(),
        };

        let result = engine.capture(&[pane], SnapshotTrigger::Manual).await;
        assert!(result.is_ok(), "capture with minimal pane should succeed");
        assert_eq!(result.unwrap().pane_count, 1);
    });
}

#[test]
fn snapshot_multiple_checkpoints_have_increasing_ids() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

        let r1 = engine
            .capture(&[make_test_pane(1, 24, 80)], SnapshotTrigger::Manual)
            .await
            .unwrap();
        let r2 = engine
            .capture(&[make_test_pane(2, 30, 120)], SnapshotTrigger::Manual)
            .await
            .unwrap();
        let r3 = engine
            .capture(&[make_test_pane(3, 40, 160)], SnapshotTrigger::Manual)
            .await
            .unwrap();

        assert!(
            r1.checkpoint_id < r2.checkpoint_id,
            "checkpoint IDs should be monotonically increasing"
        );
        assert!(
            r2.checkpoint_id < r3.checkpoint_id,
            "checkpoint IDs should be monotonically increasing"
        );
    });
}

#[test]
fn snapshot_capture_total_bytes_is_nonzero() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        let r = engine
            .capture(&panes, SnapshotTrigger::Manual)
            .await
            .unwrap();
        assert!(
            r.total_bytes > 0,
            "total_bytes should be positive for a valid capture"
        );
    });
}

#[test]
fn snapshot_shutdown_returns_some_despite_same_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        engine
            .capture(&panes, SnapshotTrigger::Periodic)
            .await
            .unwrap();

        // Shutdown trigger bypasses dedup
        let result = engine
            .shutdown_checkpoint(&panes, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result.is_some(), "Shutdown trigger bypasses dedup");
    });
}

#[test]
fn snapshot_shutdown_with_empty_panes_errors() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

        engine
            .capture(&[make_test_pane(1, 24, 80)], SnapshotTrigger::Startup)
            .await
            .unwrap();

        let result = engine
            .shutdown_checkpoint(&[], Duration::from_secs(5))
            .await;
        assert!(result.is_err());
    });
}

#[test]
fn snapshot_dedup_does_not_skip_shutdown_trigger() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        engine
            .capture(&panes, SnapshotTrigger::Periodic)
            .await
            .unwrap();

        let r2 = engine.capture(&panes, SnapshotTrigger::Shutdown).await;
        assert!(r2.is_ok(), "Shutdown bypasses dedup");
    });
}

#[test]
fn snapshot_dedup_does_not_skip_work_completed_trigger() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        engine
            .capture(&panes, SnapshotTrigger::Periodic)
            .await
            .unwrap();

        let r2 = engine.capture(&panes, SnapshotTrigger::WorkCompleted).await;
        assert!(r2.is_ok(), "WorkCompleted bypasses dedup");
    });
}

#[test]
fn snapshot_cleanup_with_zero_retention_deletes_all() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let config = SnapshotConfig {
            retention_count: 0,
            retention_days: 365,
            ..SnapshotConfig::default()
        };
        let engine = SnapshotEngine::new(db_path.clone(), config);

        for i in 0..3u64 {
            engine
                .capture(
                    &[make_test_pane(i, 24 + i as u32, 80)],
                    SnapshotTrigger::Manual,
                )
                .await
                .unwrap();
        }

        let deleted = engine.cleanup().await.unwrap();
        assert_eq!(
            deleted, 3,
            "retention_count=0 should delete all checkpoints"
        );
    });
}

// ===========================================================================
// Section 3: Trigger and config tests (sync, no runtime needed)
// ===========================================================================

#[test]
fn snapshot_trigger_serde_roundtrip() {
    let triggers = vec![
        SnapshotTrigger::Periodic,
        SnapshotTrigger::PeriodicFallback,
        SnapshotTrigger::Manual,
        SnapshotTrigger::Shutdown,
        SnapshotTrigger::Startup,
        SnapshotTrigger::WorkCompleted,
        SnapshotTrigger::StateTransition,
        SnapshotTrigger::IdleWindow,
        SnapshotTrigger::MemoryPressure,
        SnapshotTrigger::HazardThreshold,
        SnapshotTrigger::Event,
    ];
    for trigger in triggers {
        let json = serde_json::to_string(&trigger).unwrap();
        let parsed: SnapshotTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(trigger, parsed, "roundtrip failed for {trigger:?}");
    }
}

// Note: snapshot_trigger_db_str_values omitted — as_db_str() is private

#[test]
fn snapshot_trigger_serde_json_is_snake_case() {
    assert_eq!(
        serde_json::to_string(&SnapshotTrigger::Periodic).unwrap(),
        "\"periodic\""
    );
    assert_eq!(
        serde_json::to_string(&SnapshotTrigger::PeriodicFallback).unwrap(),
        "\"periodic_fallback\""
    );
    assert_eq!(
        serde_json::to_string(&SnapshotTrigger::WorkCompleted).unwrap(),
        "\"work_completed\""
    );
    assert_eq!(
        serde_json::to_string(&SnapshotTrigger::HazardThreshold).unwrap(),
        "\"hazard_threshold\""
    );
    assert_eq!(
        serde_json::to_string(&SnapshotTrigger::StateTransition).unwrap(),
        "\"state_transition\""
    );
    assert_eq!(
        serde_json::to_string(&SnapshotTrigger::IdleWindow).unwrap(),
        "\"idle_window\""
    );
    assert_eq!(
        serde_json::to_string(&SnapshotTrigger::MemoryPressure).unwrap(),
        "\"memory_pressure\""
    );
}

#[test]
fn snapshot_trigger_copy_semantics() {
    let a = SnapshotTrigger::Manual;
    let b = a;
    let c = a;
    assert_eq!(b, c);
    assert_eq!(a, SnapshotTrigger::Manual);
}

#[test]
fn snapshot_error_display_messages() {
    assert_eq!(
        SnapshotError::InProgress.to_string(),
        "snapshot already in progress"
    );
    assert_eq!(SnapshotError::NoPanes.to_string(), "no panes found");
    assert_eq!(
        SnapshotError::NoChanges.to_string(),
        "no changes since last snapshot"
    );
    assert!(
        SnapshotError::PaneList("timeout".into())
            .to_string()
            .contains("timeout")
    );
    assert!(
        SnapshotError::Database("disk full".into())
            .to_string()
            .contains("disk full")
    );
    assert!(
        SnapshotError::Serialization("bad json".into())
            .to_string()
            .contains("bad json")
    );
}

#[test]
fn snapshot_config_default_has_sane_values() {
    let config = SnapshotConfig::default();
    assert!(
        config.interval_seconds >= 30,
        "interval should be at least 30s"
    );
    assert!(
        config.retention_count > 0,
        "retention count should be positive"
    );
    assert!(
        config.retention_days > 0,
        "retention days should be positive"
    );
}

#[test]
fn snapshot_telemetry_initial_zero() {
    let engine = SnapshotEngine::new(Arc::new(":memory:".to_string()), SnapshotConfig::default());
    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.captures_attempted, 0);
    assert_eq!(snap.captures_succeeded, 0);
    assert_eq!(snap.dedup_skips, 0);
    assert_eq!(snap.capture_errors, 0);
    assert_eq!(snap.cleanup_runs, 0);
    assert_eq!(snap.cleanup_removed, 0);
    assert_eq!(snap.triggers_emitted, 0);
    assert_eq!(snap.triggers_accepted, 0);
    assert_eq!(snap.panes_captured, 0);
    assert_eq!(snap.bytes_persisted, 0);
}

#[test]
fn snapshot_emit_trigger_updates_telemetry() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path, SnapshotConfig::default());

        assert!(engine.emit_trigger(SnapshotTrigger::WorkCompleted));
        assert!(engine.emit_trigger(SnapshotTrigger::StateTransition));

        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.triggers_emitted, 2);
        assert_eq!(snap.triggers_accepted, 2);
    });
}

#[test]
fn snapshot_capture_updates_telemetry() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        engine
            .capture(&panes, SnapshotTrigger::Manual)
            .await
            .unwrap();

        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.captures_attempted, 1);
        assert_eq!(snap.captures_succeeded, 1);
        assert_eq!(snap.panes_captured, 1);
        assert!(snap.bytes_persisted > 0);
    });
}

#[test]
fn snapshot_dedup_skip_updates_telemetry() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (_tmp, db_path, _storage) = setup_test_db().await;
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        engine
            .capture(&panes, SnapshotTrigger::Periodic)
            .await
            .unwrap();

        // Second capture with same data → dedup skip
        let _ = engine.capture(&panes, SnapshotTrigger::Periodic).await;

        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.captures_attempted, 2);
        assert_eq!(snap.captures_succeeded, 1);
        assert_eq!(snap.dedup_skips, 1);
    });
}

// ===========================================================================
// Note: Intelligent scheduling tests (16+ tests) and emit_trigger channel
// tests omitted — they use `runtime_compat::task::spawn` (delegated to
// tokio::spawn) for `run_periodic` and access private fields (trigger_rx).
// These are fully exercised as unit tests in snapshot_engine.rs.
//
// LabRuntime sections (2-5) also omitted: SnapshotEngine uses direct
// rusqlite I/O (not spawn_blocking), which is synchronous and doesn't
// interact with the LabRuntime's deterministic task scheduler.
// ===========================================================================
