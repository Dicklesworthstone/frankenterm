//! Notification interface + shared payloads.
//!
//! Centralizes payload formatting and redaction before dispatching to
//! delivery backends (webhook, desktop, etc.).

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::event_templates::{RenderedEvent, render_event};
use crate::events::{NotificationGate, NotifyDecision};
use crate::patterns::Detection;
use crate::policy::Redactor;
use crate::storage::StoredEvent;

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
}

impl NotificationPipeline {
    /// Create a pipeline with a gate and sender list.
    #[must_use]
    pub fn new(gate: NotificationGate, senders: Vec<Box<dyn NotificationSender>>) -> Self {
        Self { gate, senders }
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
        event_id: Option<i64>,
    ) -> NotificationOutcome {
        let decision = self.gate.should_notify(detection, pane_id);
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
            .handle_detection(&test_detection(), 7, Some(42))
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

        let outcome = pipeline.handle_detection(&test_detection(), 7, None).await;

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

        let _ = pipeline.handle_detection(&test_detection(), 7, None).await;
        let outcome = pipeline.handle_detection(&test_detection(), 7, None).await;

        assert!(matches!(
            outcome.decision,
            NotifyDecision::Deduplicated { .. }
        ));
        assert_eq!(sent.lock().unwrap().len(), 1);
    }
}
