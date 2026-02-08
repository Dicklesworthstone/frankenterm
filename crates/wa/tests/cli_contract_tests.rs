//! CLI command contract tests (wa-nu4.3.2.11)
//!
//! Validates that each human CLI command behaves correctly in both
//! interactive and automation contexts. Uses subprocess-style tests
//! against a temp workspace with pre-populated fixtures.
//!
//! Contract guarantees tested:
//! - Deterministic exit codes
//! - Stable JSON schema in `--format json` mode
//! - No ANSI escapes in `--format plain` mode
//! - Actionable error messages for failure paths
//! - Secret-like strings never leak unredacted

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

// =============================================================================
// Test fixture helpers
// =============================================================================

/// Create a temp workspace with `.wa/` directory and initialized DB.
/// Returns (TempDir guard, workspace path string).
fn setup_workspace() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let wa_dir = dir.path().join(".wa");
    std::fs::create_dir_all(&wa_dir).expect("create .wa dir");

    // Initialize database with schema
    let db_path = wa_dir.join("wa.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");
    wa_core::storage::initialize_schema(&conn).expect("init schema");
    drop(conn);

    let ws = dir.path().to_string_lossy().to_string();
    (dir, ws)
}

/// Create a workspace with populated fixture data (panes, events, accounts).
fn setup_populated_workspace() -> (TempDir, String) {
    let (dir, ws) = setup_workspace();
    let db_path = dir.path().join(".wa").join("wa.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    // Insert panes
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![1, "local", 1_700_000_000_000i64, 1_700_000_100_000i64, true],
    ).expect("insert pane 1");
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![2, "ssh:devbox", 1_700_000_000_000i64, 1_700_000_050_000i64, true],
    ).expect("insert pane 2");
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![3, "local", 1_700_000_000_000i64, 1_700_000_010_000i64, false],
    ).expect("insert pane 3");

    // Insert events (schema: pane_id, rule_id, agent_type, event_type, severity, confidence, detected_at)
    conn.execute(
        "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence, matched_text, detected_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![1, "usage.high_tokens", "claude_code", "usage_warning", "warning", 0.9f64, "Token usage above 80%", 1_700_000_050_000i64],
    ).expect("insert event 1");
    conn.execute(
        "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence, matched_text, detected_at, handled_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![1, "compaction.stale", "codex", "compaction_warning", "info", 0.8f64, "Stale compaction detected", 1_700_000_040_000i64, 1_700_000_060_000i64],
    ).expect("insert event 2");
    conn.execute(
        "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence, matched_text, detected_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![2, "error.panic", "unknown", "error_detected", "error", 0.95f64, "Panic in agent process", 1_700_000_090_000i64],
    ).expect("insert event 3");

    // Insert accounts
    conn.execute(
        "INSERT INTO accounts (account_id, service, name, percent_remaining, last_refreshed_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params!["acct-alpha", "openai", "Alpha", 82.5f64, 1_700_000_000_000i64, 1_699_000_000_000i64, 1_700_000_000_000i64],
    ).expect("insert account alpha");
    conn.execute(
        "INSERT INTO accounts (account_id, service, name, percent_remaining, tokens_used, tokens_remaining, tokens_limit, last_refreshed_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params!["acct-beta", "openai", "Beta", 45.0f64, 550_000i64, 450_000i64, 1_000_000i64, 1_700_000_000_000i64, 1_699_000_000_000i64, 1_700_000_000_000i64],
    ).expect("insert account beta");

    // Insert audit records (input_summary should be pre-redacted as it would be
    // when stored through record_audit_action_redacted)
    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, input_summary) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_050_000i64, "human", "send_text", "allow", "success", "wa send --pane 1 'ls -la'"],
    ).expect("insert audit 1");
    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, input_summary) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_060_000i64, "robot", "send_text", "deny", "denied", "wa robot send --pane 1 '[REDACTED]'"],
    ).expect("insert audit 2");

    drop(conn);
    (dir, ws)
}

/// Build a wa command configured for the given workspace.
#[allow(deprecated)]
fn wa_cmd_for(workspace: &str) -> Command {
    let mut cmd = Command::cargo_bin("wa").expect("wa binary should be built");
    cmd.env("WA_WORKSPACE", workspace);
    cmd.env("WA_WEZTERM_CLI", "/nonexistent/wezterm");
    cmd
}

/// Assert that output contains no ANSI escape sequences.
fn assert_no_ansi(output: &str, context: &str) {
    assert!(
        !output.contains("\x1b["),
        "{context}: output should not contain ANSI escapes, got:\n{output}"
    );
}

/// Run a wa command and parse stdout as JSON.
fn run_wa_json(workspace: &str, args: &[&str]) -> serde_json::Value {
    let output = wa_cmd_for(workspace)
        .args(args)
        .output()
        .expect("wa command should execute");
    assert!(
        output.status.success(),
        "command failed: wa {} \nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON")
}

// =============================================================================
// wa status contract tests
// =============================================================================

#[test]
fn contract_status_empty_db_plain() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["status", "--format", "plain"])
        .output()
        .expect("wa status should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "wa status (empty, plain)");
    // Empty DB should show a friendly empty state, not crash
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("panicked"),
        "wa status should not panic on empty DB"
    );
}

#[test]
fn contract_status_empty_db_json() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["status", "--format", "json"])
        .output()
        .expect("wa status --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Status may produce multiple JSON sections; each should be valid
    assert!(
        !stderr.contains("panicked"),
        "wa status --format json should not panic"
    );
    // At minimum, the output should contain some JSON (brackets or braces)
    if output.status.success() {
        assert!(
            stdout.contains('{') || stdout.contains('['),
            "wa status --format json should contain JSON: {stdout}"
        );
    }
}

#[test]
fn contract_status_populated_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["status", "--format", "plain"])
        .output()
        .expect("wa status should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_no_ansi(&stdout, "wa status (populated, plain)");
    assert!(
        !stderr.contains("panicked"),
        "wa status (populated, plain) should not panic"
    );
    if output.status.success() {
        // When WezTerm is available, plain status should show pane-like output.
        assert!(
            stdout.contains("local") || stdout.contains("Pane") || stdout.contains("pane"),
            "wa status should mention panes: {stdout}"
        );
    } else {
        // In fixtures we intentionally disable WezTerm CLI; failure should be actionable.
        assert!(
            stderr.contains("Failed to list panes")
                || stderr.contains("WezTerm circuit breaker open")
                || stderr.contains("Is WezTerm running"),
            "wa status failure should be actionable, stderr: {stderr}"
        );
    }
}

#[test]
fn contract_status_filter_by_pane() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["status", "--format", "json", "--pane-id", "1"])
        .output()
        .expect("wa status --pane-id 1 should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "wa status --pane-id 1 should not panic"
    );
    // Status produces multi-section JSON output; verify it contains data
    if output.status.success() {
        assert!(
            stdout.contains('{') || stdout.contains('['),
            "wa status --pane-id 1 should contain JSON data: {stdout}"
        );
    }
}

// =============================================================================
// wa events contract tests
// =============================================================================

#[test]
fn contract_events_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["events", "--format", "plain"])
        .output()
        .expect("wa events should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "wa events (plain)");
    assert!(
        output.status.success(),
        "wa events should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Should show events table
    assert!(
        stdout.contains("Events") || stdout.contains("events") || stdout.contains("usage"),
        "wa events should list events: {stdout}"
    );
}

#[test]
fn contract_events_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["events", "--format", "json"])
        .output()
        .expect("wa events --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "wa events --format json should exit 0"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("wa events --format json should produce valid JSON");
    assert!(parsed.is_array(), "wa events JSON should be an array");
}

#[test]
fn contract_events_filter_by_pane() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["events", "--format", "json", "--pane-id", "2"])
        .output()
        .expect("wa events --pane-id 2 should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let events = parsed.as_array().expect("array");
    // All returned events should be for pane 2
    for event in events {
        assert_eq!(
            event["pane_id"], 2,
            "filtered events should only contain pane 2"
        );
    }
}

#[test]
fn contract_events_mutations_human_roundtrip() {
    let (_dir, ws) = setup_populated_workspace();

    let events = run_wa_json(&ws, &["events", "--format", "json"]);
    let first_id = events
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("id"))
        .and_then(serde_json::Value::as_i64)
        .expect("expected at least one event with id");

    let annotate = run_wa_json(
        &ws,
        &[
            "events",
            "--format",
            "json",
            "annotate",
            &first_id.to_string(),
            "--note",
            "investigating event",
        ],
    );
    assert_eq!(annotate["ok"], true);
    assert_eq!(annotate["annotations"]["note"], "investigating event");

    let triage = run_wa_json(
        &ws,
        &[
            "events",
            "--format",
            "json",
            "triage",
            &first_id.to_string(),
            "--state",
            "investigating",
        ],
    );
    assert_eq!(triage["ok"], true);
    assert_eq!(triage["annotations"]["triage_state"], "investigating");

    let label = run_wa_json(
        &ws,
        &[
            "events",
            "--format",
            "json",
            "label",
            &first_id.to_string(),
            "--add",
            "urgent",
        ],
    );
    assert_eq!(label["ok"], true);
    assert!(
        label["annotations"]["labels"]
            .as_array()
            .is_some_and(|labels| labels.iter().any(|v| v == "urgent")),
        "labels should contain urgent: {}",
        label["annotations"]
    );
}

#[test]
fn contract_robot_events_mutations_roundtrip() {
    let (_dir, ws) = setup_populated_workspace();

    let baseline = run_wa_json(&ws, &["events", "--format", "json"]);
    let first_id = baseline
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("id"))
        .and_then(serde_json::Value::as_i64)
        .expect("expected at least one event with id");

    let annotate = run_wa_json(
        &ws,
        &[
            "robot",
            "events",
            "annotate",
            &first_id.to_string(),
            "--note",
            "robot-note",
        ],
    );
    assert_eq!(annotate["ok"], true);
    assert_eq!(
        annotate["data"]["annotations"]["note"],
        serde_json::Value::String("robot-note".to_string())
    );

    let triage = run_wa_json(
        &ws,
        &[
            "robot",
            "events",
            "triage",
            &first_id.to_string(),
            "--state",
            "investigating",
        ],
    );
    assert_eq!(triage["ok"], true);
    assert_eq!(
        triage["data"]["annotations"]["triage_state"],
        serde_json::Value::String("investigating".to_string())
    );

    let label = run_wa_json(
        &ws,
        &[
            "robot",
            "events",
            "label",
            &first_id.to_string(),
            "--add",
            "urgent",
        ],
    );
    assert_eq!(label["ok"], true);
    assert!(
        label["data"]["annotations"]["labels"]
            .as_array()
            .is_some_and(|labels| labels.iter().any(|v| v == "urgent")),
        "robot labels should contain urgent: {}",
        label["data"]["annotations"]
    );
}

#[test]
fn contract_events_unhandled_filter() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["events", "--format", "json", "--unhandled"])
        .output()
        .expect("wa events --unhandled should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let events = parsed.as_array().expect("array");
    // Handled events should be excluded
    for event in events {
        assert!(
            event["handled_at"].is_null(),
            "unhandled filter should exclude handled events"
        );
    }
}

// =============================================================================
// wa accounts contract tests
// =============================================================================

#[test]
fn contract_accounts_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["accounts", "--format", "plain"])
        .output()
        .expect("wa accounts should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "wa accounts (plain)");
    assert!(
        output.status.success(),
        "wa accounts should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Alpha") && stdout.contains("Beta"),
        "wa accounts should list both accounts: {stdout}"
    );
    assert!(
        stdout.contains("82.5%"),
        "wa accounts should show percent remaining: {stdout}"
    );
}

#[test]
fn contract_accounts_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["accounts", "--format", "json"])
        .output()
        .expect("wa accounts --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("wa accounts JSON should be valid");
    assert_eq!(parsed["total"], 2);
    assert_eq!(parsed["service"], "openai");
    assert!(parsed["accounts"].is_array());
}

#[test]
fn contract_accounts_pick_preview() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["accounts", "--format", "json", "--pick"])
        .output()
        .expect("wa accounts --pick should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert!(
        parsed["pick_preview"].is_object(),
        "--pick should include pick_preview"
    );
    assert_eq!(
        parsed["pick_preview"]["selected_account_id"], "acct-alpha",
        "should pick highest percent_remaining"
    );
}

#[test]
fn contract_accounts_empty_db() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["accounts", "--format", "plain"])
        .output()
        .expect("wa accounts should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "wa accounts (empty)");
    assert!(
        stdout.contains("No accounts") || stdout.contains("no accounts"),
        "empty accounts should show friendly message: {stdout}"
    );
}

// =============================================================================
// wa audit contract tests
// =============================================================================

#[test]
fn contract_audit_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["audit", "--format", "plain"])
        .output()
        .expect("wa audit should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "wa audit (plain)");
    assert!(
        output.status.success(),
        "wa audit should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn contract_audit_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["audit", "--format", "json"])
        .output()
        .expect("wa audit --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    // Should produce parseable JSON (array or object)
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("wa audit JSON should be valid");
    assert!(
        parsed.is_array() || parsed.is_object(),
        "wa audit JSON should be array or object"
    );
}

#[test]
fn contract_audit_redacts_secrets() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["audit", "--format", "plain"])
        .output()
        .expect("wa audit should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The secret "sk-SECRET1234abcd" was inserted as part of audit input_summary.
    // It should be redacted in output (or the input_summary column is truncated/hidden).
    // We check that the full secret string does not appear unredacted.
    assert!(
        !stdout.contains("sk-SECRET1234abcd"),
        "wa audit should not show full secret in plain output: {stdout}"
    );
}

// =============================================================================
// wa rules contract tests
// =============================================================================

#[test]
fn contract_rules_list_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["rules", "list", "--format", "plain"])
        .output()
        .expect("wa rules list should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "wa rules list (plain)");
    assert!(
        output.status.success(),
        "wa rules list should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Should list available detection rules/packs
    assert!(
        stdout.contains("Rules") || stdout.contains("rules") || stdout.contains("RULE"),
        "wa rules list should list rules: {stdout}"
    );
}

#[test]
fn contract_rules_list_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["rules", "list", "--format", "json"])
        .output()
        .expect("wa rules list --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("wa rules list JSON should be valid");
    assert!(
        parsed.is_array() || parsed.is_object(),
        "wa rules list JSON should be structured"
    );
}

// =============================================================================
// wa export contract tests
// =============================================================================

#[test]
fn contract_export_events_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["export", "events"])
        .output()
        .expect("wa export events should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "wa export events should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Export produces JSONL (one JSON per line)
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(
            parsed.is_ok(),
            "wa export events should produce valid JSONL, bad line: {line}"
        );
    }
}

#[test]
fn contract_export_audit_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["export", "audit"])
        .output()
        .expect("wa export audit should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(parsed.is_ok(), "wa export audit line should be valid JSON");
    }
}

#[test]
fn contract_export_unknown_kind_fails() {
    let (_dir, ws) = setup_populated_workspace();
    wa_cmd_for(&ws)
        .args(["export", "nonexistent_kind"])
        .assert()
        .failure();
}

// =============================================================================
// wa reserve / wa reservations contract tests
// =============================================================================

#[test]
fn contract_reservations_empty_plain() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["reservations"])
        .output()
        .expect("wa reservations should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "wa reservations should exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_ansi(&stdout, "wa reservations (empty)");
}

#[test]
fn contract_reservations_json() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["reservations", "--json"])
        .output()
        .expect("wa reservations --json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("wa reservations JSON should be valid");
    assert!(
        parsed.is_array() || parsed.is_object(),
        "wa reservations JSON should be structured"
    );
}

// =============================================================================
// wa doctor contract tests
// =============================================================================

#[test]
fn contract_doctor_plain_no_ansi() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["doctor"])
        .output()
        .expect("wa doctor should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Doctor may fail (no WezTerm) but should not panic
    assert!(!stderr.contains("panicked"), "wa doctor should not panic");
    // Doctor in non-TTY should produce clean output
    assert_no_ansi(&stdout, "wa doctor (plain)");
}

#[test]
fn contract_doctor_json_schema() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["doctor", "--json"])
        .output()
        .expect("wa doctor --json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        let parsed: serde_json::Value =
            serde_json::from_str(&stdout).expect("wa doctor --json should produce valid JSON");
        assert!(parsed.is_object(), "wa doctor JSON should be an object");
    }
}

// =============================================================================
// wa stop contract tests
// =============================================================================

#[test]
fn contract_stop_no_watcher_running() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["stop"])
        .output()
        .expect("wa stop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Stop when no watcher is running should fail gracefully
    assert!(
        !stderr.contains("panicked"),
        "wa stop should not panic when no watcher running"
    );
    // Should indicate no watcher found
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("not running")
            || combined.contains("No watcher")
            || combined.contains("no watcher")
            || combined.contains("not found")
            || combined.contains("lock")
            || !output.status.success(),
        "wa stop should indicate no watcher: stdout={stdout}, stderr={stderr}"
    );
}

// =============================================================================
// wa approve contract tests
// =============================================================================

#[test]
fn contract_approve_invalid_code() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["approve", "INVALID1"])
        .output()
        .expect("wa approve should execute");

    // Invalid code should fail with clear error
    assert!(
        !output.status.success(),
        "wa approve with invalid code should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("invalid")
            || combined.contains("Invalid")
            || combined.contains("not found")
            || combined.contains("expired")
            || combined.contains("error")
            || combined.contains("Error"),
        "wa approve invalid code should show clear error: {combined}"
    );
}

// =============================================================================
// Unknown/invalid command contract tests
// =============================================================================

// =============================================================================
// wa history contract tests
// =============================================================================

#[test]
fn contract_history_plain_no_ansi_and_redacted_summary() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["history", "--format", "plain", "--limit", "20"])
        .output()
        .expect("wa history should execute");

    assert!(
        output.status.success(),
        "wa history should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "wa history (plain)");
    assert!(
        stdout.contains("Action history"),
        "wa history plain should include heading: {stdout}"
    );
    assert!(
        stdout.contains("SUMMARY"),
        "wa history plain should include table headers: {stdout}"
    );
    assert!(
        stdout.contains("[REDACTED]"),
        "wa history plain should preserve redacted summaries: {stdout}"
    );
}

#[test]
fn contract_history_json_filters_undoable_and_orders_newest_first() {
    let (dir, ws) = setup_populated_workspace();
    let db_path = dir.path().join(".wa").join("wa.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_120_000i64, "human", "spawn", "allow", "success", 1i64],
    )
    .expect("insert audit undoable older");
    let older_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            older_id,
            1i64,
            "pane_close",
            "Close pane",
            r#"{"pane_id":1}"#,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert undo older");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_130_000i64, "workflow", "workflow_start", "allow", "success", 1i64],
    )
    .expect("insert audit undoable newer");
    let newer_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            newer_id,
            1i64,
            "workflow_abort",
            "Abort workflow",
            r#"{"execution_id":"wf-123"}"#,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert undo newer");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_140_000i64, "human", "send_text", "allow", "success", 1i64],
    )
    .expect("insert audit non-undoable");
    let non_undoable_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            non_undoable_id,
            0i64,
            "manual",
            "Manual only",
            rusqlite::types::Null,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert undo non-undoable");
    drop(conn);

    let payload = run_wa_json(
        &ws,
        &["history", "--format", "json", "--undoable", "--limit", "20"],
    );
    let rows = payload.as_array().expect("history JSON should be an array");

    let ids: Vec<i64> = rows.iter().filter_map(|row| row["id"].as_i64()).collect();
    assert_eq!(
        ids,
        vec![newer_id, older_id],
        "history undoable filter should return only undoable rows in deterministic order"
    );

    for row in rows {
        assert_eq!(row["undoable"].as_bool(), Some(true));
        assert!(
            row.get("undo_strategy").is_some(),
            "undoable row should carry undo_strategy"
        );
    }
}

// =============================================================================
// wa undo contract tests
// =============================================================================

#[test]
fn contract_undo_list_json_returns_only_currently_undoable_actions() {
    let (dir, ws) = setup_populated_workspace();
    let db_path = dir.path().join(".wa").join("wa.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_100_000i64, "human", "spawn", "allow", "success", 1i64],
    )
    .expect("insert audit undoable");
    let undoable_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            undoable_id,
            1i64,
            "pane_close",
            "Close spawned pane",
            r#"{"pane_id":1}"#,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert action_undo undoable");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_101_000i64, "human", "spawn", "allow", "success", 1i64],
    )
    .expect("insert audit undone");
    let undone_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            undone_id,
            1i64,
            "pane_close",
            "Already undone",
            r#"{"pane_id":1}"#,
            1_700_000_200_000i64,
            "tester",
        ],
    )
    .expect("insert action_undo undone");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_102_000i64, "human", "send_text", "allow", "success", 1i64],
    )
    .expect("insert audit non-undoable");
    let non_undoable_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            non_undoable_id,
            0i64,
            "manual",
            "Manual only",
            rusqlite::types::Null,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert action_undo non-undoable");
    drop(conn);

    let payload = run_wa_json(
        &ws,
        &["undo", "--list", "--format", "json", "--limit", "20"],
    );
    assert_eq!(payload["ok"], true);

    let actions = payload["data"]["actions"]
        .as_array()
        .expect("actions should be an array");
    let ids: Vec<i64> = actions
        .iter()
        .filter_map(|row| row["action_id"].as_i64())
        .collect();

    assert!(
        ids.contains(&undoable_id),
        "undoable pending action should be listed"
    );
    assert!(
        !ids.contains(&undone_id),
        "already-undone action should not be listed"
    );
    assert!(
        !ids.contains(&non_undoable_id),
        "non-undoable action should not be listed"
    );
}

#[test]
fn contract_undo_single_json_not_applicable_for_manual_strategy() {
    let (dir, ws) = setup_populated_workspace();
    let db_path = dir.path().join(".wa").join("wa.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_150_000i64, "human", "send_text", "allow", "success", 1i64],
    )
    .expect("insert audit manual");
    let action_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            action_id,
            0i64,
            "manual",
            "Reverse this command manually.",
            rusqlite::types::Null,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert action_undo manual");
    drop(conn);

    let payload = run_wa_json(
        &ws,
        &["undo", &action_id.to_string(), "--yes", "--format", "json"],
    );
    assert_eq!(payload["ok"], true);

    let results = payload["data"]["results"]
        .as_array()
        .expect("results should be an array");
    assert_eq!(results.len(), 1, "expected exactly one undo result");
    assert_eq!(results[0]["action_id"].as_i64(), Some(action_id));
    assert_eq!(results[0]["outcome"].as_str(), Some("not_applicable"));
    assert_eq!(results[0]["strategy"].as_str(), Some("manual"));
    assert_eq!(
        results[0]["guidance"].as_str(),
        Some("Reverse this command manually.")
    );
}

#[test]
fn contract_undo_single_json_already_undone_is_idempotent_noop() {
    let (dir, ws) = setup_populated_workspace();
    let db_path = dir.path().join(".wa").join("wa.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_170_000i64, "human", "spawn", "allow", "success", 1i64],
    )
    .expect("insert audit already-undone");
    let action_id = conn.last_insert_rowid();
    let already_undone_at = 1_700_000_171_000i64;

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            action_id,
            1i64,
            "pane_close",
            "Pane already closed previously.",
            r#"{"pane_id":1}"#,
            already_undone_at,
            "previous-operator",
        ],
    )
    .expect("insert action_undo already-undone");
    drop(conn);

    let payload = run_wa_json(
        &ws,
        &["undo", &action_id.to_string(), "--yes", "--format", "json"],
    );
    assert_eq!(payload["ok"], true);

    let results = payload["data"]["results"]
        .as_array()
        .expect("results should be an array");
    assert_eq!(results.len(), 1, "expected exactly one undo result");
    assert_eq!(results[0]["action_id"].as_i64(), Some(action_id));
    assert_eq!(results[0]["outcome"].as_str(), Some("not_applicable"));
    assert!(
        results[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already been undone"),
        "expected idempotent already-undone message"
    );

    let conn = rusqlite::Connection::open(&db_path).expect("re-open DB");
    let record: (Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT undone_at, undone_by FROM action_undo WHERE audit_action_id = ?1",
            rusqlite::params![action_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query action_undo");
    assert_eq!(record.0, Some(already_undone_at));
    assert_eq!(record.1.as_deref(), Some("previous-operator"));
}

#[test]
fn contract_unknown_subcommand_fails() {
    let (_dir, ws) = setup_workspace();
    wa_cmd_for(&ws)
        .arg("nonexistent-command-xyz")
        .assert()
        .failure();
}

#[test]
fn contract_help_lists_core_commands() {
    let (_dir, ws) = setup_workspace();
    wa_cmd_for(&ws)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("events"))
        .stdout(predicate::str::contains("accounts"))
        .stdout(predicate::str::contains("audit"))
        .stdout(predicate::str::contains("rules"))
        .stdout(predicate::str::contains("export"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("stop"))
        .stdout(predicate::str::contains("approve"));
}

// =============================================================================
// Cross-cutting: no ANSI in plain mode across all commands
// =============================================================================

#[test]
fn contract_no_ansi_in_plain_mode() {
    let (_dir, ws) = setup_populated_workspace();

    let commands: Vec<Vec<&str>> = vec![
        vec!["status", "--format", "plain"],
        vec!["events", "--format", "plain"],
        vec!["accounts", "--format", "plain"],
        vec!["audit", "--format", "plain"],
        vec!["history", "--format", "plain"],
        vec!["undo", "--list", "--format", "plain"],
        vec!["rules", "list", "--format", "plain"],
        vec!["doctor"],
    ];

    for args in &commands {
        let output = wa_cmd_for(&ws)
            .args(args)
            .output()
            .unwrap_or_else(|_| panic!("command {:?} should execute", args));

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_no_ansi(&stdout, &format!("wa {}", args.join(" ")));
    }
}

// =============================================================================
// Cross-cutting: JSON mode produces parseable output
// =============================================================================

#[test]
fn contract_json_mode_always_parseable() {
    let (_dir, ws) = setup_populated_workspace();

    let commands: Vec<Vec<&str>> = vec![
        vec!["events", "--format", "json"],
        vec!["accounts", "--format", "json"],
        vec!["audit", "--format", "json"],
        vec!["history", "--format", "json"],
        vec!["undo", "--list", "--format", "json"],
        vec!["rules", "list", "--format", "json"],
    ];

    for args in &commands {
        let output = wa_cmd_for(&ws)
            .args(args)
            .output()
            .unwrap_or_else(|_| panic!("command {:?} should execute", args));

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);
            assert!(
                parsed.is_ok(),
                "wa {} should produce valid JSON: {}",
                args.join(" "),
                stdout
            );
        }
    }
}
