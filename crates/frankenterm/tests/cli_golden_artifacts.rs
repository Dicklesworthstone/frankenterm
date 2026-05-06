//! Golden artifact regression suite for stable CLI machine surfaces.
//!
//! Covers:
//! - `ft robot --format toon state`
//! - `ft snapshot list -f json`
//!
//! Regenerate committed goldens with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p frankenterm --test cli_golden_artifacts -- --nocapture
//! ```

use assert_cmd::Command;
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{FtsSyncConfig, PaneRecord, StorageHandle};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const INCIDENT_SECRET: &str = "sk-abc123456789012345678901234567890123456789012345678901";
const INCIDENT_PANE_ID: u64 = 4_242;

fn setup_workspace() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let ft_dir = dir.path().join(".ft");
    fs::create_dir_all(&ft_dir).expect("create .ft dir");
    fs::write(
        ft_dir.join("config.toml"),
        "[general]\nlog_level = \"error\"\n",
    )
    .expect("write quiet test config");

    let db_path = ft_dir.join("ft.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");
    frankenterm_core::storage::initialize_schema(&conn).expect("init schema");
    drop(conn);

    let ws = dir.path().to_string_lossy().to_string();
    (dir, ws)
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("golden_artifacts")
}

fn wezterm_cli_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("frankenterm-core")
        .join("tests")
        .join("fixtures")
        .join("wezterm_cli")
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::new();
            for (k, v) in sorted {
                out.insert(k.clone(), canonicalize(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn is_uuid_like(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for &idx in &[8usize, 13, 18, 23] {
        if bytes[idx] != b'-' {
            return false;
        }
    }
    bytes.iter().enumerate().all(|(idx, b)| match idx {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

fn scrub_dynamic(value: &mut Value, parent_key: Option<&str>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                scrub_dynamic(child, Some(key));
            }
        }
        Value::Array(items) => {
            for item in items {
                scrub_dynamic(item, parent_key);
            }
        }
        Value::String(text) => {
            if matches!(parent_key, Some("session_id")) && is_uuid_like(text) {
                *text = "<uuid-session>".to_string();
            } else if is_uuid_like(text) {
                *text = "<uuid>".to_string();
            }
        }
        Value::Number(_) => {
            if matches!(
                parent_key,
                Some("elapsed_ms" | "checkpoint_at" | "created_at" | "updated_at" | "now")
            ) {
                *value = Value::from(0);
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
}

fn pretty_canonical_json(value: &Value) -> String {
    serde_json::to_string_pretty(&canonicalize(value)).expect("serialize canonical JSON")
}

fn canonical_response_value(value: &Value) -> Value {
    let mut scrubbed = value.clone();
    scrub_dynamic(&mut scrubbed, None);
    canonicalize(&scrubbed)
}

fn canonical_toon(value: &Value) -> String {
    toon_rust::encode(canonicalize(value), None)
}

fn read_or_update_golden(path: &Path, actual: &str) -> String {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixtures dir");
        }
        fs::write(path, format!("{actual}\n")).expect("write golden");
        return actual.to_string();
    }

    fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm --test cli_golden_artifacts -- --nocapture",
            path.display()
        )
    })
}

fn assert_matches_golden(actual: &str, path: &Path) {
    let expected = read_or_update_golden(path, actual);
    let expected_trimmed = expected.trim_end_matches('\n');
    let actual_trimmed = actual.trim_end_matches('\n');

    if expected_trimmed != actual_trimmed {
        let actual_path = path.with_extension(format!(
            "actual.{}",
            path.extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("txt")
        ));
        let _ = fs::write(&actual_path, format!("{actual}\n"));
        panic!(
            "golden drift detected.\n  expected: {}\n  actual:   {}\n\n\
             Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm --test cli_golden_artifacts -- --nocapture",
            path.display(),
            actual_path.display()
        );
    }
}

fn write_wezterm_stub(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("wezterm-stub.sh");
    let script = r#"#!/bin/sh
set -eu

if [ "${1:-}" = "cli" ] && [ "${2:-}" = "list" ]; then
  cat "$FT_TEST_WEZTERM_LIST_JSON"
  exit 0
fi

if [ "${1:-}" = "cli" ] && [ "${2:-}" = "get-text" ]; then
  pane_id=""
  shift 2
  while [ "$#" -gt 0 ]; do
    case "${1:-}" in
      --pane-id)
        pane_id="${2:-}"
        shift 2
        ;;
      --escapes)
        shift
        ;;
      *)
        echo "unsupported get-text args: $*" >&2
        exit 64
        ;;
    esac
  done
  if [ -z "$pane_id" ]; then
    echo "missing --pane-id" >&2
    exit 64
  fi
  cat "$FT_TEST_WEZTERM_TEXT_DIR/$pane_id.txt"
  exit 0
fi

echo "unsupported wezterm stub invocation: $*" >&2
exit 64
"#;
    fs::write(&path, script).expect("write wezterm stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).expect("stat stub").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod stub");
    }
    path
}

fn runtime() -> frankenterm_core::runtime_async::Runtime {
    RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |dur| i64::try_from(dur.as_millis()).unwrap_or(i64::MAX))
}

fn pane_record(pane_id: u64, ts: i64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: Some(3),
        tab_id: Some(7),
        title: Some("incident-robot".to_string()),
        cwd: Some("file:///tmp/ft-incident-robot".to_string()),
        tty_name: None,
        first_seen_at: ts,
        last_seen_at: ts,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn seed_robot_search_segment(workspace: &str, pane_id: u64, text: &str) {
    let db_path = Path::new(workspace).join(".ft").join("ft.db");
    let db_path_string = db_path.to_string_lossy().to_string();
    let text = text.to_string();
    runtime().block_on(async move {
        let storage = StorageHandle::new(&db_path_string)
            .await
            .expect("open robot search storage");
        storage
            .upsert_pane(pane_record(pane_id, now_ms()))
            .await
            .expect("upsert robot incident pane");
        storage
            .append_segment(pane_id, &text, None)
            .await
            .expect("append robot incident segment");
        storage
            .rebuild_fts(FtsSyncConfig::default())
            .await
            .expect("rebuild robot incident FTS fixture");
        storage.shutdown().await.expect("shutdown robot storage");
    });
}

fn log_incident_drill_case(case: Value) {
    eprintln!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "schema": "ft.hp70k.incident_drill.v1",
            "case": case,
        }))
        .expect("serialize incident drill log")
    );
}

fn assert_robot_response_redacted(case_id: &str, response: &Value) {
    let rendered = serde_json::to_string(response).expect("serialize robot response");
    assert!(
        !rendered.contains(INCIDENT_SECRET),
        "{case_id} leaked raw secret in response: {rendered}"
    );
    assert!(
        rendered.contains("[REDACTED"),
        "{case_id} should include a redaction marker: {rendered}"
    );
}

fn run_robot_state_toon(case_name: &str, fixture_name: &str) -> String {
    let (dir, workspace) = setup_workspace();
    let stub_path = write_wezterm_stub(&dir);
    let wezterm_json = wezterm_cli_fixtures_dir().join(fixture_name);
    assert!(
        wezterm_json.exists(),
        "missing wezterm fixture {}",
        wezterm_json.display()
    );

    let output = Command::cargo_bin("ft")
        .expect("locate ft binary")
        .env("FT_WORKSPACE", &workspace)
        .env("FT_WEZTERM_CLI", &stub_path)
        .env("FT_TEST_WEZTERM_LIST_JSON", &wezterm_json)
        .args(["robot", "--format", "toon", "state"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("robot state stdout should be utf-8");
    assert!(
        !stdout.trim_start().starts_with('{'),
        "expected TOON output for case {case_name}"
    );
    let decoded = toon_rust::try_decode(&stdout, None)
        .unwrap_or_else(|err| panic!("failed to decode TOON for {case_name}: {err}"));
    let decoded_json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let mut value: Value = serde_json::from_str(&decoded_json)
        .unwrap_or_else(|err| panic!("failed to parse decoded TOON JSON for {case_name}: {err}"));
    scrub_dynamic(&mut value, None);
    canonical_toon(&value)
}

fn run_robot_state_toon_from_json(case_name: &str, panes_json: &Value) -> String {
    let (dir, workspace) = setup_workspace();
    let stub_path = write_wezterm_stub(&dir);
    let wezterm_json = dir.path().join(format!("{case_name}.json"));
    fs::write(
        &wezterm_json,
        serde_json::to_string_pretty(panes_json).expect("serialize wezterm panes"),
    )
    .expect("write temporary wezterm fixture");

    let output = Command::cargo_bin("ft")
        .expect("locate ft binary")
        .env("FT_WORKSPACE", &workspace)
        .env("FT_WEZTERM_CLI", &stub_path)
        .env("FT_TEST_WEZTERM_LIST_JSON", &wezterm_json)
        .args(["robot", "--format", "toon", "state"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("robot state stdout should be utf-8");
    let decoded = toon_rust::try_decode(&stdout, None)
        .unwrap_or_else(|err| panic!("failed to decode TOON for {case_name}: {err}"));
    let decoded_json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let mut value: Value = serde_json::from_str(&decoded_json)
        .unwrap_or_else(|err| panic!("failed to parse decoded TOON JSON for {case_name}: {err}"));
    scrub_dynamic(&mut value, None);
    pretty_canonical_json(&value)
}

fn seed_snapshot_rows(conn: &rusqlite::Connection, rows: &[SnapshotRow]) {
    let topology = r#"{"schema_version":1,"captured_at":0,"windows":[]}"#;
    let mut seeded_sessions = std::collections::BTreeSet::new();
    for row in rows {
        if seeded_sessions.insert(row.session_id) {
            conn.execute(
                "INSERT INTO mux_sessions \
                 (session_id, created_at, topology_json, ft_version, shutdown_clean) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![row.session_id, row.checkpoint_at, topology, "test", 0i64],
            )
            .expect("insert mux session");
        }
        conn.execute(
            "INSERT INTO session_checkpoints \
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                row.session_id,
                row.checkpoint_at,
                row.checkpoint_type,
                row.state_hash,
                row.pane_count,
                row.total_bytes,
            ],
        )
        .expect("insert session checkpoint");
    }
}

fn run_snapshot_list_json(case_name: &str, rows: &[SnapshotRow], args: &[&str]) -> String {
    let (dir, workspace) = setup_workspace();
    let db_path = dir.path().join(".ft").join("ft.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");
    seed_snapshot_rows(&conn, rows);
    drop(conn);

    let output = Command::cargo_bin("ft")
        .expect("locate ft binary")
        .env("FT_WORKSPACE", &workspace)
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("snapshot list stdout should be utf-8");
    let mut value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("snapshot list JSON parse failed for {case_name}: {err}"));
    scrub_dynamic(&mut value, None);
    pretty_canonical_json(&value)
}

#[derive(Clone, Copy)]
struct SnapshotRow<'a> {
    session_id: &'a str,
    checkpoint_at: i64,
    checkpoint_type: &'a str,
    state_hash: &'a str,
    pane_count: i64,
    total_bytes: i64,
}

fn robot_toon_golden(case_name: &str) -> PathBuf {
    fixtures_dir()
        .join("robot_state_toon")
        .join(format!("{case_name}.toon"))
}

fn snapshot_json_golden(case_name: &str) -> PathBuf {
    fixtures_dir()
        .join("snapshot_list_json")
        .join(format!("{case_name}.json"))
}

#[test]
fn robot_state_toon_local_single_pane_matches_golden() {
    let actual = run_robot_state_toon("local_single_pane", "local_single_pane.json");
    assert_matches_golden(&actual, &robot_toon_golden("local_single_pane"));
}

#[test]
fn robot_state_toon_multi_pane_split_matches_golden() {
    let actual = run_robot_state_toon("multi_pane_split", "multi_pane_split.json");
    assert_matches_golden(&actual, &robot_toon_golden("multi_pane_split"));
}

#[test]
fn robot_state_toon_ssh_multiplexed_matches_golden() {
    let actual = run_robot_state_toon("ssh_multiplexed", "ssh_multiplexed.json");
    assert_matches_golden(&actual, &robot_toon_golden("ssh_multiplexed"));
}

#[test]
fn robot_state_toon_minimal_fields_matches_golden() {
    let actual = run_robot_state_toon("minimal_fields", "minimal_fields.json");
    assert_matches_golden(&actual, &robot_toon_golden("minimal_fields"));
}

#[test]
fn robot_state_toon_unicode_fields_matches_golden() {
    let actual = run_robot_state_toon("unicode_fields", "unicode_fields.json");
    assert_matches_golden(&actual, &robot_toon_golden("unicode_fields"));
}

#[test]
fn robot_state_redacts_title_and_cwd_secrets() {
    let raw_secret = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz12345678901234567890";
    let panes = serde_json::json!([
        {
            "pane_id": 4242,
            "tab_id": 7,
            "window_id": 3,
            "domain_name": "local",
            "title": format!("codex {raw_secret}"),
            "cwd": format!("file:///tmp/{raw_secret}")
        }
    ]);

    let actual = run_robot_state_toon_from_json("redacted_secret_fields", &panes);
    assert!(
        !actual.contains(raw_secret),
        "robot state output leaked raw secret: {actual}"
    );
    assert!(
        actual.contains("[REDACTED]"),
        "robot state output should include redaction marker: {actual}"
    );
}

#[test]
fn robot_get_text_and_search_redact_incident_secrets() {
    let (dir, workspace) = setup_workspace();
    let stub_path = write_wezterm_stub(&dir);
    let text_dir = dir.path().join("texts");
    fs::create_dir_all(&text_dir).expect("create robot incident text dir");
    fs::write(
        text_dir.join(format!("{INCIDENT_PANE_ID}.txt")),
        format!("incident alpha\nOPENAI_API_KEY={INCIDENT_SECRET}\nomega\n"),
    )
    .expect("write robot incident pane text");

    let panes = serde_json::json!([
        {
            "pane_id": INCIDENT_PANE_ID,
            "tab_id": 7,
            "window_id": 3,
            "domain_name": "local",
            "title": "incident-robot",
            "cwd": "file:///tmp/ft-incident-robot"
        }
    ]);
    let wezterm_json = dir.path().join("incident_panes.json");
    fs::write(
        &wezterm_json,
        serde_json::to_string_pretty(&panes).expect("serialize incident panes"),
    )
    .expect("write incident panes fixture");
    seed_robot_search_segment(
        &workspace,
        INCIDENT_PANE_ID,
        &format!("incident search OPENAI_API_KEY={INCIDENT_SECRET} stable"),
    );

    let get_text_command = [
        "robot", "--format", "json", "get-text", "4242", "--tail", "10",
    ];
    let get_text_stdout = Command::cargo_bin("ft")
        .expect("locate ft binary")
        .env("FT_WORKSPACE", &workspace)
        .env("FT_WEZTERM_CLI", &stub_path)
        .env("FT_TEST_WEZTERM_LIST_JSON", &wezterm_json)
        .env("FT_TEST_WEZTERM_TEXT_DIR", &text_dir)
        .args(get_text_command)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let get_text_response: Value =
        serde_json::from_slice(&get_text_stdout).expect("parse robot get-text JSON");
    assert_eq!(get_text_response["ok"], true);
    assert_robot_response_redacted("robot.get_text.redaction", &get_text_response);
    log_incident_drill_case(serde_json::json!({
        "id": "robot.get_text.redaction",
        "command": get_text_command,
        "redaction_tier": "secret-redactor",
        "policy_decision": "allow",
        "audit_row_id": Value::Null,
        "normalized_response": canonical_response_value(&get_text_response),
    }));

    let search_command = [
        "robot",
        "--format",
        "json",
        "search",
        "incident",
        "--pane",
        "4242",
        "--limit",
        "5",
        "--snippets=false",
        "--mode",
        "lexical",
    ];
    let search_stdout = Command::cargo_bin("ft")
        .expect("locate ft binary")
        .env("FT_WORKSPACE", &workspace)
        .args(search_command)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search_response: Value =
        serde_json::from_slice(&search_stdout).expect("parse robot search JSON");
    assert_eq!(search_response["ok"], true);
    assert_robot_response_redacted("robot.search.redaction", &search_response);
    log_incident_drill_case(serde_json::json!({
        "id": "robot.search.redaction",
        "command": search_command,
        "redaction_tier": "secret-redactor",
        "policy_decision": "allow",
        "audit_row_id": Value::Null,
        "normalized_response": canonical_response_value(&search_response),
    }));
}

#[test]
fn snapshot_list_json_single_periodic_matches_golden() {
    let rows = [SnapshotRow {
        session_id: "11111111-1111-1111-1111-111111111111",
        checkpoint_at: 1_710_000_000_001,
        checkpoint_type: "periodic",
        state_hash: "hash-single",
        pane_count: 3,
        total_bytes: 2048,
    }];
    let actual = run_snapshot_list_json(
        "single_periodic",
        &rows,
        &["snapshot", "list", "-f", "json", "--limit", "10"],
    );
    assert_matches_golden(&actual, &snapshot_json_golden("single_periodic"));
}

#[test]
fn snapshot_list_json_multi_snapshot_desc_order_matches_golden() {
    let rows = [
        SnapshotRow {
            session_id: "alpha-session",
            checkpoint_at: 1_710_000_000_010,
            checkpoint_type: "periodic",
            state_hash: "hash-old",
            pane_count: 2,
            total_bytes: 512,
        },
        SnapshotRow {
            session_id: "alpha-session",
            checkpoint_at: 1_710_000_000_030,
            checkpoint_type: "startup",
            state_hash: "hash-new",
            pane_count: 4,
            total_bytes: 4096,
        },
        SnapshotRow {
            session_id: "beta-session",
            checkpoint_at: 1_710_000_000_020,
            checkpoint_type: "shutdown",
            state_hash: "hash-mid",
            pane_count: 1,
            total_bytes: 128,
        },
    ];
    let actual = run_snapshot_list_json(
        "multi_desc_order",
        &rows,
        &["snapshot", "list", "-f", "json", "--limit", "10"],
    );
    assert_matches_golden(&actual, &snapshot_json_golden("multi_desc_order"));
}

#[test]
fn snapshot_list_json_session_filter_matches_golden() {
    let rows = [
        SnapshotRow {
            session_id: "22222222-2222-2222-2222-222222222222",
            checkpoint_at: 1_710_000_000_101,
            checkpoint_type: "periodic",
            state_hash: "hash-keep-a",
            pane_count: 5,
            total_bytes: 8192,
        },
        SnapshotRow {
            session_id: "22222222-2222-2222-2222-222222222222",
            checkpoint_at: 1_710_000_000_202,
            checkpoint_type: "startup",
            state_hash: "hash-keep-b",
            pane_count: 6,
            total_bytes: 16_384,
        },
        SnapshotRow {
            session_id: "33333333-3333-3333-3333-333333333333",
            checkpoint_at: 1_710_000_000_303,
            checkpoint_type: "periodic",
            state_hash: "hash-drop",
            pane_count: 7,
            total_bytes: 32_768,
        },
    ];
    let actual = run_snapshot_list_json(
        "session_filter",
        &rows,
        &[
            "snapshot",
            "list",
            "-f",
            "json",
            "--session",
            "22222222-2222-2222-2222-222222222222",
            "--limit",
            "10",
        ],
    );
    assert_matches_golden(&actual, &snapshot_json_golden("session_filter"));
}

#[test]
fn snapshot_list_json_empty_matches_golden() {
    let actual = run_snapshot_list_json(
        "empty",
        &[],
        &["snapshot", "list", "-f", "json", "--limit", "10"],
    );
    assert_matches_golden(&actual, &snapshot_json_golden("empty"));
}

#[test]
fn snapshot_list_json_limit_two_matches_golden() {
    let rows = [
        SnapshotRow {
            session_id: "limit-a",
            checkpoint_at: 1_710_000_000_010,
            checkpoint_type: "periodic",
            state_hash: "hash-a",
            pane_count: 1,
            total_bytes: 100,
        },
        SnapshotRow {
            session_id: "limit-b",
            checkpoint_at: 1_710_000_000_020,
            checkpoint_type: "periodic",
            state_hash: "hash-b",
            pane_count: 2,
            total_bytes: 200,
        },
        SnapshotRow {
            session_id: "limit-c",
            checkpoint_at: 1_710_000_000_030,
            checkpoint_type: "shutdown",
            state_hash: "hash-c",
            pane_count: 3,
            total_bytes: 300,
        },
    ];
    let actual = run_snapshot_list_json(
        "limit_two",
        &rows,
        &["snapshot", "list", "-f", "json", "--limit", "2"],
    );
    assert_matches_golden(&actual, &snapshot_json_golden("limit_two"));
}
