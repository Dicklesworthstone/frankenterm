//! Notification interface + shared payloads.
//!
//! Centralizes payload formatting and redaction before dispatching to
//! delivery backends (webhook, desktop, etc.).

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::event_templates::RenderedEvent;
use crate::patterns::Detection;
use crate::policy::Redactor;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Severity};

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
}
