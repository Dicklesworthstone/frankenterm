//! Integration tests for cass CLI wrapper + correlation + accounting.

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
#[cfg(feature = "cass-export")]
use std::time::Duration;

use frankenterm_core::cass::{CassAgent, CassClient, CassError};
#[cfg(feature = "cass-export")]
use frankenterm_core::cass::{
    CassContentExportQuery, CassExportQuery, export_content, export_sessions,
};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::session_correlation::{
    CassCorrelationOptions, CassSummaryRefreshOptions, correlate_with_cass,
    refresh_cass_summary_for_session,
};
use frankenterm_core::storage::{AgentSessionRecord, PaneRecord, StorageHandle};
use tempfile::TempDir;

fn write_cass_stub(
    dir: &Path,
    search_json: &str,
    query_json: &str,
    log_path: Option<&Path>,
) -> PathBuf {
    let script_path = dir.join("cass");
    let log_snippet = log_path.map_or(String::new(), |path| {
        format!("echo \"$0 $@\" >> \"{}\"\n", path.display())
    });
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

{log_snippet}

cmd="${{1:-}}"
shift || true

case "$cmd" in
  search)
    cat <<'EOF'
{search_json}
EOF
    ;;
  query)
    cat <<'EOF'
{query_json}
EOF
    ;;
  status)
    cat <<'EOF'
{{"healthy":true}}
EOF
    ;;
  *)
    echo "unsupported cass stub command: $cmd" >&2
    exit 1
    ;;
esac
"#
    );

    fs::write(&script_path, script).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path).expect("stat stub").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod stub");
    }

    script_path
}

fn run_async_test<F>(future: F)
where
    F: Future<Output = ()>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("failed to build runtime_async current-thread runtime");
    CompatRuntime::block_on(&runtime, future);
}

#[test]
fn missing_cass_binary_returns_not_installed() {
    run_async_test(async {
        let cass = CassClient::new().with_binary("cass-missing-xyz");
        let tmp = TempDir::new().expect("temp dir");
        let err = cass
            .search_sessions(tmp.path(), Some(CassAgent::ClaudeCode))
            .await
            .expect_err("missing binary should error");

        assert!(matches!(err, CassError::NotInstalled));
    });
}

#[test]
fn stub_cass_search_drives_deterministic_correlation() {
    run_async_test(async {
        let tmp = TempDir::new().expect("temp dir");
        let log_path = tmp.path().join("cass.log");

        let search_json = r#"[
  {
    "session_id": "cass-old",
    "agent": "claude_code",
    "project_path": "/repo",
    "started_at": "2026-01-29T09:59:00Z"
  },
  {
    "session_id": "cass-new",
    "agent": "claude_code",
    "project_path": "/repo",
    "started_at": "2026-01-29T10:01:00Z"
  }
]"#;
        let query_json = r#"{
  "session_id": "cass-new",
  "agent": "claude_code",
  "project_path": "/repo",
    "started_at": "2026-01-29T10:01:00Z",
    "messages": []
}"#;

        let stub = write_cass_stub(tmp.path(), search_json, query_json, Some(&log_path));
        let cass = CassClient::new().with_binary(stub.to_string_lossy().to_string());

        let options = CassCorrelationOptions::default();
        let start_ms =
            frankenterm_core::cass::parse_cass_timestamp_ms("2026-01-29T10:00:00Z").unwrap();
        let result = correlate_with_cass(
            &cass,
            Path::new("/repo"),
            CassAgent::ClaudeCode,
            start_ms,
            &options,
        )
        .await;

        let log = fs::read_to_string(&log_path).expect("log stub");
        assert!(log.contains("search"), "stub should log search invocation");
        assert_eq!(result.external_id.as_deref(), Some("cass-new"));
        assert!(result.reasons.iter().any(|r| r == "ambiguous_candidates"));
    });
}

#[test]
fn refresh_cass_summary_updates_agent_session() {
    run_async_test(async {
        let tmp = TempDir::new().expect("temp dir");
        let log_path = tmp.path().join("cass.log");

        let search_json = r#"[
  {
    "session_id": "cass-session-1",
    "agent": "claude_code",
    "project_path": "/repo",
    "started_at": "2026-01-29T10:00:00Z"
  }
]"#;
        let query_json = r#"{
  "session_id": "cass-session-1",
  "agent": "claude_code",
  "project_path": "/repo",
  "started_at": "2026-01-29T10:00:00Z",
  "ended_at": "2026-01-29T10:10:00Z",
  "messages": [
    { "role": "user", "token_count": 11, "timestamp": "2026-01-29T10:00:00Z" },
    { "role": "assistant", "token_count": 22, "timestamp": "2026-01-29T10:05:00Z" }
  ]
}"#;

        let stub = write_cass_stub(tmp.path(), search_json, query_json, Some(&log_path));
        let cass = CassClient::new().with_binary(stub.to_string_lossy().to_string());

        let db_path = tmp.path().join("ft.db");
        let db_path_str = db_path.to_string_lossy().to_string();
        let storage = StorageHandle::new(&db_path_str).await.unwrap();

        let now_ms =
            frankenterm_core::cass::parse_cass_timestamp_ms("2026-01-29T10:00:00Z").unwrap();

        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: Some("/repo".to_string()),
            tty_name: None,
            first_seen_at: now_ms,
            last_seen_at: now_ms,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();

        let mut session = AgentSessionRecord::new_start(1, "claude_code");
        session.started_at = now_ms;
        session.external_id = Some("cass-session-1".to_string());
        let session_id = storage.upsert_agent_session(session).await.unwrap();

        let summary = refresh_cass_summary_for_session(
            &storage,
            &cass,
            session_id,
            &CassSummaryRefreshOptions::default(),
        )
        .await
        .expect("refresh summary");

        let updated = storage
            .get_agent_session(session_id)
            .await
            .unwrap()
            .unwrap();

        let log = fs::read_to_string(&log_path).expect("log stub");
        assert!(log.contains("query"), "stub should log query invocation");
        assert_eq!(summary.total_tokens, Some(33));
        assert_eq!(updated.total_tokens, Some(33));
        assert!(updated.external_meta.is_some());
    });
}

#[cfg(feature = "cass-export")]
#[test]
fn cass_export_sessions_use_fallback_identifier_and_workspace() {
    run_async_test(async {
        let tmp = TempDir::new().expect("temp dir");
        let db_path = tmp.path().join("ft.db");
        let db_path_string = db_path.to_string_lossy().into_owned();
        let storage = StorageHandle::new(&db_path_string).await.unwrap();
        let base_ms =
            frankenterm_core::cass::parse_cass_timestamp_ms("2026-01-29T10:00:00Z").unwrap();

        storage
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/repo".to_string()),
                tty_name: None,
                first_seen_at: base_ms,
                last_seen_at: base_ms,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();

        let mut session = AgentSessionRecord::new_start(1, "codex");
        session.started_at = base_ms;
        session.model_name = Some("gpt-5.4".to_string());
        let session_row_id = storage.upsert_agent_session(session).await.unwrap();

        storage.append_segment(1, "alpha beta", None).await.unwrap();
        storage.append_segment(1, "gamma", None).await.unwrap();

        let exported = export_sessions(&storage, &CassExportQuery::default())
            .await
            .unwrap();
        assert_eq!(exported.len(), 1);

        let record = &exported[0];
        assert_eq!(record.session_row_id, session_row_id);
        assert_eq!(record.session_id, format!("ft-session-{session_row_id}"));
        assert_eq!(record.agent_type, "codex");
        assert_eq!(record.workspace.as_deref(), Some("/repo"));
        assert_eq!(record.pane_ids, vec![1]);
        assert_eq!(record.content_tokens, 4);
        assert_eq!(record.model_name.as_deref(), Some("gpt-5.4"));
    });
}

#[cfg(feature = "cass-export")]
#[test]
fn cass_export_sessions_filter_after_id_and_limit() {
    run_async_test(async {
        let tmp = TempDir::new().expect("temp dir");
        let db_path = tmp.path().join("ft.db");
        let db_path_string = db_path.to_string_lossy().into_owned();
        let storage = StorageHandle::new(&db_path_string).await.unwrap();
        let base_ms =
            frankenterm_core::cass::parse_cass_timestamp_ms("2026-01-29T10:00:00Z").unwrap();

        for pane_id in [1_u64, 2] {
            storage
                .upsert_pane(PaneRecord {
                    pane_id,
                    pane_uuid: None,
                    domain: "local".to_string(),
                    window_id: None,
                    tab_id: None,
                    title: None,
                    cwd: Some(format!("/repo/{pane_id}")),
                    tty_name: None,
                    first_seen_at: base_ms,
                    last_seen_at: base_ms,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                })
                .await
                .unwrap();
        }

        let mut first = AgentSessionRecord::new_start(1, "claude_code");
        first.started_at = base_ms;
        first.session_id = Some("sess-a".to_string());
        let first_row_id = storage.upsert_agent_session(first).await.unwrap();

        let mut second = AgentSessionRecord::new_start(2, "codex");
        second.started_at = base_ms + 1_000;
        second.session_id = Some("sess-b".to_string());
        storage.upsert_agent_session(second).await.unwrap();

        let exported = export_sessions(
            &storage,
            &CassExportQuery {
                after_id: Some(first_row_id),
                limit: 1,
                ..CassExportQuery::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].session_id, "sess-b");
        assert_eq!(exported[0].pane_ids, vec![2]);
    });
}

#[cfg(feature = "cass-export")]
#[test]
fn cass_export_sessions_page_by_row_id_even_when_started_at_reorders() {
    run_async_test(async {
        let tmp = TempDir::new().expect("temp dir");
        let db_path = tmp.path().join("ft.db");
        let db_path_string = db_path.to_string_lossy().into_owned();
        let storage = StorageHandle::new(&db_path_string).await.unwrap();
        let base_ms =
            frankenterm_core::cass::parse_cass_timestamp_ms("2026-01-29T10:00:00Z").unwrap();

        storage
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/repo".to_string()),
                tty_name: None,
                first_seen_at: base_ms,
                last_seen_at: base_ms,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();

        let mut first_inserted = AgentSessionRecord::new_start(1, "codex");
        first_inserted.session_id = Some("sess-row-1".to_string());
        first_inserted.started_at = base_ms + 10_000;
        let first_row_id = storage.upsert_agent_session(first_inserted).await.unwrap();

        let mut second_inserted = AgentSessionRecord::new_start(1, "codex");
        second_inserted.session_id = Some("sess-row-2".to_string());
        second_inserted.started_at = base_ms;
        let second_row_id = storage.upsert_agent_session(second_inserted).await.unwrap();

        let first_page = export_sessions(
            &storage,
            &CassExportQuery {
                limit: 1,
                ..CassExportQuery::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(first_page.len(), 1);
        assert_eq!(first_page[0].session_row_id, first_row_id);

        let second_page = export_sessions(
            &storage,
            &CassExportQuery {
                after_id: Some(first_row_id),
                limit: 10,
                ..CassExportQuery::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(second_page.len(), 1);
        assert_eq!(second_page[0].session_row_id, second_row_id);
    });
}

#[cfg(feature = "cass-export")]
#[test]
fn cass_export_content_respects_session_window_and_cursor() {
    run_async_test(async {
        let tmp = TempDir::new().expect("temp dir");
        let db_path = tmp.path().join("ft.db");
        let db_path_string = db_path.to_string_lossy().into_owned();
        let storage = StorageHandle::new(&db_path_string).await.unwrap();
        let base_ms =
            frankenterm_core::cass::parse_cass_timestamp_ms("2026-01-29T10:00:00Z").unwrap();

        storage
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/repo".to_string()),
                tty_name: None,
                first_seen_at: base_ms,
                last_seen_at: base_ms,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();

        let before = storage.append_segment(1, "before", None).await.unwrap();
        std::thread::sleep(Duration::from_millis(3));
        let during_a = storage.append_segment(1, "during one", None).await.unwrap();
        std::thread::sleep(Duration::from_millis(3));
        let during_b = storage.append_segment(1, "during two", None).await.unwrap();

        let mut session = AgentSessionRecord::new_start(1, "codex");
        session.session_id = Some("sess-window".to_string());
        session.started_at = during_a.captured_at;
        session.ended_at = Some(during_b.captured_at);
        let session_row_id = storage.upsert_agent_session(session).await.unwrap();

        std::thread::sleep(Duration::from_millis(3));
        let after = storage.append_segment(1, "after", None).await.unwrap();

        let exported = export_content(&storage, "sess-window", &CassContentExportQuery::default())
            .await
            .unwrap();

        assert_eq!(
            exported
                .iter()
                .map(|chunk| chunk.segment_id)
                .collect::<Vec<_>>(),
            vec![during_a.id, during_b.id]
        );
        assert!(exported.iter().all(|chunk| chunk.content_type == "output"));
        assert!(
            exported
                .iter()
                .all(|chunk| chunk.session_row_id == session_row_id)
        );
        assert!(!exported.iter().any(|chunk| chunk.segment_id == before.id));
        assert!(!exported.iter().any(|chunk| chunk.segment_id == after.id));

        let incremental = export_content(
            &storage,
            "sess-window",
            &CassContentExportQuery {
                after_id: Some(during_a.id),
                limit: 10,
            },
        )
        .await
        .unwrap();

        assert_eq!(incremental.len(), 1);
        assert_eq!(incremental[0].segment_id, during_b.id);
    });
}

#[cfg(feature = "cass-export")]
#[test]
fn cass_export_content_rejects_unexported_fallback_identifier() {
    run_async_test(async {
        let tmp = TempDir::new().expect("temp dir");
        let db_path = tmp.path().join("ft.db");
        let db_path_string = db_path.to_string_lossy().into_owned();
        let storage = StorageHandle::new(&db_path_string).await.unwrap();
        let base_ms =
            frankenterm_core::cass::parse_cass_timestamp_ms("2026-01-29T10:00:00Z").unwrap();

        storage
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/repo".to_string()),
                tty_name: None,
                first_seen_at: base_ms,
                last_seen_at: base_ms,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();

        let mut session = AgentSessionRecord::new_start(1, "codex");
        session.session_id = Some("sess-explicit".to_string());
        session.started_at = base_ms;
        let session_row_id = storage.upsert_agent_session(session).await.unwrap();

        storage.append_segment(1, "payload", None).await.unwrap();

        let err = export_content(
            &storage,
            &format!("ft-session-{session_row_id}"),
            &CassContentExportQuery::default(),
        )
        .await
        .expect_err("synthetic id should not resolve when the session exports an explicit id");

        assert!(
            format!("{err}").contains("cass export session"),
            "unexpected error: {err}"
        );
    });
}
