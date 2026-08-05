//! E2E coverage for `ft robot watch-events` + `ft robot await` (ft-7h5da.4.5).
//!
//! Seeds a temp-workspace DB with events (including a planted secret canary),
//! then drives the real `ft` binary to assert the DB-cursor paths end-to-end:
//! NDJSON framing + per-record cursor, redaction-before-emission (the canary
//! never streams), rule-glob filtering, composite `await --any/--all`
//! satisfaction + timeout, and the typed rejection of unsupported condition
//! sources.
//!
//! The IPC live-subscribe paths (watcher-up real-time delivery, bounded-
//! broadcast lag gap) ride the W3.1 IPC follow-on and are covered there; this
//! file deterministically exercises everything reachable without a running
//! watcher.

use assert_cmd::Command;
use frankenterm_core::config::Config;
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, StorageHandle, StoredEvent};
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

/// A well-known AWS access-key-id pattern; must never appear in any output.
const CANARY: &str = "AKIAIOSFODNN7EXAMPLE";

fn pane(ts: i64) -> PaneRecord {
    PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: None,
        cwd: None,
        tty_name: None,
        first_seen_at: ts,
        last_seen_at: ts,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn event(
    rule_id: &str,
    severity: &str,
    ts: i64,
    matched: Option<&str>,
    extracted: Option<serde_json::Value>,
) -> StoredEvent {
    StoredEvent {
        id: 0,
        pane_id: 1,
        rule_id: rule_id.to_string(),
        agent_type: "codex".to_string(),
        event_type: "detection".to_string(),
        severity: severity.to_string(),
        confidence: 0.9,
        extracted,
        matched_text: matched.map(str::to_string),
        segment_id: None,
        detected_at: ts,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

/// Seed a temp workspace DB exactly where the `ft` binary derives it
/// (`Config::effective_db_path` = `<ws>/.ft/<db>`), then return the workspace.
fn seed_workspace() -> TempDir {
    let ws = TempDir::new().expect("temp workspace");
    let db_path = Config::default().effective_db_path(ws.path());
    std::fs::create_dir_all(db_path.parent().expect("db parent")).expect("create .ft dir");
    let db = db_path.to_string_lossy().to_string();
    RuntimeBuilder::current_thread()
        .build()
        .expect("runtime")
        .block_on(async move {
            let storage = StorageHandle::new(&db).await.expect("storage");
            let ts = 1_700_000_000_000_i64;
            storage.upsert_pane(pane(ts)).await.expect("pane");
            // Planted secret canary in BOTH matched_text and the structured
            // `extracted` payload — redaction must scrub both.
            storage
                .record_event(event(
                    "codex.usage_reached",
                    "warning",
                    ts,
                    Some(&format!("leaked {CANARY} in output")),
                    Some(serde_json::json!({ "key": CANARY, "ok": true })),
                ))
                .await
                .expect("ev1");
            storage
                .record_event(event(
                    "build.failed",
                    "error",
                    ts + 1,
                    Some("build broke"),
                    None,
                ))
                .await
                .expect("ev2");
            storage
                .record_event(event("codex.idle", "info", ts + 2, None, None))
                .await
                .expect("ev3");
            storage.shutdown().await.expect("shutdown");
        });
    ws
}

fn ft(ws: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ft").expect("ft binary");
    cmd.env("FT_WORKSPACE", ws)
        .env("FT_WEZTERM_CLI", "/nonexistent/wezterm");
    cmd
}

fn current_cursor_epoch(ws: &Path) -> String {
    let db_path = Config::default().effective_db_path(ws);
    let connection = rusqlite::Connection::open(db_path).expect("open cursor epoch database");
    connection
        .query_row(
            "SELECT cursor_epoch FROM event_retention_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("read current event cursor epoch")
}

fn different_cursor_epoch(epoch: &str) -> String {
    let replacement = if epoch.starts_with('0') { '1' } else { '0' };
    format!("{replacement}{}", &epoch[1..])
}

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout")
}

#[test]
fn watch_events_emits_ndjson_with_cursor_and_redacts_canary() {
    let ws = seed_workspace();
    let out = stdout_of(
        ft(ws.path())
            .args(["robot", "watch-events"])
            .assert()
            .success(),
    );

    assert!(
        out.contains("\"type\":\"event\""),
        "no event records: {out}"
    );
    assert!(
        out.contains("\"cursor\":"),
        "missing per-record cursor: {out}"
    );
    assert!(
        out.contains("\"cursor_epoch\":"),
        "missing per-record cursor epoch: {out}"
    );
    assert!(
        out.contains("codex.usage_reached"),
        "missing seeded rule: {out}"
    );
    assert!(out.contains("build.failed"), "missing seeded rule: {out}");

    // Redaction-before-emission: the planted canary must NEVER stream (it was
    // seeded into both matched_text and extracted).
    assert!(
        !out.contains(CANARY),
        "SECRET CANARY LEAKED into the watch-events stream: {out}"
    );

    // Every non-empty line is a standalone JSON object (NDJSON framing).
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("malformed NDJSON line `{line}`: {e}"));
    }
}

#[test]
fn watch_events_rule_glob_filter_excludes_non_matches() {
    let ws = seed_workspace();
    let out = stdout_of(
        ft(ws.path())
            .args(["robot", "watch-events", "--rule-id", "codex.*"])
            .assert()
            .success(),
    );
    assert!(out.contains("codex.usage_reached"), "{out}");
    assert!(out.contains("codex.idle"), "{out}");
    assert!(
        !out.contains("build.failed"),
        "rule glob `codex.*` must exclude build.failed: {out}"
    );
}

#[test]
fn watch_events_stale_epoch_is_terminal_and_never_rebaselines() {
    let ws = seed_workspace();
    let stale_epoch = different_cursor_epoch(&current_cursor_epoch(ws.path()));
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "watch-events",
                "--cursor",
                "0",
                "--cursor-epoch",
                stale_epoch.as_str(),
            ])
            .assert()
            .success(),
    );
    assert!(out.contains("\"type\":\"cursor_discontinuity\""), "{out}");
    assert!(out.contains("\"terminal\":true"), "{out}");
    assert!(out.contains("\"reason\":\"cursor_epoch_mismatch\""), "{out}");
    assert!(
        !out.contains("\"type\":\"event\""),
        "a stale epoch must not emit or rebaseline onto retained events: {out}"
    );
}

#[test]
fn await_satisfies_on_matching_all_rule() {
    let ws = seed_workspace();
    let cursor_epoch = current_cursor_epoch(ws.path());
    // `--cursor 0` considers the already-seeded events.
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "await",
                "--all",
                "rule:codex.usage_reached",
                "--cursor",
                "0",
                "--cursor-epoch",
                cursor_epoch.as_str(),
                "--timeout-secs",
                "5",
            ])
            .assert()
            .success(),
    );
    assert!(out.contains("\"type\":\"await_result\""), "{out}");
    assert!(
        out.contains("\"satisfied\":true"),
        "should be satisfied: {out}"
    );
    assert!(!out.contains("\"timed_out\":true"), "{out}");
}

#[test]
fn await_any_satisfies_via_glob() {
    let ws = seed_workspace();
    let cursor_epoch = current_cursor_epoch(ws.path());
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "await",
                "--any",
                "rule:nope.nope",
                "--any",
                "rule:build.*",
                "--cursor",
                "0",
                "--cursor-epoch",
                cursor_epoch.as_str(),
                "--timeout-secs",
                "5",
            ])
            .assert()
            .success(),
    );
    assert!(
        out.contains("\"satisfied\":true"),
        "any-condition should match build.*: {out}"
    );
}

#[test]
fn await_times_out_when_no_condition_matches() {
    let ws = seed_workspace();
    let cursor_epoch = current_cursor_epoch(ws.path());
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "await",
                "--all",
                "rule:does.not.exist",
                "--cursor",
                "0",
                "--cursor-epoch",
                cursor_epoch.as_str(),
                "--timeout-secs",
                "1",
            ])
            .assert()
            .success(),
    );
    assert!(out.contains("\"timed_out\":true"), "should time out: {out}");
    assert!(out.contains("\"satisfied\":false"), "{out}");
}

#[test]
fn await_rejects_unsupported_condition_sources() {
    let ws = seed_workspace();
    let cursor_epoch = current_cursor_epoch(ws.path());
    // state: requires the watcher IPC transport — a clear typed rejection,
    // never a silent hang or mis-evaluation. Storage-backed quiescence is
    // supported independently.
    ft(ws.path())
        .args([
            "robot",
            "await",
            "--all",
            "state:1:stuck",
            "--cursor",
            "0",
            "--cursor-epoch",
            cursor_epoch.as_str(),
            "--timeout-secs",
            "1",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("IPC transport")
                .or(predicate::str::contains("state:/quiescence")),
        );
}
