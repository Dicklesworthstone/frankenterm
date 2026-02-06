// Tests for notification_history storage (wa-5ap)
use tempfile::TempDir;
use wa_core::storage::{
    NotificationHistoryQuery, NotificationHistoryRecord, NotificationStatus, StorageHandle,
};

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("test.db").to_string_lossy().to_string();
    (dir, path)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

fn make_record(
    channel: &str,
    title: &str,
    severity: &str,
    status: NotificationStatus,
) -> NotificationHistoryRecord {
    NotificationHistoryRecord {
        id: 0,
        timestamp: 1_700_000_000_000,
        event_id: None,
        channel: channel.to_string(),
        title: title.to_string(),
        body: format!("Body for {title}"),
        severity: severity.to_string(),
        status,
        error_message: None,
        acknowledged_at: None,
        acknowledged_by: None,
        action_taken: None,
        retry_count: 0,
        metadata: None,
        created_at: 1_700_000_000_000,
    }
}

// ---- NotificationStatus roundtrip ----

#[test]
fn notification_status_roundtrip() {
    for status in [
        NotificationStatus::Pending,
        NotificationStatus::Sent,
        NotificationStatus::Failed,
        NotificationStatus::Throttled,
    ] {
        let s = status.as_str();
        let parsed: NotificationStatus = s.parse().unwrap();
        assert_eq!(parsed, status);
        assert_eq!(status.to_string(), s);
    }
}

#[test]
fn notification_status_parse_unknown_returns_error() {
    let result: Result<NotificationStatus, _> = "invalid_status".parse();
    assert!(result.is_err());
}

#[test]
fn notification_status_serde_roundtrip() {
    for status in [
        NotificationStatus::Pending,
        NotificationStatus::Sent,
        NotificationStatus::Failed,
        NotificationStatus::Throttled,
    ] {
        let json = serde_json::to_string(&status).unwrap();
        let parsed: NotificationStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);
    }
}

// ---- Record + Query ----

#[test]
fn record_and_query_single_notification() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let record = make_record("webhook", "Test alert", "warning", NotificationStatus::Sent);
        let id = storage.record_notification(record).await.unwrap();
        assert!(id > 0);

        let results = storage
            .query_notification_history(NotificationHistoryQuery::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id);
        assert_eq!(results[0].channel, "webhook");
        assert_eq!(results[0].title, "Test alert");
        assert_eq!(results[0].severity, "warning");
        assert_eq!(results[0].status, NotificationStatus::Sent);
    });
}

#[test]
fn get_notification_by_id() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let record = make_record(
            "desktop",
            "Desktop alert",
            "info",
            NotificationStatus::Pending,
        );
        let id = storage.record_notification(record).await.unwrap();

        let fetched = storage.get_notification(id).await.unwrap();
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.channel, "desktop");
        assert_eq!(fetched.title, "Desktop alert");
    });
}

#[test]
fn get_notification_not_found() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let result = storage.get_notification(99999).await;
        assert!(result.is_err());
    });
}

// ---- Update status ----

#[test]
fn update_notification_status() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let record = make_record("webhook", "Alert", "error", NotificationStatus::Pending);
        let id = storage.record_notification(record).await.unwrap();

        storage
            .update_notification_status(id, NotificationStatus::Sent, None)
            .await
            .unwrap();

        let fetched = storage.get_notification(id).await.unwrap();
        assert_eq!(fetched.status, NotificationStatus::Sent);
        assert!(fetched.error_message.is_none());
    });
}

#[test]
fn update_notification_status_with_error() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let record = make_record("webhook", "Alert", "error", NotificationStatus::Pending);
        let id = storage.record_notification(record).await.unwrap();

        storage
            .update_notification_status(
                id,
                NotificationStatus::Failed,
                Some("Connection refused".to_string()),
            )
            .await
            .unwrap();

        let fetched = storage.get_notification(id).await.unwrap();
        assert_eq!(fetched.status, NotificationStatus::Failed);
        assert_eq!(fetched.error_message.as_deref(), Some("Connection refused"));
    });
}

#[test]
fn update_nonexistent_notification_fails() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let result = storage
            .update_notification_status(99999, NotificationStatus::Sent, None)
            .await;
        assert!(result.is_err());
    });
}

// ---- Acknowledge ----

#[test]
fn acknowledge_notification() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let record = make_record("slack", "Slack alert", "info", NotificationStatus::Sent);
        let id = storage.record_notification(record).await.unwrap();

        storage
            .acknowledge_notification(id, "admin".to_string(), Some("Resolved".to_string()))
            .await
            .unwrap();

        let fetched = storage.get_notification(id).await.unwrap();
        assert!(fetched.acknowledged_at.is_some());
        assert_eq!(fetched.acknowledged_by.as_deref(), Some("admin"));
        assert_eq!(fetched.action_taken.as_deref(), Some("Resolved"));
    });
}

#[test]
fn acknowledge_nonexistent_notification_fails() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let result = storage
            .acknowledge_notification(99999, "admin".to_string(), None)
            .await;
        assert!(result.is_err());
    });
}

// ---- Retry ----

#[test]
fn increment_retry_count() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let record = make_record("webhook", "Alert", "error", NotificationStatus::Failed);
        let id = storage.record_notification(record).await.unwrap();

        storage.increment_notification_retry(id).await.unwrap();
        let fetched = storage.get_notification(id).await.unwrap();
        assert_eq!(fetched.retry_count, 1);
        assert_eq!(fetched.status, NotificationStatus::Pending);

        storage.increment_notification_retry(id).await.unwrap();
        let fetched = storage.get_notification(id).await.unwrap();
        assert_eq!(fetched.retry_count, 2);
    });
}

#[test]
fn increment_retry_nonexistent_fails() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let result = storage.increment_notification_retry(99999).await;
        assert!(result.is_err());
    });
}

// ---- Purge ----

#[test]
fn purge_old_notifications() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let mut old = make_record("webhook", "Old alert", "info", NotificationStatus::Sent);
        old.timestamp = 1_000_000_000;
        old.created_at = 1_000_000_000;
        storage.record_notification(old).await.unwrap();

        let recent = make_record("webhook", "Recent alert", "info", NotificationStatus::Sent);
        storage.record_notification(recent).await.unwrap();

        let purged = storage
            .purge_notification_history(1_500_000_000_000)
            .await
            .unwrap();
        assert_eq!(purged, 1);

        let results = storage
            .query_notification_history(NotificationHistoryQuery::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Recent alert");
    });
}

#[test]
fn purge_with_no_matching_records() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let record = make_record("webhook", "Alert", "info", NotificationStatus::Sent);
        storage.record_notification(record).await.unwrap();

        let purged = storage.purge_notification_history(100).await.unwrap();
        assert_eq!(purged, 0);
    });
}

// ---- Query filters ----

#[test]
fn query_by_channel() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        storage
            .record_notification(make_record(
                "webhook",
                "WH1",
                "info",
                NotificationStatus::Sent,
            ))
            .await
            .unwrap();
        storage
            .record_notification(make_record(
                "desktop",
                "D1",
                "info",
                NotificationStatus::Sent,
            ))
            .await
            .unwrap();
        storage
            .record_notification(make_record(
                "webhook",
                "WH2",
                "warning",
                NotificationStatus::Sent,
            ))
            .await
            .unwrap();

        let results = storage
            .query_notification_history(NotificationHistoryQuery {
                channel: Some("webhook".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.channel == "webhook"));
    });
}

#[test]
fn query_by_status() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        storage
            .record_notification(make_record(
                "webhook",
                "Sent1",
                "info",
                NotificationStatus::Sent,
            ))
            .await
            .unwrap();
        storage
            .record_notification(make_record(
                "webhook",
                "Failed1",
                "error",
                NotificationStatus::Failed,
            ))
            .await
            .unwrap();

        let results = storage
            .query_notification_history(NotificationHistoryQuery {
                status: Some(NotificationStatus::Failed),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Failed1");
    });
}

#[test]
fn query_by_event_id() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let mut r1 = make_record("webhook", "Ev42", "info", NotificationStatus::Sent);
        r1.event_id = Some(42);
        storage.record_notification(r1).await.unwrap();

        let mut r2 = make_record("webhook", "Ev99", "info", NotificationStatus::Sent);
        r2.event_id = Some(99);
        storage.record_notification(r2).await.unwrap();

        let results = storage
            .query_notification_history(NotificationHistoryQuery {
                event_id: Some(42),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Ev42");
        assert_eq!(results[0].event_id, Some(42));
    });
}

#[test]
fn query_with_time_range() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let mut r1 = make_record("webhook", "Early", "info", NotificationStatus::Sent);
        r1.timestamp = 1_000_000;
        r1.created_at = 1_000_000;
        storage.record_notification(r1).await.unwrap();

        let mut r2 = make_record("webhook", "Middle", "info", NotificationStatus::Sent);
        r2.timestamp = 2_000_000;
        r2.created_at = 2_000_000;
        storage.record_notification(r2).await.unwrap();

        let mut r3 = make_record("webhook", "Late", "info", NotificationStatus::Sent);
        r3.timestamp = 3_000_000;
        r3.created_at = 3_000_000;
        storage.record_notification(r3).await.unwrap();

        let results = storage
            .query_notification_history(NotificationHistoryQuery {
                since: Some(1_500_000),
                until: Some(2_500_000),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Middle");
    });
}

#[test]
fn query_with_limit() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        for i in 0..5 {
            let mut r = make_record(
                "webhook",
                &format!("N{i}"),
                "info",
                NotificationStatus::Sent,
            );
            r.timestamp = 1_700_000_000_000 + i;
            r.created_at = 1_700_000_000_000 + i;
            storage.record_notification(r).await.unwrap();
        }

        let results = storage
            .query_notification_history(NotificationHistoryQuery {
                limit: Some(3),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 3);
        // Results are ordered DESC by timestamp
        assert_eq!(results[0].title, "N4");
        assert_eq!(results[2].title, "N2");
    });
}

#[test]
fn empty_query_returns_empty() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        let results = storage
            .query_notification_history(NotificationHistoryQuery::default())
            .await
            .unwrap();
        assert!(results.is_empty());
    });
}

// ---- Full record with all fields ----

#[test]
fn record_with_all_fields() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        let record = NotificationHistoryRecord {
            id: 0,
            timestamp: 1_700_000_000_000,
            event_id: Some(42),
            channel: "webhook".to_string(),
            title: "Full record".to_string(),
            body: "Full body".to_string(),
            severity: "critical".to_string(),
            status: NotificationStatus::Sent,
            error_message: None,
            acknowledged_at: None,
            acknowledged_by: None,
            action_taken: None,
            retry_count: 0,
            metadata: Some(r#"{"endpoint":"https://example.com/hook"}"#.to_string()),
            created_at: 1_700_000_000_000,
        };

        let id = storage.record_notification(record).await.unwrap();
        let fetched = storage.get_notification(id).await.unwrap();

        assert_eq!(fetched.event_id, Some(42));
        assert_eq!(fetched.severity, "critical");
        assert_eq!(
            fetched.metadata.as_deref(),
            Some(r#"{"endpoint":"https://example.com/hook"}"#)
        );
    });
}

// ---- Combined filter query ----

#[test]
fn query_combined_filters() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");

        // webhook + sent
        storage
            .record_notification(make_record(
                "webhook",
                "WS1",
                "info",
                NotificationStatus::Sent,
            ))
            .await
            .unwrap();
        // webhook + failed
        storage
            .record_notification(make_record(
                "webhook",
                "WF1",
                "error",
                NotificationStatus::Failed,
            ))
            .await
            .unwrap();
        // desktop + sent
        storage
            .record_notification(make_record(
                "desktop",
                "DS1",
                "info",
                NotificationStatus::Sent,
            ))
            .await
            .unwrap();

        let results = storage
            .query_notification_history(NotificationHistoryQuery {
                channel: Some("webhook".to_string()),
                status: Some(NotificationStatus::Sent),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "WS1");
    });
}

// ---- Schema migration ----

#[test]
fn schema_migration_creates_notification_history_table() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        // Just verify we can use the table (schema was applied during open)
        let record = make_record(
            "webhook",
            "Migration test",
            "info",
            NotificationStatus::Sent,
        );
        let id = storage.record_notification(record).await.unwrap();
        assert!(id > 0);
    });
}
