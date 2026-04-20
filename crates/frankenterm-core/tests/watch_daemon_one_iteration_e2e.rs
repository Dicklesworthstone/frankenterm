//! End-to-end test for the `ft watch` daemon's discovery+persist loop.
//!
//! Mock-free: spins up a real SQLite database in a tempfile, drives one
//! iteration of the discovery loop against a 2-pane mock pane list,
//! persists each new pane through the real storage writer, verifies the
//! rows round-trip from disk, and exercises graceful shutdown.
//!
//! This mirrors the work that
//! `crates/frankenterm-core/src/runtime.rs::ObservationRuntime::start`
//! performs inside its spawned discovery task — without requiring the
//! tokio-reactor migration in `runtime_compat::task`, which is still
//! in flight (see header of `runtime_labruntime.rs` for context). The
//! pieces under test are the real public APIs the daemon uses in
//! production:
//!
//! - `PaneRegistry::new()` → `discovery_tick(Vec<PaneInfo>)` →
//!   `DiscoveryDiff`: the single discovery iteration.
//! - `StorageHandle::new(&path)` → `upsert_pane(...)` → `get_panes()`:
//!   real SQLite write and readback through the writer-thread channel.
//! - `StorageHandle::shutdown()`: graceful drain of the writer thread.
//!
//! Scope note: this test does NOT edit `runtime.rs` (pane 6 is working
//! there). It only calls the already-public surface that `ft watch`
//! drives, so it can be extended independently when the full
//! `ObservationRuntime::start` path becomes testable under asupersync.
//!
//! Domain: watch daemon real-service E2E (pane 5).

use frankenterm_core::ingest::PaneRegistry;
use frankenterm_core::runtime_compat::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, StorageHandle, now_ms};
use frankenterm_core::wezterm::{PaneInfo, PaneSize};
use tempfile::TempDir;

/// Create a fresh tempdir with a SQLite file path. The TempDir must
/// outlive every StorageHandle it backs — so callers keep `_dir` in
/// scope for the duration of the test.
fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create watch daemon e2e tempdir");
    let path = dir.path().join("watch_daemon_e2e.sqlite3");
    (dir, path.to_string_lossy().into_owned())
}

fn make_pane(pane_id: u64, title: &str, cwd: &str, is_active: bool) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id: 10,
        window_id: 100,
        domain_id: Some(1),
        domain_name: Some("local".to_string()),
        workspace: Some("default".to_string()),
        size: Some(PaneSize {
            rows: 24,
            cols: 80,
            pixel_width: None,
            pixel_height: None,
            dpi: None,
        }),
        rows: None,
        cols: None,
        title: Some(title.to_string()),
        cwd: Some(cwd.to_string()),
        tty_name: None,
        cursor_x: Some(0),
        cursor_y: Some(0),
        cursor_visibility: None,
        left_col: Some(0),
        top_row: Some(0),
        is_active,
        is_zoomed: false,
        extra: std::collections::HashMap::new(),
    }
}

fn pane_record_from_info(info: &PaneInfo, now: i64) -> PaneRecord {
    PaneRecord {
        pane_id: info.pane_id,
        pane_uuid: None,
        domain: info.inferred_domain(),
        window_id: Some(info.window_id),
        tab_id: Some(info.tab_id),
        title: info.title.clone(),
        cwd: info.cwd.clone(),
        tty_name: info.tty_name.clone(),
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: Some(now),
    }
}

#[test]
#[ignore = "e2e wip — build loop interrupted before local verification; un-ignore once green"]
fn watch_daemon_one_iteration_persists_two_panes_and_shuts_down_cleanly() {
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");

    rt.block_on(async {
        // ── Arrange: real sqlite under a tempdir (no mocks) ─────────────
        let (_dir, db_path) = temp_db();
        let storage = StorageHandle::new(&db_path)
            .await
            .expect("open real sqlite storage");

        // Pre-condition: empty pane table.
        let initial = storage.get_panes().await.expect("read panes");
        assert!(
            initial.is_empty(),
            "fresh tempfile sqlite must have no panes, got {:?}",
            initial
        );

        let mut registry = PaneRegistry::new();

        let mock_panes = vec![
            make_pane(1, "claude", "file:///home/agent", true),
            make_pane(2, "editor", "file:///srv/repo", false),
        ];

        // ── Act: one discovery iteration ────────────────────────────────
        let diff = registry.discovery_tick(mock_panes.clone());
        assert_eq!(
            diff.new_panes,
            vec![1, 2],
            "both mock panes must be reported as newly discovered"
        );
        assert!(
            diff.closed_panes.is_empty() && diff.changed_panes.is_empty(),
            "a fresh registry's first tick cannot have closed or changed panes, got {:?}",
            diff
        );

        // Persist each newly-discovered pane through the real writer.
        let now = now_ms();
        for info in &mock_panes {
            storage
                .upsert_pane(pane_record_from_info(info, now))
                .await
                .expect("upsert pane through real writer thread");
        }

        // ── Assert: E2E readback from real sqlite ──────────────────────
        let persisted = storage.get_panes().await.expect("read panes");
        assert_eq!(
            persisted.len(),
            2,
            "two panes must round-trip through real sqlite, got {}",
            persisted.len()
        );

        let by_id: std::collections::BTreeMap<u64, PaneRecord> =
            persisted.into_iter().map(|r| (r.pane_id, r)).collect();
        let p1 = by_id.get(&1).expect("pane 1 persisted");
        let p2 = by_id.get(&2).expect("pane 2 persisted");

        assert_eq!(p1.title.as_deref(), Some("claude"));
        assert_eq!(p1.cwd.as_deref(), Some("file:///home/agent"));
        assert_eq!(p1.domain, "local");
        assert!(p1.observed);
        assert_eq!(p1.window_id, Some(100));
        assert_eq!(p1.tab_id, Some(10));

        assert_eq!(p2.title.as_deref(), Some("editor"));
        assert_eq!(p2.cwd.as_deref(), Some("file:///srv/repo"));
        assert_eq!(p2.domain, "local");
        assert!(p2.observed);

        // Registry invariants after the tick.
        assert!(
            registry.get_entry(1).is_some(),
            "registry must retain entry for pane 1 after discovery_tick"
        );
        assert!(
            registry.get_entry(2).is_some(),
            "registry must retain entry for pane 2 after discovery_tick"
        );
        let entry_count = registry.entries().count();
        assert_eq!(entry_count, 2, "registry must carry exactly 2 entries");

        // ── Second iteration: idempotent when panes unchanged ──────────
        let diff2 = registry.discovery_tick(mock_panes.clone());
        assert!(
            diff2.new_panes.is_empty() && diff2.closed_panes.is_empty(),
            "second tick with identical panes must not produce new/closed diffs, got {:?}",
            diff2
        );

        // ── Act (shutdown): graceful drain of the writer thread ────────
        storage
            .shutdown()
            .await
            .expect("graceful storage shutdown must succeed");

        // ── Assert: sqlite file still exists on disk after shutdown ────
        let db_metadata =
            std::fs::metadata(&db_path).expect("sqlite file must exist after shutdown");
        assert!(
            db_metadata.len() > 0,
            "sqlite file must have non-zero size on disk, got {} bytes",
            db_metadata.len()
        );
    });
}

#[test]
#[ignore = "e2e wip — build loop interrupted before local verification; un-ignore once green"]
fn watch_daemon_discovery_tick_detects_closed_panes_on_second_iteration() {
    // A second-iteration E2E: the first tick registers 2 panes, the
    // second tick presents only 1 — the registry must report pane 2
    // as closed, but the persisted row in sqlite is NOT removed (the
    // daemon treats close-diffs as events to record, not as row
    // deletions). This pins that contract so a future refactor that
    // starts auto-deleting closed-pane rows would fail the test.
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");

    rt.block_on(async {
        let (_dir, db_path) = temp_db();
        let storage = StorageHandle::new(&db_path)
            .await
            .expect("open real sqlite storage");
        let mut registry = PaneRegistry::new();

        let initial_panes = vec![
            make_pane(1, "claude", "file:///home/agent", true),
            make_pane(2, "editor", "file:///srv/repo", false),
        ];
        let diff = registry.discovery_tick(initial_panes.clone());
        assert_eq!(diff.new_panes, vec![1, 2]);

        let now = now_ms();
        for info in &initial_panes {
            storage
                .upsert_pane(pane_record_from_info(info, now))
                .await
                .expect("persist pane");
        }

        // Second iteration: only pane 1 is still alive.
        let reduced_panes = vec![make_pane(1, "claude", "file:///home/agent", true)];
        let diff2 = registry.discovery_tick(reduced_panes);
        assert_eq!(
            diff2.closed_panes,
            vec![2],
            "pane 2 must be reported as closed"
        );
        assert!(
            diff2.new_panes.is_empty(),
            "no new panes on reduction tick, got {:?}",
            diff2.new_panes
        );

        // The persisted row for pane 2 must still be readable —
        // discovery_tick does not delete rows.
        let persisted = storage.get_panes().await.expect("read panes");
        let ids: std::collections::BTreeSet<u64> =
            persisted.iter().map(|r| r.pane_id).collect();
        assert!(
            ids.contains(&1) && ids.contains(&2),
            "both panes must remain in sqlite after pane 2 is reported closed; ids={:?}",
            ids
        );

        storage.shutdown().await.expect("graceful shutdown");
    });
}
