//! Expanded property-based tests for storage.rs record and query types.
//!
//! Covers WorkflowRecord, WorkflowActionPlanRecord, PreparedPlanRecord,
//! WorkflowStepLogRecord, AuditActionRecord, AuditStreamRecord,
//! ActionUndoRecord, ActionHistoryRecord, NotificationHistoryRecord,
//! NotificationStatus, SavedSearchRecord, PaneBookmarkRecord,
//! TimelineQuery builder, and query type defaults.
//!
//! Complements existing proptest_storage.rs, proptest_storage_health.rs,
//! proptest_storage_targets.rs, and proptest_storage_telemetry.rs.

use frankenterm_core::storage::{
    ActionHistoryRecord, ActionUndoRecord, AuditActionRecord, AuditStreamRecord,
    NotificationHistoryRecord, NotificationStatus, PaneBookmarkRecord, PreparedPlanRecord,
    SavedSearchRecord, TimelineQuery, WorkflowActionPlanRecord, WorkflowRecord,
    WorkflowStepLogRecord,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_id_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_-]{4,20}"
}

fn arb_short_text() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_ ]{1,40}"
}

fn arb_notification_status() -> impl Strategy<Value = NotificationStatus> {
    prop_oneof![
        Just(NotificationStatus::Pending),
        Just(NotificationStatus::Sent),
        Just(NotificationStatus::Failed),
        Just(NotificationStatus::Throttled),
    ]
}

fn arb_actor_kind() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("human".to_string()),
        Just("robot".to_string()),
        Just("mcp".to_string()),
        Just("workflow".to_string()),
    ]
}

fn arb_policy_decision() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("allow".to_string()),
        Just("deny".to_string()),
        Just("require_approval".to_string()),
    ]
}

fn arb_result_value() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("success".to_string()),
        Just("denied".to_string()),
        Just("failed".to_string()),
        Just("timeout".to_string()),
    ]
}

fn arb_workflow_status() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("running".to_string()),
        Just("waiting".to_string()),
        Just("completed".to_string()),
        Just("aborted".to_string()),
    ]
}

fn arb_result_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("continue".to_string()),
        Just("done".to_string()),
        Just("retry".to_string()),
        Just("abort".to_string()),
        Just("wait_for".to_string()),
    ]
}

fn arb_undo_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("none".to_string()),
        Just("manual".to_string()),
        Just("workflow_abort".to_string()),
        Just("pane_close".to_string()),
        Just("custom".to_string()),
    ]
}

fn arb_since_mode() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("last_run".to_string()),
        Just("fixed".to_string()),
    ]
}

fn arb_severity() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("info".to_string()),
        Just("warning".to_string()),
        Just("error".to_string()),
        Just("critical".to_string()),
    ]
}

fn arb_timestamp() -> impl Strategy<Value = i64> {
    1_700_000_000_000i64..1_800_000_000_000i64
}

// ── NotificationStatus ──────────────────────────────────────────────────────

proptest! {
    #[test]
    fn notification_status_serde_roundtrip(status in arb_notification_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: NotificationStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn notification_status_as_str_roundtrip(status in arb_notification_status()) {
        let s = status.as_str();
        let back: NotificationStatus = s.parse().unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn notification_status_display_matches_as_str(status in arb_notification_status()) {
        let display = format!("{}", status);
        let as_str = status.as_str();
        prop_assert_eq!(&display, as_str);
    }
}

// ── WorkflowRecord ──────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn workflow_record_serde_roundtrip(
        id in arb_id_string(),
        workflow_name in arb_short_text(),
        pane_id in any::<u64>(),
        current_step in 0usize..50,
        status in arb_workflow_status(),
        started_at in arb_timestamp(),
    ) {
        let updated_at = started_at + 100;
        let record = WorkflowRecord {
            id: id.clone(),
            workflow_name: workflow_name.clone(),
            pane_id,
            trigger_event_id: None,
            current_step,
            status: status.clone(),
            wait_condition: None,
            context: Some(serde_json::json!({"key": "value"})),
            result: None,
            error: None,
            started_at,
            updated_at,
            completed_at: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: WorkflowRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &id);
        prop_assert_eq!(&back.workflow_name, &workflow_name);
        prop_assert_eq!(back.pane_id, pane_id);
        prop_assert_eq!(back.current_step, current_step);
        prop_assert_eq!(&back.status, &status);
        prop_assert_eq!(back.started_at, started_at);
        prop_assert_eq!(back.updated_at, updated_at);
    }

    #[test]
    fn workflow_record_completed_has_timestamp(
        id in arb_id_string(),
        started_at in arb_timestamp(),
        elapsed in 0i64..100_000,
    ) {
        let completed_at = started_at + elapsed;
        let record = WorkflowRecord {
            id,
            workflow_name: "test_wf".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 0,
            status: "completed".to_string(),
            wait_condition: None,
            context: None,
            result: Some(serde_json::json!({"ok": true})),
            error: None,
            started_at,
            updated_at: completed_at,
            completed_at: Some(completed_at),
        };
        prop_assert!(record.completed_at.unwrap() >= record.started_at);
    }
}

// ── WorkflowActionPlanRecord ────────────────────────────────────────────────

proptest! {
    #[test]
    fn workflow_action_plan_record_serde_roundtrip(
        workflow_id in arb_id_string(),
        plan_id in arb_id_string(),
        plan_hash in "[a-f0-9]{16}",
        created_at in arb_timestamp(),
    ) {
        let plan_json = serde_json::json!({"steps": []}).to_string();
        let record = WorkflowActionPlanRecord {
            workflow_id: workflow_id.clone(),
            plan_id: plan_id.clone(),
            plan_hash: plan_hash.clone(),
            plan_json: plan_json.clone(),
            created_at,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: WorkflowActionPlanRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.workflow_id, &workflow_id);
        prop_assert_eq!(&back.plan_id, &plan_id);
        prop_assert_eq!(&back.plan_hash, &plan_hash);
        prop_assert_eq!(&back.plan_json, &plan_json);
        prop_assert_eq!(back.created_at, created_at);
    }
}

// ── PreparedPlanRecord ──────────────────────────────────────────────────────

proptest! {
    #[test]
    fn prepared_plan_record_serde_roundtrip(
        plan_id in arb_id_string(),
        plan_hash in "[a-f0-9]{16}",
        workspace_id in arb_id_string(),
        action_kind in arb_short_text(),
        requires_approval in any::<bool>(),
        created_at in arb_timestamp(),
        ttl_ms in 1000i64..3_600_000,
    ) {
        let expires_at = created_at + ttl_ms;
        let record = PreparedPlanRecord {
            plan_id: plan_id.clone(),
            plan_hash: plan_hash.clone(),
            workspace_id: workspace_id.clone(),
            action_kind: action_kind.clone(),
            pane_id: Some(42),
            pane_uuid: None,
            params_json: None,
            plan_json: "{}".to_string(),
            requires_approval,
            created_at,
            expires_at,
            consumed_at: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: PreparedPlanRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.plan_id, &plan_id);
        prop_assert_eq!(back.requires_approval, requires_approval);
        prop_assert!(back.expires_at > back.created_at, "expires_at should be after created_at");
    }

    #[test]
    fn prepared_plan_temporal_invariant(
        created_at in arb_timestamp(),
        ttl_ms in 1000i64..3_600_000,
    ) {
        let expires_at = created_at + ttl_ms;
        let record = PreparedPlanRecord {
            plan_id: "p1".to_string(),
            plan_hash: "abc123".to_string(),
            workspace_id: "ws1".to_string(),
            action_kind: "send_text".to_string(),
            pane_id: None,
            pane_uuid: None,
            params_json: None,
            plan_json: "{}".to_string(),
            requires_approval: false,
            created_at,
            expires_at,
            consumed_at: None,
        };
        prop_assert!(record.expires_at > record.created_at);
    }
}

// ── WorkflowStepLogRecord ───────────────────────────────────────────────────

proptest! {
    #[test]
    fn workflow_step_log_record_serde_roundtrip(
        id in any::<i64>(),
        workflow_id in arb_id_string(),
        step_index in 0usize..50,
        step_name in arb_short_text(),
        result_type in arb_result_type(),
        started_at in arb_timestamp(),
        duration_ms in 0i64..60_000,
    ) {
        let completed_at = started_at + duration_ms;
        let record = WorkflowStepLogRecord {
            id,
            workflow_id: workflow_id.clone(),
            audit_action_id: None,
            step_index,
            step_name: step_name.clone(),
            step_id: None,
            step_kind: None,
            result_type: result_type.clone(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at,
            completed_at,
            duration_ms,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: WorkflowStepLogRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, id);
        prop_assert_eq!(&back.workflow_id, &workflow_id);
        prop_assert_eq!(back.step_index, step_index);
        prop_assert_eq!(&back.result_type, &result_type);
        prop_assert_eq!(back.duration_ms, duration_ms);
    }

    #[test]
    fn workflow_step_log_duration_consistency(
        started_at in arb_timestamp(),
        duration_ms in 0i64..60_000,
    ) {
        let completed_at = started_at + duration_ms;
        let record = WorkflowStepLogRecord {
            id: 1,
            workflow_id: "wf1".to_string(),
            audit_action_id: None,
            step_index: 0,
            step_name: "step1".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "continue".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at,
            completed_at,
            duration_ms,
        };
        prop_assert_eq!(record.completed_at - record.started_at, record.duration_ms);
    }
}

// ── AuditActionRecord ───────────────────────────────────────────────────────

proptest! {
    #[test]
    fn audit_action_record_serde_roundtrip(
        id in any::<i64>(),
        ts in arb_timestamp(),
        actor_kind in arb_actor_kind(),
        action_kind in arb_short_text(),
        policy_decision in arb_policy_decision(),
        result in arb_result_value(),
    ) {
        let record = AuditActionRecord {
            id,
            ts,
            actor_kind: actor_kind.clone(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(42),
            domain: None,
            action_kind: action_kind.clone(),
            policy_decision: policy_decision.clone(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: result.clone(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: AuditActionRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, id);
        prop_assert_eq!(back.ts, ts);
        prop_assert_eq!(&back.actor_kind, &actor_kind);
        prop_assert_eq!(&back.action_kind, &action_kind);
        prop_assert_eq!(&back.policy_decision, &policy_decision);
        prop_assert_eq!(&back.result, &result);
    }
}

// ── AuditStreamRecord ───────────────────────────────────────────────────────

proptest! {
    #[test]
    fn audit_stream_record_serde_roundtrip(
        id in any::<i64>(),
        ts in arb_timestamp(),
        actor_kind in arb_actor_kind(),
        action_kind in arb_short_text(),
        policy_decision in arb_policy_decision(),
        result in arb_result_value(),
    ) {
        let record = AuditStreamRecord {
            id,
            ts,
            actor_kind: actor_kind.clone(),
            actor_id: None,
            correlation_id: None,
            pane_id: None,
            domain: None,
            action_kind: action_kind.clone(),
            policy_decision: policy_decision.clone(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: result.clone(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: AuditStreamRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, id);
        prop_assert_eq!(&back.actor_kind, &actor_kind);
        prop_assert_eq!(&back.policy_decision, &policy_decision);
    }
}

// ── ActionUndoRecord ────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn action_undo_record_serde_roundtrip(
        audit_action_id in any::<i64>(),
        undoable in any::<bool>(),
        undo_strategy in arb_undo_strategy(),
    ) {
        let record = ActionUndoRecord {
            audit_action_id,
            undoable,
            undo_strategy: undo_strategy.clone(),
            undo_hint: None,
            undo_payload: None,
            undone_at: None,
            undone_by: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: ActionUndoRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.audit_action_id, audit_action_id);
        prop_assert_eq!(back.undoable, undoable);
        prop_assert_eq!(&back.undo_strategy, &undo_strategy);
    }
}

// ── NotificationHistoryRecord ───────────────────────────────────────────────

proptest! {
    #[test]
    fn notification_history_record_serde_roundtrip(
        id in any::<i64>(),
        timestamp in arb_timestamp(),
        channel in arb_short_text(),
        title in arb_short_text(),
        body in arb_short_text(),
        severity in arb_severity(),
        status in arb_notification_status(),
        retry_count in 0i64..10,
    ) {
        let record = NotificationHistoryRecord {
            id,
            timestamp,
            event_id: None,
            channel: channel.clone(),
            title: title.clone(),
            body: body.clone(),
            severity: severity.clone(),
            status,
            error_message: None,
            acknowledged_at: None,
            acknowledged_by: None,
            action_taken: None,
            retry_count,
            metadata: None,
            created_at: timestamp,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: NotificationHistoryRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, id);
        prop_assert_eq!(&back.channel, &channel);
        prop_assert_eq!(&back.title, &title);
        prop_assert_eq!(back.status, status);
        prop_assert_eq!(back.retry_count, retry_count);
    }
}

// ── SavedSearchRecord ───────────────────────────────────────────────────────

proptest! {
    #[test]
    fn saved_search_record_serde_roundtrip(
        name in arb_short_text(),
        query in arb_short_text(),
        limit in 1i64..1000,
        since_mode in arb_since_mode(),
        enabled in any::<bool>(),
        created_at in arb_timestamp(),
    ) {
        let record = SavedSearchRecord {
            id: "ss-test-1".to_string(),
            name: name.clone(),
            query: query.clone(),
            pane_id: None,
            limit,
            since_mode: since_mode.clone(),
            since_ms: if since_mode == "fixed" { Some(created_at - 60_000) } else { None },
            schedule_interval_ms: None,
            enabled,
            last_run_at: None,
            last_result_count: None,
            last_error: None,
            created_at,
            updated_at: created_at,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: SavedSearchRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
        prop_assert_eq!(&back.query, &query);
        prop_assert_eq!(back.limit, limit);
        prop_assert_eq!(&back.since_mode, &since_mode);
        prop_assert_eq!(back.enabled, enabled);
    }

    #[test]
    fn saved_search_fixed_mode_has_since_ms(
        created_at in arb_timestamp(),
        offset in 1000i64..3_600_000,
    ) {
        let since_ms = created_at - offset;
        let record = SavedSearchRecord {
            id: "ss-test-2".to_string(),
            name: "test search".to_string(),
            query: "hello".to_string(),
            pane_id: None,
            limit: 50,
            since_mode: "fixed".to_string(),
            since_ms: Some(since_ms),
            schedule_interval_ms: None,
            enabled: true,
            last_run_at: None,
            last_result_count: None,
            last_error: None,
            created_at,
            updated_at: created_at,
        };
        prop_assert!(record.since_ms.is_some(), "fixed mode should have since_ms");
        prop_assert!(record.since_ms.unwrap() < record.created_at);
    }
}

// ── PaneBookmarkRecord ──────────────────────────────────────────────────────

proptest! {
    #[test]
    fn pane_bookmark_record_serde_roundtrip(
        id in any::<i64>(),
        pane_id in any::<u64>(),
        alias in arb_short_text(),
        created_at in arb_timestamp(),
    ) {
        let record = PaneBookmarkRecord {
            id,
            pane_id,
            alias: alias.clone(),
            tags: Some(vec!["dev".to_string(), "test".to_string()]),
            description: Some("test bookmark".to_string()),
            created_at,
            updated_at: created_at,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: PaneBookmarkRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, id);
        prop_assert_eq!(back.pane_id, pane_id);
        prop_assert_eq!(&back.alias, &alias);
        prop_assert!(back.tags.is_some());
        prop_assert_eq!(back.tags.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn pane_bookmark_updated_at_consistency(
        created_at in arb_timestamp(),
        update_delta in 0i64..100_000,
    ) {
        let updated_at = created_at + update_delta;
        let record = PaneBookmarkRecord {
            id: 1,
            pane_id: 42,
            alias: "test".to_string(),
            tags: None,
            description: None,
            created_at,
            updated_at,
        };
        prop_assert!(record.updated_at >= record.created_at);
    }
}

// ── TimelineQuery builder ───────────────────────────────────────────────────

proptest! {
    #[test]
    fn timeline_query_builder_preserves_range(
        start in arb_timestamp(),
        duration in 1000i64..3_600_000,
    ) {
        let end = start + duration;
        let query = TimelineQuery::new().with_range(start, end);
        prop_assert_eq!(query.start, Some(start));
        prop_assert_eq!(query.end, Some(end));
    }

    #[test]
    fn timeline_query_builder_preserves_panes(
        pane_ids in proptest::collection::vec(1u64..1000, 1..5),
    ) {
        let query = TimelineQuery::new().with_panes(pane_ids.clone());
        prop_assert_eq!(query.pane_ids.as_ref().unwrap(), &pane_ids);
    }

    #[test]
    fn timeline_query_builder_preserves_pagination(
        limit in 1usize..1000,
        offset in 0usize..100,
    ) {
        let query = TimelineQuery::new().with_pagination(limit, offset);
        prop_assert_eq!(query.limit, limit);
        prop_assert_eq!(query.offset, offset);
    }

    #[test]
    fn timeline_query_builder_unhandled_only(
        start in arb_timestamp(),
        duration in 1000i64..3_600_000,
    ) {
        let end = start + duration;
        let query = TimelineQuery::new()
            .with_range(start, end)
            .unhandled_only();
        prop_assert!(query.unhandled_only);
        // Range should still be preserved
        prop_assert_eq!(query.start, Some(start));
        prop_assert_eq!(query.end, Some(end));
    }
}

// ── TimelineQuery defaults ──────────────────────────────────────────────────

#[test]
fn timeline_query_default_values() {
    let query = TimelineQuery::new();
    assert_eq!(query.limit, 100);
    assert!(query.include_correlations);
    assert!(!query.unhandled_only);
    assert!(query.start.is_none());
    assert!(query.end.is_none());
    assert!(query.pane_ids.is_none());
    assert!(query.severities.is_none());
    assert!(query.event_types.is_none());
    assert_eq!(query.offset, 0);
}

// ── ActionHistoryRecord ─────────────────────────────────────────────────────

proptest! {
    #[test]
    fn action_history_record_serde_roundtrip(
        id in any::<i64>(),
        ts in arb_timestamp(),
        actor_kind in arb_actor_kind(),
        action_kind in arb_short_text(),
        policy_decision in arb_policy_decision(),
        result in arb_result_value(),
        undoable in any::<bool>(),
        undo_strategy in arb_undo_strategy(),
    ) {
        let record = ActionHistoryRecord {
            id,
            ts,
            actor_kind: actor_kind.clone(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(42),
            domain: None,
            action_kind: action_kind.clone(),
            policy_decision: policy_decision.clone(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: result.clone(),
            undoable: Some(undoable),
            undo_strategy: Some(undo_strategy.clone()),
            undone_at: None,
            workflow_id: None,
            step_name: None,
            undo_hint: None,
            undone_by: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: ActionHistoryRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, id);
        prop_assert_eq!(&back.actor_kind, &actor_kind);
        prop_assert_eq!(&back.action_kind, &action_kind);
        prop_assert_eq!(&back.policy_decision, &policy_decision);
        prop_assert_eq!(&back.result, &result);
        prop_assert_eq!(back.undoable, Some(undoable));
        prop_assert_eq!(&back.undo_strategy, &Some(undo_strategy));
    }
}
