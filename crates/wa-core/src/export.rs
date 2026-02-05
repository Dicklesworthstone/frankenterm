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
}
