//! LabRuntime-ported event bus tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` functions from `events.rs` to asupersync-based
//! `RuntimeFixture`, gaining seed-based reproducibility for EventBus
//! pub/sub operations.
//!
//! The EventBus uses `tokio::sync::broadcast` internally, which does not
//! require a tokio reactor — the sync primitives work purely through
//! waker notifications, making them compatible with asupersync RuntimeFixture.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::events::{Event, EventBus, RecvError};
use frankenterm_core::patterns::Detection;

// ===========================================================================
// Section 1: EventBus tests ported from tokio::test to RuntimeFixture
// ===========================================================================

#[test]
fn events_publish_with_no_subscribers_counts_drops() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let bus = EventBus::new(10);

        let count = bus.publish(Event::PaneDisappeared { pane_id: 1 });
        assert_eq!(count, 0);

        let metrics = bus.metrics().snapshot();
        assert_eq!(metrics.events_published, 1);
        assert_eq!(metrics.events_dropped_no_subscribers, 1);
    });
}

#[test]
fn events_subscriber_receives_published_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let bus = EventBus::new(10);
        let mut sub = bus.subscribe();

        let _ = bus.publish(Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "shell".to_string(),
        });

        let event = sub.recv().await.unwrap();
        assert!(matches!(event, Event::PaneDiscovered { pane_id: 1, .. }));
    });
}

#[test]
fn events_multiple_subscribers_fanout() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let bus = EventBus::new(10);
        let mut sub1 = bus.subscribe();
        let mut sub2 = bus.subscribe();

        assert_eq!(bus.subscriber_count(), 2);

        let _ = bus.publish(Event::PaneDisappeared { pane_id: 42 });

        let e1 = sub1.recv().await.unwrap();
        let e2 = sub2.recv().await.unwrap();

        assert!(matches!(e1, Event::PaneDisappeared { pane_id: 42 }));
        assert!(matches!(e2, Event::PaneDisappeared { pane_id: 42 }));
    });
}

#[test]
fn events_delta_subscriber_only_sees_delta_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let bus = EventBus::new(10);
        let mut delta_sub = bus.subscribe_deltas();

        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 5,
            seq: 1,
            content_len: 10,
        });

        let event = delta_sub.recv().await.unwrap();
        assert!(matches!(event, Event::SegmentCaptured { pane_id: 5, .. }));

        let _ = bus.publish(Event::PaneDiscovered {
            pane_id: 5,
            domain: "local".to_string(),
            title: "shell".to_string(),
        });

        assert!(delta_sub.try_recv().is_none());
    });
}

#[test]
fn events_detection_subscriber_receives_pattern_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let bus = EventBus::new(10);
        let mut detection_sub = bus.subscribe_detections();

        let detection = Detection {
            rule_id: "codex.test".to_string(),
            agent_type: frankenterm_core::patterns::AgentType::Codex,
            event_type: "test".to_string(),
            severity: frankenterm_core::patterns::Severity::Info,
            confidence: 0.9,
            extracted: serde_json::json!({}),
            matched_text: "anchor".to_string(),
            span: (0, 0),
        };

        let _ = bus.publish(Event::PatternDetected {
            pane_id: 1,
            pane_uuid: None,
            detection,
            event_id: None,
        });

        let event = detection_sub.recv().await.unwrap();
        assert!(matches!(event, Event::PatternDetected { pane_id: 1, .. }));
    });
}

#[test]
fn events_subscriber_drop_decrements_count() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let bus = EventBus::new(10);

        {
            let _sub1 = bus.subscribe();
            let _sub2 = bus.subscribe();
            assert_eq!(bus.subscriber_count(), 2);
        }

        assert_eq!(bus.subscriber_count(), 0);

        let metrics = bus.metrics().snapshot();
        assert_eq!(metrics.active_subscribers, 0);
    });
}

#[test]
fn events_try_recv_returns_none_when_empty() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let bus = EventBus::new(10);
        let mut sub = bus.subscribe();

        assert!(sub.try_recv().is_none());
    });
}

#[test]
fn events_try_recv_returns_event_when_available() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let bus = EventBus::new(10);
        let mut sub = bus.subscribe();

        let _ = bus.publish(Event::PaneDisappeared { pane_id: 1 });

        let result = sub.try_recv();
        assert!(result.is_some());
        assert!(matches!(
            result.unwrap().unwrap(),
            Event::PaneDisappeared { pane_id: 1 }
        ));
    });
}

#[test]
fn events_backpressure_causes_lag() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Small capacity to trigger lag
        let bus = EventBus::new(2);
        let mut sub = bus.subscribe();

        // Publish more events than capacity
        for i in 0..5 {
            let _ = bus.publish(Event::SegmentCaptured {
                pane_id: 1,
                seq: i,
                content_len: 10,
            });
        }

        // First recv should report lag
        let result = sub.recv().await;
        match result {
            Err(RecvError::Lagged { missed_count }) => {
                assert!(missed_count > 0);
            }
            Ok(_) => {
                // Might get an event if timing works out, that's ok too
            }
            Err(RecvError::Closed) => panic!("unexpected close"),
        }

        // Lag should be tracked in metrics
        let metrics = bus.metrics().snapshot();
        assert!(metrics.subscriber_lag_events > 0 || sub.lagged_count() > 0);
    });
}

#[test]
fn events_recv_error_display() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let err = RecvError::Closed;
        assert_eq!(format!("{err}"), "event bus closed");

        let err = RecvError::Lagged { missed_count: 42 };
        assert_eq!(format!("{err}"), "subscriber lagged, missed 42 events");
    });
}

#[test]
fn events_uptime_increases() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let bus = EventBus::new(10);
        let t1 = bus.uptime();
        frankenterm_core::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;
        let t2 = bus.uptime();
        assert!(t2 > t1);
    });
}

// ===========================================================================
// E2E storage integration tests ported from tokio::test to RuntimeFixture
//
// These tests exercise the StorageHandle (sqlite) + EventBus mute lifecycle.
// StorageHandle internally uses tokio::task::spawn_blocking for sqlite I/O,
// which requires a runtime that supports blocking tasks.
// ===========================================================================

#[test]
fn events_e2e_mute_lifecycle_with_storage() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use frankenterm_core::events::event_identity_key;
        use frankenterm_core::storage::{EventMuteRecord, StorageHandle};

        let db_path = std::env::temp_dir().join(format!("wa_labrt_mute_{}.db", std::process::id()));
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));

        let storage = StorageHandle::new(&db_str).await.expect("open test db");
        let now = frankenterm_core::storage::now_ms();

        let detection = Detection {
            rule_id: "codex.usage_reached".to_string(),
            agent_type: frankenterm_core::patterns::AgentType::Codex,
            event_type: "test".to_string(),
            severity: frankenterm_core::patterns::Severity::Warning,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "test".to_string(),
            span: (0, 4),
        };
        let identity_key = event_identity_key(&detection, 1, None);

        // Initially not muted
        assert!(
            !storage.is_event_muted(&identity_key, now).await.unwrap(),
            "should not be muted initially"
        );

        // Add mute with no expiry (permanent)
        storage
            .add_event_mute(EventMuteRecord {
                identity_key: identity_key.clone(),
                scope: "workspace".to_string(),
                created_at: now,
                expires_at: None,
                created_by: Some("test".to_string()),
                reason: Some("noisy test event".to_string()),
            })
            .await
            .unwrap();

        // Now muted
        assert!(
            storage.is_event_muted(&identity_key, now).await.unwrap(),
            "should be muted after add"
        );

        // Appears in active mutes list
        let mutes = storage.list_active_mutes(now).await.unwrap();
        assert!(
            mutes.iter().any(|m| m.identity_key == identity_key),
            "mute should appear in active list"
        );

        // Remove mute
        storage.remove_event_mute(&identity_key).await.unwrap();

        // No longer muted
        assert!(
            !storage.is_event_muted(&identity_key, now).await.unwrap(),
            "should not be muted after remove"
        );

        // Clean up
        storage.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));
    });
}

#[test]
fn events_e2e_mute_expiry() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use frankenterm_core::storage::{EventMuteRecord, StorageHandle};

        let db_path =
            std::env::temp_dir().join(format!("wa_labrt_mute_expiry_{}.db", std::process::id()));
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));

        let storage = StorageHandle::new(&db_str).await.expect("open test db");
        let now = frankenterm_core::storage::now_ms();

        let identity_key = "evt:test_expiry_key".to_string();

        // Add mute that already expired (expires_at in the past)
        storage
            .add_event_mute(EventMuteRecord {
                identity_key: identity_key.clone(),
                scope: "workspace".to_string(),
                created_at: now - 60_000,
                expires_at: Some(now - 1000),
                created_by: None,
                reason: None,
            })
            .await
            .unwrap();

        // Should not be active since it's expired
        assert!(
            !storage.is_event_muted(&identity_key, now).await.unwrap(),
            "expired mute should not be active"
        );

        // Should not appear in active mutes list
        let mutes = storage.list_active_mutes(now).await.unwrap();
        assert!(
            !mutes.iter().any(|m| m.identity_key == identity_key),
            "expired mute should not appear in active list"
        );

        // Add a mute that expires in the future
        let future_key = "evt:test_future_key".to_string();
        storage
            .add_event_mute(EventMuteRecord {
                identity_key: future_key.clone(),
                scope: "workspace".to_string(),
                created_at: now,
                expires_at: Some(now + 3_600_000),
                created_by: None,
                reason: Some("temporary mute".to_string()),
            })
            .await
            .unwrap();

        // Should be active
        assert!(
            storage.is_event_muted(&future_key, now).await.unwrap(),
            "future mute should be active"
        );

        // Clean up
        storage.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));
    });
}

#[test]
fn events_e2e_full_pipeline_dedup_cooldown_mute() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use frankenterm_core::events::{
            EventFilter, NotificationGate, NotifyDecision, event_identity_key,
        };
        use frankenterm_core::storage::{EventMuteRecord, StorageHandle};
        use std::time::Duration;

        let db_path =
            std::env::temp_dir().join(format!("wa_labrt_full_pipeline_{}.db", std::process::id()));
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));

        let storage = StorageHandle::new(&db_str).await.expect("open test db");
        let now = frankenterm_core::storage::now_ms();

        let detection = Detection {
            rule_id: "codex.compaction".to_string(),
            agent_type: frankenterm_core::patterns::AgentType::Codex,
            event_type: "test".to_string(),
            severity: frankenterm_core::patterns::Severity::Warning,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "test".to_string(),
            span: (0, 4),
        };
        let identity_key = event_identity_key(&detection, 1, None);

        // Step 1: gate allows first event
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        let r1 = gate.should_notify(&detection, 1, None);
        assert!(matches!(r1, NotifyDecision::Send { .. }));

        // Step 2: gate deduplicates second event
        let r2 = gate.should_notify(&detection, 1, None);
        assert!(matches!(r2, NotifyDecision::Deduplicated { .. }));

        // Step 3: mute the event via storage
        storage
            .add_event_mute(EventMuteRecord {
                identity_key: identity_key.clone(),
                scope: "workspace".to_string(),
                created_at: now,
                expires_at: None,
                created_by: Some("operator".to_string()),
                reason: Some("too noisy".to_string()),
            })
            .await
            .unwrap();

        // Step 4: verify mute is active
        assert!(storage.is_event_muted(&identity_key, now).await.unwrap());

        // Step 5: muted event is visible in muted list
        let mutes = storage.list_active_mutes(now).await.unwrap();
        let our_mute = mutes.iter().find(|m| m.identity_key == identity_key);
        assert!(our_mute.is_some(), "muted event should be in list");
        assert_eq!(our_mute.unwrap().reason.as_deref(), Some("too noisy"));

        // Step 6: after unmuting, is_event_muted returns false
        storage.remove_event_mute(&identity_key).await.unwrap();
        assert!(!storage.is_event_muted(&identity_key, now).await.unwrap());

        // Clean up
        storage.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));
    });
}

// ===========================================================================
// Note: LabRuntime sections (2-5) omitted for events tests.
//
// The EventBus uses `tokio::sync::broadcast` which requires tokio waker
// infrastructure for `.recv().await` to resolve properly. While this works
// under RuntimeFixture (which provides a full async runtime), the LabRuntime's
// deterministic scheduler cannot drive tokio broadcast waker notifications,
// causing tasks blocked on `recv()` to appear "leaked".
//
// The StorageHandle uses `tokio::task::spawn_blocking` for sqlite I/O,
// which similarly requires a full runtime with blocking thread pool support.
// ===========================================================================
