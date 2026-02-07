//! Report generation: human-readable Markdown summaries from wa storage.
//!
//! Produces session reports (per-pane or global), including major events,
//! workflow executions with step logs, and output gaps.

use crate::policy::Redactor;
use crate::storage::{AuditQuery, EventQuery, ExportQuery, StorageHandle};

/// Options for report generation.
pub struct ReportOptions {
    /// Filter by pane ID (None = all panes)
    pub pane_id: Option<u64>,
    /// Only include records since this timestamp (epoch ms)
    pub since: Option<i64>,
    /// Only include records until this timestamp (epoch ms)
    pub until: Option<i64>,
    /// Maximum events/workflows to include per section
    pub limit: Option<usize>,
    /// Apply secret redaction
    pub redact: bool,
}

/// Generate a session report as Markdown.
///
/// The report includes:
/// - Summary header with time range and pane filter
/// - Events section with severity, type, and timestamps
/// - Workflows section with status, steps, and errors
/// - Gaps section with reasons and sequence ranges
/// - Audit highlights (policy denials)
pub async fn generate_session_report(
    storage: &StorageHandle,
    opts: &ReportOptions,
) -> crate::Result<String> {
    let redactor = if opts.redact {
        Some(Redactor::new())
    } else {
        None
    };

    let mut md = String::new();

    // ── Header ──────────────────────────────────────────────────────────
    md.push_str("# Session Report\n\n");

    if let Some(pane_id) = opts.pane_id {
        md.push_str(&format!("**Pane:** {pane_id}\n"));
    } else {
        md.push_str("**Pane:** all\n");
    }

    if let Some(since) = opts.since {
        md.push_str(&format!("**Since:** {}\n", format_ts(since)));
    }
    if let Some(until) = opts.until {
        md.push_str(&format!("**Until:** {}\n", format_ts(until)));
    }
    if opts.redact {
        md.push_str("**Redacted:** yes\n");
    }
    md.push('\n');

    let query = ExportQuery {
        pane_id: opts.pane_id,
        since: opts.since,
        until: opts.until,
        limit: opts.limit,
    };

    // ── Events ──────────────────────────────────────────────────────────
    let event_query = EventQuery {
        pane_id: opts.pane_id,
        since: opts.since,
        until: opts.until,
        limit: opts.limit,
        ..Default::default()
    };
    let events = storage.get_events(event_query).await?;

    md.push_str("## Events\n\n");
    if events.is_empty() {
        md.push_str("No events detected.\n\n");
    } else {
        md.push_str("| Severity | Type | Pane | Detected | Detail |\n");
        md.push_str("|----------|------|------|----------|--------|\n");
        for event in &events {
            let detail = match (&event.matched_text, &redactor) {
                (Some(text), Some(r)) => r.redact(text),
                (Some(text), None) => truncate(text, 60),
                (None, _) => "—".to_string(),
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                event.severity,
                event.event_type,
                event.pane_id,
                format_ts(event.detected_at),
                detail.replace('|', "\\|"),
            ));
        }
        md.push('\n');
    }

    // ── Workflows ───────────────────────────────────────────────────────
    let workflows = storage.export_workflows(query.clone()).await?;

    md.push_str("## Workflows\n\n");
    if workflows.is_empty() {
        md.push_str("No workflow executions.\n\n");
    } else {
        for wf in &workflows {
            let status_icon = match wf.status.as_str() {
                "completed" => "✅",
                "failed" | "aborted" => "❌",
                "running" => "🔄",
                _ => "⏳",
            };
            md.push_str(&format!(
                "### {status_icon} {} (`{}`)\n\n",
                wf.workflow_name, wf.id
            ));
            md.push_str(&format!("- **Status:** {}\n", wf.status));
            md.push_str(&format!("- **Pane:** {}\n", wf.pane_id));
            md.push_str(&format!("- **Started:** {}\n", format_ts(wf.started_at)));
            if let Some(completed_at) = wf.completed_at {
                md.push_str(&format!("- **Completed:** {}\n", format_ts(completed_at)));
                let duration_ms = completed_at - wf.started_at;
                md.push_str(&format!(
                    "- **Duration:** {}\n",
                    format_duration(duration_ms)
                ));
            }
            if let Some(ref error) = wf.error {
                let err_text = match &redactor {
                    Some(r) => r.redact(error),
                    None => error.clone(),
                };
                md.push_str(&format!("- **Error:** {err_text}\n"));
            }

            // Step logs
            if let Ok(steps) = storage.get_step_logs(&wf.id).await {
                if !steps.is_empty() {
                    md.push_str("\n**Steps:**\n\n");
                    md.push_str("| # | Step | Result | Duration |\n");
                    md.push_str("|---|------|--------|----------|\n");
                    for step in &steps {
                        md.push_str(&format!(
                            "| {} | {} | {} | {} |\n",
                            step.step_index,
                            step.step_name,
                            step.result_type,
                            format_duration(step.duration_ms),
                        ));
                    }
                }
            }
            md.push('\n');
        }
    }

    // ── Gaps ────────────────────────────────────────────────────────────
    let gaps = storage.export_gaps(query.clone()).await?;

    md.push_str("## Gaps\n\n");
    if gaps.is_empty() {
        md.push_str("No output gaps detected.\n\n");
    } else {
        md.push_str("| Pane | Seq Range | Reason | Detected |\n");
        md.push_str("|------|-----------|--------|----------|\n");
        for gap in &gaps {
            md.push_str(&format!(
                "| {} | {}→{} | {} | {} |\n",
                gap.pane_id,
                gap.seq_before,
                gap.seq_after,
                gap.reason,
                format_ts(gap.detected_at),
            ));
        }
        md.push('\n');
    }

    // ── Audit highlights (denials only) ─────────────────────────────────
    let audit_query = AuditQuery {
        pane_id: opts.pane_id,
        since: opts.since,
        until: opts.until,
        limit: opts.limit,
        ..Default::default()
    };
    let audits = storage.get_audit_actions(audit_query).await?;
    let denials: Vec<_> = audits
        .iter()
        .filter(|a| a.policy_decision != "allow")
        .collect();

    if !denials.is_empty() {
        md.push_str("## Policy Denials\n\n");
        md.push_str("| Time | Actor | Action | Decision | Reason |\n");
        md.push_str("|------|-------|--------|----------|--------|\n");
        for d in &denials {
            let reason = match (&d.decision_reason, &redactor) {
                (Some(r), Some(red)) => red.redact(r),
                (Some(r), None) => truncate(r, 60),
                (None, _) => "—".to_string(),
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                format_ts(d.ts),
                d.actor_kind,
                d.action_kind,
                d.policy_decision,
                reason.replace('|', "\\|"),
            ));
        }
        md.push('\n');
    }

    // ── Footer ──────────────────────────────────────────────────────────
    md.push_str(&format!("---\n*Generated by wa v{}*\n", crate::VERSION));

    Ok(md)
}

/// Format epoch-ms timestamp as a human-readable string.
fn format_ts(epoch_ms: i64) -> String {
    // Simple UTC formatting without external deps
    let secs = epoch_ms / 1000;
    let millis = epoch_ms % 1000;

    // Calculate date/time components from unix timestamp
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Gregorian calendar from days since 1970-01-01
    let (year, month, day) = days_to_ymd(days_since_epoch);

    format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Format duration in milliseconds as human-readable.
fn format_duration(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let minutes = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{minutes}m {secs}s")
    }
}

/// Truncate string to max chars, adding ellipsis if needed.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.min(s.len())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit tests for helpers ──────────────────────────────────────────

    #[test]
    fn format_ts_basic() {
        // 2025-01-15 12:00:45.123 UTC = 1736942445123 ms
        let ts = 1_736_942_445_123;
        let formatted = format_ts(ts);
        assert!(
            formatted.contains("2025-01-15"),
            "Expected date 2025-01-15, got: {formatted}"
        );
        assert!(
            formatted.contains("12:00:45.123Z"),
            "Expected time, got: {formatted}"
        );
    }

    #[test]
    fn format_ts_epoch_zero() {
        assert_eq!(format_ts(0), "1970-01-01 00:00:00.000Z");
    }

    #[test]
    fn format_duration_millis() {
        assert_eq!(format_duration(500), "500ms");
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(999), "999ms");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(1000), "1.0s");
        assert_eq!(format_duration(2500), "2.5s");
        assert_eq!(format_duration(59999), "60.0s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(60_000), "1m 0s");
        assert_eq!(format_duration(90_000), "1m 30s");
        assert_eq!(format_duration(3_600_000), "60m 0s");
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long() {
        let result = truncate("hello world, this is long", 10);
        assert!(result.len() <= 14); // 10 + ellipsis char
        assert!(result.ends_with('…'));
    }

    #[test]
    fn days_to_ymd_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2025-01-01 = day 20089 from epoch
        assert_eq!(days_to_ymd(20089), (2025, 1, 1));
    }

    // ── Fixture-driven integration tests ────────────────────────────────

    async fn test_db(suffix: &str) -> (StorageHandle, std::path::PathBuf) {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_report_{suffix}_{}.db", std::process::id()));
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
            last_seen_at: 5000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();

        (storage, tmp)
    }

    #[tokio::test]
    async fn report_empty_db() {
        let (storage, tmp) = test_db("empty").await;

        let opts = ReportOptions {
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            redact: false,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        assert!(report.contains("# Session Report"));
        assert!(report.contains("**Pane:** all"));
        assert!(report.contains("## Events"));
        assert!(report.contains("No events detected."));
        assert!(report.contains("## Workflows"));
        assert!(report.contains("No workflow executions."));
        assert!(report.contains("## Gaps"));
        assert!(report.contains("No output gaps detected."));
        assert!(report.contains(&format!("wa v{}", crate::VERSION)));
        // No Policy Denials section when there are none
        assert!(!report.contains("## Policy Denials"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn report_with_events() {
        let (storage, tmp) = test_db("events").await;

        let event = crate::storage::StoredEvent {
            id: 0,
            pane_id: 1,
            rule_id: "api_key_leak".to_string(),
            agent_type: "codex".to_string(),
            event_type: "secret_detected".to_string(),
            severity: "critical".to_string(),
            confidence: 0.95,
            extracted: None,
            matched_text: Some("Found API key in output".to_string()),
            segment_id: None,
            detected_at: 2000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        storage.record_event(event).await.unwrap();

        let opts = ReportOptions {
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            redact: false,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        assert!(report.contains("## Events"));
        assert!(!report.contains("No events detected."));
        assert!(report.contains("critical"));
        assert!(report.contains("secret_detected"));
        assert!(report.contains("Found API key in output"));
        // Should have table headers
        assert!(report.contains("| Severity | Type | Pane | Detected | Detail |"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn report_with_workflows() {
        let (storage, tmp) = test_db("workflows").await;

        let wf = crate::storage::WorkflowRecord {
            id: "wf-auth-1".to_string(),
            workflow_name: "auth_recovery".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 2,
            status: "completed".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: 1000,
            updated_at: 3000,
            completed_at: Some(3000),
        };
        storage.upsert_workflow(wf).await.unwrap();

        storage
            .insert_step_log(
                "wf-auth-1",
                None,
                0,
                "wait_for_prompt",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                1000,
                1500,
            )
            .await
            .unwrap();
        storage
            .insert_step_log(
                "wf-auth-1",
                None,
                1,
                "send_credentials",
                None,
                None,
                "done",
                None,
                None,
                None,
                None,
                1500,
                3000,
            )
            .await
            .unwrap();

        let opts = ReportOptions {
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            redact: false,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        assert!(report.contains("## Workflows"));
        assert!(!report.contains("No workflow executions."));
        assert!(report.contains("auth_recovery"));
        assert!(report.contains("`wf-auth-1`"));
        assert!(report.contains("**Status:** completed"));
        assert!(report.contains("**Duration:**"));
        assert!(report.contains("wait_for_prompt"));
        assert!(report.contains("send_credentials"));
        // Table structure
        assert!(report.contains("| # | Step | Result | Duration |"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn report_workflow_with_error() {
        let (storage, tmp) = test_db("wf_error").await;

        let wf = crate::storage::WorkflowRecord {
            id: "wf-fail-1".to_string(),
            workflow_name: "fix_build".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 0,
            status: "failed".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: Some("Timeout waiting for prompt".to_string()),
            started_at: 1000,
            updated_at: 5000,
            completed_at: Some(5000),
        };
        storage.upsert_workflow(wf).await.unwrap();

        let opts = ReportOptions {
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            redact: false,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        assert!(report.contains("❌ fix_build"));
        assert!(report.contains("**Error:** Timeout waiting for prompt"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn report_with_gaps() {
        let (storage, tmp) = test_db("gaps").await;

        // Need a segment first for gap recording
        storage.append_segment(1, "before gap", None).await.unwrap();
        storage.record_gap(1, "timeout").await.unwrap();

        let opts = ReportOptions {
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            redact: false,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        assert!(report.contains("## Gaps"));
        assert!(!report.contains("No output gaps detected."));
        assert!(report.contains("timeout"));
        // Table structure
        assert!(report.contains("| Pane | Seq Range | Reason | Detected |"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn report_with_policy_denials() {
        let (storage, tmp) = test_db("denials").await;

        let action = crate::storage::AuditActionRecord {
            id: 0,
            ts: 2000,
            actor_kind: "workflow".to_string(),
            actor_id: Some("wf-1".to_string()),
            correlation_id: None,
            pane_id: Some(1),
            domain: None,
            action_kind: "send_text".to_string(),
            policy_decision: "deny".to_string(),
            decision_reason: Some("Rate limit exceeded".to_string()),
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "blocked".to_string(),
        };
        storage.record_audit_action(action).await.unwrap();

        // Also add an allow action — should NOT appear in denials
        let allow_action = crate::storage::AuditActionRecord {
            id: 0,
            ts: 3000,
            actor_kind: "operator".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: None,
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "ok".to_string(),
        };
        storage.record_audit_action(allow_action).await.unwrap();

        let opts = ReportOptions {
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            redact: false,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        assert!(report.contains("## Policy Denials"));
        assert!(report.contains("deny"));
        assert!(report.contains("Rate limit exceeded"));
        assert!(report.contains("workflow"));
        // The allow action should NOT show up in denials table
        let denial_section = report.split("## Policy Denials").nth(1).unwrap();
        assert!(!denial_section.contains("operator"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn report_with_redaction() {
        let (storage, tmp) = test_db("redact").await;

        let event = crate::storage::StoredEvent {
            id: 0,
            pane_id: 1,
            rule_id: "leak".to_string(),
            agent_type: "codex".to_string(),
            event_type: "secret".to_string(),
            severity: "critical".to_string(),
            confidence: 1.0,
            extracted: None,
            matched_text: Some("Key: sk-abc123def456ghi789jkl012mno345pqr678stu901v".to_string()),
            segment_id: None,
            detected_at: 2000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        storage.record_event(event).await.unwrap();

        let wf = crate::storage::WorkflowRecord {
            id: "wf-1".to_string(),
            workflow_name: "fix".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 0,
            status: "failed".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: Some("Token ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ123456789012 expired".to_string()),
            started_at: 1000,
            updated_at: 2000,
            completed_at: Some(2000),
        };
        storage.upsert_workflow(wf).await.unwrap();

        let opts = ReportOptions {
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            redact: true,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        assert!(report.contains("**Redacted:** yes"));
        // Secrets should be redacted
        assert!(!report.contains("sk-abc123"));
        assert!(report.contains("[REDACTED]"));
        // GitHub PAT in workflow error should be redacted
        assert!(!report.contains("ghp_"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn report_pane_filter() {
        let (storage, tmp) = test_db("pane_filter").await;

        // Add pane 2
        let pane2 = crate::storage::PaneRecord {
            pane_id: 2,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: 1000,
            last_seen_at: 5000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane2).await.unwrap();

        // Event on pane 1
        let event1 = crate::storage::StoredEvent {
            id: 0,
            pane_id: 1,
            rule_id: "r1".to_string(),
            agent_type: "codex".to_string(),
            event_type: "pane1_event".to_string(),
            severity: "info".to_string(),
            confidence: 0.5,
            extracted: None,
            matched_text: Some("pane1 detail".to_string()),
            segment_id: None,
            detected_at: 2000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        storage.record_event(event1).await.unwrap();

        // Event on pane 2
        let event2 = crate::storage::StoredEvent {
            id: 0,
            pane_id: 2,
            rule_id: "r2".to_string(),
            agent_type: "codex".to_string(),
            event_type: "pane2_event".to_string(),
            severity: "warning".to_string(),
            confidence: 0.5,
            extracted: None,
            matched_text: Some("pane2 detail".to_string()),
            segment_id: None,
            detected_at: 3000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        storage.record_event(event2).await.unwrap();

        // Report filtered to pane 1 only
        let opts = ReportOptions {
            pane_id: Some(1),
            since: None,
            until: None,
            limit: None,
            redact: false,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        assert!(report.contains("**Pane:** 1"));
        assert!(report.contains("pane1_event"));
        assert!(!report.contains("pane2_event"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn report_full_fixture() {
        // Comprehensive fixture: events + workflows + gaps + denials
        let (storage, tmp) = test_db("full").await;

        // Segment + gap
        storage
            .append_segment(1, "initial output", None)
            .await
            .unwrap();
        storage.record_gap(1, "network timeout").await.unwrap();

        // Event
        let event = crate::storage::StoredEvent {
            id: 0,
            pane_id: 1,
            rule_id: "auth_required".to_string(),
            agent_type: "claude_code".to_string(),
            event_type: "auth.prompt".to_string(),
            severity: "warning".to_string(),
            confidence: 0.85,
            extracted: None,
            matched_text: Some("Please authenticate".to_string()),
            segment_id: None,
            detected_at: 2000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        storage.record_event(event).await.unwrap();

        // Workflow with steps
        let wf = crate::storage::WorkflowRecord {
            id: "wf-full-1".to_string(),
            workflow_name: "auth_handler".to_string(),
            pane_id: 1,
            trigger_event_id: Some(1),
            current_step: 1,
            status: "completed".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: 2000,
            updated_at: 4000,
            completed_at: Some(4000),
        };
        storage.upsert_workflow(wf).await.unwrap();

        storage
            .insert_step_log(
                "wf-full-1",
                None,
                0,
                "detect_prompt",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                2000,
                2500,
            )
            .await
            .unwrap();
        storage
            .insert_step_log(
                "wf-full-1",
                None,
                1,
                "authenticate",
                None,
                None,
                "done",
                None,
                None,
                None,
                None,
                2500,
                4000,
            )
            .await
            .unwrap();

        // Audit denial
        let denial = crate::storage::AuditActionRecord {
            id: 0,
            ts: 1500,
            actor_kind: "workflow".to_string(),
            actor_id: Some("wf-full-1".to_string()),
            correlation_id: None,
            pane_id: Some(1),
            domain: None,
            action_kind: "send_text".to_string(),
            policy_decision: "deny".to_string(),
            decision_reason: Some("Cooldown active".to_string()),
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "blocked".to_string(),
        };
        storage.record_audit_action(denial).await.unwrap();

        let opts = ReportOptions {
            pane_id: Some(1),
            since: None,
            until: None,
            limit: None,
            redact: false,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        // Verify all sections are present and populated
        assert!(report.contains("# Session Report"));
        assert!(report.contains("**Pane:** 1"));

        // Events
        assert!(report.contains("auth.prompt"));
        assert!(report.contains("warning"));

        // Workflows
        assert!(report.contains("auth_handler"));
        assert!(report.contains("detect_prompt"));
        assert!(report.contains("authenticate"));
        assert!(report.contains("**Status:** completed"));
        assert!(report.contains("**Duration:**"));

        // Gaps
        assert!(report.contains("network timeout"));

        // Denials
        assert!(report.contains("## Policy Denials"));
        assert!(report.contains("Cooldown active"));

        // Footer
        assert!(report.contains(&format!("wa v{}", crate::VERSION)));

        // Stable heading order
        let events_pos = report.find("## Events").unwrap();
        let workflows_pos = report.find("## Workflows").unwrap();
        let gaps_pos = report.find("## Gaps").unwrap();
        let denials_pos = report.find("## Policy Denials").unwrap();
        assert!(events_pos < workflows_pos);
        assert!(workflows_pos < gaps_pos);
        assert!(gaps_pos < denials_pos);

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn report_heading_order_is_stable() {
        let (storage, tmp) = test_db("order").await;

        let opts = ReportOptions {
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            redact: false,
        };

        let report = generate_session_report(&storage, &opts).await.unwrap();

        // Even with empty data, sections appear in defined order
        let header_pos = report.find("# Session Report").unwrap();
        let events_pos = report.find("## Events").unwrap();
        let workflows_pos = report.find("## Workflows").unwrap();
        let gaps_pos = report.find("## Gaps").unwrap();
        let footer_pos = report.find("---").unwrap();

        assert!(header_pos < events_pos);
        assert!(events_pos < workflows_pos);
        assert!(workflows_pos < gaps_pos);
        assert!(gaps_pos < footer_pos);

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    }
}
