//! LabRuntime-ported notification pipeline tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` functions from `notifications.rs` to asupersync-based
//! `RuntimeFixture`, gaining seed-based reproducibility for notification pipeline,
//! rate-limited sender, and mute-store integration tests.
//!
//! The notification pipeline uses `Box::pin(async { ... })` futures which work
//! correctly under RuntimeFixture's current-thread runtime.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::events::{EventFilter, NotificationGate, NotifyDecision};
use frankenterm_core::notifications::{
    NotificationDelivery, NotificationFuture, NotificationOutcome, NotificationPayload,
    NotificationPipeline, NotificationSender, RateLimitedSender,
};
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ===========================================================================
// Mock types (mirrors the ones in notifications.rs tests module)
// ===========================================================================

#[derive(Clone)]
struct MockSender {
    name: &'static str,
    sent: Arc<Mutex<Vec<NotificationPayload>>>,
}

impl MockSender {
    fn new(name: &'static str, sent: Arc<Mutex<Vec<NotificationPayload>>>) -> Self {
        Self { name, sent }
    }
}

impl NotificationSender for MockSender {
    fn name(&self) -> &'static str {
        self.name
    }

    fn send<'a>(&'a self, payload: &'a NotificationPayload) -> NotificationFuture<'a> {
        let sent = Arc::clone(&self.sent);
        let payload = payload.clone();
        Box::pin(async move {
            let mut guard = sent.lock().unwrap_or_else(|e| e.into_inner());
            guard.push(payload);
            NotificationDelivery {
                sender: "mock".to_string(),
                success: true,
                rate_limited: false,
                error: None,
                records: Vec::new(),
            }
        })
    }
}

struct FailingSender;

impl NotificationSender for FailingSender {
    fn name(&self) -> &'static str {
        "failing"
    }

    fn send<'a>(&'a self, _payload: &'a NotificationPayload) -> NotificationFuture<'a> {
        Box::pin(async {
            NotificationDelivery {
                sender: "failing".to_string(),
                success: false,
                rate_limited: false,
                error: Some("connection refused".to_string()),
                records: Vec::new(),
            }
        })
    }
}

fn test_detection() -> Detection {
    Detection {
        rule_id: "core.codex:usage_reached".to_string(),
        agent_type: AgentType::Codex,
        event_type: "usage_reached".to_string(),
        severity: Severity::Warning,
        confidence: 0.95,
        extracted: serde_json::json!({}),
        matched_text: "Usage limit reached".to_string(),
        span: (0, 19),
    }
}

fn test_rendered() -> frankenterm_core::event_templates::RenderedEvent {
    frankenterm_core::event_templates::RenderedEvent {
        summary: "Agent hit usage limit: sk-abc123456789012345678901234567890123456789012345678901"
            .to_string(),
        description: "Codex usage limit reached".to_string(),
        suggestions: vec![],
        severity: Severity::Warning,
    }
}

// ===========================================================================
// Section 1: Pipeline basic tests
// ===========================================================================

#[test]
fn notif_pipeline_sends_when_gate_allows() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let filter = EventFilter::allow_all();
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender::new("mock", Arc::clone(&sent));
        let mut pipeline = NotificationPipeline::new(gate, vec![Box::new(sender)]);

        let outcome = pipeline
            .handle_detection(&test_detection(), 7, None, Some(42))
            .await;

        assert!(matches!(outcome.decision, NotifyDecision::Send { .. }));
        assert_eq!(outcome.deliveries.len(), 1);
        assert_eq!(sent.lock().unwrap().len(), 1);
    });
}

#[test]
fn notif_pipeline_filters_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let include: Vec<String> = Vec::new();
        let exclude = vec!["core.*".to_string()];
        let agent_types: Vec<String> = Vec::new();
        let filter = EventFilter::from_config(&include, &exclude, None, &agent_types);
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender::new("mock", Arc::clone(&sent));
        let mut pipeline = NotificationPipeline::new(gate, vec![Box::new(sender)]);

        let outcome = pipeline
            .handle_detection(&test_detection(), 7, None, None)
            .await;

        assert!(matches!(outcome.decision, NotifyDecision::Filtered));
        assert!(outcome.deliveries.is_empty());
        assert!(sent.lock().unwrap().is_empty());
    });
}

#[test]
fn notif_pipeline_deduplicates_repeated_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let filter = EventFilter::allow_all();
        let gate = NotificationGate::from_config(
            filter,
            Duration::from_secs(300),
            Duration::from_secs(60),
        );
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender::new("mock", Arc::clone(&sent));
        let mut pipeline = NotificationPipeline::new(gate, vec![Box::new(sender)]);

        let _ = pipeline
            .handle_detection(&test_detection(), 7, None, None)
            .await;
        let outcome = pipeline
            .handle_detection(&test_detection(), 7, None, None)
            .await;

        assert!(matches!(
            outcome.decision,
            NotifyDecision::Deduplicated { .. }
        ));
        assert_eq!(sent.lock().unwrap().len(), 1);
    });
}

// ===========================================================================
// Section 2: Rate-limited sender tests
// ===========================================================================

#[test]
fn notif_rate_limited_sender_allows_first_send() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inner = MockSender::new("inner", Arc::clone(&sent));
        let limited = RateLimitedSender::new(inner, Duration::from_secs(60));

        let payload =
            NotificationPayload::from_detection(&test_detection(), 1, &test_rendered(), 0);
        let delivery = limited.send(&payload).await;

        assert!(delivery.success);
        assert!(!delivery.rate_limited);
        assert_eq!(sent.lock().unwrap().len(), 1);
    });
}

#[test]
fn notif_rate_limited_sender_blocks_rapid_second_send() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inner = MockSender::new("inner", Arc::clone(&sent));
        let limited = RateLimitedSender::new(inner, Duration::from_secs(60));

        let payload =
            NotificationPayload::from_detection(&test_detection(), 1, &test_rendered(), 0);

        let d1 = limited.send(&payload).await;
        assert!(d1.success);

        let d2 = limited.send(&payload).await;
        assert!(!d2.success);
        assert!(d2.rate_limited);
        assert_eq!(
            d2.error.as_deref(),
            Some("rate_limited"),
            "should report rate_limited error"
        );

        assert_eq!(sent.lock().unwrap().len(), 1);
    });
}

#[test]
fn notif_rate_limited_sender_allows_after_interval() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inner = MockSender::new("inner", Arc::clone(&sent));
        let limited = RateLimitedSender::new(inner, Duration::from_millis(10));

        let payload =
            NotificationPayload::from_detection(&test_detection(), 1, &test_rendered(), 0);

        let d1 = limited.send(&payload).await;
        assert!(d1.success);

        // Wait for rate limit to expire
        std::thread::sleep(Duration::from_millis(15));

        let d2 = limited.send(&payload).await;
        assert!(d2.success, "should allow send after interval");
        assert!(!d2.rate_limited);
        assert_eq!(sent.lock().unwrap().len(), 2);
    });
}

// ===========================================================================
// Section 3: Multi-sender fan-out
// ===========================================================================

#[test]
fn notif_pipeline_fans_out_to_multiple_senders() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let filter = EventFilter::allow_all();
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));

        let sent_a = Arc::new(Mutex::new(Vec::new()));
        let sent_b = Arc::new(Mutex::new(Vec::new()));
        let sender_a = MockSender::new("a", Arc::clone(&sent_a));
        let sender_b = MockSender::new("b", Arc::clone(&sent_b));
        let mut pipeline =
            NotificationPipeline::new(gate, vec![Box::new(sender_a), Box::new(sender_b)]);

        let outcome = pipeline
            .handle_detection(&test_detection(), 1, None, None)
            .await;

        assert!(matches!(outcome.decision, NotifyDecision::Send { .. }));
        assert_eq!(
            outcome.deliveries.len(),
            2,
            "should deliver to both senders"
        );
        assert_eq!(sent_a.lock().unwrap().len(), 1);
        assert_eq!(sent_b.lock().unwrap().len(), 1);
    });
}

#[test]
fn notif_pipeline_empty_senders_still_returns_outcome() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let filter = EventFilter::allow_all();
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));
        let mut pipeline = NotificationPipeline::new(gate, vec![]);

        let outcome = pipeline
            .handle_detection(&test_detection(), 1, None, None)
            .await;

        assert!(matches!(outcome.decision, NotifyDecision::Send { .. }));
        assert!(
            outcome.deliveries.is_empty(),
            "no senders means no deliveries"
        );
    });
}

// ===========================================================================
// Section 4: Mute store integration
// ===========================================================================

#[test]
fn notif_pipeline_with_mute_store_blocks_muted_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use frankenterm_core::events::event_identity_key;
        use frankenterm_core::storage::{EventMuteRecord, StorageHandle};

        let db_path = std::env::temp_dir().join(format!(
            "wa_notif_labrt_mute_{}.db",
            std::process::id()
        ));
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));

        let storage = StorageHandle::new(&db_str).await.expect("open test db");

        let detection = test_detection();
        let identity_key = event_identity_key(&detection, 7, None);
        let now_ms = frankenterm_core::storage::now_ms();
        storage
            .add_event_mute(EventMuteRecord {
                identity_key,
                scope: "workspace".to_string(),
                created_at: now_ms,
                expires_at: None,
                created_by: Some("test".to_string()),
                reason: Some("too noisy".to_string()),
            })
            .await
            .unwrap();

        let filter = EventFilter::allow_all();
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender = MockSender::new("mock", Arc::clone(&sent));
        let storage_arc =
            Arc::new(frankenterm_core::runtime_compat::RwLock::new(storage));
        let mut pipeline =
            NotificationPipeline::with_mute_store(gate, vec![Box::new(sender)], storage_arc);

        let outcome = pipeline.handle_detection(&detection, 7, None, None).await;

        assert!(
            matches!(outcome.decision, NotifyDecision::Filtered),
            "muted event should be filtered"
        );
        assert!(
            sent.lock().unwrap().is_empty(),
            "muted event should not be sent"
        );

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));
    });
}

// ===========================================================================
// Section 5: Failure handling
// ===========================================================================

#[test]
fn notif_pipeline_handles_sender_failure_gracefully() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let filter = EventFilter::allow_all();
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));
        let mut pipeline = NotificationPipeline::new(gate, vec![Box::new(FailingSender)]);

        let outcome = pipeline
            .handle_detection(&test_detection(), 1, None, None)
            .await;

        assert!(matches!(outcome.decision, NotifyDecision::Send { .. }));
        assert_eq!(outcome.deliveries.len(), 1);
        assert!(!outcome.deliveries[0].success);
        assert_eq!(
            outcome.deliveries[0].error.as_deref(),
            Some("connection refused")
        );
    });
}

#[test]
fn notif_pipeline_partial_failure_still_delivers_to_healthy_senders() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let filter = EventFilter::allow_all();
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let good_sender = MockSender::new("good", Arc::clone(&sent));
        let mut pipeline =
            NotificationPipeline::new(gate, vec![Box::new(FailingSender), Box::new(good_sender)]);

        let outcome = pipeline
            .handle_detection(&test_detection(), 1, None, None)
            .await;

        assert_eq!(outcome.deliveries.len(), 2);
        assert!(!outcome.deliveries[0].success);
        assert!(outcome.deliveries[1].success);
        assert_eq!(sent.lock().unwrap().len(), 1);
    });
}

#[test]
fn notif_failed_delivery_error_does_not_contain_payload() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let filter = EventFilter::allow_all();
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));
        let mut pipeline = NotificationPipeline::new(gate, vec![Box::new(FailingSender)]);

        let outcome = pipeline
            .handle_detection(&test_detection(), 1, None, None)
            .await;

        let err = outcome.deliveries[0].error.as_deref().unwrap_or("");
        assert!(!err.contains("sk-abc"), "error should not leak secrets");
    });
}
