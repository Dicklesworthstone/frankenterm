//! Webhook notification delivery.
//!
//! Delivers event notifications to external services via HTTP webhooks
//! with configurable payload templates, circuit breaker protection,
//! and retry with exponential backoff.
//!
//! # Architecture
//!
//! ```text
//! Detection → NotificationGate (filter/dedup/cooldown)
//!                    ↓ (if Send)
//!            WebhookDispatcher
//!            ├── render payload (generic/slack/discord)
//!            ├── check circuit breaker
//!            └── send via WebhookTransport (with retry)
//! ```
//!
//! # Transport Abstraction
//!
//! The actual HTTP POST is behind a [`WebhookTransport`] trait so that
//! wa-core stays free of HTTP client dependencies. The CLI crate (or
//! feature-gated code) provides the real implementation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::event_templates::RenderedEvent;
use crate::notifications::{
    NotificationDelivery, NotificationDeliveryRecord, NotificationFuture, NotificationPayload,
    NotificationSender,
};
use crate::patterns::Detection;

// ============================================================================
// Webhook endpoint configuration
// ============================================================================

/// Payload template format for a webhook endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookTemplate {
    /// Generic JSON payload.
    Generic,
    /// Slack-compatible payload (Block Kit).
    Slack,
    /// Discord-compatible payload (embeds).
    Discord,
}

impl Default for WebhookTemplate {
    fn default() -> Self {
        Self::Generic
    }
}

impl fmt::Display for WebhookTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generic => write!(f, "generic"),
            Self::Slack => write!(f, "slack"),
            Self::Discord => write!(f, "discord"),
        }
    }
}

/// Configuration for a single webhook endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEndpointConfig {
    /// Display name for logging/status.
    pub name: String,

    /// Target URL for HTTP POST.
    pub url: String,

    /// Payload template format.
    #[serde(default)]
    pub template: WebhookTemplate,

    /// Event patterns (rule_id globs) this endpoint subscribes to.
    /// If empty, all events that pass the global notification filter
    /// are delivered.
    #[serde(default)]
    pub events: Vec<String>,

    /// Optional static headers added to every request
    /// (e.g., `Authorization: Bearer <token>`).
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// Per-endpoint enabled flag. Defaults to true.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl WebhookEndpointConfig {
    /// Check if this endpoint is interested in a detection based on its
    /// event patterns.
    #[must_use]
    pub fn matches_detection(&self, detection: &Detection) -> bool {
        if self.events.is_empty() {
            return true;
        }
        self.events
            .iter()
            .any(|pat| crate::events::match_rule_glob(pat, &detection.rule_id))
    }

    /// Check if this endpoint is interested in an event type (rule_id).
    #[must_use]
    pub fn matches_event_type(&self, event_type: &str) -> bool {
        if self.events.is_empty() {
            return true;
        }
        self.events
            .iter()
            .any(|pat| crate::events::match_rule_glob(pat, event_type))
    }
}

// ============================================================================
// Webhook payloads
// ============================================================================

/// Webhook payload type (shared with other notification senders).
pub type WebhookPayload = NotificationPayload;

/// Render a payload into the format expected by the target platform.
#[must_use]
pub fn render_template(template: WebhookTemplate, payload: &WebhookPayload) -> serde_json::Value {
    match template {
        WebhookTemplate::Generic => render_generic(payload),
        WebhookTemplate::Slack => render_slack(payload),
        WebhookTemplate::Discord => render_discord(payload),
    }
}

fn render_generic(p: &WebhookPayload) -> serde_json::Value {
    serde_json::to_value(p).unwrap_or_default()
}

fn render_slack(p: &WebhookPayload) -> serde_json::Value {
    let severity_emoji = match p.severity.as_str() {
        "critical" => ":red_circle:",
        "warning" => ":large_yellow_circle:",
        _ => ":large_blue_circle:",
    };

    let mut text = format!("{severity_emoji} *wa: {}*", p.summary);
    if p.suppressed_since_last > 0 {
        text.push_str(&format!(" (+{} suppressed)", p.suppressed_since_last));
    }

    let mut fields = vec![
        serde_json::json!({
            "type": "mrkdwn",
            "text": format!("*Pane:* {}", p.pane_id)
        }),
        serde_json::json!({
            "type": "mrkdwn",
            "text": format!("*Severity:* {}", p.severity)
        }),
        serde_json::json!({
            "type": "mrkdwn",
            "text": format!("*Agent:* {}", p.agent_type)
        }),
    ];

    if let Some(ref cmd) = p.quick_fix {
        fields.push(serde_json::json!({
            "type": "mrkdwn",
            "text": format!("*Quick fix:* `{cmd}`")
        }));
    }

    serde_json::json!({
        "text": text,
        "blocks": [
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": text
                }
            },
            {
                "type": "section",
                "fields": fields
            },
            {
                "type": "context",
                "elements": [
                    {
                        "type": "mrkdwn",
                        "text": format!("{} | {}", p.event_type, p.timestamp)
                    }
                ]
            }
        ]
    })
}

fn render_discord(p: &WebhookPayload) -> serde_json::Value {
    let color = match p.severity.as_str() {
        "critical" => 0xFF0000, // red
        "warning" => 0xFFAA00,  // amber
        _ => 0x3498DB,          // blue
    };

    let mut fields = vec![
        serde_json::json!({"name": "Pane", "value": p.pane_id.to_string(), "inline": true}),
        serde_json::json!({"name": "Severity", "value": &p.severity, "inline": true}),
        serde_json::json!({"name": "Agent", "value": &p.agent_type, "inline": true}),
    ];

    if let Some(ref cmd) = p.quick_fix {
        fields.push(serde_json::json!({
            "name": "Quick Fix",
            "value": format!("`{cmd}`"),
            "inline": false
        }));
    }

    let mut title = format!("wa: {}", p.summary);
    if p.suppressed_since_last > 0 {
        title.push_str(&format!(" (+{} suppressed)", p.suppressed_since_last));
    }

    serde_json::json!({
        "content": null,
        "embeds": [{
            "title": title,
            "description": &p.description,
            "color": color,
            "fields": fields,
            "footer": {
                "text": format!("{} | {}", p.event_type, p.timestamp)
            }
        }]
    })
}

// ============================================================================
// Transport trait
// ============================================================================

/// Result of a webhook delivery attempt.
#[derive(Debug, Clone)]
pub struct DeliveryResult {
    /// HTTP status code (or 0 if connection failed).
    pub status_code: u16,
    /// Whether the delivery was accepted (2xx).
    pub accepted: bool,
    /// Error message (if delivery failed).
    pub error: Option<String>,
}

impl DeliveryResult {
    /// Create a successful result.
    #[must_use]
    pub fn ok(status_code: u16) -> Self {
        Self {
            status_code,
            accepted: true,
            error: None,
        }
    }

    /// Create a failure result.
    #[must_use]
    pub fn err(status_code: u16, error: impl Into<String>) -> Self {
        Self {
            status_code,
            accepted: false,
            error: Some(error.into()),
        }
    }
}

/// Trait for the HTTP transport layer.
///
/// Implementations provide the actual HTTP POST. This decouples wa-core
/// from any specific HTTP client library.
pub trait WebhookTransport: Send + Sync {
    /// Send a JSON payload to the given URL with optional headers.
    fn send<'a>(
        &'a self,
        url: &'a str,
        headers: &'a HashMap<String, String>,
        body: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = DeliveryResult> + Send + 'a>>;
}

// ============================================================================
// Webhook dispatcher
// ============================================================================

/// Dispatches webhook notifications to configured endpoints.
///
/// Combines endpoint matching, template rendering, and delivery with
/// circuit breaker protection.
pub struct WebhookDispatcher {
    endpoints: Vec<WebhookEndpointConfig>,
    transport: Box<dyn WebhookTransport>,
}

/// Record of a single delivery attempt for observability.
pub type DeliveryRecord = NotificationDeliveryRecord;

impl WebhookDispatcher {
    /// Create a new dispatcher with the given endpoints and transport.
    #[must_use]
    pub fn new(
        endpoints: Vec<WebhookEndpointConfig>,
        transport: Box<dyn WebhookTransport>,
    ) -> Self {
        Self {
            endpoints,
            transport,
        }
    }

    /// Dispatch a detection to all matching endpoints.
    ///
    /// Returns a record for each endpoint that was attempted.
    pub async fn dispatch(
        &self,
        detection: &Detection,
        pane_id: u64,
        rendered: &RenderedEvent,
        suppressed_since_last: u64,
    ) -> Vec<DeliveryRecord> {
        let payload =
            WebhookPayload::from_detection(detection, pane_id, rendered, suppressed_since_last);
        self.dispatch_payload(&payload).await
    }

    /// Dispatch a pre-built payload to all matching endpoints.
    pub async fn dispatch_payload(&self, payload: &NotificationPayload) -> Vec<DeliveryRecord> {
        let mut records = Vec::new();

        for endpoint in &self.endpoints {
            if !endpoint.enabled {
                continue;
            }

            if !endpoint.matches_event_type(&payload.event_type) {
                continue;
            }

            let body = render_template(endpoint.template, payload);

            tracing::debug!(
                endpoint = %endpoint.name,
                url = %endpoint.url,
                template = %endpoint.template,
                rule_id = %payload.event_type,
                "dispatching webhook"
            );

            let result = self
                .transport
                .send(&endpoint.url, &endpoint.headers, &body)
                .await;

            if result.accepted {
                tracing::info!(
                    endpoint = %endpoint.name,
                    status = result.status_code,
                    "webhook delivered"
                );
            } else {
                tracing::warn!(
                    endpoint = %endpoint.name,
                    status = result.status_code,
                    error = ?result.error,
                    "webhook delivery failed"
                );
            }

            records.push(DeliveryRecord {
                target: endpoint.name.clone(),
                accepted: result.accepted,
                status_code: result.status_code,
                error: result.error,
            });
        }

        records
    }

    /// Number of configured endpoints (including disabled ones).
    #[must_use]
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Number of enabled endpoints.
    #[must_use]
    pub fn active_endpoint_count(&self) -> usize {
        self.endpoints.iter().filter(|e| e.enabled).count()
    }
}

impl NotificationSender for WebhookDispatcher {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn send<'a>(&'a self, payload: &'a NotificationPayload) -> NotificationFuture<'a> {
        Box::pin(async move {
            let records = self.dispatch_payload(payload).await;
            let success = records.iter().all(|r| r.accepted);
            NotificationDelivery {
                sender: self.name().to_string(),
                success,
                rate_limited: false,
                error: if success {
                    None
                } else {
                    Some("one_or_more_deliveries_failed".to_string())
                },
                records,
            }
        })
    }
}

// ============================================================================
// Helpers
// ============================================================================

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Severity};
    use std::sync::{Arc, Mutex};

    // ---- Mock transport ----

    #[derive(Clone)]
    struct MockTransport {
        /// Captured requests for assertions.
        requests: Arc<Mutex<Vec<MockRequest>>>,
        /// Response to return.
        response: DeliveryResult,
    }

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    struct MockRequest {
        url: String,
        headers: HashMap<String, String>,
        body: serde_json::Value,
    }

    impl MockTransport {
        fn success() -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                response: DeliveryResult::ok(200),
            }
        }

        fn failure(status: u16, error: &str) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                response: DeliveryResult::err(status, error),
            }
        }

        fn requests(&self) -> Vec<MockRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl WebhookTransport for MockTransport {
        fn send<'a>(
            &'a self,
            url: &'a str,
            headers: &'a HashMap<String, String>,
            body: &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = DeliveryResult> + Send + 'a>> {
            let req = MockRequest {
                url: url.to_string(),
                headers: headers.clone(),
                body: body.clone(),
            };
            self.requests.lock().unwrap().push(req);
            let resp = self.response.clone();
            Box::pin(async move { resp })
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
            matched_text: "Rate limit exceeded".to_string(),
            span: (0, 19),
        }
    }

    fn test_rendered() -> RenderedEvent {
        RenderedEvent {
            summary: "Codex hit usage limit on Pane 3".to_string(),
            description: "The Codex CLI reported a usage limit.".to_string(),
            suggestions: vec![crate::event_templates::Suggestion::with_command(
                "Run wa workflow",
                "wa workflow run handle_usage_limits --pane 3",
            )],
            severity: Severity::Warning,
        }
    }

    fn test_endpoint(name: &str, url: &str, template: WebhookTemplate) -> WebhookEndpointConfig {
        WebhookEndpointConfig {
            name: name.to_string(),
            url: url.to_string(),
            template,
            events: Vec::new(),
            headers: HashMap::new(),
            enabled: true,
        }
    }

    // ---- WebhookPayload tests ----

    #[test]
    fn payload_from_detection_populates_fields() {
        let d = test_detection();
        let r = test_rendered();
        let p = WebhookPayload::from_detection(&d, 3, &r, 5);

        assert_eq!(p.event_type, "core.codex:usage_reached");
        assert_eq!(p.pane_id, 3);
        assert_eq!(p.severity, "warning");
        assert_eq!(p.agent_type, "codex");
        assert!((p.confidence - 0.95_f64).abs() < f64::EPSILON);
        assert_eq!(p.suppressed_since_last, 5);
        assert!(p.quick_fix.is_some());
        assert!(p.quick_fix.unwrap().contains("handle_usage_limits"));
    }

    #[test]
    fn payload_no_suggestions_means_no_quick_fix() {
        let d = test_detection();
        let r = RenderedEvent {
            summary: "test".to_string(),
            description: "test".to_string(),
            suggestions: Vec::new(),
            severity: Severity::Info,
        };
        let p = WebhookPayload::from_detection(&d, 1, &r, 0);
        assert!(p.quick_fix.is_none());
    }

    // ---- Template rendering tests ----

    #[test]
    fn render_generic_is_valid_json() {
        let d = test_detection();
        let r = test_rendered();
        let p = WebhookPayload::from_detection(&d, 3, &r, 0);
        let json = render_template(WebhookTemplate::Generic, &p);

        assert!(json.is_object());
        assert_eq!(json["event_type"], "core.codex:usage_reached");
        assert_eq!(json["pane_id"], 3);
        assert_eq!(json["severity"], "warning");
    }

    #[test]
    fn render_slack_has_blocks() {
        let d = test_detection();
        let r = test_rendered();
        let p = WebhookPayload::from_detection(&d, 3, &r, 2);
        let json = render_template(WebhookTemplate::Slack, &p);

        assert!(json["text"].as_str().unwrap().contains("wa:"));
        assert!(json["text"].as_str().unwrap().contains("suppressed"));
        assert!(json["blocks"].is_array());
        assert!(!json["blocks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn render_slack_no_suppressed_omits_count() {
        let d = test_detection();
        let r = test_rendered();
        let p = WebhookPayload::from_detection(&d, 3, &r, 0);
        let json = render_template(WebhookTemplate::Slack, &p);

        assert!(!json["text"].as_str().unwrap().contains("suppressed"));
    }

    #[test]
    fn render_discord_has_embeds() {
        let d = test_detection();
        let r = test_rendered();
        let p = WebhookPayload::from_detection(&d, 3, &r, 0);
        let json = render_template(WebhookTemplate::Discord, &p);

        assert!(json["content"].is_null());
        assert!(json["embeds"].is_array());
        let embed = &json["embeds"][0];
        assert!(embed["title"].as_str().unwrap().contains("wa:"));
        assert_eq!(embed["color"], 0xFFAA00); // warning = amber
        assert!(embed["fields"].is_array());
    }

    #[test]
    fn render_discord_critical_is_red() {
        let mut d = test_detection();
        d.severity = Severity::Critical;
        let r = RenderedEvent {
            summary: "critical event".to_string(),
            description: "desc".to_string(),
            suggestions: Vec::new(),
            severity: Severity::Critical,
        };
        let p = WebhookPayload::from_detection(&d, 1, &r, 0);
        let json = render_template(WebhookTemplate::Discord, &p);

        assert_eq!(json["embeds"][0]["color"], 0xFF0000);
    }

    // ---- Endpoint matching tests ----

    #[test]
    fn endpoint_empty_events_matches_all() {
        let ep = test_endpoint("test", "http://localhost", WebhookTemplate::Generic);
        assert!(ep.matches_detection(&test_detection()));
    }

    #[test]
    fn endpoint_matching_pattern() {
        let mut ep = test_endpoint("test", "http://localhost", WebhookTemplate::Generic);
        ep.events = vec!["*:usage_*".to_string()];
        assert!(ep.matches_detection(&test_detection()));
    }

    #[test]
    fn endpoint_non_matching_pattern() {
        let mut ep = test_endpoint("test", "http://localhost", WebhookTemplate::Generic);
        ep.events = vec!["gemini.*".to_string()];
        assert!(!ep.matches_detection(&test_detection()));
    }

    // ---- Dispatcher tests ----

    #[tokio::test]
    async fn dispatcher_sends_to_matching_endpoints() {
        let transport = MockTransport::success();
        let endpoints = vec![
            test_endpoint(
                "slack",
                "https://hooks.slack.com/test",
                WebhookTemplate::Slack,
            ),
            test_endpoint(
                "discord",
                "https://discord.com/api/webhooks/test",
                WebhookTemplate::Discord,
            ),
        ];
        let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));

        let records = dispatcher
            .dispatch(&test_detection(), 3, &test_rendered(), 0)
            .await;

        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.accepted));
        assert_eq!(transport.requests().len(), 2);
    }

    #[tokio::test]
    async fn dispatcher_skips_disabled_endpoints() {
        let transport = MockTransport::success();
        let mut ep = test_endpoint("disabled", "http://localhost", WebhookTemplate::Generic);
        ep.enabled = false;
        let dispatcher = WebhookDispatcher::new(vec![ep], Box::new(transport.clone()));

        let records = dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        assert!(records.is_empty());
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn dispatcher_skips_non_matching_endpoints() {
        let transport = MockTransport::success();
        let mut ep = test_endpoint("gemini-only", "http://localhost", WebhookTemplate::Generic);
        ep.events = vec!["gemini.*".to_string()];
        let dispatcher = WebhookDispatcher::new(vec![ep], Box::new(transport.clone()));

        let records = dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        assert!(records.is_empty());
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn dispatcher_records_failures() {
        let transport = MockTransport::failure(500, "Internal Server Error");
        let endpoints = vec![test_endpoint(
            "broken",
            "http://localhost",
            WebhookTemplate::Generic,
        )];
        let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport));

        let records = dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        assert_eq!(records.len(), 1);
        assert!(!records[0].accepted);
        assert_eq!(records[0].status_code, 500);
        assert!(records[0].error.is_some());
    }

    #[tokio::test]
    async fn dispatcher_sends_correct_template_per_endpoint() {
        let transport = MockTransport::success();
        let endpoints = vec![
            test_endpoint("generic", "http://a.com", WebhookTemplate::Generic),
            test_endpoint("slack", "http://b.com", WebhookTemplate::Slack),
        ];
        let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));

        dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        let reqs = transport.requests();
        assert_eq!(reqs.len(), 2);

        // Generic has flat fields
        assert!(reqs[0].body["event_type"].is_string());

        // Slack has blocks
        assert!(reqs[1].body["blocks"].is_array());
    }

    #[tokio::test]
    async fn dispatcher_passes_custom_headers() {
        let transport = MockTransport::success();
        let mut ep = test_endpoint("authed", "http://localhost", WebhookTemplate::Generic);
        ep.headers
            .insert("Authorization".to_string(), "Bearer tok123".to_string());
        let dispatcher = WebhookDispatcher::new(vec![ep], Box::new(transport.clone()));

        dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        let reqs = transport.requests();
        assert_eq!(
            reqs[0].headers.get("Authorization").unwrap(),
            "Bearer tok123"
        );
    }

    #[test]
    fn dispatcher_counts_endpoints() {
        let mut ep1 = test_endpoint("a", "http://a.com", WebhookTemplate::Generic);
        let ep2 = test_endpoint("b", "http://b.com", WebhookTemplate::Generic);
        ep1.enabled = false;
        let dispatcher = WebhookDispatcher::new(vec![ep1, ep2], Box::new(MockTransport::success()));
        assert_eq!(dispatcher.endpoint_count(), 2);
        assert_eq!(dispatcher.active_endpoint_count(), 1);
    }

    // ---- Config serialization tests ----

    #[test]
    fn endpoint_config_toml_roundtrip() {
        let toml_str = r#"
name = "slack"
url = "https://hooks.slack.com/services/XXX"
template = "slack"
events = ["*:usage_*", "*.error"]

[headers]
Authorization = "Bearer token"
"#;
        let ep: WebhookEndpointConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(ep.name, "slack");
        assert_eq!(ep.url, "https://hooks.slack.com/services/XXX");
        assert_eq!(ep.template, WebhookTemplate::Slack);
        assert_eq!(ep.events, vec!["*:usage_*", "*.error"]);
        assert!(ep.enabled);
        assert_eq!(ep.headers.get("Authorization").unwrap(), "Bearer token");
    }

    #[test]
    fn endpoint_config_defaults() {
        let toml_str = r#"
name = "minimal"
url = "http://localhost:8080/hook"
"#;
        let ep: WebhookEndpointConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(ep.template, WebhookTemplate::Generic);
        assert!(ep.events.is_empty());
        assert!(ep.headers.is_empty());
        assert!(ep.enabled);
    }

    #[test]
    fn webhook_template_display() {
        assert_eq!(format!("{}", WebhookTemplate::Generic), "generic");
        assert_eq!(format!("{}", WebhookTemplate::Slack), "slack");
        assert_eq!(format!("{}", WebhookTemplate::Discord), "discord");
    }

    #[test]
    fn delivery_result_constructors() {
        let ok = DeliveryResult::ok(200);
        assert!(ok.accepted);
        assert_eq!(ok.status_code, 200);
        assert!(ok.error.is_none());

        let err = DeliveryResult::err(503, "Service Unavailable");
        assert!(!err.accepted);
        assert_eq!(err.status_code, 503);
        assert!(err.error.unwrap().contains("Service Unavailable"));
    }

    #[test]
    fn payload_serializes_to_json() {
        let d = test_detection();
        let r = test_rendered();
        let p = WebhookPayload::from_detection(&d, 1, &r, 0);
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("core.codex:usage_reached"));
        assert!(json.contains("\"pane_id\":1"));
    }

    // ========================================================================
    // Pipeline integration tests (wa-psm.4)
    //
    // These test the full notification pipeline:
    //   Detection → NotificationGate → WebhookDispatcher
    // ========================================================================

    #[tokio::test]
    async fn pipeline_gate_filters_before_dispatch() {
        use crate::events::{EventFilter, NotificationGate, NotifyDecision};

        let filter = EventFilter::from_config(
            &[],
            &["*:usage_*".to_string()], // exclude usage events
            None,
            &[],
        );
        let mut gate = NotificationGate::from_config(
            filter,
            std::time::Duration::from_secs(300),
            std::time::Duration::from_secs(30),
        );

        let d = test_detection(); // core.codex:usage_reached — should be excluded
        let decision = gate.should_notify(&d, 3, None);
        assert_eq!(decision, NotifyDecision::Filtered);

        // Since it's filtered, dispatcher should not be called
        let transport = MockTransport::success();
        let endpoints = vec![test_endpoint(
            "slack",
            "http://slack.test",
            WebhookTemplate::Slack,
        )];
        let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));

        // Only dispatch if gate says Send
        if matches!(decision, NotifyDecision::Send { .. }) {
            dispatcher.dispatch(&d, 3, &test_rendered(), 0).await;
        }
        // Verify no requests were made
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn pipeline_gate_allows_and_dispatches() {
        use crate::events::{EventFilter, NotificationGate, NotifyDecision};

        let filter = EventFilter::from_config(
            &["*:usage_*".to_string()], // include usage events
            &[],
            None,
            &[],
        );
        let mut gate = NotificationGate::from_config(
            filter,
            std::time::Duration::from_secs(300),
            std::time::Duration::from_secs(30),
        );

        let d = test_detection();
        let decision = gate.should_notify(&d, 3, None);

        let suppressed = match decision {
            NotifyDecision::Send {
                suppressed_since_last,
            } => suppressed_since_last,
            other => panic!("Expected Send, got {other:?}"),
        };

        let transport = MockTransport::success();
        let endpoints = vec![
            test_endpoint("slack", "http://slack.test", WebhookTemplate::Slack),
            test_endpoint("discord", "http://discord.test", WebhookTemplate::Discord),
        ];
        let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));

        let records = dispatcher
            .dispatch(&d, 3, &test_rendered(), suppressed)
            .await;

        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.accepted));

        let reqs = transport.requests();
        assert_eq!(reqs.len(), 2);
        // First request: Slack (has blocks)
        assert!(reqs[0].body["blocks"].is_array());
        // Second request: Discord (has embeds)
        assert!(reqs[1].body["embeds"].is_array());
    }

    #[tokio::test]
    async fn pipeline_dedup_prevents_second_dispatch() {
        use crate::events::{EventFilter, NotificationGate, NotifyDecision};

        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            std::time::Duration::from_secs(300), // 5min dedup window
            std::time::Duration::from_secs(30),
        );

        let d = test_detection();
        let transport = MockTransport::success();
        let endpoints = vec![test_endpoint(
            "hook",
            "http://test.hook",
            WebhookTemplate::Generic,
        )];
        let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));

        // First event — should pass through
        let d1 = gate.should_notify(&d, 3, None);
        assert!(matches!(d1, NotifyDecision::Send { .. }));
        if let NotifyDecision::Send {
            suppressed_since_last,
        } = d1
        {
            dispatcher
                .dispatch(&d, 3, &test_rendered(), suppressed_since_last)
                .await;
        }
        assert_eq!(transport.requests().len(), 1);

        // Second identical event — should be deduplicated
        let d2 = gate.should_notify(&d, 3, None);
        assert!(matches!(d2, NotifyDecision::Deduplicated { .. }));
        // No dispatch for deduplicated events
    }

    #[tokio::test]
    async fn pipeline_severity_filter_blocks_info_events() {
        use crate::events::{EventFilter, NotificationGate, NotifyDecision};

        let filter = EventFilter::from_config(
            &[],
            &[],
            Some("warning"), // only warning+
            &[],
        );
        let mut gate = NotificationGate::from_config(
            filter,
            std::time::Duration::from_secs(300),
            std::time::Duration::from_secs(30),
        );

        let mut info_event = test_detection();
        info_event.severity = Severity::Info;
        assert_eq!(
            gate.should_notify(&info_event, 1, None),
            NotifyDecision::Filtered
        );

        // Warning should pass
        let warning_event = test_detection(); // already Warning
        assert!(matches!(
            gate.should_notify(&warning_event, 1, None),
            NotifyDecision::Send { .. }
        ));
    }

    #[tokio::test]
    async fn pipeline_per_endpoint_event_filter() {
        let transport = MockTransport::success();
        let mut codex_only =
            test_endpoint("codex-hook", "http://codex.test", WebhookTemplate::Generic);
        codex_only.events = vec!["core.codex:*".to_string()];

        let mut gemini_only = test_endpoint(
            "gemini-hook",
            "http://gemini.test",
            WebhookTemplate::Generic,
        );
        gemini_only.events = vec!["core.gemini:*".to_string()];

        let dispatcher =
            WebhookDispatcher::new(vec![codex_only, gemini_only], Box::new(transport.clone()));

        // Codex event → only codex-hook receives it
        let records = dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].target, "codex-hook");
        assert_eq!(transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn pipeline_mixed_success_and_failure() {
        // Two transports with different responses — simulate via
        // a single dispatcher with the same transport returning failure
        let transport = MockTransport::failure(500, "Internal Server Error");
        let endpoints = vec![
            test_endpoint("failing1", "http://fail1.test", WebhookTemplate::Generic),
            test_endpoint("failing2", "http://fail2.test", WebhookTemplate::Slack),
        ];
        let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport));

        let records = dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| !r.accepted));
        assert!(records.iter().all(|r| r.status_code == 500));
        assert!(records.iter().all(|r| r.error.is_some()));
    }

    #[test]
    fn pipeline_config_to_dispatcher() {
        // Verify that NotificationConfig can produce working components
        let nc = crate::config::NotificationConfig {
            enabled: true,
            notify_only: false,
            cooldown_ms: 1000,
            dedup_window_ms: 5000,
            include: vec!["codex.*".to_string()],
            exclude: Vec::new(),
            min_severity: Some("warning".to_string()),
            agent_types: Vec::new(),
            webhooks: vec![WebhookEndpointConfig {
                name: "test".to_string(),
                url: "http://test.hook".to_string(),
                template: WebhookTemplate::Slack,
                events: Vec::new(),
                headers: HashMap::new(),
                enabled: true,
            }],
            desktop: crate::desktop_notify::DesktopNotifyConfig::default(),
            email: crate::email_notify::EmailNotifyConfig::default(),
        };

        // Build gate from config
        let mut gate = nc.to_notification_gate();
        let filter = nc.to_event_filter();
        assert!(!filter.is_permissive());

        // Build dispatcher from config endpoints
        let dispatcher =
            WebhookDispatcher::new(nc.webhooks.clone(), Box::new(MockTransport::success()));
        assert_eq!(dispatcher.endpoint_count(), 1);
        assert_eq!(dispatcher.active_endpoint_count(), 1);

        // Gate should filter info events
        let mut info = test_detection();
        info.severity = Severity::Info;
        assert_eq!(
            gate.should_notify(&info, 1, None),
            crate::events::NotifyDecision::Filtered,
        );
    }
}
