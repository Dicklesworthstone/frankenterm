//! LabRuntime-ported report generation tests for deterministic async testing.
//!
//! Ports all 10 `#[tokio::test]` integration tests from `reports.rs` to
//! asupersync-based `RuntimeFixture`, gaining deterministic scheduling for
//! StorageHandle I/O and report generation.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::VERSION;
use frankenterm_core::reports::{ReportOptions, generate_session_report};
use frankenterm_core::storage::{
    AuditActionRecord, PaneRecord, StorageHandle, StoredEvent, WorkflowRecord,
};

// ===========================================================================
// Helpers
// ===========================================================================

async fn test_db(suffix: &str) -> (StorageHandle, std::path::PathBuf) {
    let tmp = std::env::temp_dir().join(format!(
        "wa_test_report_labrt_{suffix}_{}.db",
        std::process::id()
    ));
    let db_path = tmp.to_string_lossy().to_string();
    let storage = StorageHandle::new(&db_path).await.unwrap();

    let pane = PaneRecord {
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

// ===========================================================================
// Tests
// ===========================================================================

#[test]
fn report_empty_db() {
    RuntimeFixture::current_thread().block_on(async {
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
        assert!(report.contains(&format!("ft v{}", VERSION)));
        // No Policy Denials section when there are none
        assert!(!report.contains("## Policy Denials"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&tmp);
    });
}

#[test]
fn report_with_events() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, tmp) = test_db("events").await;

        let event = StoredEvent {
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
    });
}

#[test]
fn report_with_workflows() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, tmp) = test_db("workflows").await;

        let wf = WorkflowRecord {
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
    });
}

#[test]
fn report_workflow_with_error() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, tmp) = test_db("wf_error").await;

        let wf = WorkflowRecord {
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
    });
}

#[test]
fn report_with_gaps() {
    RuntimeFixture::current_thread().block_on(async {
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
    });
}

#[test]
fn report_with_policy_denials() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, tmp) = test_db("denials").await;

        let action = AuditActionRecord {
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

        // Also add an allow action -- should NOT appear in denials
        let allow_action = AuditActionRecord {
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
    });
}

#[test]
fn report_with_redaction() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, tmp) = test_db("redact").await;

        let event = StoredEvent {
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

        let wf = WorkflowRecord {
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
    });
}

#[test]
fn report_pane_filter() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, tmp) = test_db("pane_filter").await;

        // Add pane 2
        let pane2 = PaneRecord {
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
        let event1 = StoredEvent {
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
        let event2 = StoredEvent {
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
    });
}

#[test]
fn report_full_fixture() {
    RuntimeFixture::current_thread().block_on(async {
        // Comprehensive fixture: events + workflows + gaps + denials
        let (storage, tmp) = test_db("full").await;

        // Segment + gap
        storage
            .append_segment(1, "initial output", None)
            .await
            .unwrap();
        storage.record_gap(1, "network timeout").await.unwrap();

        // Event
        let event = StoredEvent {
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
        let wf = WorkflowRecord {
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
        let denial = AuditActionRecord {
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
        assert!(report.contains(&format!("ft v{}", VERSION)));

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
    });
}

#[test]
fn report_heading_order_is_stable() {
    RuntimeFixture::current_thread().block_on(async {
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
    });
}
