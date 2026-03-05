//! LabRuntime-ported session_correlation tests for deterministic async testing.
//!
//! Ports the `#[tokio::test]` async test from `session_correlation.rs` to
//! asupersync-based `RuntimeFixture`, gaining deterministic scheduling for
//! StorageHandle I/O and CassClient interactions.
//!
//! Ported tests:
//! - `correlate_and_persist_override_updates_session`
//!
//! Bead: ft-22x4r

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::cass::{CassAgent, CassClient, parse_cass_timestamp_ms};
use frankenterm_core::session_correlation::{
    CassCorrelationOptions, CorrelationStatus, correlate_and_persist_for_pane,
};
use frankenterm_core::storage::{AgentSessionRecord, PaneRecord, StorageHandle};
use std::sync::atomic::{AtomicU64, Ordering};

// ===========================================================================
// Helpers
// ===========================================================================

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir();
    dir.join(format!(
        "session_corr_labrt_{counter}_{}.db",
        std::process::id()
    ))
    .to_str()
    .unwrap()
    .to_string()
}

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn correlate_and_persist_override_updates_session() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        let now = parse_cass_timestamp_ms("2026-01-29T17:00:00Z").unwrap();

        let dir = tempfile::tempdir().unwrap();
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: Some(dir.path().to_string_lossy().to_string()),
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        handle.upsert_pane(pane).await.unwrap();

        let mut session = AgentSessionRecord::new_start(1, "claude_code");
        session.started_at = now;
        let session_id = handle.upsert_agent_session(session).await.unwrap();

        let mut options = CassCorrelationOptions::default();
        options.override_session_id = Some("cass-override".to_string());

        let cass = CassClient::new();
        let correlation =
            correlate_and_persist_for_pane(&handle, &cass, 1, CassAgent::ClaudeCode, now, &options)
                .await
                .unwrap();

        let updated = handle.get_agent_session(session_id).await.unwrap().unwrap();
        assert_eq!(updated.external_id.as_deref(), Some("cass-override"));
        assert!(updated.external_meta.is_some());
        assert_eq!(correlation.status, CorrelationStatus::Linked);

        handle.shutdown().await.unwrap();
    });
}
