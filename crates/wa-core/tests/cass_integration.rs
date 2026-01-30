//! Integration tests for cass CLI wrapper + correlation + accounting.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use wa_core::cass::{CassAgent, CassClient, CassError};
use wa_core::session_correlation::{
    CassCorrelationOptions, CassSummaryRefreshOptions, correlate_with_cass,
    refresh_cass_summary_for_session,
};
use wa_core::storage::{AgentSessionRecord, PaneRecord, StorageHandle};

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

#[tokio::test]
async fn missing_cass_binary_returns_not_installed() {
    let cass = CassClient::new().with_binary("cass-missing-xyz");
    let tmp = TempDir::new().expect("temp dir");
    let err = cass
        .search_sessions(tmp.path(), Some(CassAgent::ClaudeCode))
        .await
        .expect_err("missing binary should error");

    assert!(matches!(err, CassError::NotInstalled));
}

#[tokio::test]
async fn stub_cass_search_drives_deterministic_correlation() {
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
    let start_ms = wa_core::cass::parse_cass_timestamp_ms("2026-01-29T10:00:00Z").unwrap();
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
}

#[tokio::test]
async fn refresh_cass_summary_updates_agent_session() {
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

    let db_path = tmp.path().join("wa.db");
    let db_path_str = db_path.to_string_lossy().to_string();
    let storage = StorageHandle::new(&db_path_str).await.unwrap();

    let now_ms = wa_core::cass::parse_cass_timestamp_ms("2026-01-29T10:00:00Z").unwrap();

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
}
