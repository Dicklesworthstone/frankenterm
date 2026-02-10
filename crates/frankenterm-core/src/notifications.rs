//! Notification interface + shared payloads.
//!
//! Centralizes payload formatting and redaction before dispatching to
//! delivery backends (webhook, desktop, etc.).

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::event_templates::{RenderedEvent, render_event};
use crate::events::{NotificationGate, NotifyDecision, event_identity_key};
use crate::patterns::Detection;
use crate::policy::Redactor;
use crate::storage::{StorageHandle, StoredEvent};

/// Unified notification payload for all senders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationPayload {
    /// Event type (rule_id).
    pub event_type: String,
    /// Pane where the event was detected.
    pub pane_id: u64,
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// Human-readable summary (redacted).
    pub summary: String,
    /// Longer description (redacted).
    pub description: String,
    /// Severity level (lowercase).
    pub severity: String,
    /// Agent type.
    pub agent_type: String,
    /// Confidence score 0.0-1.0.
    pub confidence: f64,
    /// Suggested quick-fix command (redacted), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_fix: Option<String>,
    /// Number of similar events suppressed since the last notification.
    pub suppressed_since_last: u64,
}

impl NotificationPayload {
    /// Build a redacted payload from a detection and rendered template.
    #[must_use]
    pub fn from_detection(
        detection: &Detection,
        pane_id: u64,
        rendered: &RenderedEvent,
        suppressed_since_last: u64,
    ) -> Self {
        let redactor = Redactor::new();
        Self::from_detection_with_redactor(
            detection,
            pane_id,
            rendered,
            suppressed_since_last,
            &redactor,
        )
    }

    /// Build a redacted payload using a provided redactor (useful for tests).
    #[must_use]
    pub fn from_detection_with_redactor(
        detection: &Detection,
        pane_id: u64,
        rendered: &RenderedEvent,
        suppressed_since_last: u64,
        redactor: &Redactor,
    ) -> Self {
        let quick_fix = rendered
            .suggestions
            .first()
            .and_then(|s| s.command.clone())
            .map(|command| redact_text(redactor, &command));

        Self {
            event_type: detection.rule_id.clone(),
            pane_id,
            timestamp: now_iso8601(),
            summary: redact_text(redactor, &rendered.summary),
            description: redact_text(redactor, &rendered.description),
            severity: severity_str(detection),
            agent_type: detection.agent_type.to_string(),
            confidence: detection.confidence,
            quick_fix,
            suppressed_since_last,
        }
    }
}

fn redact_text(redactor: &Redactor, text: &str) -> String {
    redactor.redact(text)
}

fn severity_str(detection: &Detection) -> String {
    match detection.severity {
        crate::patterns::Severity::Info => "info".to_string(),
        crate::patterns::Severity::Warning => "warning".to_string(),
        crate::patterns::Severity::Critical => "critical".to_string(),
    }
}

fn now_iso8601() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("{ts}"))
}

/// Record of a single delivery attempt for observability.
#[derive(Debug, Clone, Serialize)]
pub struct NotificationDeliveryRecord {
    /// Target name (endpoint, backend, etc.).
    pub target: String,
    /// Whether the delivery was accepted.
    pub accepted: bool,
    /// HTTP status code or equivalent (0 for non-HTTP senders).
    pub status_code: u16,
    /// Optional error message.
    pub error: Option<String>,
}

/// Delivery outcome for a notification sender.
#[derive(Debug, Clone, Serialize)]
pub struct NotificationDelivery {
    /// Sender name or identifier.
    pub sender: String,
    /// Whether the delivery succeeded.
    pub success: bool,
    /// Whether the delivery was rate limited.
    pub rate_limited: bool,
    /// Optional error message.
    pub error: Option<String>,
    /// Per-target delivery records (if any).
    pub records: Vec<NotificationDeliveryRecord>,
}

/// Async notification sender interface.
pub trait NotificationSender: Send + Sync {
    /// Sender identifier used in logs and delivery records.
    fn name(&self) -> &'static str;

    /// Send the notification payload.
    fn send<'a>(&'a self, payload: &'a NotificationPayload) -> NotificationFuture<'a>;
}

/// Notification future type.
pub type NotificationFuture<'a> = Pin<Box<dyn Future<Output = NotificationDelivery> + Send + 'a>>;

/// Rate-limited sender wrapper.
pub struct RateLimitedSender<S> {
    inner: S,
    min_interval: Duration,
    last_sent: Mutex<Option<Instant>>,
}

impl<S> RateLimitedSender<S> {
    /// Wrap a sender with a minimum interval between deliveries.
    #[must_use]
    pub fn new(inner: S, min_interval: Duration) -> Self {
        Self {
            inner,
            min_interval,
            last_sent: Mutex::new(None),
        }
    }

    /// Access the wrapped sender.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S> NotificationSender for RateLimitedSender<S>
where
    S: NotificationSender,
{
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn send<'a>(&'a self, payload: &'a NotificationPayload) -> NotificationFuture<'a> {
        let now = Instant::now();
        let mut guard = self.last_sent.lock().unwrap_or_else(|e| e.into_inner());
        let within_window = guard
            .as_ref()
            .is_some_and(|last| now.duration_since(*last) < self.min_interval);

        if within_window {
            let delivery = NotificationDelivery {
                sender: self.name().to_string(),
                success: false,
                rate_limited: true,
                error: Some("rate_limited".to_string()),
                records: Vec::new(),
            };
            return Box::pin(async move { delivery });
        }

        *guard = Some(now);
        drop(guard);
        self.inner.send(payload)
    }
}

/// Outcome of attempting to notify about a detection.
#[derive(Debug, Clone)]
pub struct NotificationOutcome {
    /// Gate decision (send / filtered / deduped / throttled).
    pub decision: NotifyDecision,
    /// Delivery results per sender (empty if not sent).
    pub deliveries: Vec<NotificationDelivery>,
}

/// Notification pipeline that gates and fans out deliveries.
pub struct NotificationPipeline {
    gate: NotificationGate,
    senders: Vec<Box<dyn NotificationSender>>,
    mute_store: Option<Arc<tokio::sync::Mutex<StorageHandle>>>,
}

impl NotificationPipeline {
    /// Create a pipeline with a gate and sender list.
    #[must_use]
    pub fn new(gate: NotificationGate, senders: Vec<Box<dyn NotificationSender>>) -> Self {
        Self {
            gate,
            senders,
            mute_store: None,
        }
    }

    /// Create a pipeline with access to persistent mute storage.
    #[must_use]
    pub fn with_mute_store(
        gate: NotificationGate,
        senders: Vec<Box<dyn NotificationSender>>,
        storage: Arc<tokio::sync::Mutex<StorageHandle>>,
    ) -> Self {
        Self {
            gate,
            senders,
            mute_store: Some(storage),
        }
    }

    /// Number of active senders in this pipeline.
    #[must_use]
    pub fn sender_count(&self) -> usize {
        self.senders.len()
    }

    /// Gate and dispatch a detection event.
    pub async fn handle_detection(
        &mut self,
        detection: &Detection,
        pane_id: u64,
        pane_uuid: Option<&str>,
        event_id: Option<i64>,
    ) -> NotificationOutcome {
        if let Some(storage) = &self.mute_store {
            let identity_key = event_identity_key(detection, pane_id, pane_uuid);
            let now_ms = now_epoch_ms();
            let muted = {
                let storage_guard = storage.lock().await;
                storage_guard
                    .is_event_muted(&identity_key, now_ms)
                    .await
                    .unwrap_or(false)
            };
            if muted {
                return NotificationOutcome {
                    decision: NotifyDecision::Filtered,
                    deliveries: Vec::new(),
                };
            }
        }

        let decision = self.gate.should_notify(detection, pane_id, pane_uuid);
        match decision {
            NotifyDecision::Send {
                suppressed_since_last,
            } => {
                let rendered = render_detection(detection, pane_id, event_id);
                let payload = NotificationPayload::from_detection(
                    detection,
                    pane_id,
                    &rendered,
                    suppressed_since_last,
                );
                let deliveries = self.dispatch_payload(&payload).await;
                NotificationOutcome {
                    decision,
                    deliveries,
                }
            }
            _ => NotificationOutcome {
                decision,
                deliveries: Vec::new(),
            },
        }
    }

    async fn dispatch_payload(&self, payload: &NotificationPayload) -> Vec<NotificationDelivery> {
        let mut deliveries = Vec::with_capacity(self.senders.len());
        for sender in &self.senders {
            deliveries.push(sender.send(payload).await);
        }
        deliveries
    }
}

fn render_detection(detection: &Detection, pane_id: u64, event_id: Option<i64>) -> RenderedEvent {
    let event = StoredEvent {
        id: event_id.unwrap_or(0),
        pane_id,
        rule_id: detection.rule_id.clone(),
        agent_type: detection.agent_type.to_string(),
        event_type: detection.event_type.clone(),
        severity: severity_str(detection),
        confidence: detection.confidence,
        extracted: Some(detection.extracted.clone()),
        matched_text: Some(detection.matched_text.clone()),
        segment_id: None,
        detected_at: now_epoch_ms(),
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    };

    render_event(&event)
}

fn now_epoch_ms() -> i64 {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(ts.as_millis()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventFilter, NotificationGate};
    use crate::patterns::{AgentType, Severity};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn test_detection() -> Detection {
        Detection {
            rule_id: "core.codex:usage_reached".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage_reached".to_string(),
            severity: Severity::Warning,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "Rate limit exceeded".to_string(),
            span: (0, 19),
        }
    }

    fn test_rendered() -> RenderedEvent {
        RenderedEvent {
            summary: "My API key is sk-abc123456789012345678901234567890123456789012345678901"
                .to_string(),
            description: "Key: sk-abc123456789012345678901234567890123456789012345678901".to_string(),
            suggestions: vec![crate::event_templates::Suggestion {
                text: "Use API key".to_string(),
                command: Some(
                    "export OPENAI_API_KEY=sk-abc123456789012345678901234567890123456789012345678901"
                        .to_string(),
                ),
                doc_link: None,
            }],
            severity: Severity::Warning,
        }
    }

    #[test]
    fn payload_redacts_sensitive_fields() {
        let payload =
            NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);
        assert!(!payload.summary.contains("sk-abc"));
        assert!(!payload.description.contains("sk-abc"));
        assert!(payload.quick_fix.is_some());
        assert!(!payload.quick_fix.unwrap().contains("sk-abc"));
    }

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

    #[tokio::test]
    async fn pipeline_sends_when_gate_allows() {
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
    }

    #[tokio::test]
    async fn pipeline_filters_events() {
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
    }

    #[tokio::test]
    async fn pipeline_deduplicates_repeated_events() {
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
    }

    // ========================================================================
    // Redaction tests (bd-wolc)
    // ========================================================================

    #[test]
    fn payload_redacts_openai_key_in_summary() {
        let detection = test_detection();
        let rendered = RenderedEvent {
            summary: "Token: sk-abc123456789012345678901234567890123456789012345678901".to_string(),
            description: "No secrets here".to_string(),
            suggestions: vec![],
            severity: Severity::Warning,
        };
        let payload = NotificationPayload::from_detection(&detection, 1, &rendered, 0);
        assert!(
            !payload.summary.contains("sk-abc"),
            "OpenAI key should be redacted from summary"
        );
        assert!(
            payload.summary.contains("[REDACTED]"),
            "should contain redaction marker"
        );
    }

    #[test]
    fn payload_redacts_github_token_in_description() {
        let detection = test_detection();
        let rendered = RenderedEvent {
            summary: "Alert".to_string(),
            description: "GH token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789".to_string(),
            suggestions: vec![],
            severity: Severity::Warning,
        };
        let payload = NotificationPayload::from_detection(&detection, 1, &rendered, 0);
        assert!(
            !payload.description.contains("ghp_"),
            "GitHub token should be redacted from description"
        );
    }

    #[test]
    fn payload_redacts_quick_fix_command() {
        let detection = test_detection();
        let rendered = RenderedEvent {
            summary: "Set API key".to_string(),
            description: "Configure key".to_string(),
            suggestions: vec![crate::event_templates::Suggestion {
                text: "Set key".to_string(),
                command: Some(
                    "export KEY=sk-abc123456789012345678901234567890123456789012345678901"
                        .to_string(),
                ),
                doc_link: None,
            }],
            severity: Severity::Warning,
        };
        let payload = NotificationPayload::from_detection(&detection, 1, &rendered, 0);
        let fix = payload.quick_fix.expect("should have quick_fix");
        assert!(
            !fix.contains("sk-abc"),
            "quick_fix should not contain raw secret"
        );
    }

    #[test]
    fn payload_with_no_secrets_passes_through() {
        let detection = test_detection();
        let rendered = RenderedEvent {
            summary: "Agent usage limit reached".to_string(),
            description: "The codex agent hit its rate limit.".to_string(),
            suggestions: vec![],
            severity: Severity::Warning,
        };
        let payload = NotificationPayload::from_detection(&detection, 5, &rendered, 3);
        assert_eq!(payload.summary, "Agent usage limit reached");
        assert_eq!(payload.description, "The codex agent hit its rate limit.");
        assert_eq!(payload.suppressed_since_last, 3);
    }

    #[test]
    fn payload_redacts_database_url_password() {
        let detection = test_detection();
        let rendered = RenderedEvent {
            summary: "DB: postgres://admin:hunter2@db.example.com:5432/prod".to_string(),
            description: "Connection failed".to_string(),
            suggestions: vec![],
            severity: Severity::Critical,
        };
        let payload = NotificationPayload::from_detection(&detection, 1, &rendered, 0);
        assert!(
            !payload.summary.contains("hunter2"),
            "DB password should be redacted"
        );
    }

    #[test]
    fn payload_json_serialization_does_not_leak_secrets() {
        let payload =
            NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);
        let json = serde_json::to_string(&payload).expect("serialize");
        assert!(
            !json.contains("sk-abc"),
            "JSON should not contain raw API key"
        );
    }

    #[test]
    fn payload_preserves_metadata_fields() {
        let detection = Detection {
            rule_id: "custom.agent:session_end".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "session_end".to_string(),
            severity: Severity::Info,
            confidence: 0.85,
            extracted: serde_json::json!({"count": 42}),
            matched_text: "Session ended".to_string(),
            span: (0, 13),
        };
        let rendered = RenderedEvent {
            summary: "Session ended".to_string(),
            description: "Agent session concluded".to_string(),
            suggestions: vec![],
            severity: Severity::Info,
        };
        let payload = NotificationPayload::from_detection(&detection, 99, &rendered, 7);
        assert_eq!(payload.event_type, "custom.agent:session_end");
        assert_eq!(payload.pane_id, 99);
        assert_eq!(payload.severity, "info");
        assert_eq!(payload.agent_type, "gemini");
        assert!((payload.confidence - 0.85).abs() < f64::EPSILON);
        assert_eq!(payload.suppressed_since_last, 7);
        assert!(payload.quick_fix.is_none());
    }

    // ========================================================================
    // Rate limiting (RateLimitedSender)
    // ========================================================================

    #[tokio::test]
    async fn rate_limited_sender_allows_first_send() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inner = MockSender::new("inner", Arc::clone(&sent));
        let limited = RateLimitedSender::new(inner, Duration::from_secs(60));

        let payload =
            NotificationPayload::from_detection(&test_detection(), 1, &test_rendered(), 0);
        let delivery = limited.send(&payload).await;

        assert!(delivery.success);
        assert!(!delivery.rate_limited);
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rate_limited_sender_blocks_rapid_second_send() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inner = MockSender::new("inner", Arc::clone(&sent));
        let limited = RateLimitedSender::new(inner, Duration::from_secs(60));

        let payload =
            NotificationPayload::from_detection(&test_detection(), 1, &test_rendered(), 0);

        // First send succeeds
        let d1 = limited.send(&payload).await;
        assert!(d1.success);

        // Immediate second send is rate limited
        let d2 = limited.send(&payload).await;
        assert!(!d2.success);
        assert!(d2.rate_limited);
        assert_eq!(
            d2.error.as_deref(),
            Some("rate_limited"),
            "should report rate_limited error"
        );

        // Only 1 delivery reached the inner sender
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rate_limited_sender_allows_after_interval() {
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
    }

    #[test]
    fn rate_limited_sender_exposes_inner() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inner = MockSender::new("test_inner", Arc::clone(&sent));
        let limited = RateLimitedSender::new(inner, Duration::from_secs(60));
        assert_eq!(limited.inner().name, "test_inner");
    }

    #[test]
    fn rate_limited_sender_name_delegates() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let inner = MockSender::new("delegate_name", Arc::clone(&sent));
        let limited = RateLimitedSender::new(inner, Duration::from_secs(60));
        assert_eq!(limited.name(), "delegate_name");
    }

    // ========================================================================
    // Pipeline: multi-sender fan-out
    // ========================================================================

    #[tokio::test]
    async fn pipeline_fans_out_to_multiple_senders() {
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
    }

    #[test]
    fn pipeline_sender_count() {
        let filter = EventFilter::allow_all();
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));

        let sent = Arc::new(Mutex::new(Vec::new()));
        let s1 = MockSender::new("s1", Arc::clone(&sent));
        let s2 = MockSender::new("s2", Arc::clone(&sent));
        let pipeline = NotificationPipeline::new(gate, vec![Box::new(s1), Box::new(s2)]);

        assert_eq!(pipeline.sender_count(), 2);
    }

    #[tokio::test]
    async fn pipeline_empty_senders_still_returns_outcome() {
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
    }

    // ========================================================================
    // Pipeline: mute store integration
    // ========================================================================

    #[tokio::test]
    async fn pipeline_with_mute_store_blocks_muted_events() {
        use crate::events::event_identity_key;
        use crate::storage::{EventMuteRecord, StorageHandle};

        let db_path =
            std::env::temp_dir().join(format!("wa_notif_test_mute_{}.db", std::process::id()));
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));

        let storage = StorageHandle::new(&db_str).await.expect("open test db");

        // Mute the event
        let detection = test_detection();
        let identity_key = event_identity_key(&detection, 7, None);
        let now_ms = crate::storage::now_ms();
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
        let storage_arc = Arc::new(tokio::sync::Mutex::new(storage));
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
    }

    // ========================================================================
    // Failure handling mock
    // ========================================================================

    /// Mock sender that simulates failures.
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

    #[tokio::test]
    async fn pipeline_handles_sender_failure_gracefully() {
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
    }

    #[tokio::test]
    async fn pipeline_partial_failure_still_delivers_to_healthy_senders() {
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
        // First delivery fails
        assert!(!outcome.deliveries[0].success);
        // Second delivery succeeds
        assert!(outcome.deliveries[1].success);
        // Good sender received the payload
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    // ========================================================================
    // Failure sender doesn't leak secrets
    // ========================================================================

    #[tokio::test]
    async fn failed_delivery_error_does_not_contain_payload() {
        let filter = EventFilter::allow_all();
        let gate =
            NotificationGate::from_config(filter, Duration::from_secs(60), Duration::from_secs(60));
        let mut pipeline = NotificationPipeline::new(gate, vec![Box::new(FailingSender)]);

        let outcome = pipeline
            .handle_detection(&test_detection(), 1, None, None)
            .await;

        // Error message should not contain the event payload or secrets
        let err = outcome.deliveries[0].error.as_deref().unwrap_or("");
        assert!(!err.contains("sk-abc"), "error should not leak secrets");
    }

    // ========================================================================
    // Helper function tests
    // ========================================================================

    #[test]
    fn severity_str_maps_all_variants() {
        let mut d = test_detection();
        d.severity = Severity::Info;
        assert_eq!(severity_str(&d), "info");
        d.severity = Severity::Warning;
        assert_eq!(severity_str(&d), "warning");
        d.severity = Severity::Critical;
        assert_eq!(severity_str(&d), "critical");
    }

    #[test]
    fn now_iso8601_produces_valid_timestamp() {
        let ts = now_iso8601();
        // Should contain 'T' separator (ISO 8601 format)
        assert!(ts.contains('T'), "should be ISO 8601 format: {ts}");
    }

    // ========================================================================
    // NotificationDeliveryRecord / NotificationDelivery
    // ========================================================================

    #[test]
    fn delivery_record_serializes() {
        let record = NotificationDeliveryRecord {
            target: "slack-webhook".to_string(),
            accepted: true,
            status_code: 200,
            error: None,
        };
        let json = serde_json::to_string(&record).expect("serialize");
        assert!(json.contains("slack-webhook"));
        assert!(json.contains("200"));
    }

    #[test]
    fn delivery_with_error_serializes() {
        let delivery = NotificationDelivery {
            sender: "webhook".to_string(),
            success: false,
            rate_limited: false,
            error: Some("timeout".to_string()),
            records: vec![NotificationDeliveryRecord {
                target: "https://hooks.example.com".to_string(),
                accepted: false,
                status_code: 0,
                error: Some("connection reset".to_string()),
            }],
        };
        let json = serde_json::to_string(&delivery).expect("serialize");
        assert!(json.contains("timeout"));
        assert!(json.contains("connection reset"));
    }
}
