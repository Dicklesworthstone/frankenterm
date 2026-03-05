//! LabRuntime port of all `#[tokio::test]` async tests from `export.rs`.
//!
//! Each test that previously used `#[tokio::test]` is wrapped in
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { … })`.
//! Feature-gated behind `asupersync-runtime`.

#![cfg(feature = "asupersync-runtime")]

mod common;

use frankenterm_core::export::{ExportKind, ExportOptions, export_jsonl};
use frankenterm_core::storage::{ExportQuery, StorageHandle};

use common::fixtures::RuntimeFixture;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_db_path(suffix: &str) -> String {
    std::env::temp_dir()
        .join(format!(
            "wa_labrt_export_{}_{}.db",
            suffix,
            std::process::id()
        ))
        .to_str()
        .unwrap()
        .to_string()
}

async fn test_db_with_pane(suffix: &str) -> (StorageHandle, String) {
    let db_path = temp_db_path(suffix);
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));

    let storage = StorageHandle::new(&db_path).await.unwrap();

    let pane = frankenterm_core::storage::PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: None,
        cwd: None,
        tty_name: None,
        first_seen_at: 1000,
        last_seen_at: 1000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    };
    storage.upsert_pane(pane).await.unwrap();

    (storage, db_path)
}

async fn teardown(storage: StorageHandle, db_path: &str) {
    storage.shutdown().await.unwrap();
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

/// Expected required fields for each exported record type.
fn expected_segment_fields() -> Vec<&'static str> {
    vec![
        "id",
        "pane_id",
        "seq",
        "content",
        "content_len",
        "captured_at",
    ]
}

fn expected_gap_fields() -> Vec<&'static str> {
    vec![
        "id",
        "pane_id",
        "seq_before",
        "seq_after",
        "reason",
        "detected_at",
    ]
}

fn expected_event_fields() -> Vec<&'static str> {
    vec![
        "id",
        "pane_id",
        "rule_id",
        "agent_type",
        "event_type",
        "severity",
        "confidence",
        "detected_at",
    ]
}

fn expected_workflow_fields() -> Vec<&'static str> {
    vec![
        "id",
        "workflow_name",
        "pane_id",
        "current_step",
        "status",
        "started_at",
        "updated_at",
    ]
}

fn expected_step_log_fields() -> Vec<&'static str> {
    vec![
        "id",
        "workflow_id",
        "step_index",
        "step_name",
        "result_type",
        "started_at",
        "completed_at",
        "duration_ms",
    ]
}

fn expected_session_fields() -> Vec<&'static str> {
    vec!["id", "pane_id", "agent_type", "started_at"]
}

fn expected_reservation_fields() -> Vec<&'static str> {
    vec![
        "id",
        "pane_id",
        "owner_kind",
        "owner_id",
        "created_at",
        "expires_at",
        "status",
    ]
}

fn expected_header_fields() -> Vec<&'static str> {
    vec![
        "_export",
        "version",
        "kind",
        "redacted",
        "exported_at_ms",
        "record_count",
    ]
}

/// Helper: parse JSONL output into header + records
fn parse_jsonl(output: &str) -> (serde_json::Value, Vec<serde_json::Value>) {
    let lines: Vec<&str> = output.trim().lines().collect();
    assert!(
        !lines.is_empty(),
        "Export output must have at least a header line"
    );
    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let records: Vec<serde_json::Value> = lines[1..]
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (header, records)
}

/// Validate that a JSON object contains all expected fields.
fn assert_fields_present(obj: &serde_json::Value, expected: &[&str], context: &str) {
    let map = obj.as_object().expect("Expected JSON object");
    for field in expected {
        assert!(
            map.contains_key(*field),
            "{context}: missing expected field '{field}'"
        );
    }
}

// ===========================================================================
// 1. Basic segment export
// ===========================================================================

#[test]
fn export_segments_to_buffer() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("seg_buf").await;

        storage
            .append_segment(1, "test content", None)
            .await
            .unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 record

        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["_export"], true);
        assert_eq!(header["kind"], "segments");
        assert_eq!(header["record_count"], 1);

        let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(record["content"], "test content");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 2. Redaction
// ===========================================================================

#[test]
fn export_with_redaction() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("redact").await;

        storage
            .append_segment(
                1,
                "secret: sk-abc123def456ghi789jkl012mno345pqr678stu901v",
                None,
            )
            .await
            .unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: true,
            pretty: false,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.trim().lines().collect();
        let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        let content = record["content"].as_str().unwrap();
        assert!(content.contains("[REDACTED]"));
        assert!(!content.contains("sk-abc123"));

        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["redacted"], true);

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 3. Pane filter
// ===========================================================================

#[test]
fn export_with_pane_filter() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path("pane_filter");
        let _ = std::fs::remove_file(&db_path);
        let storage = StorageHandle::new(&db_path).await.unwrap();

        for pane_id in [1u64, 2u64] {
            let pane = frankenterm_core::storage::PaneRecord {
                pane_id,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 1000,
                last_seen_at: 1000,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            };
            storage.upsert_pane(pane).await.unwrap();
        }

        storage.append_segment(1, "pane1 data", None).await.unwrap();
        storage.append_segment(2, "pane2 data", None).await.unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery {
                pane_id: Some(1),
                ..Default::default()
            },
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("pane1 data"));
        assert!(!output.contains("pane2 data"));

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 4. Pretty format
// ===========================================================================

#[test]
fn export_pretty_format() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("pretty").await;

        storage.append_segment(1, "test", None).await.unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: true,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("  \""));

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 5. Audit with actor/action filter
// ===========================================================================

#[test]
fn export_audit_with_actor_filter() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("audit_filter").await;

        let action1 = frankenterm_core::storage::AuditActionRecord {
            id: 0,
            ts: 1000,
            actor_kind: "workflow".to_string(),
            actor_id: Some("wf-1".to_string()),
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "auth_required".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "ok".to_string(),
        };
        storage.record_audit_action(action1).await.unwrap();

        let action2 = frankenterm_core::storage::AuditActionRecord {
            id: 0,
            ts: 2000,
            actor_kind: "operator".to_string(),
            actor_id: Some("human-1".to_string()),
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "ok".to_string(),
        };
        storage.record_audit_action(action2).await.unwrap();

        // Export with actor filter = "workflow"
        let opts = ExportOptions {
            kind: ExportKind::Audit,
            query: ExportQuery::default(),
            audit_actor: Some("workflow".to_string()),
            audit_action: None,
            redact: true,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 2);

        let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(record["actor_kind"], "workflow");
        assert_eq!(record["action_kind"], "auth_required");

        // Export with action filter = "send_text"
        let opts2 = ExportOptions {
            kind: ExportKind::Audit,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: Some("send_text".to_string()),
            redact: true,
            pretty: false,
        };

        let mut buf2 = Vec::new();
        let count2 = export_jsonl(&storage, &opts2, &mut buf2).await.unwrap();
        assert_eq!(count2, 1);

        let output2 = String::from_utf8(buf2).unwrap();
        let lines2: Vec<&str> = output2.trim().lines().collect();
        let record2: serde_json::Value = serde_json::from_str(lines2[1]).unwrap();
        assert_eq!(record2["actor_kind"], "operator");
        assert_eq!(record2["action_kind"], "send_text");

        // Export all audit (no actor/action filter) should return 2
        let opts3 = ExportOptions {
            kind: ExportKind::Audit,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: true,
            pretty: false,
        };

        let mut buf3 = Vec::new();
        let count3 = export_jsonl(&storage, &opts3, &mut buf3).await.unwrap();
        assert_eq!(count3, 2);

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 6. Audit redaction
// ===========================================================================

#[test]
fn export_audit_redacts_fields() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("audit_redact").await;

        let action = frankenterm_core::storage::AuditActionRecord {
            id: 0,
            ts: 1000,
            actor_kind: "workflow".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: None,
            action_kind: "test".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some(
                "API key: sk-abc123def456ghi789jkl012mno345pqr678stu901v".to_string(),
            ),
            rule_id: None,
            input_summary: Some(
                "input with sk-abc123def456ghi789jkl012mno345pqr678stu901v secret".to_string(),
            ),
            verification_summary: None,
            decision_context: None,
            result: "ok".to_string(),
        };
        storage.record_audit_action(action).await.unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Audit,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: true,
            pretty: false,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.trim().lines().collect();
        let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();

        let reason = record["decision_reason"].as_str().unwrap();
        assert!(reason.contains("[REDACTED]"));
        assert!(!reason.contains("sk-abc123"));

        let summary = record["input_summary"].as_str().unwrap();
        assert!(summary.contains("[REDACTED]"));
        assert!(!summary.contains("sk-abc123"));

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 7. Empty export produces header only
// ===========================================================================

#[test]
fn export_empty_produces_header_only() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("empty").await;

        let opts = ExportOptions {
            kind: ExportKind::Events,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 0);

        let output = String::from_utf8(buf).unwrap();
        let (header, records) = parse_jsonl(&output);
        assert_eq!(header["_export"], true);
        assert_eq!(header["kind"], "events");
        assert_eq!(header["record_count"], 0);
        assert!(records.is_empty());

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 8. Gaps export
// ===========================================================================

#[test]
fn export_gaps_end_to_end() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("gaps").await;

        storage.append_segment(1, "before gap", None).await.unwrap();
        let gap = storage.record_gap(1, "timeout").await.unwrap();
        assert!(gap.is_some(), "Gap should be recorded after segment exists");

        let opts = ExportOptions {
            kind: ExportKind::Gaps,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf).unwrap();
        let (header, records) = parse_jsonl(&output);
        assert_fields_present(&header, &expected_header_fields(), "Gap export header");
        assert_eq!(header["kind"], "gaps");
        assert_eq!(records.len(), 1);
        assert_fields_present(&records[0], &expected_gap_fields(), "Gap record");
        assert_eq!(records[0]["reason"], "timeout");
        assert_eq!(records[0]["pane_id"], 1);

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 9. Events export
// ===========================================================================

#[test]
fn export_events_end_to_end() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("events_e2e").await;

        let event = frankenterm_core::storage::StoredEvent {
            id: 0,
            pane_id: 1,
            rule_id: "api_key_leak".to_string(),
            agent_type: "claude_code".to_string(),
            event_type: "secret_detected".to_string(),
            severity: "critical".to_string(),
            confidence: 0.95,
            extracted: Some(serde_json::json!({"type": "openai_key"})),
            matched_text: Some("sk-test123".to_string()),
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        storage.record_event(event).await.unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Events,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf).unwrap();
        let (header, records) = parse_jsonl(&output);
        assert_eq!(header["kind"], "events");
        assert_fields_present(&records[0], &expected_event_fields(), "Event record");
        assert_eq!(records[0]["event_type"], "secret_detected");
        assert_eq!(records[0]["severity"], "critical");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 10. Events with redaction
// ===========================================================================

#[test]
fn export_events_with_redaction() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("events_redact").await;

        let event = frankenterm_core::storage::StoredEvent {
            id: 0,
            pane_id: 1,
            rule_id: "leak".to_string(),
            agent_type: "codex".to_string(),
            event_type: "secret".to_string(),
            severity: "high".to_string(),
            confidence: 1.0,
            extracted: Some(
                serde_json::json!({"key": "sk-abc123def456ghi789jkl012mno345pqr678stu901v"}),
            ),
            matched_text: Some(
                "Found sk-abc123def456ghi789jkl012mno345pqr678stu901v in output".to_string(),
            ),
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        storage.record_event(event).await.unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Events,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: true,
            pretty: false,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        let (_header, records) = parse_jsonl(&output);
        let matched = records[0]["matched_text"].as_str().unwrap();
        assert!(matched.contains("[REDACTED]"));
        assert!(!matched.contains("sk-abc123"));

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 11. Sessions export
// ===========================================================================

#[test]
fn export_sessions_end_to_end() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("sessions").await;

        let session = frankenterm_core::storage::AgentSessionRecord {
            id: 0,
            pane_id: 1,
            agent_type: "codex".to_string(),
            session_id: Some("sess-42".to_string()),
            external_id: None,
            external_meta: None,
            started_at: 1000,
            ended_at: Some(5000),
            end_reason: Some("completed".to_string()),
            total_tokens: Some(2000),
            input_tokens: Some(1500),
            output_tokens: Some(500),
            cached_tokens: None,
            reasoning_tokens: None,
            model_name: Some("gpt-4".to_string()),
            estimated_cost_usd: None,
        };
        storage.upsert_agent_session(session).await.unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Sessions,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf).unwrap();
        let (header, records) = parse_jsonl(&output);
        assert_eq!(header["kind"], "sessions");
        assert_fields_present(&records[0], &expected_session_fields(), "Session record");
        assert_eq!(records[0]["agent_type"], "codex");
        assert_eq!(records[0]["model_name"], "gpt-4");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 12. Reservations export
// ===========================================================================

#[test]
fn export_reservations_end_to_end() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("reservations").await;

        storage
            .create_reservation(1, "workflow", "wf-auth-fix", Some("fixing auth"), 60_000)
            .await
            .unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Reservations,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf).unwrap();
        let (header, records) = parse_jsonl(&output);
        assert_eq!(header["kind"], "reservations");
        assert_fields_present(
            &records[0],
            &expected_reservation_fields(),
            "Reservation record",
        );
        assert_eq!(records[0]["owner_kind"], "workflow");
        assert_eq!(records[0]["owner_id"], "wf-auth-fix");
        assert_eq!(records[0]["reason"], "fixing auth");
        assert_eq!(records[0]["status"], "active");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 13. Workflows with step logs
// ===========================================================================

#[test]
fn export_workflows_with_step_logs() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("workflows").await;

        let wf = frankenterm_core::storage::WorkflowRecord {
            id: "wf-test-1".to_string(),
            workflow_name: "auth_recovery".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 1,
            status: "running".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: 1000,
            updated_at: 2000,
            completed_at: None,
        };
        storage.upsert_workflow(wf).await.unwrap();

        storage
            .insert_step_log(
                "wf-test-1",
                None,
                0,
                "wait_for_prompt",
                Some("s0".to_string()),
                Some("wait_for".to_string()),
                "continue",
                Some("matched prompt".to_string()),
                None,
                None,
                None,
                1000,
                1500,
            )
            .await
            .unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Workflows,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 1);

        let output = String::from_utf8(buf).unwrap();
        let (header, records) = parse_jsonl(&output);
        assert_eq!(header["kind"], "workflows");
        assert_eq!(records.len(), 2);

        assert_fields_present(&records[0], &expected_workflow_fields(), "Workflow record");
        assert_eq!(records[0]["workflow_name"], "auth_recovery");
        assert_eq!(records[0]["status"], "running");

        assert_fields_present(&records[1], &expected_step_log_fields(), "Step log record");
        assert_eq!(records[1]["step_name"], "wait_for_prompt");
        assert_eq!(records[1]["result_type"], "continue");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 14. Workflow step log redaction
// ===========================================================================

#[test]
fn export_workflows_redacts_step_logs() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("wf_redact").await;

        let wf = frankenterm_core::storage::WorkflowRecord {
            id: "wf-redact-1".to_string(),
            workflow_name: "fix_leak".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 0,
            status: "completed".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: 1000,
            updated_at: 2000,
            completed_at: Some(2000),
        };
        storage.upsert_workflow(wf).await.unwrap();

        storage
            .insert_step_log(
                "wf-redact-1",
                None,
                0,
                "check",
                None,
                None,
                "done",
                Some("key sk-abc123def456ghi789jkl012mno345pqr678stu901v found".to_string()),
                Some("policy: ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ123456789012".to_string()),
                None,
                None,
                1000,
                2000,
            )
            .await
            .unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Workflows,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: true,
            pretty: false,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        let (_header, records) = parse_jsonl(&output);
        assert_eq!(records.len(), 2);

        let step = &records[1];
        let result_data = step["result_data"].as_str().unwrap();
        assert!(result_data.contains("[REDACTED]"));
        assert!(!result_data.contains("sk-abc123"));

        let policy = step["policy_summary"].as_str().unwrap();
        assert!(policy.contains("[REDACTED]"));
        assert!(!policy.contains("ghp_"));

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 15. Segment schema validation
// ===========================================================================

#[test]
fn export_segment_schema_validation() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("seg_schema").await;
        storage
            .append_segment(1, "schema test content", None)
            .await
            .unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        let (header, records) = parse_jsonl(&output);

        assert_fields_present(&header, &expected_header_fields(), "Segment export header");
        assert_eq!(header["_export"], true);
        assert_eq!(header["kind"], "segments");
        assert!(!header["version"].as_str().unwrap().is_empty());

        assert_eq!(records.len(), 1);
        assert_fields_present(&records[0], &expected_segment_fields(), "Segment record");
        assert_eq!(records[0]["content"], "schema test content");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 16. Header version matches crate version
// ===========================================================================

#[test]
fn export_header_version_matches_crate() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("version").await;

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        let (header, _) = parse_jsonl(&output);
        assert_eq!(
            header["version"].as_str().unwrap(),
            frankenterm_core::VERSION,
            "Export header version must match crate VERSION"
        );

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 17. Multiple segments count
// ===========================================================================

#[test]
fn export_multiple_segments_count() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("multi_seg").await;

        for i in 0..5 {
            storage
                .append_segment(1, &format!("segment content {}", i), None)
                .await
                .unwrap();
        }

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
        assert_eq!(count, 5);

        let output = String::from_utf8(buf).unwrap();
        let (header, records) = parse_jsonl(&output);
        assert_eq!(header["record_count"], 5);
        assert_eq!(records.len(), 5);
        for i in 0..5 {
            let expected = format!("segment content {}", i);
            assert!(
                records.iter().any(|r| r["content"] == expected),
                "Missing segment content {}",
                i
            );
        }

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 18. Header exported_at_ms is recent
// ===========================================================================

#[test]
fn export_header_exported_at_ms_is_recent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("timestamp").await;

        let before_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let after_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let output = String::from_utf8(buf).unwrap();
        let (header, _) = parse_jsonl(&output);
        let exported_at = header["exported_at_ms"].as_i64().unwrap();
        assert!(
            exported_at >= before_ms,
            "exported_at_ms should be >= before"
        );
        assert!(exported_at <= after_ms, "exported_at_ms should be <= after");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 19. Header reflects query filters
// ===========================================================================

#[test]
fn export_header_reflects_query_filters() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("query_hdr").await;

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery {
                pane_id: Some(42),
                since: Some(1000),
                until: Some(9000),
                limit: Some(25),
            },
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        let (header, _) = parse_jsonl(&output);
        assert_eq!(header["pane_id"], 42);
        assert_eq!(header["since"], 1000);
        assert_eq!(header["until"], 9000);
        assert_eq!(header["limit"], 25);

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 20. Empty all kinds produce header
// ===========================================================================

#[test]
fn export_empty_all_kinds_produce_header() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("empty_all").await;

        let kinds = [
            ExportKind::Segments,
            ExportKind::Gaps,
            ExportKind::Events,
            ExportKind::Workflows,
            ExportKind::Sessions,
            ExportKind::Audit,
            ExportKind::Reservations,
        ];

        for kind in &kinds {
            let opts = ExportOptions {
                kind: *kind,
                query: ExportQuery::default(),
                audit_actor: None,
                audit_action: None,
                redact: false,
                pretty: false,
            };

            let mut buf = Vec::new();
            let count = export_jsonl(&storage, &opts, &mut buf).await.unwrap();
            assert_eq!(
                count,
                0,
                "Empty DB should have 0 records for kind {}",
                kind.as_str()
            );

            let output = String::from_utf8(buf).unwrap();
            let (header, records) = parse_jsonl(&output);
            assert_eq!(
                header["kind"],
                kind.as_str(),
                "Header kind mismatch for {}",
                kind.as_str()
            );
            assert_eq!(
                header["record_count"],
                0,
                "Header record_count should be 0 for {}",
                kind.as_str()
            );
            assert!(
                records.is_empty(),
                "No data records expected for {}",
                kind.as_str()
            );
        }

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// 21. Redact false does not alter content
// ===========================================================================

#[test]
fn export_redact_false_does_not_alter_content() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = test_db_with_pane("no_redact").await;

        let secret = "sk-abc123def456ghi789jkl012mno345pqr678stu901v";
        storage
            .append_segment(1, &format!("has secret: {}", secret), None)
            .await
            .unwrap();

        let opts = ExportOptions {
            kind: ExportKind::Segments,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };

        let mut buf = Vec::new();
        export_jsonl(&storage, &opts, &mut buf).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        let (_header, records) = parse_jsonl(&output);
        let content = records[0]["content"].as_str().unwrap();
        assert!(
            content.contains(secret),
            "Content should contain secret when redact is false"
        );
        assert!(!content.contains("[REDACTED]"));

        teardown(storage, &db_path).await;
    });
}
