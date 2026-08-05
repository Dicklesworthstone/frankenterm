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

/// Initialize a temp workspace DB exactly where the `ft` binary derives it
/// (`Config::effective_db_path` = `<ws>/.ft/<db>`) with one known pane and no
/// events. Cursor bootstrap must happen against this empty history so tests
/// consume the CLI's opaque scope token rather than duplicating its hash logic.
fn empty_workspace() -> TempDir {
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
            storage.shutdown().await.expect("shutdown");
        });
    ws
}

fn seed_events(ws: &Path) {
    let db_path = Config::default().effective_db_path(ws);
    let db = db_path.to_string_lossy().to_string();
    RuntimeBuilder::current_thread()
        .build()
        .expect("runtime")
        .block_on(async move {
            let storage = StorageHandle::new(&db).await.expect("storage");
            let ts = 1_700_000_000_000_i64;
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
}

fn seed_rule_event(ws: &Path, rule_id: &str, ts: i64) -> i64 {
    let db_path = Config::default().effective_db_path(ws);
    let db = db_path.to_string_lossy().to_string();
    let rule_id = rule_id.to_string();
    RuntimeBuilder::current_thread()
        .build()
        .expect("runtime")
        .block_on(async move {
            let storage = StorageHandle::new(&db).await.expect("storage");
            let event_id = storage
                .record_event(event(&rule_id, "info", ts, None, None))
                .await
                .expect("record rule event");
            storage.shutdown().await.expect("shutdown");
            event_id
        })
}

fn seed_workspace() -> TempDir {
    let ws = empty_workspace();
    seed_events(ws.path());
    ws
}

fn ft(ws: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ft").expect("ft binary");
    cmd.env("FT_WORKSPACE", ws)
        .env("FT_WEZTERM_CLI", "/nonexistent/wezterm");
    cmd
}

#[derive(Debug, Clone)]
struct CursorToken {
    cursor: i64,
    epoch: String,
    scope: String,
}

fn ndjson_records(output: &str) -> Vec<serde_json::Value> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("malformed NDJSON line `{line}`: {error}"))
        })
        .collect()
}

fn checkpoint_token(output: &str) -> CursorToken {
    let checkpoint = ndjson_records(output)
        .into_iter()
        .find(|record| record["type"] == "cursor_checkpoint")
        .unwrap_or_else(|| panic!("missing cursor_checkpoint record: {output}"));
    CursorToken {
        cursor: checkpoint["cursor"]
            .as_i64()
            .expect("checkpoint cursor is an integer"),
        epoch: checkpoint["cursor_epoch"]
            .as_str()
            .expect("checkpoint epoch is a string")
            .to_string(),
        scope: checkpoint["cursor_scope"]
            .as_str()
            .expect("checkpoint scope is a string")
            .to_string(),
    }
}

fn await_result(output: &str) -> serde_json::Value {
    ndjson_records(output)
        .into_iter()
        .find(|record| record["type"] == "await_result")
        .unwrap_or_else(|| panic!("missing await_result record: {output}"))
}

fn await_result_token(output: &str) -> CursorToken {
    let result = await_result(output);
    CursorToken {
        cursor: result["final_cursor"]
            .as_i64()
            .expect("final cursor is an integer"),
        epoch: result["final_cursor_epoch"]
            .as_str()
            .expect("final cursor epoch is a string")
            .to_string(),
        scope: result["final_cursor_scope"]
            .as_str()
            .expect("final cursor scope is a string")
            .to_string(),
    }
}

fn bootstrap_watch(ws: &Path, options: &[&str]) -> CursorToken {
    let mut command = ft(ws);
    command.args(["robot", "watch-events"]).args(options);
    let output = stdout_of(command.assert().success());
    checkpoint_token(&output)
}

fn bootstrap_await(ws: &Path, conditions: &[&str]) -> CursorToken {
    let mut command = ft(ws);
    command
        .args(["robot", "await"])
        .args(conditions)
        .arg("--checkpoint-only");
    let output = stdout_of(command.assert().success());
    let records = ndjson_records(&output);
    assert_eq!(
        records.len(),
        1,
        "checkpoint-only must emit exactly one record: {output}"
    );
    checkpoint_token(&output)
}

fn different_cursor_epoch(epoch: &str) -> String {
    let replacement = if epoch.starts_with('0') { '1' } else { '0' };
    format!("{replacement}{}", &epoch[1..])
}

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout")
}

#[test]
fn invalid_events_arguments_do_not_create_or_open_storage() {
    let ws = TempDir::new().expect("temp workspace");
    let db_path = Config::default().effective_db_path(ws.path());
    assert!(!db_path.exists());
    let out = stdout_of(
        ft(ws.path())
            .args(["robot", "events", "--since=-1"])
            .assert()
            .success(),
    );
    assert!(out.contains("robot.invalid_args"), "{out}");
    assert!(
        !db_path.exists(),
        "pure validation must finish before SQLite open or migration: {}",
        db_path.display()
    );
}

#[test]
fn malformed_stream_cursor_tokens_are_rejected_without_echoing_untrusted_fields() {
    const VALID_SCOPE: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    let ws = TempDir::new().expect("temp workspace");
    for (surface, args) in [
        (
            "watch-events",
            vec![
                "robot",
                "watch-events",
                "--cursor",
                "7",
                "--cursor-epoch",
                CANARY,
                "--cursor-scope",
                VALID_SCOPE,
            ],
        ),
        (
            "await",
            vec![
                "robot",
                "await",
                "--all",
                "rule:build.done",
                "--cursor",
                "7",
                "--cursor-epoch",
                CANARY,
                "--cursor-scope",
                VALID_SCOPE,
            ],
        ),
    ] {
        let out = stdout_of(ft(ws.path()).args(args).assert().success());
        assert!(
            !out.contains(CANARY),
            "{surface} reflected an untrusted cursor epoch: {out}"
        );
        let records = ndjson_records(&out);
        assert_eq!(records.len(), 1, "{surface} must fail exactly once: {out}");
        let error = &records[0];
        assert_eq!(error["type"], "error", "{surface}: {out}");
        assert_eq!(error["code"], "robot.invalid_args", "{surface}: {out}");
        assert_eq!(error["cursor"], serde_json::Value::Null, "{surface}: {out}");
        assert_eq!(
            error["cursor_epoch"],
            serde_json::Value::Null,
            "{surface}: {out}"
        );
        assert_eq!(
            error["cursor_scope"],
            serde_json::Value::Null,
            "{surface}: {out}"
        );
    }
}

#[test]
fn watch_events_emits_ndjson_with_cursor_and_redacts_canary() {
    let ws = empty_workspace();
    let token = bootstrap_watch(ws.path(), &[]);
    seed_events(ws.path());
    let cursor = token.cursor.to_string();
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "watch-events",
                "--cursor",
                cursor.as_str(),
                "--cursor-epoch",
                token.epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
            ])
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
        out.contains("\"cursor_scope\":"),
        "missing per-record cursor scope: {out}"
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
fn cursorless_watch_establishes_tail_without_replaying_history() {
    let ws = seed_workspace();
    let out = stdout_of(
        ft(ws.path())
            .args(["robot", "watch-events"])
            .assert()
            .success(),
    );
    let records = ndjson_records(&out);
    assert_eq!(records.len(), 1, "fresh non-follow watch: {out}");
    assert_eq!(records[0]["type"], "cursor_checkpoint");
    assert_eq!(records[0]["reason"], "fresh_watch_tail_baseline");
    assert!(
        !out.contains("\"type\":\"event\""),
        "cursorless watch must not replay seeded history: {out}"
    );
}

#[test]
fn watch_stream_is_compact_json_even_when_global_format_is_toon() {
    let ws = empty_workspace();
    let out = stdout_of(
        ft(ws.path())
            .args(["robot", "--format", "toon", "watch-events"])
            .assert()
            .success(),
    );
    let records = ndjson_records(&out);
    assert_eq!(records.len(), 1, "fixed JSON stream output: {out}");
    assert_eq!(records[0]["type"], "cursor_checkpoint");
}

#[test]
fn watch_events_rule_glob_filter_excludes_non_matches() {
    let ws = empty_workspace();
    let token = bootstrap_watch(ws.path(), &["--rule-id", "codex.*"]);
    seed_events(ws.path());
    let cursor = token.cursor.to_string();
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "watch-events",
                "--rule-id",
                "codex.*",
                "--cursor",
                cursor.as_str(),
                "--cursor-epoch",
                token.epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
            ])
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
    let ws = empty_workspace();
    let token = bootstrap_watch(ws.path(), &[]);
    seed_events(ws.path());
    let stale_epoch = different_cursor_epoch(&token.epoch);
    let cursor = token.cursor.to_string();
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "watch-events",
                "--cursor",
                cursor.as_str(),
                "--cursor-epoch",
                stale_epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
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
fn watch_events_rejects_scope_reuse_after_filter_change() {
    let ws = empty_workspace();
    let token = bootstrap_watch(ws.path(), &["--rule-id", "codex.*"]);
    let cursor = token.cursor.to_string();
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "watch-events",
                "--cursor",
                cursor.as_str(),
                "--cursor-epoch",
                token.epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
            ])
            .assert()
            .success(),
    );
    let records = ndjson_records(&out);
    assert_eq!(records.len(), 1, "scope mismatch is terminal: {out}");
    assert_eq!(records[0]["type"], "cursor_discontinuity");
    assert_eq!(records[0]["reason"], "cursor_scope_mismatch");
    assert_eq!(records[0]["requested_cursor_scope"], token.scope);
    assert!(records[0].get("current_cursor_epoch").is_none());
}

#[test]
fn watch_claim_emits_pending_record_before_committed_checkpoint() {
    let ws = empty_workspace();
    let token = bootstrap_watch(ws.path(), &["--claim"]);
    seed_events(ws.path());
    let cursor = token.cursor.to_string();
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "watch-events",
                "--claim",
                "--cursor",
                cursor.as_str(),
                "--cursor-epoch",
                token.epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
            ])
            .assert()
            .success(),
    );
    let records = ndjson_records(&out);
    let (event_index, pending) = records
        .iter()
        .enumerate()
        .find(|(_, record)| record["type"] == "event")
        .unwrap_or_else(|| panic!("missing pending claim event: {out}"));
    assert_eq!(pending["cursor"], token.cursor);
    assert_eq!(pending["cursor_epoch"], token.epoch);
    assert_eq!(pending["cursor_scope"], token.scope);
    assert_eq!(pending["cursor_commit_state"], "pending_finalize");
    assert_eq!(pending["pending_finalize"], true);
    let candidate = pending["candidate_cursor"]
        .as_i64()
        .expect("pending claim has candidate cursor");
    let committed = records[event_index + 1..]
        .iter()
        .find(|record| {
            record["type"] == "cursor_checkpoint"
                && record["event_id"].as_i64() == Some(candidate)
        })
        .unwrap_or_else(|| panic!("missing post-finalization checkpoint: {out}"));
    assert_eq!(committed["cursor"], candidate);
    assert_eq!(committed["cursor_commit_state"], "committed");
}

#[test]
fn await_satisfies_on_matching_all_rule() {
    let ws = empty_workspace();
    let token = bootstrap_await(ws.path(), &["--all", "rule:codex.usage_reached"]);
    seed_events(ws.path());
    let cursor = token.cursor.to_string();
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "await",
                "--all",
                "rule:codex.usage_reached",
                "--cursor",
                cursor.as_str(),
                "--cursor-epoch",
                token.epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
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
    let ws = empty_workspace();
    let token = bootstrap_await(
        ws.path(),
        &[
            "--any",
            "rule:nope.nope",
            "--any",
            "rule:build.*",
        ],
    );
    seed_events(ws.path());
    let cursor = token.cursor.to_string();
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
                cursor.as_str(),
                "--cursor-epoch",
                token.epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
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
    let ws = empty_workspace();
    let token = bootstrap_await(ws.path(), &["--all", "rule:does.not.exist"]);
    seed_events(ws.path());
    let cursor = token.cursor.to_string();
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "await",
                "--all",
                "rule:does.not.exist",
                "--cursor",
                cursor.as_str(),
                "--cursor-epoch",
                token.epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
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
fn await_timeout_resume_replays_partial_all_rule_latches() {
    let ws = empty_workspace();
    let conditions = ["--all", "rule:phase.a", "--all", "rule:phase.b"];
    let token = bootstrap_await(ws.path(), &conditions);
    let event_a = seed_rule_event(ws.path(), "phase.a", 1_700_000_000_010);
    let cursor = token.cursor.to_string();
    let first = stdout_of(
        ft(ws.path())
            .args(["robot", "await"])
            .args(conditions)
            .args([
                "--cursor",
                cursor.as_str(),
                "--cursor-epoch",
                token.epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
                "--timeout-secs",
                "1",
            ])
            .assert()
            .success(),
    );
    let first_result = await_result(&first);
    assert_eq!(first_result["satisfied"], false, "{first}");
    assert_eq!(first_result["timed_out"], true, "{first}");
    let resume = await_result_token(&first);
    assert!(
        resume.cursor < event_a,
        "a timeout must hold before the first hidden rule latch: {first}"
    );

    let event_b = seed_rule_event(ws.path(), "phase.b", 1_700_000_000_011);
    let resume_cursor = resume.cursor.to_string();
    let second = stdout_of(
        ft(ws.path())
            .args(["robot", "await"])
            .args(conditions)
            .args([
                "--cursor",
                resume_cursor.as_str(),
                "--cursor-epoch",
                resume.epoch.as_str(),
                "--cursor-scope",
                resume.scope.as_str(),
                "--timeout-secs",
                "5",
            ])
            .assert()
            .success(),
    );
    let second_result = await_result(&second);
    assert_eq!(second_result["satisfied"], true, "{second}");
    assert_eq!(second_result["timed_out"], false, "{second}");
    assert_eq!(second_result["final_cursor"], event_b, "{second}");
}

#[test]
fn await_success_commits_only_the_exact_completing_occurrence() {
    let ws = empty_workspace();
    let conditions = ["--any", "rule:build.done"];
    let token = bootstrap_await(ws.path(), &conditions);
    let first_id = seed_rule_event(ws.path(), "build.done", 1_700_000_000_020);
    let second_id = seed_rule_event(ws.path(), "build.done", 1_700_000_000_021);
    let cursor = token.cursor.to_string();
    let first = stdout_of(
        ft(ws.path())
            .args(["robot", "await"])
            .args(conditions)
            .args([
                "--cursor",
                cursor.as_str(),
                "--cursor-epoch",
                token.epoch.as_str(),
                "--cursor-scope",
                token.scope.as_str(),
                "--timeout-secs",
                "5",
            ])
            .assert()
            .success(),
    );
    let first_result = await_result(&first);
    assert_eq!(first_result["satisfied"], true, "{first}");
    assert_eq!(first_result["final_cursor"], first_id, "{first}");

    let resume = await_result_token(&first);
    let resume_cursor = resume.cursor.to_string();
    let second = stdout_of(
        ft(ws.path())
            .args(["robot", "await"])
            .args(conditions)
            .args([
                "--cursor",
                resume_cursor.as_str(),
                "--cursor-epoch",
                resume.epoch.as_str(),
                "--cursor-scope",
                resume.scope.as_str(),
                "--timeout-secs",
                "5",
            ])
            .assert()
            .success(),
    );
    let second_result = await_result(&second);
    assert_eq!(second_result["satisfied"], true, "{second}");
    assert_eq!(second_result["final_cursor"], second_id, "{second}");
}

#[test]
fn await_quiescence_distinguishes_known_empty_and_missing_panes() {
    let ws = empty_workspace();
    let known_conditions = ["--all", "quiescence:1:0"];
    let known = bootstrap_await(ws.path(), &known_conditions);
    let known_cursor = known.cursor.to_string();
    let output = stdout_of(
        ft(ws.path())
            .args(["robot", "await"])
            .args(known_conditions)
            .args([
                "--cursor",
                known_cursor.as_str(),
                "--cursor-epoch",
                known.epoch.as_str(),
                "--cursor-scope",
                known.scope.as_str(),
                "--timeout-secs",
                "1",
            ])
            .assert()
            .success(),
    );
    assert_eq!(await_result(&output)["satisfied"], true, "{output}");

    let missing_conditions = ["--all", "quiescence:999:0"];
    let missing = bootstrap_await(ws.path(), &missing_conditions);
    let missing_cursor = missing.cursor.to_string();
    let output = stdout_of(
        ft(ws.path())
            .args(["robot", "await"])
            .args(missing_conditions)
            .args([
                "--cursor",
                missing_cursor.as_str(),
                "--cursor-epoch",
                missing.epoch.as_str(),
                "--cursor-scope",
                missing.scope.as_str(),
                "--timeout-secs",
                "1",
            ])
            .assert()
            .success(),
    );
    let records = ndjson_records(&output);
    assert_eq!(records.len(), 1, "{output}");
    assert_eq!(records[0]["type"], "error", "{output}");
    assert_eq!(records[0]["code"], "robot.pane_not_found", "{output}");
    assert_eq!(records[0]["cursor"], missing.cursor, "{output}");
}

#[test]
fn cursorless_await_checkpoints_then_ignores_seeded_history() {
    let ws = seed_workspace();
    let out = stdout_of(
        ft(ws.path())
            .args([
                "robot",
                "await",
                "--all",
                "rule:codex.usage_reached",
                "--timeout-secs",
                "1",
            ])
            .assert()
            .success(),
    );
    let records = ndjson_records(&out);
    assert_eq!(records.len(), 2, "fresh await output: {out}");
    assert_eq!(records[0]["type"], "cursor_checkpoint");
    assert_eq!(records[0]["reason"], "fresh_await_tail_baseline");
    assert_eq!(records[1]["type"], "await_result");
    assert_eq!(records[1]["satisfied"], false);
    assert_eq!(records[1]["timed_out"], true);
}

#[test]
fn await_rejects_unsupported_condition_sources() {
    let ws = seed_workspace();
    // state: requires the watcher IPC transport — a clear typed rejection,
    // never a silent hang or mis-evaluation. Storage-backed quiescence is
    // supported independently.
    ft(ws.path())
        .args([
            "robot",
            "await",
            "--all",
            "state:1:stuck",
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
