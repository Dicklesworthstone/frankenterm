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
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

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
