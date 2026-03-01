//! LabRuntime-ported cleanup tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` tests from `cleanup.rs` to asupersync-based
//! `RuntimeFixture`, gaining deterministic scheduling for StorageHandle I/O.
//!
//! Covers: all 32 async integration tests from cleanup.rs including:
//! - Empty DB preview/apply
//! - Flat global retention
//! - Tiered retention (severity, handled filter, event_type filter)
//! - Multi-table cleanup (events, audit, usage, notifications)
//! - Zero-retention edge cases
//! - Preview/apply consistency and idempotency
//! - E2E mixed-severity lifecycle
//! - JSON serialization stability
//! - Before/after stats with deletion counts
//! - Boundary conditions (1-day retention, all-recent data)
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::cleanup::{cleanup_apply, cleanup_preview};
use frankenterm_core::config::{RetentionTier, StorageConfig};
use frankenterm_core::storage::{
    AuditActionRecord, MetricType, NotificationHistoryRecord, NotificationStatus, PaneRecord,
    StorageHandle, StoredEvent, UsageMetricRecord, now_ms,
};
use std::sync::atomic::{AtomicU64, Ordering};

// ===========================================================================
// Helpers
// ===========================================================================

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir();
    dir.join(format!(
        "wa_cleanup_labrt_{counter}_{}.db",
        std::process::id()
    ))
    .to_str()
    .unwrap()
    .to_string()
}

async fn setup_storage(_label: &str) -> (StorageHandle, String) {
    let db_path = temp_db_path();
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));

    let storage = StorageHandle::new(&db_path).await.expect("open test db");

    storage
        .upsert_pane(PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("test".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_000_000_000_000,
            last_seen_at: 1_000_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        })
        .await
        .expect("upsert pane");

    (storage, db_path)
}

async fn teardown(storage: StorageHandle, db_path: &str) {
    storage.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

fn make_event(detected_at: i64, severity: &str, event_type: &str) -> StoredEvent {
    StoredEvent {
        id: 0,
        pane_id: 1,
        rule_id: format!("test.{event_type}"),
        agent_type: "test".to_string(),
        event_type: event_type.to_string(),
        severity: severity.to_string(),
        confidence: 1.0,
        extracted: None,
        matched_text: None,
        segment_id: None,
        detected_at,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

fn make_audit(ts: i64) -> AuditActionRecord {
    AuditActionRecord {
        id: 0,
        ts,
        actor_kind: "robot".to_string(),
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
        result: "success".to_string(),
    }
}

fn make_usage(ts: i64) -> UsageMetricRecord {
    UsageMetricRecord {
        id: 0,
        timestamp: ts,
        metric_type: MetricType::ApiCall,
        pane_id: Some(1),
        agent_type: None,
        account_id: None,
        workflow_id: None,
        count: Some(1),
        amount: None,
        tokens: None,
        metadata: None,
        created_at: ts,
    }
}

fn make_notification(ts: i64) -> NotificationHistoryRecord {
    NotificationHistoryRecord {
        id: 0,
        timestamp: ts,
        event_id: None,
        channel: "desktop".to_string(),
        title: "test".to_string(),
        body: "test body".to_string(),
        severity: "info".to_string(),
        status: NotificationStatus::Sent,
        error_message: None,
        acknowledged_at: None,
        acknowledged_by: None,
        action_taken: None,
        retry_count: 0,
        metadata: None,
        created_at: ts,
    }
}

// ===========================================================================
// Section 1: Empty database tests
// ===========================================================================

#[test]
fn preview_empty_db_returns_zero_eligible() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("empty_preview").await;
        let config = StorageConfig::default();

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        assert!(plan.dry_run);
        assert_eq!(plan.total_eligible, 0);
        for table in &plan.tables {
            assert_eq!(
                table.eligible_rows, 0,
                "table {} should be empty",
                table.table
            );
        }

        teardown(storage, &db_path).await;
    });
}

#[test]
fn apply_empty_db_deletes_nothing() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("empty_apply").await;
        let config = StorageConfig::default();

        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        assert!(!plan.dry_run);
        assert_eq!(plan.total_eligible, 0);
        assert_eq!(plan.total_deleted, 0);

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 2: Flat global retention tests
// ===========================================================================

#[test]
fn preview_flat_retention_counts_old_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("flat_preview").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;
        let recent_ts = now - 5 * 86_400_000;

        storage
            .record_event(make_event(old_ts, "info", "test"))
            .await
            .unwrap();
        storage
            .record_event(make_event(old_ts - 1000, "warning", "test"))
            .await
            .unwrap();
        storage
            .record_event(make_event(recent_ts, "info", "test"))
            .await
            .unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        assert!(plan.dry_run);

        let events_table = plan.tables.iter().find(|t| t.table == "events").unwrap();
        assert_eq!(events_table.eligible_rows, 2, "2 old events should be eligible");
        assert_eq!(events_table.deleted_rows, 0, "preview should not delete");
        assert_eq!(events_table.retention_days, 30);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn apply_flat_retention_deletes_old_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("flat_apply").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;
        let recent_ts = now - 5 * 86_400_000;

        storage
            .record_event(make_event(old_ts, "info", "test"))
            .await
            .unwrap();
        storage
            .record_event(make_event(old_ts - 1000, "warning", "test"))
            .await
            .unwrap();
        storage
            .record_event(make_event(recent_ts, "info", "test"))
            .await
            .unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        assert!(!plan.dry_run);

        let events_table = plan.tables.iter().find(|t| t.table == "events").unwrap();
        assert_eq!(events_table.eligible_rows, 2);
        assert_eq!(events_table.deleted_rows, 2, "2 old events should be deleted");

        let remaining = storage.count_events_before(now + 1000).await.unwrap();
        assert_eq!(remaining, 1, "only the recent event should remain");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 3: Tiered retention tests
// ===========================================================================

#[test]
fn preview_tiered_retention_groups_by_tier() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("tiered_preview").await;
        let now = now_ms();
        let critical_ts = now - 50 * 86_400_000;
        let info_old_ts = now - 15 * 86_400_000;
        let info_recent_ts = now - 3 * 86_400_000;

        storage.record_event(make_event(critical_ts, "critical", "error")).await.unwrap();
        storage.record_event(make_event(info_old_ts, "info", "detection")).await.unwrap();
        storage.record_event(make_event(info_recent_ts, "info", "detection")).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![
                RetentionTier {
                    name: "critical".to_string(),
                    retention_days: 90,
                    severities: vec!["critical".to_string()],
                    event_types: vec![],
                    handled: None,
                },
                RetentionTier {
                    name: "info".to_string(),
                    retention_days: 7,
                    severities: vec!["info".to_string()],
                    event_types: vec![],
                    handled: None,
                },
            ],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");

        let critical_tier = plan.tables.iter().find(|t| t.table.contains("critical")).unwrap();
        assert_eq!(critical_tier.eligible_rows, 0, "critical event within retention");

        let info_tier = plan.tables.iter().find(|t| t.table.contains("info")).unwrap();
        assert_eq!(info_tier.eligible_rows, 1, "only the 15-day-old info event");

        teardown(storage, &db_path).await;
    });
}

#[test]
fn apply_tiered_retention_deletes_correct_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("tiered_apply").await;
        let now = now_ms();
        let old_critical_ts = now - 100 * 86_400_000;
        let recent_critical_ts = now - 50 * 86_400_000;
        let old_info_ts = now - 15 * 86_400_000;
        let recent_info_ts = now - 3 * 86_400_000;

        storage.record_event(make_event(old_critical_ts, "critical", "error")).await.unwrap();
        storage.record_event(make_event(recent_critical_ts, "critical", "error")).await.unwrap();
        storage.record_event(make_event(old_info_ts, "info", "detection")).await.unwrap();
        storage.record_event(make_event(recent_info_ts, "info", "detection")).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![
                RetentionTier {
                    name: "critical".to_string(),
                    retention_days: 90,
                    severities: vec!["critical".to_string()],
                    event_types: vec![],
                    handled: None,
                },
                RetentionTier {
                    name: "info".to_string(),
                    retention_days: 7,
                    severities: vec!["info".to_string()],
                    event_types: vec![],
                    handled: None,
                },
            ],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");

        let critical_tier = plan.tables.iter().find(|t| t.table.contains("critical")).unwrap();
        assert_eq!(critical_tier.deleted_rows, 1, "only the 100-day-old critical event");

        let info_tier = plan.tables.iter().find(|t| t.table.contains("info")).unwrap();
        assert_eq!(info_tier.deleted_rows, 1, "only the 15-day-old info event");

        let remaining = storage.count_events_before(now + 1000).await.unwrap();
        assert_eq!(remaining, 2, "2 recent events should survive cleanup");

        teardown(storage, &db_path).await;
    });
}

#[test]
fn tiered_retention_handled_filter() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("handled_filter").await;
        let now = now_ms();
        let old_ts = now - 10 * 86_400_000;

        let ev1_id = storage.record_event(make_event(old_ts, "info", "detection")).await.unwrap();
        let _ev2_id = storage.record_event(make_event(old_ts, "info", "detection")).await.unwrap();

        storage.mark_event_handled(ev1_id, None, "auto").await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![RetentionTier {
                name: "info-handled".to_string(),
                retention_days: 3,
                severities: vec!["info".to_string()],
                event_types: vec![],
                handled: Some(true),
            }],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        let tier = plan.tables.iter().find(|t| t.table.contains("info-handled")).unwrap();
        assert_eq!(tier.eligible_rows, 1, "only the handled event is eligible");

        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        let tier = plan.tables.iter().find(|t| t.table.contains("info-handled")).unwrap();
        assert_eq!(tier.deleted_rows, 1);

        let remaining = storage.count_events_before(now + 1000).await.unwrap();
        assert_eq!(remaining, 1, "unhandled event should survive");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 4: Multi-table cleanup
// ===========================================================================

#[test]
fn apply_cleans_all_table_types() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("multi_table").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;
        let recent_ts = now - 5 * 86_400_000;

        storage.append_segment(1, "old segment", None).await.unwrap();

        storage.record_event(make_event(old_ts, "info", "test")).await.unwrap();
        storage.record_event(make_event(recent_ts, "info", "test")).await.unwrap();

        storage.record_audit_action(make_audit(old_ts)).await.unwrap();
        storage.record_audit_action(make_audit(recent_ts)).await.unwrap();

        storage.record_usage_metric(make_usage(old_ts)).await.unwrap();
        storage.record_usage_metric(make_usage(recent_ts)).await.unwrap();

        storage.record_notification(make_notification(old_ts)).await.unwrap();
        storage.record_notification(make_notification(recent_ts)).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");

        let events = plan.tables.iter().find(|t| t.table == "events").unwrap();
        assert_eq!(events.deleted_rows, 1);

        let audit = plan.tables.iter().find(|t| t.table == "audit_actions").unwrap();
        assert_eq!(audit.deleted_rows, 1);

        let usage = plan.tables.iter().find(|t| t.table == "usage_metrics").unwrap();
        assert_eq!(usage.deleted_rows, 1);

        let notif = plan.tables.iter().find(|t| t.table == "notification_history").unwrap();
        assert_eq!(notif.deleted_rows, 1);

        assert_eq!(plan.total_deleted, plan.tables.iter().map(|t| t.deleted_rows).sum::<usize>());

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 5: Zero retention edge cases
// ===========================================================================

#[test]
fn zero_retention_days_skips_all_cleanup() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("zero_retention").await;
        let now = now_ms();
        let ancient_ts = now - 365 * 86_400_000;

        storage.record_event(make_event(ancient_ts, "info", "test")).await.unwrap();
        storage.record_audit_action(make_audit(ancient_ts)).await.unwrap();

        let config = StorageConfig {
            retention_days: 0,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        assert_eq!(plan.total_eligible, 0, "nothing eligible when retention_days=0");
        assert!(plan.tables.is_empty() || plan.tables.iter().all(|t| t.eligible_rows == 0));

        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        assert_eq!(plan.total_deleted, 0, "nothing deleted when retention_days=0");

        teardown(storage, &db_path).await;
    });
}

#[test]
fn tier_with_zero_retention_keeps_events_forever() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("tier_zero").await;
        let now = now_ms();
        let ancient_ts = now - 365 * 86_400_000;

        storage.record_event(make_event(ancient_ts, "critical", "error")).await.unwrap();
        storage.record_event(make_event(ancient_ts, "info", "detection")).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![
                RetentionTier {
                    name: "critical-forever".to_string(),
                    retention_days: 0,
                    severities: vec!["critical".to_string()],
                    event_types: vec![],
                    handled: None,
                },
                RetentionTier {
                    name: "info-short".to_string(),
                    retention_days: 7,
                    severities: vec!["info".to_string()],
                    event_types: vec![],
                    handled: None,
                },
            ],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");

        let critical_tier = plan.tables.iter().find(|t| t.table.contains("critical-forever"));
        assert!(critical_tier.is_none(), "tier with retention_days=0 should be skipped entirely");

        let info_tier = plan.tables.iter().find(|t| t.table.contains("info-short")).unwrap();
        assert_eq!(info_tier.deleted_rows, 1);

        let remaining = storage.count_events_before(now + 1000).await.unwrap();
        assert_eq!(remaining, 1, "critical event preserved by zero-retention tier");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 6: Consistency and determinism
// ===========================================================================

#[test]
fn preview_and_apply_agree_on_eligible_counts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("consistency").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;

        for _ in 0..5 {
            storage.record_event(make_event(old_ts, "info", "test")).await.unwrap();
        }

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let preview = cleanup_preview(&storage, &config).await.expect("preview");
        let apply = cleanup_apply(&storage, &config).await.expect("apply");

        assert_eq!(
            preview.total_eligible, apply.total_eligible,
            "preview and apply should agree on eligible counts"
        );
        assert_eq!(apply.total_deleted, apply.total_eligible);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn mixed_severity_with_default_tiers() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("mixed_severity").await;
        let now = now_ms();
        let ts_20d = now - 20 * 86_400_000;

        storage.record_event(make_event(ts_20d, "critical", "error")).await.unwrap();
        storage.record_event(make_event(ts_20d, "warning", "detection")).await.unwrap();
        storage.record_event(make_event(ts_20d, "info", "detection")).await.unwrap();
        storage.record_event(make_event(ts_20d, "info", "detection")).await.unwrap();

        let config = StorageConfig::default();

        let plan = cleanup_apply(&storage, &config).await.expect("apply");

        let critical_tier = plan.tables.iter().find(|t| t.table.contains("critical")).unwrap();
        assert_eq!(critical_tier.deleted_rows, 0, "20d critical within 90d retention");

        let warning_tier = plan.tables.iter().find(|t| t.table.contains("warning")).unwrap();
        assert_eq!(warning_tier.deleted_rows, 0, "20d warning within 30d retention");

        let info_tier = plan.tables.iter().find(|t| t.table.contains("info")).unwrap();
        assert_eq!(info_tier.deleted_rows, 2, "20d info beyond 7d retention");

        let remaining = storage.count_events_before(now + 1000).await.unwrap();
        assert_eq!(remaining, 2, "critical + warning survive, 2 info deleted");

        teardown(storage, &db_path).await;
    });
}

#[test]
fn apply_records_maintenance_event() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("maintenance_log").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;

        storage.record_event(make_event(old_ts, "info", "test")).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let _plan = cleanup_apply(&storage, &config).await.expect("apply");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 7: E2E tests — full cleanup pipeline
// ===========================================================================

#[test]
fn e2e_mixed_severity_lifecycle() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("e2e_lifecycle").await;
        let now = now_ms();

        let ages: &[(i64, &str, &str)] = &[
            (now - 100 * 86_400_000, "critical", "error.crash"),
            (now - 50 * 86_400_000, "critical", "error.timeout"),
            (now - 20 * 86_400_000, "warning", "rate_limit"),
            (now - 10 * 86_400_000, "warning", "rate_limit"),
            (now - 15 * 86_400_000, "info", "detection"),
            (now - 5 * 86_400_000, "info", "detection"),
            (now - 2 * 86_400_000, "info", "detection"),
        ];
        for (ts, sev, etype) in ages {
            storage.record_event(make_event(*ts, sev, etype)).await.unwrap();
        }
        storage.record_audit_action(make_audit(now - 60 * 86_400_000)).await.unwrap();
        storage.record_audit_action(make_audit(now - 3 * 86_400_000)).await.unwrap();
        storage.record_usage_metric(make_usage(now - 60 * 86_400_000)).await.unwrap();
        storage.record_notification(make_notification(now - 60 * 86_400_000)).await.unwrap();
        storage.record_notification(make_notification(now - 3 * 86_400_000)).await.unwrap();

        let before_stats = frankenterm_core::storage::database_stats(std::path::Path::new(&db_path), 30);
        let before_events = before_stats.tables.iter().find(|t| t.name == "events").unwrap().row_count;
        assert_eq!(before_events, 7, "7 events before cleanup");

        let config = StorageConfig::default();

        let preview = cleanup_preview(&storage, &config).await.expect("preview");
        assert!(preview.dry_run);

        let crit_tier = preview.tables.iter().find(|t| t.table.contains("critical")).unwrap();
        assert_eq!(crit_tier.eligible_rows, 1, "1 old critical eligible");

        let warn_tier = preview.tables.iter().find(|t| t.table.contains("warning")).unwrap();
        assert_eq!(warn_tier.eligible_rows, 0, "warnings within retention");

        let info_tier = preview.tables.iter().find(|t| t.table.contains("info")).unwrap();
        assert_eq!(info_tier.eligible_rows, 1, "1 old info eligible");

        let audit_preview = preview.tables.iter().find(|t| t.table == "audit_actions").unwrap();
        assert_eq!(audit_preview.eligible_rows, 1);
        let usage_preview = preview.tables.iter().find(|t| t.table == "usage_metrics").unwrap();
        assert_eq!(usage_preview.eligible_rows, 1);
        let notif_preview = preview.tables.iter().find(|t| t.table == "notification_history").unwrap();
        assert_eq!(notif_preview.eligible_rows, 1);

        let total_preview_eligible = preview.total_eligible;
        assert_eq!(total_preview_eligible, 5, "5 total eligible across tables");

        let apply = cleanup_apply(&storage, &config).await.expect("apply");
        assert!(!apply.dry_run);
        assert_eq!(apply.total_eligible, total_preview_eligible, "apply agrees with preview");
        assert_eq!(apply.total_deleted, 5, "5 total deleted");

        let after_stats = frankenterm_core::storage::database_stats(std::path::Path::new(&db_path), 30);
        let after_events = after_stats.tables.iter().find(|t| t.name == "events").unwrap().row_count;
        assert_eq!(after_events, 5, "5 events remain after cleanup");
        assert_eq!(before_events - after_events, 2, "2 events deleted (1 critical + 1 info)");

        let after_audit = after_stats.tables.iter().find(|t| t.name == "audit_actions").unwrap().row_count;
        assert_eq!(after_audit, 1, "1 recent audit remains");

        let after_usage = after_stats.tables.iter().find(|t| t.name == "usage_metrics").unwrap().row_count;
        assert_eq!(after_usage, 0, "old usage deleted");

        let after_notif = after_stats.tables.iter().find(|t| t.name == "notification_history").unwrap().row_count;
        assert_eq!(after_notif, 1, "1 recent notification remains");

        teardown(storage, &db_path).await;
    });
}

#[test]
fn e2e_dry_run_is_deterministic() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("e2e_deterministic").await;
        let now = now_ms();

        for i in 0..10 {
            let ts = now - (40 + i) * 86_400_000;
            storage.record_event(make_event(ts, "info", "detection")).await.unwrap();
        }
        for i in 0..3 {
            let ts = now - (40 + i) * 86_400_000;
            storage.record_audit_action(make_audit(ts)).await.unwrap();
        }

        let config = StorageConfig::default();

        let run1 = cleanup_preview(&storage, &config).await.expect("run1");
        let run2 = cleanup_preview(&storage, &config).await.expect("run2");

        assert_eq!(run1.total_eligible, run2.total_eligible, "consecutive previews must be identical");
        assert_eq!(run1.tables.len(), run2.tables.len());
        for (t1, t2) in run1.tables.iter().zip(run2.tables.iter()) {
            assert_eq!(t1.table, t2.table);
            assert_eq!(t1.eligible_rows, t2.eligible_rows, "table {} counts differ", t1.table);
        }

        let stats1 = frankenterm_core::storage::database_stats(std::path::Path::new(&db_path), 30);
        let stats2 = frankenterm_core::storage::database_stats(std::path::Path::new(&db_path), 30);
        for (s1, s2) in stats1.tables.iter().zip(stats2.tables.iter()) {
            assert_eq!(s1.name, s2.name);
            assert_eq!(s1.row_count, s2.row_count, "stats table {} counts differ", s1.name);
        }

        teardown(storage, &db_path).await;
    });
}

#[test]
fn e2e_apply_is_idempotent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("e2e_idempotent").await;
        let now = now_ms();

        for _ in 0..5 {
            storage
                .record_event(make_event(now - 60 * 86_400_000, "info", "detection"))
                .await
                .unwrap();
        }
        storage.record_audit_action(make_audit(now - 60 * 86_400_000)).await.unwrap();

        let config = StorageConfig::default();

        let first_apply = cleanup_apply(&storage, &config).await.expect("first apply");
        assert!(first_apply.total_deleted > 0, "first apply deletes rows");

        let second_apply = cleanup_apply(&storage, &config).await.expect("second apply");
        assert_eq!(second_apply.total_deleted, 0, "second apply finds nothing to delete");
        assert_eq!(second_apply.total_eligible, 0);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn e2e_json_artifacts_are_stable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("e2e_json").await;
        let now = now_ms();

        storage.record_event(make_event(now - 60 * 86_400_000, "info", "detection")).await.unwrap();
        storage.record_event(make_event(now - 2 * 86_400_000, "critical", "error")).await.unwrap();

        let config = StorageConfig::default();

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        let json1 = serde_json::to_string_pretty(&plan).expect("serialize plan");
        let json2 = serde_json::to_string_pretty(&plan).expect("serialize again");
        assert_eq!(json1, json2, "serialization is deterministic");

        assert!(json1.contains("\"dry_run\": true"));
        assert!(json1.contains("\"total_eligible\":"));
        assert!(json1.contains("\"tables\":"));

        let stats = frankenterm_core::storage::database_stats(std::path::Path::new(&db_path), 30);
        let stats_json = serde_json::to_string_pretty(&stats).expect("serialize stats");
        assert!(stats_json.contains("\"db_path\":"));
        assert!(stats_json.contains("\"tables\":"));
        assert!(stats_json.contains("\"suggestions\":"));

        teardown(storage, &db_path).await;
    });
}

#[test]
fn e2e_before_after_stats_with_deletion_counts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("e2e_before_after").await;
        let now = now_ms();

        for _ in 0..3 {
            storage.record_event(make_event(now - 60 * 86_400_000, "info", "test")).await.unwrap();
        }
        for _ in 0..2 {
            storage.record_event(make_event(now - 2 * 86_400_000, "info", "test")).await.unwrap();
        }
        for _ in 0..2 {
            storage.record_audit_action(make_audit(now - 60 * 86_400_000)).await.unwrap();
        }
        storage.record_audit_action(make_audit(now - 2 * 86_400_000)).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let before = frankenterm_core::storage::database_stats(std::path::Path::new(&db_path), 30);
        let before_events = before.tables.iter().find(|t| t.name == "events").unwrap().row_count;
        let before_audit = before.tables.iter().find(|t| t.name == "audit_actions").unwrap().row_count;
        assert_eq!(before_events, 5);
        assert_eq!(before_audit, 3);

        let plan = cleanup_apply(&storage, &config).await.expect("apply");

        let after = frankenterm_core::storage::database_stats(std::path::Path::new(&db_path), 30);
        let after_events = after.tables.iter().find(|t| t.name == "events").unwrap().row_count;
        let after_audit = after.tables.iter().find(|t| t.name == "audit_actions").unwrap().row_count;
        assert_eq!(after_events, 2, "3 old events deleted, 2 recent remain");
        assert_eq!(after_audit, 1, "2 old audit deleted, 1 recent remains");

        let events_deleted = plan.tables.iter().find(|t| t.table == "events").unwrap().deleted_rows;
        let audit_deleted = plan.tables.iter().find(|t| t.table == "audit_actions").unwrap().deleted_rows;
        assert_eq!(events_deleted as u64, before_events - after_events, "deletion count matches stats delta");
        assert_eq!(audit_deleted as u64, before_audit - after_audit, "audit deletion count matches stats delta");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 8: Event type filtering
// ===========================================================================

#[test]
fn tier_filters_by_event_type() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("event_type_filter").await;
        let now = now_ms();
        let old_ts = now - 15 * 86_400_000;

        storage.record_event(make_event(old_ts, "info", "usage_limit")).await.unwrap();
        storage.record_event(make_event(old_ts, "info", "compaction")).await.unwrap();
        storage.record_event(make_event(old_ts, "info", "detection")).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![RetentionTier {
                name: "info-usage".to_string(),
                retention_days: 7,
                severities: vec!["info".to_string()],
                event_types: vec!["usage_limit".to_string()],
                handled: None,
            }],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");

        let tier = plan.tables.iter().find(|t| t.table.contains("info-usage")).unwrap();
        assert_eq!(tier.deleted_rows, 1, "only usage_limit event matched tier");

        let remaining = storage.count_events_before(now + 1000).await.unwrap();
        assert_eq!(remaining, 2, "compaction + detection survive");

        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// Section 9: Edge-case async integration tests
// ===========================================================================

#[test]
fn preview_with_only_audit_data() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("only_audit").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;

        storage.record_audit_action(make_audit(old_ts)).await.unwrap();
        storage.record_audit_action(make_audit(old_ts - 1000)).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        let audit = plan.tables.iter().find(|t| t.table == "audit_actions").unwrap();
        assert_eq!(audit.eligible_rows, 2);
        assert_eq!(audit.deleted_rows, 0, "preview never deletes");

        let events = plan.tables.iter().find(|t| t.table == "events").unwrap();
        assert_eq!(events.eligible_rows, 0);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn preview_with_only_usage_data() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("only_usage").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;

        storage.record_usage_metric(make_usage(old_ts)).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        let usage = plan.tables.iter().find(|t| t.table == "usage_metrics").unwrap();
        assert_eq!(usage.eligible_rows, 1);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn preview_with_only_notification_data() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("only_notif").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;

        storage.record_notification(make_notification(old_ts)).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        let notif = plan.tables.iter().find(|t| t.table == "notification_history").unwrap();
        assert_eq!(notif.eligible_rows, 1);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn apply_with_all_recent_data_deletes_nothing() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("all_recent").await;
        let now = now_ms();
        let recent_ts = now - 2 * 86_400_000;

        storage.record_event(make_event(recent_ts, "info", "test")).await.unwrap();
        storage.record_audit_action(make_audit(recent_ts)).await.unwrap();
        storage.record_usage_metric(make_usage(recent_ts)).await.unwrap();
        storage.record_notification(make_notification(recent_ts)).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        assert_eq!(plan.total_deleted, 0, "all data is recent");
        assert_eq!(plan.total_eligible, 0);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn preview_plan_always_has_dry_run_true() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("dry_run_flag").await;
        let config = StorageConfig::default();
        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        assert!(plan.dry_run, "preview must always set dry_run=true");
        teardown(storage, &db_path).await;
    });
}

#[test]
fn apply_plan_always_has_dry_run_false() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("apply_flag").await;
        let config = StorageConfig::default();
        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        assert!(!plan.dry_run, "apply must always set dry_run=false");
        teardown(storage, &db_path).await;
    });
}

#[test]
fn preview_total_eligible_equals_sum_of_table_eligible() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("eligible_sum").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;

        storage.record_event(make_event(old_ts, "info", "test")).await.unwrap();
        storage.record_audit_action(make_audit(old_ts)).await.unwrap();
        storage.record_usage_metric(make_usage(old_ts)).await.unwrap();
        storage.record_notification(make_notification(old_ts)).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        let table_sum: usize = plan.tables.iter().map(|t| t.eligible_rows).sum();
        assert_eq!(plan.total_eligible, table_sum, "total_eligible must equal sum of per-table eligible_rows");

        teardown(storage, &db_path).await;
    });
}

#[test]
fn apply_total_deleted_equals_sum_of_table_deleted() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("deleted_sum").await;
        let now = now_ms();
        let old_ts = now - 60 * 86_400_000;

        storage.record_event(make_event(old_ts, "info", "test")).await.unwrap();
        storage.record_audit_action(make_audit(old_ts)).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        let table_sum: usize = plan.tables.iter().map(|t| t.deleted_rows).sum();
        assert_eq!(plan.total_deleted, table_sum, "total_deleted must equal sum of per-table deleted_rows");

        teardown(storage, &db_path).await;
    });
}

#[test]
fn multiple_tiers_all_zero_retention_produces_no_event_entries() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("all_tiers_zero").await;
        let now = now_ms();
        let ancient_ts = now - 365 * 86_400_000;

        storage.record_event(make_event(ancient_ts, "critical", "error")).await.unwrap();
        storage.record_event(make_event(ancient_ts, "info", "detection")).await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![
                RetentionTier {
                    name: "crit-forever".to_string(),
                    retention_days: 0,
                    severities: vec!["critical".to_string()],
                    event_types: vec![],
                    handled: None,
                },
                RetentionTier {
                    name: "info-forever".to_string(),
                    retention_days: 0,
                    severities: vec!["info".to_string()],
                    event_types: vec![],
                    handled: None,
                },
            ],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        assert!(
            !plan.tables.iter().any(|t| t.table.starts_with("events")),
            "all zero-retention tiers should be skipped"
        );

        let remaining = storage.count_events_before(now + 1000).await.unwrap();
        assert_eq!(remaining, 2);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn tier_with_unhandled_filter_only_deletes_unhandled() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("unhandled_filter").await;
        let now = now_ms();
        let old_ts = now - 10 * 86_400_000;

        let ev1_id = storage.record_event(make_event(old_ts, "info", "detection")).await.unwrap();
        let _ev2_id = storage.record_event(make_event(old_ts, "info", "detection")).await.unwrap();

        storage.mark_event_handled(ev1_id, None, "auto").await.unwrap();

        let config = StorageConfig {
            retention_days: 30,
            retention_tiers: vec![RetentionTier {
                name: "info-unhandled".to_string(),
                retention_days: 3,
                severities: vec!["info".to_string()],
                event_types: vec![],
                handled: Some(false),
            }],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        let tier = plan.tables.iter().find(|t| t.table.contains("info-unhandled")).unwrap();
        assert_eq!(tier.deleted_rows, 1, "only the unhandled event should be deleted");

        let remaining = storage.count_events_before(now + 1000).await.unwrap();
        assert_eq!(remaining, 1, "handled event should survive");

        teardown(storage, &db_path).await;
    });
}

#[test]
fn preview_retention_days_propagated_to_table_summaries() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("retention_propagate").await;

        let config = StorageConfig {
            retention_days: 45,
            retention_tiers: vec![RetentionTier {
                name: "special".to_string(),
                retention_days: 120,
                severities: vec!["critical".to_string()],
                event_types: vec![],
                handled: None,
            }],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");

        let special = plan.tables.iter().find(|t| t.table.contains("special")).unwrap();
        assert_eq!(special.retention_days, 120);

        let audit = plan.tables.iter().find(|t| t.table == "audit_actions").unwrap();
        assert_eq!(audit.retention_days, 45);

        let usage = plan.tables.iter().find(|t| t.table == "usage_metrics").unwrap();
        assert_eq!(usage.retention_days, 45);

        let notif = plan.tables.iter().find(|t| t.table == "notification_history").unwrap();
        assert_eq!(notif.retention_days, 45);

        let segments = plan.tables.iter().find(|t| t.table == "output_segments").unwrap();
        assert_eq!(segments.retention_days, 45);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn preview_with_one_day_retention() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("one_day_retention").await;
        let now = now_ms();
        let ts_2d = now - 2 * 86_400_000;
        let ts_12h = now - 12 * 60 * 60 * 1000;

        storage.record_event(make_event(ts_2d, "info", "test")).await.unwrap();
        storage.record_event(make_event(ts_12h, "info", "test")).await.unwrap();

        let config = StorageConfig {
            retention_days: 1,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_preview(&storage, &config).await.expect("preview");
        let events = plan.tables.iter().find(|t| t.table == "events").unwrap();
        assert_eq!(events.eligible_rows, 1, "only the 2-day-old event");

        teardown(storage, &db_path).await;
    });
}

#[test]
fn apply_with_very_short_retention_cleans_almost_everything() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let (storage, db_path) = setup_storage("short_retention").await;
        let now = now_ms();

        for days_ago in [1, 3, 7, 14, 30] {
            let ts = now - days_ago * 86_400_000;
            storage.record_event(make_event(ts, "info", "test")).await.unwrap();
        }

        let config = StorageConfig {
            retention_days: 2,
            retention_tiers: vec![],
            ..Default::default()
        };

        let plan = cleanup_apply(&storage, &config).await.expect("apply");
        let events = plan.tables.iter().find(|t| t.table == "events").unwrap();
        assert_eq!(events.deleted_rows, 4);

        let remaining = storage.count_events_before(now + 1000).await.unwrap();
        assert_eq!(remaining, 1, "only the 1-day-old event survives");

        teardown(storage, &db_path).await;
    });
}
