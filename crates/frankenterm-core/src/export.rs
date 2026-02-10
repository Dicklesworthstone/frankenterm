//! Export module: JSONL/NDJSON export for wa data.
//!
//! Exports segments, gaps, events, workflows, sessions, audit actions,
//! and reservations to newline-delimited JSON with optional redaction.

use std::io::Write;

use serde::Serialize;

use crate::policy::Redactor;
use crate::storage::{
    AuditQuery, EventQuery, ExportQuery, Segment, StorageHandle, StoredEvent, WorkflowStepLogRecord,
};

/// Data kinds available for export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Segments,
    Gaps,
    Events,
    Workflows,
    Sessions,
    Audit,
    Reservations,
}

impl ExportKind {
    /// Parse from a string (case-insensitive).
    #[must_use]
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "segments" | "segment" | "output" => Some(Self::Segments),
            "gaps" | "gap" => Some(Self::Gaps),
            "events" | "event" | "detections" => Some(Self::Events),
            "workflows" | "workflow" => Some(Self::Workflows),
            "sessions" | "session" => Some(Self::Sessions),
            "audit" | "audit_actions" | "audit-actions" => Some(Self::Audit),
            "reservations" | "reservation" | "reserves" => Some(Self::Reservations),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Segments => "segments",
            Self::Gaps => "gaps",
            Self::Events => "events",
            Self::Workflows => "workflows",
            Self::Sessions => "sessions",
            Self::Audit => "audit",
            Self::Reservations => "reservations",
        }
    }

    /// All valid kind strings for help text.
    #[must_use]
    pub fn all_names() -> &'static [&'static str] {
        &[
            "segments",
            "gaps",
            "events",
            "workflows",
            "sessions",
            "audit",
            "reservations",
        ]
    }
}

/// JSONL header written as the first line of export output.
#[derive(Debug, Clone, Serialize)]
pub struct ExportHeader {
    #[serde(rename = "_export")]
    pub export: bool,
    pub version: String,
    pub kind: String,
    pub redacted: bool,
    pub exported_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    pub record_count: usize,
}

/// Options controlling export behavior.
pub struct ExportOptions {
    pub kind: ExportKind,
    pub query: ExportQuery,
    /// Filter by actor kind (audit exports only)
    pub audit_actor: Option<String>,
    /// Filter by action kind (audit exports only)
    pub audit_action: Option<String>,
    pub redact: bool,
    pub pretty: bool,
}

/// Write JSONL export to the provided writer.
///
/// Writes a header line followed by one JSON object per line.
/// Returns the number of records exported.
pub async fn export_jsonl<W: Write>(
    storage: &StorageHandle,
    opts: &ExportOptions,
    writer: &mut W,
) -> crate::Result<usize> {
    let redactor = if opts.redact {
        Some(Redactor::new())
    } else {
        None
    };

    let count = match opts.kind {
        ExportKind::Segments => {
            let records = storage.export_segments(opts.query.clone()).await?;
            let count = records.len();
            write_header(writer, opts, count)?;
            for record in records {
                let record = if let Some(ref r) = redactor {
                    redact_segment(record, r)
                } else {
                    record
                };
                write_record(writer, &record, opts.pretty)?;
            }
            count
        }
        ExportKind::Gaps => {
            let records = storage.export_gaps(opts.query.clone()).await?;
            let count = records.len();
            write_header(writer, opts, count)?;
            for record in records {
                write_record(writer, &record, opts.pretty)?;
            }
            count
        }
        ExportKind::Events => {
            let query = EventQuery {
                limit: opts.query.limit,
                pane_id: opts.query.pane_id,
                since: opts.query.since,
                until: opts.query.until,
                ..Default::default()
            };
            let records = storage.get_events(query).await?;
            let count = records.len();
            write_header(writer, opts, count)?;
            for record in records {
                let record = if let Some(ref r) = redactor {
                    redact_event(record, r)
                } else {
                    record
                };
                write_record(writer, &record, opts.pretty)?;
            }
            count
        }
        ExportKind::Workflows => {
            let records = storage.export_workflows(opts.query.clone()).await?;
            let count = records.len();
            write_header(writer, opts, count)?;
            for wf in &records {
                write_record(writer, wf, opts.pretty)?;
                // Also export step logs for each workflow
                if let Ok(steps) = storage.get_step_logs(&wf.id).await {
                    for step in &steps {
                        let step = if let Some(ref r) = redactor {
                            redact_step_log(step.clone(), r)
                        } else {
                            step.clone()
                        };
                        write_record(writer, &step, opts.pretty)?;
                    }
                }
            }
            count
        }
        ExportKind::Sessions => {
            let records = storage.export_sessions(opts.query.clone()).await?;
            let count = records.len();
            write_header(writer, opts, count)?;
            for record in &records {
                write_record(writer, record, opts.pretty)?;
            }
            count
        }
        ExportKind::Audit => {
            let query = AuditQuery {
                limit: opts.query.limit,
                pane_id: opts.query.pane_id,
                since: opts.query.since,
                until: opts.query.until,
                actor_kind: opts.audit_actor.clone(),
                action_kind: opts.audit_action.clone(),
                ..Default::default()
            };
            let mut records = storage.get_audit_actions(query).await?;
            let count = records.len();
            write_header(writer, opts, count)?;
            for record in &mut records {
                if let Some(ref r) = redactor {
                    record.redact_fields(r);
                }
                write_record(writer, record, opts.pretty)?;
            }
            count
        }
        ExportKind::Reservations => {
            let records = storage.export_reservations(opts.query.clone()).await?;
            let count = records.len();
            write_header(writer, opts, count)?;
            for record in &records {
                write_record(writer, record, opts.pretty)?;
            }
            count
        }
    };

    writer.flush().map_err(|e| {
        crate::Error::Storage(crate::StorageError::Database(format!("Flush failed: {e}")))
    })?;

    Ok(count)
}

fn write_header<W: Write>(
    writer: &mut W,
    opts: &ExportOptions,
    record_count: usize,
) -> crate::Result<()> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let header = ExportHeader {
        export: true,
        version: crate::VERSION.to_string(),
        kind: opts.kind.as_str().to_string(),
        redacted: opts.redact,
        exported_at_ms: now_ms,
        pane_id: opts.query.pane_id,
        since: opts.query.since,
        until: opts.query.until,
        limit: opts.query.limit,
        record_count,
    };

    write_record(writer, &header, opts.pretty)
}

fn write_record<W: Write, T: Serialize>(
    writer: &mut W,
    record: &T,
    pretty: bool,
) -> crate::Result<()> {
    let json = if pretty {
        serde_json::to_string_pretty(record)
    } else {
        serde_json::to_string(record)
    }
    .map_err(|e| {
        crate::Error::Storage(crate::StorageError::Database(format!(
            "JSON serialization failed: {e}"
        )))
    })?;

    writeln!(writer, "{json}").map_err(|e| {
        crate::Error::Storage(crate::StorageError::Database(format!("Write failed: {e}")))
    })
}

// =============================================================================
// Redaction helpers
// =============================================================================

fn redact_segment(mut seg: Segment, redactor: &Redactor) -> Segment {
    seg.content = redactor.redact(&seg.content);
    seg
}

fn redact_event(mut event: StoredEvent, redactor: &Redactor) -> StoredEvent {
    if let Some(ref text) = event.matched_text {
        event.matched_text = Some(redactor.redact(text));
    }
    if let Some(ref extracted) = event.extracted {
        if let Ok(s) = serde_json::to_string(extracted) {
            let redacted = redactor.redact(&s);
            if let Ok(v) = serde_json::from_str(&redacted) {
                event.extracted = Some(v);
            }
        }
    }
    event
}

fn redact_step_log(mut step: WorkflowStepLogRecord, redactor: &Redactor) -> WorkflowStepLogRecord {
    if let Some(ref data) = step.result_data {
        step.result_data = Some(redactor.redact(data));
    }
    if let Some(ref summary) = step.policy_summary {
        step.policy_summary = Some(redactor.redact(summary));
    }
    step
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_kind_from_str_loose() {
        assert_eq!(
            ExportKind::from_str_loose("segments"),
            Some(ExportKind::Segments)
        );
        assert_eq!(
            ExportKind::from_str_loose("Segment"),
            Some(ExportKind::Segments)
        );
        assert_eq!(
            ExportKind::from_str_loose("output"),
            Some(ExportKind::Segments)
        );
        assert_eq!(ExportKind::from_str_loose("gaps"), Some(ExportKind::Gaps));
        assert_eq!(ExportKind::from_str_loose("Gap"), Some(ExportKind::Gaps));
        assert_eq!(
            ExportKind::from_str_loose("events"),
            Some(ExportKind::Events)
        );
        assert_eq!(
            ExportKind::from_str_loose("detections"),
            Some(ExportKind::Events)
        );
        assert_eq!(
            ExportKind::from_str_loose("workflows"),
            Some(ExportKind::Workflows)
        );
        assert_eq!(
            ExportKind::from_str_loose("sessions"),
            Some(ExportKind::Sessions)
        );
        assert_eq!(ExportKind::from_str_loose("audit"), Some(ExportKind::Audit));
        assert_eq!(
            ExportKind::from_str_loose("audit-actions"),
            Some(ExportKind::Audit)
        );
        assert_eq!(
            ExportKind::from_str_loose("reservations"),
            Some(ExportKind::Reservations)
        );
        assert_eq!(ExportKind::from_str_loose("unknown"), None);
    }

    #[test]
    fn export_kind_round_trips() {
        for name in ExportKind::all_names() {
            let kind = ExportKind::from_str_loose(name).unwrap();
            assert_eq!(kind.as_str(), *name);
        }
    }

    #[test]
    fn export_header_serializes() {
        let header = ExportHeader {
            export: true,
            version: "0.1.0".to_string(),
            kind: "segments".to_string(),
            redacted: true,
            exported_at_ms: 1000,
            pane_id: Some(3),
            since: None,
            until: None,
            limit: Some(100),
            record_count: 42,
        };
        let json = serde_json::to_string(&header).unwrap();
        assert!(json.contains("\"_export\":true"));
        assert!(json.contains("\"kind\":\"segments\""));
        assert!(json.contains("\"record_count\":42"));
        // None fields should be skipped
        assert!(!json.contains("\"since\""));
        assert!(!json.contains("\"until\""));
    }

    #[test]
    fn redact_segment_removes_secrets() {
        let r = Redactor::new();
        let seg = Segment {
            id: 1,
            pane_id: 1,
            seq: 1,
            content: "key sk-abc123def456ghi789jkl012mno345pqr678stu901v here".to_string(),
            content_len: 50,
            content_hash: None,
            captured_at: 1000,
        };
        let redacted = redact_segment(seg, &r);
        assert!(redacted.content.contains("[REDACTED]"));
        assert!(!redacted.content.contains("sk-abc123"));
    }

    #[test]
    fn redact_event_removes_secrets() {
        let r = Redactor::new();
        let event = StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test".to_string(),
            agent_type: "codex".to_string(),
            event_type: "auth.error".to_string(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: Some(
                serde_json::json!({"key": "sk-abc123def456ghi789jkl012mno345pqr678stu901v"}),
            ),
            matched_text: Some("Error: sk-abc123def456ghi789jkl012mno345pqr678stu901v".to_string()),
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        let redacted = redact_event(event, &r);
        assert!(redacted.matched_text.unwrap().contains("[REDACTED]"));
    }

    #[test]
    fn write_record_jsonl() {
        let seg = Segment {
            id: 1,
            pane_id: 2,
            seq: 3,
            content: "hello".to_string(),
            content_len: 5,
            content_hash: None,
            captured_at: 1000,
        };
        let mut buf = Vec::new();
        write_record(&mut buf, &seg, false).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        // Should be exactly one line (no embedded newlines)
        let trimmed = line.trim_end_matches('\n');
        assert!(!trimmed.contains('\n'));
        let parsed: serde_json::Value = serde_json::from_str(trimmed).unwrap();
        assert_eq!(parsed["pane_id"], 2);
        assert_eq!(parsed["content"], "hello");
    }

    #[tokio::test]
    async fn export_segments_to_buffer() {
        // Create temp DB
        let tmp = std::env::temp_dir().join(format!("wa_test_export_{}.db", std::process::id()));
        let db_path = tmp.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path).await.unwrap();

        // Insert a pane and segment
        let pane = crate::storage::PaneRecord {
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

        // Verify header
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["_export"], true);
        assert_eq!(header["kind"], "segments");
        assert_eq!(header["record_count"], 1);

        // Verify record
        let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(record["content"], "test content");

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_with_redaction() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_export_redact_{}.db", std::process::id()));
        let db_path = tmp.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path).await.unwrap();

        let pane = crate::storage::PaneRecord {
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

        // Header should indicate redacted
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["redacted"], true);

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_with_pane_filter() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_export_filter_{}.db", std::process::id()));
        let db_path = tmp.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path).await.unwrap();

        // Insert two panes
        for pane_id in [1u64, 2u64] {
            let pane = crate::storage::PaneRecord {
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

        // Export only pane 1
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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_pretty_format() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_export_pretty_{}.db", std::process::id()));
        let db_path = tmp.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path).await.unwrap();

        let pane = crate::storage::PaneRecord {
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
        // Pretty format should have indentation
        assert!(output.contains("  \""));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_audit_with_actor_filter() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_export_audit_{}.db", std::process::id()));
        let db_path = tmp.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path).await.unwrap();

        // Insert a pane (required for FK)
        let pane = crate::storage::PaneRecord {
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

        // Insert two audit actions with different actor_kinds
        let action1 = crate::storage::AuditActionRecord {
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

        let action2 = crate::storage::AuditActionRecord {
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
        assert_eq!(lines.len(), 2); // header + 1 record

        // Verify the record is the workflow one
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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_audit_redacts_fields() {
        let tmp = std::env::temp_dir().join(format!(
            "wa_test_export_audit_redact_{}.db",
            std::process::id()
        ));
        let db_path = tmp.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path).await.unwrap();

        let pane = crate::storage::PaneRecord {
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

        let action = crate::storage::AuditActionRecord {
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

        // Decision reason and input summary should be redacted
        let reason = record["decision_reason"].as_str().unwrap();
        assert!(reason.contains("[REDACTED]"));
        assert!(!reason.contains("sk-abc123"));

        let summary = record["input_summary"].as_str().unwrap();
        assert!(summary.contains("[REDACTED]"));
        assert!(!summary.contains("sk-abc123"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    // =========================================================================
    // Schema documentation & field validation
    // =========================================================================
    //
    // Each export kind produces JSONL where every data record (after the header)
    // contains the fields defined on its storage struct. The expected field sets
    // are codified below so that any accidental removal or rename breaks a test.
    //
    // Export Header (first line of every export):
    //   _export (bool), version (string), kind (string), redacted (bool),
    //   exported_at_ms (i64), pane_id? (u64), since? (i64), until? (i64),
    //   limit? (usize), record_count (usize)
    //
    // Segment record:
    //   id, pane_id, seq, content, content_len, content_hash?, captured_at
    //
    // Gap record:
    //   id, pane_id, seq_before, seq_after, reason, detected_at
    //
    // Event record (StoredEvent):
    //   id, pane_id, rule_id, agent_type, event_type, severity, confidence,
    //   extracted?, matched_text?, segment_id?, detected_at, dedupe_key?,
    //   handled_at?, handled_by_workflow_id?, handled_status?
    //
    // Workflow record:
    //   id, workflow_name, pane_id, trigger_event_id?, current_step, status,
    //   wait_condition?, context?, result?, error?, started_at, updated_at,
    //   completed_at?
    //
    // Workflow step log record:
    //   id, workflow_id, audit_action_id?, step_index, step_name, step_id?,
    //   step_kind?, result_type, result_data?, policy_summary?,
    //   verification_refs?, error_code?, started_at, completed_at, duration_ms
    //
    // Agent session record:
    //   id, pane_id, agent_type, session_id?, external_id?, external_meta?,
    //   started_at, ended_at?, end_reason?, total_tokens?, input_tokens?,
    //   output_tokens?, cached_tokens?, reasoning_tokens?, model_name?,
    //   estimated_cost_usd?
    //
    // Audit action record:
    //   id, ts, actor_kind, actor_id?, correlation_id?, pane_id?, domain?,
    //   action_kind, policy_decision, decision_reason?, rule_id?,
    //   input_summary?, verification_summary?, decision_context?, result
    //
    // Pane reservation record:
    //   id, pane_id, owner_kind, owner_id, reason?, created_at, expires_at,
    //   released_at?, status
    //
    // Versioning strategy:
    //   The export header's `version` field carries the crate version from
    //   Cargo.toml (crate::VERSION). Consumers should compare the major.minor
    //   version to decide whether they understand the schema. Field additions
    //   (new optional fields) are backward-compatible and do NOT require a
    //   version bump. Field removals or renames MUST bump the minor version
    //   at minimum. The schema drift test below enforces this invariant.

    /// Expected required fields for each exported record type.
    /// The schema drift test verifies that every field listed here appears
    /// in the serialized JSON, and that no unlisted fields have appeared.
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

    fn expected_audit_fields() -> Vec<&'static str> {
        vec![
            "id",
            "ts",
            "actor_kind",
            "action_kind",
            "policy_decision",
            "result",
        ]
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

    /// Helper to create a test DB with one pane already inserted.
    async fn test_db_with_pane(suffix: &str) -> (StorageHandle, std::path::PathBuf) {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_export_{suffix}_{}.db", std::process::id()));
        let db_path = tmp.to_string_lossy().to_string();
        let storage = StorageHandle::new(&db_path).await.unwrap();

        let pane = crate::storage::PaneRecord {
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

        (storage, tmp)
    }

    // =========================================================================
    // Schema drift tests — fail if fields are added/removed without review
    // =========================================================================

    #[test]
    fn schema_drift_segment() {
        let seg = Segment {
            id: 1,
            pane_id: 1,
            seq: 1,
            content: "test".to_string(),
            content_len: 4,
            content_hash: Some("abc".to_string()),
            captured_at: 1000,
        };
        let val: serde_json::Value = serde_json::to_value(&seg).unwrap();
        assert_fields_present(&val, &expected_segment_fields(), "Segment");
        // Also check no unexpected fields crept in without updating schema docs
        let map = val.as_object().unwrap();
        let known: std::collections::HashSet<&str> = [
            "id",
            "pane_id",
            "seq",
            "content",
            "content_len",
            "content_hash",
            "captured_at",
        ]
        .into_iter()
        .collect();
        for key in map.keys() {
            assert!(
                known.contains(key.as_str()),
                "Segment has unexpected field '{key}' — update schema docs and drift test"
            );
        }
    }

    #[test]
    fn schema_drift_gap() {
        let gap = crate::storage::Gap {
            id: 1,
            pane_id: 1,
            seq_before: 5,
            seq_after: 6,
            reason: "timeout".to_string(),
            detected_at: 1000,
        };
        let val: serde_json::Value = serde_json::to_value(&gap).unwrap();
        assert_fields_present(&val, &expected_gap_fields(), "Gap");
        let map = val.as_object().unwrap();
        let known: std::collections::HashSet<&str> = [
            "id",
            "pane_id",
            "seq_before",
            "seq_after",
            "reason",
            "detected_at",
        ]
        .into_iter()
        .collect();
        for key in map.keys() {
            assert!(
                known.contains(key.as_str()),
                "Gap has unexpected field '{key}' — update schema docs and drift test"
            );
        }
    }

    #[test]
    fn schema_drift_event() {
        let event = StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "r1".to_string(),
            agent_type: "codex".to_string(),
            event_type: "error".to_string(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: Some(serde_json::json!({"k": "v"})),
            matched_text: Some("match".to_string()),
            segment_id: Some(1),
            detected_at: 1000,
            dedupe_key: Some("dk".to_string()),
            handled_at: Some(2000),
            handled_by_workflow_id: Some("wf-1".to_string()),
            handled_status: Some("resolved".to_string()),
        };
        let val: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_fields_present(&val, &expected_event_fields(), "StoredEvent");
        let map = val.as_object().unwrap();
        let known: std::collections::HashSet<&str> = [
            "id",
            "pane_id",
            "rule_id",
            "agent_type",
            "event_type",
            "severity",
            "confidence",
            "extracted",
            "matched_text",
            "segment_id",
            "detected_at",
            "dedupe_key",
            "handled_at",
            "handled_by_workflow_id",
            "handled_status",
        ]
        .into_iter()
        .collect();
        for key in map.keys() {
            assert!(
                known.contains(key.as_str()),
                "StoredEvent has unexpected field '{key}' — update schema docs and drift test"
            );
        }
    }

    #[test]
    fn schema_drift_workflow() {
        let wf = crate::storage::WorkflowRecord {
            id: "wf-1".to_string(),
            workflow_name: "auth_fix".to_string(),
            pane_id: 1,
            trigger_event_id: Some(10),
            current_step: 2,
            status: "running".to_string(),
            wait_condition: Some(serde_json::json!({"type": "text"})),
            context: Some(serde_json::json!({})),
            result: None,
            error: None,
            started_at: 1000,
            updated_at: 2000,
            completed_at: None,
        };
        let val: serde_json::Value = serde_json::to_value(&wf).unwrap();
        assert_fields_present(&val, &expected_workflow_fields(), "WorkflowRecord");
        let map = val.as_object().unwrap();
        let known: std::collections::HashSet<&str> = [
            "id",
            "workflow_name",
            "pane_id",
            "trigger_event_id",
            "current_step",
            "status",
            "wait_condition",
            "context",
            "result",
            "error",
            "started_at",
            "updated_at",
            "completed_at",
        ]
        .into_iter()
        .collect();
        for key in map.keys() {
            assert!(
                known.contains(key.as_str()),
                "WorkflowRecord has unexpected field '{key}' — update schema docs and drift test"
            );
        }
    }

    #[test]
    fn schema_drift_step_log() {
        let step = WorkflowStepLogRecord {
            id: 1,
            workflow_id: "wf-1".to_string(),
            audit_action_id: Some(5),
            step_index: 0,
            step_name: "wait_for_prompt".to_string(),
            step_id: Some("s1".to_string()),
            step_kind: Some("wait_for".to_string()),
            result_type: "continue".to_string(),
            result_data: Some("found prompt".to_string()),
            policy_summary: Some("allowed".to_string()),
            verification_refs: Some("[]".to_string()),
            error_code: None,
            started_at: 1000,
            completed_at: 2000,
            duration_ms: 1000,
        };
        let val: serde_json::Value = serde_json::to_value(&step).unwrap();
        assert_fields_present(&val, &expected_step_log_fields(), "WorkflowStepLogRecord");
        let map = val.as_object().unwrap();
        let known: std::collections::HashSet<&str> = [
            "id",
            "workflow_id",
            "audit_action_id",
            "step_index",
            "step_name",
            "step_id",
            "step_kind",
            "result_type",
            "result_data",
            "policy_summary",
            "verification_refs",
            "error_code",
            "started_at",
            "completed_at",
            "duration_ms",
        ]
        .into_iter()
        .collect();
        for key in map.keys() {
            assert!(
                known.contains(key.as_str()),
                "WorkflowStepLogRecord has unexpected field '{key}' — update schema docs and drift test"
            );
        }
    }

    #[test]
    fn schema_drift_session() {
        let session = crate::storage::AgentSessionRecord {
            id: 1,
            pane_id: 1,
            agent_type: "codex".to_string(),
            session_id: Some("sess-1".to_string()),
            external_id: Some("ext-1".to_string()),
            external_meta: Some(serde_json::json!({"tool": "wa"})),
            started_at: 1000,
            ended_at: Some(2000),
            end_reason: Some("completed".to_string()),
            total_tokens: Some(1500),
            input_tokens: Some(1000),
            output_tokens: Some(500),
            cached_tokens: Some(200),
            reasoning_tokens: Some(100),
            model_name: Some("gpt-4".to_string()),
            estimated_cost_usd: Some(0.05),
        };
        let val: serde_json::Value = serde_json::to_value(&session).unwrap();
        assert_fields_present(&val, &expected_session_fields(), "AgentSessionRecord");
        let map = val.as_object().unwrap();
        let known: std::collections::HashSet<&str> = [
            "id",
            "pane_id",
            "agent_type",
            "session_id",
            "external_id",
            "external_meta",
            "started_at",
            "ended_at",
            "end_reason",
            "total_tokens",
            "input_tokens",
            "output_tokens",
            "cached_tokens",
            "reasoning_tokens",
            "model_name",
            "estimated_cost_usd",
        ]
        .into_iter()
        .collect();
        for key in map.keys() {
            assert!(
                known.contains(key.as_str()),
                "AgentSessionRecord has unexpected field '{key}' — update schema docs and drift test"
            );
        }
    }

    #[test]
    fn schema_drift_audit() {
        let audit = crate::storage::AuditActionRecord {
            id: 1,
            ts: 1000,
            actor_kind: "workflow".to_string(),
            actor_id: Some("wf-1".to_string()),
            correlation_id: Some("corr-1".to_string()),
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("auto".to_string()),
            rule_id: Some("r1".to_string()),
            input_summary: Some("input".to_string()),
            verification_summary: Some("ok".to_string()),
            decision_context: Some(r#"{"mode": "auto"}"#.to_string()),
            result: "ok".to_string(),
        };
        let val: serde_json::Value = serde_json::to_value(&audit).unwrap();
        assert_fields_present(&val, &expected_audit_fields(), "AuditActionRecord");
        let map = val.as_object().unwrap();
        let known: std::collections::HashSet<&str> = [
            "id",
            "ts",
            "actor_kind",
            "actor_id",
            "correlation_id",
            "pane_id",
            "domain",
            "action_kind",
            "policy_decision",
            "decision_reason",
            "rule_id",
            "input_summary",
            "verification_summary",
            "decision_context",
            "result",
        ]
        .into_iter()
        .collect();
        for key in map.keys() {
            assert!(
                known.contains(key.as_str()),
                "AuditActionRecord has unexpected field '{key}' — update schema docs and drift test"
            );
        }
    }

    #[test]
    fn schema_drift_reservation() {
        let res = crate::storage::PaneReservation {
            id: 1,
            pane_id: 1,
            owner_kind: "workflow".to_string(),
            owner_id: "wf-1".to_string(),
            reason: Some("running fix".to_string()),
            created_at: 1000,
            expires_at: 2000,
            released_at: None,
            status: "active".to_string(),
        };
        let val: serde_json::Value = serde_json::to_value(&res).unwrap();
        assert_fields_present(&val, &expected_reservation_fields(), "PaneReservation");
        let map = val.as_object().unwrap();
        let known: std::collections::HashSet<&str> = [
            "id",
            "pane_id",
            "owner_kind",
            "owner_id",
            "reason",
            "created_at",
            "expires_at",
            "released_at",
            "status",
        ]
        .into_iter()
        .collect();
        for key in map.keys() {
            assert!(
                known.contains(key.as_str()),
                "PaneReservation has unexpected field '{key}' — update schema docs and drift test"
            );
        }
    }

    #[test]
    fn schema_drift_header() {
        let header = ExportHeader {
            export: true,
            version: "0.1.0".to_string(),
            kind: "segments".to_string(),
            redacted: false,
            exported_at_ms: 1000,
            pane_id: Some(1),
            since: Some(500),
            until: Some(2000),
            limit: Some(100),
            record_count: 5,
        };
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        assert_fields_present(&val, &expected_header_fields(), "ExportHeader");
        let map = val.as_object().unwrap();
        let known: std::collections::HashSet<&str> = [
            "_export",
            "version",
            "kind",
            "redacted",
            "exported_at_ms",
            "pane_id",
            "since",
            "until",
            "limit",
            "record_count",
        ]
        .into_iter()
        .collect();
        for key in map.keys() {
            assert!(
                known.contains(key.as_str()),
                "ExportHeader has unexpected field '{key}' — update schema docs and drift test"
            );
        }
    }

    // =========================================================================
    // Step log redaction
    // =========================================================================

    #[test]
    fn redact_step_log_removes_secrets() {
        let r = Redactor::new();
        let step = WorkflowStepLogRecord {
            id: 1,
            workflow_id: "wf-1".to_string(),
            audit_action_id: None,
            step_index: 0,
            step_name: "check_auth".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "continue".to_string(),
            result_data: Some(
                "Found key: sk-abc123def456ghi789jkl012mno345pqr678stu901v".to_string(),
            ),
            policy_summary: Some(
                "Token ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ123456789012 validated".to_string(),
            ),
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 2000,
            duration_ms: 1000,
        };
        let redacted = redact_step_log(step, &r);
        assert!(
            redacted
                .result_data
                .as_ref()
                .unwrap()
                .contains("[REDACTED]"),
            "result_data should be redacted"
        );
        assert!(
            !redacted.result_data.as_ref().unwrap().contains("sk-abc123"),
            "OpenAI key should not survive redaction"
        );
        assert!(
            redacted
                .policy_summary
                .as_ref()
                .unwrap()
                .contains("[REDACTED]"),
            "policy_summary should be redacted"
        );
        assert!(
            !redacted.policy_summary.as_ref().unwrap().contains("ghp_"),
            "GitHub PAT should not survive redaction"
        );
    }

    #[test]
    fn redact_step_log_noop_when_no_secrets() {
        let r = Redactor::new();
        let step = WorkflowStepLogRecord {
            id: 1,
            workflow_id: "wf-1".to_string(),
            audit_action_id: None,
            step_index: 0,
            step_name: "wait".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "continue".to_string(),
            result_data: Some("clean data no secrets".to_string()),
            policy_summary: Some("allowed by default policy".to_string()),
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 2000,
            duration_ms: 1000,
        };
        let redacted = redact_step_log(step.clone(), &r);
        assert_eq!(redacted.result_data, step.result_data);
        assert_eq!(redacted.policy_summary, step.policy_summary);
    }

    #[test]
    fn redact_step_log_none_fields_unchanged() {
        let r = Redactor::new();
        let step = WorkflowStepLogRecord {
            id: 1,
            workflow_id: "wf-1".to_string(),
            audit_action_id: None,
            step_index: 0,
            step_name: "wait".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "continue".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 2000,
            duration_ms: 1000,
        };
        let redacted = redact_step_log(step, &r);
        assert!(redacted.result_data.is_none());
        assert!(redacted.policy_summary.is_none());
    }

    // =========================================================================
    // End-to-end export tests for less-covered kinds
    // =========================================================================

    #[tokio::test]
    async fn export_empty_produces_header_only() {
        let (storage, tmp) = test_db_with_pane("empty").await;

        // Export events (none exist) — should produce header with count=0
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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_gaps_end_to_end() {
        let (storage, tmp) = test_db_with_pane("gaps").await;

        // Need a segment first (record_gap requires last_seq)
        storage.append_segment(1, "before gap", None).await.unwrap();
        // Record a gap
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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_events_end_to_end() {
        let (storage, tmp) = test_db_with_pane("events_e2e").await;

        let event = StoredEvent {
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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_events_with_redaction() {
        let (storage, tmp) = test_db_with_pane("events_redact").await;

        let event = StoredEvent {
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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_sessions_end_to_end() {
        let (storage, tmp) = test_db_with_pane("sessions").await;

        let session = crate::storage::AgentSessionRecord {
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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_reservations_end_to_end() {
        let (storage, tmp) = test_db_with_pane("reservations").await;

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

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_workflows_with_step_logs() {
        let (storage, tmp) = test_db_with_pane("workflows").await;

        let wf = crate::storage::WorkflowRecord {
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
        // count reflects workflow records (not step logs)
        assert_eq!(count, 1);

        let output = String::from_utf8(buf).unwrap();
        let (header, records) = parse_jsonl(&output);
        assert_eq!(header["kind"], "workflows");
        // Should have workflow record + step log record = 2 data lines
        assert_eq!(records.len(), 2);

        // First record is the workflow
        assert_fields_present(&records[0], &expected_workflow_fields(), "Workflow record");
        assert_eq!(records[0]["workflow_name"], "auth_recovery");
        assert_eq!(records[0]["status"], "running");

        // Second record is the step log
        assert_fields_present(&records[1], &expected_step_log_fields(), "Step log record");
        assert_eq!(records[1]["step_name"], "wait_for_prompt");
        assert_eq!(records[1]["result_type"], "continue");

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_workflows_redacts_step_logs() {
        let (storage, tmp) = test_db_with_pane("wf_redact").await;

        let wf = crate::storage::WorkflowRecord {
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

        // Step log (second record) should be redacted
        let step = &records[1];
        let result_data = step["result_data"].as_str().unwrap();
        assert!(result_data.contains("[REDACTED]"));
        assert!(!result_data.contains("sk-abc123"));

        let policy = step["policy_summary"].as_str().unwrap();
        assert!(policy.contains("[REDACTED]"));
        assert!(!policy.contains("ghp_"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_segment_schema_validation() {
        let (storage, tmp) = test_db_with_pane("seg_schema").await;
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

        // Validate header schema
        assert_fields_present(&header, &expected_header_fields(), "Segment export header");
        assert_eq!(header["_export"], true);
        assert_eq!(header["kind"], "segments");
        assert!(!header["version"].as_str().unwrap().is_empty());

        // Validate record schema
        assert_eq!(records.len(), 1);
        assert_fields_present(&records[0], &expected_segment_fields(), "Segment record");
        assert_eq!(records[0]["content"], "schema test content");

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn export_header_version_matches_crate() {
        let (storage, tmp) = test_db_with_pane("version").await;

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
            crate::VERSION,
            "Export header version must match crate VERSION"
        );

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn all_export_kinds_covered_by_all_names() {
        let names = ExportKind::all_names();
        assert_eq!(names.len(), 7);
        // Every name should parse back
        for name in names {
            assert!(
                ExportKind::from_str_loose(name).is_some(),
                "all_names() entry '{name}' should parse via from_str_loose"
            );
        }
    }

    #[test]
    fn export_kind_case_insensitive() {
        assert_eq!(
            ExportKind::from_str_loose("SEGMENTS"),
            Some(ExportKind::Segments)
        );
        assert_eq!(
            ExportKind::from_str_loose("Events"),
            Some(ExportKind::Events)
        );
        assert_eq!(ExportKind::from_str_loose("AUDIT"), Some(ExportKind::Audit));
        assert_eq!(
            ExportKind::from_str_loose("Reservations"),
            Some(ExportKind::Reservations)
        );
    }

    #[test]
    fn write_record_pretty_has_indentation() {
        let seg = Segment {
            id: 1,
            pane_id: 1,
            seq: 1,
            content: "test".to_string(),
            content_len: 4,
            content_hash: None,
            captured_at: 1000,
        };
        let mut buf = Vec::new();
        write_record(&mut buf, &seg, true).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains("  \""),
            "Pretty output should have indentation"
        );
    }

    #[test]
    fn header_optional_fields_skipped_when_none() {
        let header = ExportHeader {
            export: true,
            version: "0.1.0".to_string(),
            kind: "events".to_string(),
            redacted: false,
            exported_at_ms: 1000,
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            record_count: 0,
        };
        let json = serde_json::to_string(&header).unwrap();
        assert!(!json.contains("pane_id"));
        assert!(!json.contains("since"));
        assert!(!json.contains("until"));
        assert!(!json.contains("limit"));
        // Required fields still present
        assert!(json.contains("\"_export\""));
        assert!(json.contains("\"version\""));
        assert!(json.contains("\"record_count\""));
    }

    #[test]
    fn redact_event_with_no_secrets_is_noop() {
        let r = Redactor::new();
        let event = StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test".to_string(),
            agent_type: "codex".to_string(),
            event_type: "info".to_string(),
            severity: "info".to_string(),
            confidence: 0.5,
            extracted: Some(serde_json::json!({"note": "clean data"})),
            matched_text: Some("just a normal message".to_string()),
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        let original_text = event.matched_text.clone();
        let original_extracted = event.extracted.clone();
        let redacted = redact_event(event, &r);
        assert_eq!(redacted.matched_text, original_text);
        assert_eq!(redacted.extracted, original_extracted);
    }

    #[test]
    fn redact_event_none_fields_unchanged() {
        let r = Redactor::new();
        let event = StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test".to_string(),
            agent_type: "codex".to_string(),
            event_type: "info".to_string(),
            severity: "info".to_string(),
            confidence: 0.5,
            extracted: None,
            matched_text: None,
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        let redacted = redact_event(event, &r);
        assert!(redacted.matched_text.is_none());
        assert!(redacted.extracted.is_none());
    }

    #[test]
    fn redact_segment_preserves_metadata() {
        let r = Redactor::new();
        let seg = Segment {
            id: 42,
            pane_id: 7,
            seq: 100,
            content: "secret: sk-abc123def456ghi789jkl012mno345pqr678stu901v here".to_string(),
            content_len: 55,
            content_hash: Some("hash123".to_string()),
            captured_at: 5000,
        };
        let redacted = redact_segment(seg, &r);
        // Metadata fields must be preserved
        assert_eq!(redacted.id, 42);
        assert_eq!(redacted.pane_id, 7);
        assert_eq!(redacted.seq, 100);
        assert_eq!(redacted.content_len, 55);
        assert_eq!(redacted.content_hash, Some("hash123".to_string()));
        assert_eq!(redacted.captured_at, 5000);
        // Content should be redacted
        assert!(redacted.content.contains("[REDACTED]"));
    }
}
