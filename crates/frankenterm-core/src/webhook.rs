//! Webhook notification delivery.
//!
//! Delivers event notifications to external services via HTTP webhooks
//! with configurable payload templates. Delivery is single-attempt and
//! best-effort: each matched endpoint is sent exactly once per event, with
//! no retry, no exponential backoff, and no circuit-breaker isolation. A
//! failed POST (5xx / connection refused) is recorded once as not-accepted
//! and is not re-attempted. (ft-37q59: corrected from an earlier doc that
//! advertised circuit-breaker protection and retry/backoff; the crate's
//! `retry.rs` / `circuit_breaker.rs` are not wired into this path.)
//!
//! # Architecture
//!
//! ```text
//! Detection → NotificationGate (filter/dedup/cooldown)
//!                    ↓ (if Send)
//!            WebhookDispatcher
//!            ├── render payload (generic/slack/discord)
//!            └── send via WebhookTransport (single-shot, no retry)
//! ```
//!
//! # Transport Abstraction
//!
//! The actual HTTP POST is behind a [`WebhookTransport`] trait so that
//! frankenterm-core stays free of HTTP client dependencies. The CLI crate (or
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookTemplate {
    /// Generic JSON payload.
    #[default]
    Generic,
    /// Slack-compatible payload (Block Kit).
    Slack,
    /// Discord-compatible payload (embeds).
    Discord,
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
#[derive(Clone, Serialize, Deserialize)]
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

impl fmt::Debug for WebhookEndpointConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookEndpointConfig")
            .field("name", &self.name)
            .field("url_scheme", &webhook_url_scheme(&self.url))
            .field("template", &self.template)
            .field("event_pattern_count", &self.events.len())
            .field("header_count", &self.headers.len())
            .field("enabled", &self.enabled)
            .finish()
    }
}

/// Return a finite, non-secret URL-scheme class for diagnostics.
///
/// Webhook paths and queries commonly contain bearer credentials. Even the
/// authority can contain user-info, so logs and Debug output deliberately
/// expose no caller-controlled URL component. Prefix inspection is bounded so
/// an enormous custom scheme cannot create proportional diagnostic work.
fn webhook_url_scheme(url: &str) -> &'static str {
    if url
        .get(.."https://".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        "https"
    } else if url
        .get(.."http://".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
    {
        "http"
    } else {
        "unknown"
    }
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

const WEBHOOK_TITLE_PREFIX: &str = "FrankenTerm";

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
    // br-ft-zkthg MEDIUM-tier: previously
    // `serde_json::to_value(p).unwrap_or_default()` substituted
    // `Value::Null` into the rendered webhook on serialization
    // failure — silently sending `null` to the remote receiver
    // (Slack / Discord / generic). `WebhookPayload`
    // (= `NotificationPayload`) is composed of primitive
    // String / u64 / f64 / Option<String> / `#[serde(default)]`
    // collection fields, so `serde_json::to_value` on it is
    // infallible — the unwrap was dead code masking the contract.
    // `expect` documents the contract and panics loudly if a
    // future refactor adds a fallible-Serialize field, surfacing
    // a webhook integrator's diagnostic instead of a silent null
    // payload to the remote receiver.
    serde_json::to_value(p).expect(
        "NotificationPayload::serialize is infallible — typed struct over primitives; \
         if this fires, a fallible-Serialize field was added (br-ft-zkthg)",
    )
}

fn render_slack(p: &WebhookPayload) -> serde_json::Value {
    let severity_emoji = match p.severity.as_str() {
        "critical" => ":red_circle:",
        "warning" => ":large_yellow_circle:",
        _ => ":large_blue_circle:",
    };

    let mut text = format!("{severity_emoji} *{WEBHOOK_TITLE_PREFIX}: {}*", p.summary);
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

    let mut title = format!("{WEBHOOK_TITLE_PREFIX}: {}", p.summary);
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

/// Finite failure classes returned by a webhook transport.
///
/// The transport boundary deliberately carries no free-text error detail:
/// HTTP client errors frequently embed the credential-bearing webhook URL,
/// response bodies may contain secrets, and cancellation reasons are caller
/// controlled. Operators get a stable class; raw detail is discarded at this
/// boundary rather than retained in the result or dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryFailureClass {
    /// Explicit caller or parent cancellation.
    Cancelled,
    /// The bounded cleanup period after cancellation was exhausted.
    CancellationCleanupTimeout,
    /// Wall/virtual-time deadline or timeout exhaustion.
    DeadlineExceeded,
    /// Cooperative poll quota exhaustion.
    PollBudgetExhausted,
    /// Cost quota exhaustion.
    CostBudgetExhausted,
    /// A pre-I/O capability-checkpoint or local JSON-serialization failure.
    ///
    /// This is a fanout-stopping control failure. URL parsing, request-client
    /// construction, connection, and protocol failures belong to `Transport`.
    Context,
    /// Connection, protocol, or other transport failure before an HTTP result.
    Transport,
    /// A syntactically valid non-2xx HTTP response.
    RemoteRejected,
    /// A transport supplied an impossible or invalid HTTP result.
    InvalidResult,
}

impl DeliveryFailureClass {
    /// Stable finite diagnostic code persisted in notification records.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Cancelled => WEBHOOK_CANCELLATION_ERROR,
            Self::CancellationCleanupTimeout => WEBHOOK_CANCELLATION_CLEANUP_TIMEOUT_ERROR,
            Self::DeadlineExceeded => WEBHOOK_DEADLINE_ERROR,
            Self::PollBudgetExhausted => WEBHOOK_POLL_BUDGET_ERROR,
            Self::CostBudgetExhausted => WEBHOOK_COST_BUDGET_ERROR,
            Self::Context => WEBHOOK_CONTEXT_ERROR,
            Self::Transport => WEBHOOK_TRANSPORT_ERROR,
            Self::RemoteRejected => WEBHOOK_REMOTE_REJECTION,
            Self::InvalidResult => WEBHOOK_INVALID_TRANSPORT_RESULT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryOutcome {
    Accepted,
    Failed(DeliveryFailureClass),
}

/// Result of a webhook delivery attempt.
///
/// Fields are private so an accepted non-2xx response, a failure carrying
/// secret free text, or a control failure paired with an HTTP status cannot be
/// constructed accidentally. Use the finite smart constructors below.
#[derive(Clone, PartialEq, Eq)]
pub struct DeliveryResult {
    status_code: u16,
    outcome: DeliveryOutcome,
}

impl fmt::Debug for DeliveryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeliveryResult")
            .field("status_code", &self.status_code)
            .field("accepted", &self.accepted())
            .field("failure_class", &self.failure_class())
            .finish()
    }
}

impl DeliveryResult {
    /// Classify a completed HTTP response.
    ///
    /// Only status codes in the HTTP range 100..=599 are accepted as valid
    /// responses. A 2xx code is accepted, another valid HTTP code is a remote
    /// rejection, zero is the conventional no-response transport failure, and
    /// every other value is an invalid transport result.
    #[must_use]
    pub const fn from_http_status(status_code: u16) -> Self {
        let outcome = if status_code >= 200 && status_code < 300 {
            DeliveryOutcome::Accepted
        } else if status_code == 0 {
            DeliveryOutcome::Failed(DeliveryFailureClass::Transport)
        } else if status_code >= 100 && status_code < 600 {
            DeliveryOutcome::Failed(DeliveryFailureClass::RemoteRejected)
        } else {
            DeliveryOutcome::Failed(DeliveryFailureClass::InvalidResult)
        };
        Self {
            status_code,
            outcome,
        }
    }

    const fn without_http_response(class: DeliveryFailureClass) -> Self {
        Self {
            status_code: 0,
            outcome: DeliveryOutcome::Failed(class),
        }
    }

    /// Create an explicit-cancellation result.
    #[must_use]
    pub const fn cancelled() -> Self {
        Self::without_http_response(DeliveryFailureClass::Cancelled)
    }

    /// Create a cancellation-cleanup-timeout result.
    #[must_use]
    pub const fn cancellation_cleanup_timeout() -> Self {
        Self::without_http_response(DeliveryFailureClass::CancellationCleanupTimeout)
    }

    /// Create a deadline/timeout-exhaustion result.
    #[must_use]
    pub const fn deadline_exceeded() -> Self {
        Self::without_http_response(DeliveryFailureClass::DeadlineExceeded)
    }

    /// Create a poll-quota exhaustion result.
    #[must_use]
    pub const fn poll_budget_exhausted() -> Self {
        Self::without_http_response(DeliveryFailureClass::PollBudgetExhausted)
    }

    /// Create a cost-quota exhaustion result.
    #[must_use]
    pub const fn cost_budget_exhausted() -> Self {
        Self::without_http_response(DeliveryFailureClass::CostBudgetExhausted)
    }

    /// Create a fanout-stopping capability or payload-serialization failure.
    #[must_use]
    pub const fn context_failure() -> Self {
        Self::without_http_response(DeliveryFailureClass::Context)
    }

    /// Create a transport failure with no HTTP response.
    #[must_use]
    pub const fn transport_failure() -> Self {
        Self::without_http_response(DeliveryFailureClass::Transport)
    }

    /// HTTP status code, or zero when no valid HTTP response exists.
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        self.status_code
    }

    /// Whether a valid 2xx response accepted the webhook.
    #[must_use]
    pub const fn accepted(&self) -> bool {
        matches!(self.outcome, DeliveryOutcome::Accepted)
    }

    /// Finite failure class, or `None` for an accepted response.
    #[must_use]
    pub const fn failure_class(&self) -> Option<DeliveryFailureClass> {
        match self.outcome {
            DeliveryOutcome::Accepted => None,
            DeliveryOutcome::Failed(class) => Some(class),
        }
    }
}

/// Trait for the HTTP transport layer.
///
/// Implementations provide the actual HTTP POST. This decouples frankenterm-core
/// from any specific HTTP client library.
pub trait WebhookTransport: Send + Sync {
    /// Send a JSON payload under the caller's capability context.
    ///
    /// This is the required transport primitive. Implementations must thread
    /// this exact Cx into request construction and I/O; they must not replace it
    /// with ambient lookup or a newly minted context.
    fn send_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
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
/// Combines endpoint matching, template rendering, and single-attempt
/// best-effort delivery (no retry/backoff or circuit breaker — see the
/// module docs).
pub struct WebhookDispatcher {
    endpoints: Vec<WebhookEndpointConfig>,
    transport: Box<dyn WebhookTransport>,
}

/// Record of a single delivery attempt for observability.
pub type DeliveryRecord = NotificationDeliveryRecord;

const WEBHOOK_DISPATCH_TARGET: &str = "webhook_dispatch";
const WEBHOOK_CANCELLATION_ERROR: &str = "webhook_dispatch_cancelled";
const WEBHOOK_CANCELLATION_CLEANUP_TIMEOUT_ERROR: &str =
    "webhook_dispatch_cancellation_cleanup_timeout";
const WEBHOOK_DEADLINE_ERROR: &str = "webhook_dispatch_deadline_exceeded";
const WEBHOOK_POLL_BUDGET_ERROR: &str = "webhook_dispatch_poll_budget_exhausted";
const WEBHOOK_COST_BUDGET_ERROR: &str = "webhook_dispatch_cost_budget_exhausted";
const WEBHOOK_CONTEXT_ERROR: &str = "webhook_dispatch_context_error";
const WEBHOOK_TRANSPORT_ERROR: &str = "webhook_transport_error";
const WEBHOOK_REMOTE_REJECTION: &str = "webhook_remote_rejected";
const WEBHOOK_INVALID_TRANSPORT_RESULT: &str = "webhook_invalid_transport_result";

fn delivery_was_accepted(result: &DeliveryResult) -> bool {
    result.accepted()
}

fn safe_delivery_error(result: &DeliveryResult) -> Option<&'static str> {
    result.failure_class().map(DeliveryFailureClass::code)
}

fn checkpoint_error_class(
    cx: &crate::cx::Cx,
    error: &crate::runtime_async::ContextError,
) -> DeliveryFailureClass {
    use crate::runtime_async::ContextErrorKind;

    match error.kind() {
        ContextErrorKind::DeadlineExceeded => DeliveryFailureClass::DeadlineExceeded,
        ContextErrorKind::CancelTimeout => DeliveryFailureClass::CancellationCleanupTimeout,
        ContextErrorKind::PollQuotaExhausted => DeliveryFailureClass::PollBudgetExhausted,
        ContextErrorKind::CostQuotaExhausted => DeliveryFailureClass::CostBudgetExhausted,
        ContextErrorKind::Cancelled => {
            match cx.cancel_reason().map(|reason| reason.root_cause().kind) {
                Some(
                    crate::outcome::CancelKind::Timeout | crate::outcome::CancelKind::Deadline,
                ) => DeliveryFailureClass::DeadlineExceeded,
                Some(crate::outcome::CancelKind::PollQuota) => {
                    DeliveryFailureClass::PollBudgetExhausted
                }
                Some(crate::outcome::CancelKind::CostBudget) => {
                    DeliveryFailureClass::CostBudgetExhausted
                }
                _ => DeliveryFailureClass::Cancelled,
            }
        }
        _ => DeliveryFailureClass::Context,
    }
}

fn is_dispatch_control_error(error: &str) -> bool {
    matches!(
        error,
        WEBHOOK_CANCELLATION_ERROR
            | WEBHOOK_CANCELLATION_CLEANUP_TIMEOUT_ERROR
            | WEBHOOK_DEADLINE_ERROR
            | WEBHOOK_POLL_BUDGET_ERROR
            | WEBHOOK_COST_BUDGET_ERROR
            | WEBHOOK_CONTEXT_ERROR
    )
}

fn checkpoint_dispatch(
    cx: &crate::cx::Cx,
    target: impl Into<String>,
) -> std::result::Result<(), DeliveryRecord> {
    cx.checkpoint().map_err(|error| DeliveryRecord {
        target: target.into(),
        accepted: false,
        status_code: 0,
        // Checkpoint diagnostics can include caller-controlled cancellation
        // reasons. Persist only the typed root-cause class, never raw detail.
        error: Some(checkpoint_error_class(cx, &error).code().to_string()),
    })
}

fn is_transport_control_failure(result: &DeliveryResult) -> bool {
    matches!(
        result.failure_class(),
        Some(
            DeliveryFailureClass::Cancelled
                | DeliveryFailureClass::CancellationCleanupTimeout
                | DeliveryFailureClass::DeadlineExceeded
                | DeliveryFailureClass::PollBudgetExhausted
                | DeliveryFailureClass::CostBudgetExhausted
                | DeliveryFailureClass::Context
        )
    )
}

fn notification_delivery_error(success: bool, records: &[DeliveryRecord]) -> Option<String> {
    if success {
        return None;
    }

    let control_error = records
        .iter()
        .filter_map(|record| record.error.as_deref())
        .find(|error| is_dispatch_control_error(error));
    let error = control_error
        .or_else(|| records.iter().find_map(|record| record.error.as_deref()))
        .unwrap_or("one_or_more_deliveries_failed");
    Some(error.to_owned())
}

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
    /// Returns a record for each endpoint that was attempted. If cancellation
    /// prevents or stops fanout, the final failed record targets
    /// `webhook_dispatch` and carries a finite cancellation, deadline, budget,
    /// or context-error class.
    pub async fn dispatch(
        &self,
        detection: &Detection,
        pane_id: u64,
        rendered: &RenderedEvent,
        suppressed_since_last: u64,
    ) -> Vec<DeliveryRecord> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.dispatch_with_cx(&cx, detection, pane_id, rendered, suppressed_since_last)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`dispatch`].
    ///
    /// Checkpoints before payload construction, then routes through
    /// [`dispatch_payload_with_cx`] so caller Cx propagates through each
    /// endpoint's required `transport.send_with_cx` call. Caller cancellation
    /// cuts fanout before another endpoint is attempted.
    pub async fn dispatch_with_cx(
        &self,
        cx: &crate::cx::Cx,
        detection: &Detection,
        pane_id: u64,
        rendered: &RenderedEvent,
        suppressed_since_last: u64,
    ) -> Vec<DeliveryRecord> {
        if let Err(record) = checkpoint_dispatch(cx, WEBHOOK_DISPATCH_TARGET) {
            return vec![record];
        }

        let payload =
            WebhookPayload::from_detection(detection, pane_id, rendered, suppressed_since_last);
        self.dispatch_payload_with_cx(cx, &payload).await
    }

    /// Dispatch a pre-built payload to all matching endpoints.
    pub async fn dispatch_payload(&self, payload: &NotificationPayload) -> Vec<DeliveryRecord> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.dispatch_payload_with_cx(&cx, payload).await
    }

    /// Cx-first `dispatch_payload` (ft-xbnl0.2.3). Checkpoints before any
    /// rendering, logging, or transport side effect and between matching
    /// endpoint attempts. The required Cx-first transport primitive can
    /// interrupt the current request, and cancellation stops all remaining
    /// delivery. Cancellation observed after the final matching request has
    /// completed does not retroactively turn completed delivery into failure.
    pub async fn dispatch_payload_with_cx(
        &self,
        cx: &crate::cx::Cx,
        payload: &NotificationPayload,
    ) -> Vec<DeliveryRecord> {
        let mut records = Vec::new();

        if let Err(record) = checkpoint_dispatch(cx, WEBHOOK_DISPATCH_TARGET) {
            records.push(record);
            return records;
        }

        // Keep only an integer cursor across await points. Holding a filtered
        // iterator there needlessly retains a predicate closure whose `&&T`
        // lifetime is harder for the async state machine to prove, and a
        // repeated full-tail search would make large endpoint sets quadratic.
        // Each successive `position` starts after the current match, so every
        // configured endpoint is inspected at most once.
        let mut next_endpoint_index = self.endpoints.iter().position(|endpoint| {
            endpoint.enabled && endpoint.matches_event_type(&payload.event_type)
        });

        while let Some(endpoint_index) = next_endpoint_index {
            let following_endpoint_index = self.endpoints[endpoint_index + 1..]
                .iter()
                .position(|endpoint| {
                    endpoint.enabled && endpoint.matches_event_type(&payload.event_type)
                })
                .map(|relative_index| endpoint_index + 1 + relative_index);
            let endpoint = &self.endpoints[endpoint_index];

            if let Err(record) = checkpoint_dispatch(cx, WEBHOOK_DISPATCH_TARGET) {
                records.push(record);
                return records;
            }

            let body = render_template(endpoint.template, payload);

            // Rendering is pure but can be non-trivial. Re-check immediately
            // before the first observable side effect so cancellation that
            // arrived during rendering cannot leak into logging or delivery.
            if let Err(record) = checkpoint_dispatch(cx, WEBHOOK_DISPATCH_TARGET) {
                records.push(record);
                return records;
            }

            tracing::debug!(
                endpoint = %endpoint.name,
                url_scheme = webhook_url_scheme(&endpoint.url),
                template = %endpoint.template,
                rule_id = %payload.event_type,
                "dispatching webhook"
            );

            if let Err(record) = checkpoint_dispatch(cx, WEBHOOK_DISPATCH_TARGET) {
                records.push(record);
                return records;
            }

            let result = self
                .transport
                .send_with_cx(cx, &endpoint.url, &endpoint.headers, &body)
                .await;
            let accepted = delivery_was_accepted(&result);
            let safe_error = safe_delivery_error(&result);

            if accepted {
                tracing::info!(
                    endpoint = %endpoint.name,
                    status = result.status_code(),
                    "webhook delivered"
                );
            } else {
                tracing::warn!(
                    endpoint = %endpoint.name,
                    status = result.status_code(),
                    error_class = ?safe_error,
                    "webhook delivery failed"
                );
            }

            records.push(DeliveryRecord {
                target: endpoint.name.clone(),
                accepted,
                status_code: result.status_code(),
                error: safe_error.map(str::to_owned),
            });

            // A required Cx-aware transport can report the exact control
            // failure that ended its own request. That finite endpoint record
            // is authoritative: stop immediately instead of checkpointing and
            // appending a second, generic control record for the same event.
            if is_transport_control_failure(&result) {
                return records;
            }

            // Check again only when another matching endpoint remains. Once
            // the final intended side effect has completed, a concurrent late
            // cancellation must not create a false failure that could invite
            // duplicate delivery.
            if following_endpoint_index.is_some() {
                if let Err(record) = checkpoint_dispatch(cx, WEBHOOK_DISPATCH_TARGET) {
                    records.push(record);
                    return records;
                }
            }
            next_endpoint_index = following_endpoint_index;
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

    fn send_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        payload: &'a NotificationPayload,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            let records = self.dispatch_payload_with_cx(cx, payload).await;
            let success = records.iter().all(|r| r.accepted);
            let error = notification_delivery_error(success, &records);
            NotificationDelivery {
                sender: self.name().to_string(),
                success,
                rate_limited: false,
                error,
                records,
            }
        })
    }

    fn send<'a>(&'a self, payload: &'a NotificationPayload) -> NotificationFuture<'a> {
        Box::pin(async move {
            let records = self.dispatch_payload(payload).await;
            let success = records.iter().all(|r| r.accepted);
            let error = notification_delivery_error(success, &records);
            NotificationDelivery {
                sender: self.name().to_string(),
                success,
                rate_limited: false,
                error,
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

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build webhook test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    // ---- Mock transport ----

    #[derive(Clone)]
    struct MockTransport {
        /// Captured requests through the required Cx-first primitive.
        cx_requests: Arc<Mutex<Vec<MockRequest>>>,
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
            Self::responding(DeliveryResult::from_http_status(200))
        }

        fn responding(response: DeliveryResult) -> Self {
            Self {
                cx_requests: Arc::new(Mutex::new(Vec::new())),
                response,
            }
        }

        fn failure(status: u16) -> Self {
            Self::responding(DeliveryResult::from_http_status(status))
        }

        fn cx_requests(&self) -> Vec<MockRequest> {
            self.cx_requests.lock().unwrap().clone()
        }
    }

    impl WebhookTransport for MockTransport {
        fn send_with_cx<'a>(
            &'a self,
            _cx: &'a crate::cx::Cx,
            url: &'a str,
            headers: &'a HashMap<String, String>,
            body: &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = DeliveryResult> + Send + 'a>> {
            let req = MockRequest {
                url: url.to_string(),
                headers: headers.clone(),
                body: body.clone(),
            };
            self.cx_requests.lock().unwrap().push(req);
            let resp = self.response.clone();
            Box::pin(async move { resp })
        }
    }

    /// Cx-aware transport that can request cancellation during one delivery.
    #[derive(Clone)]
    struct CxTestTransport {
        requests: Arc<Mutex<Vec<MockRequest>>>,
        cancel_on_send: bool,
    }

    impl CxTestTransport {
        fn success() -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                cancel_on_send: false,
            }
        }

        fn cancelling() -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                cancel_on_send: true,
            }
        }

        fn requests(&self) -> Vec<MockRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl WebhookTransport for CxTestTransport {
        fn send_with_cx<'a>(
            &'a self,
            cx: &'a crate::cx::Cx,
            url: &'a str,
            headers: &'a HashMap<String, String>,
            body: &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = DeliveryResult> + Send + 'a>> {
            self.requests.lock().unwrap().push(MockRequest {
                url: url.to_string(),
                headers: headers.clone(),
                body: body.clone(),
            });
            if self.cancel_on_send {
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("Cx-first webhook transport mid-fanout test"),
                );
            }
            Box::pin(async { DeliveryResult::from_http_status(200) })
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
                "Run ft workflow",
                "ft workflow run handle_usage_limits --pane 3",
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

        assert!(json["text"].as_str().unwrap().contains("FrankenTerm:"));
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
        assert!(embed["title"].as_str().unwrap().contains("FrankenTerm:"));
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

    #[test]
    fn dispatcher_sends_to_matching_endpoints() {
        run_async_test(async {
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
            assert_eq!(transport.cx_requests().len(), 2);
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `WebhookDispatcher::dispatch_payload_with_cx`
    /// must invoke the required `transport.send_with_cx` primitive. This pins
    /// the Cx-forward contract at the dispatcher → transport boundary so the
    /// exact caller context reaches the underlying HTTP call.
    #[test]
    fn dispatch_payload_with_cx_invokes_transport_cx_path() {
        run_async_test(async {
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

            let payload =
                NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);
            let cx = crate::cx::for_request();
            let records = dispatcher.dispatch_payload_with_cx(&cx, &payload).await;

            assert_eq!(records.len(), 2, "both endpoints should be attempted");
            assert!(
                records.iter().all(|r| r.accepted),
                "mock transport reports success for both endpoints"
            );

            assert_eq!(
                transport.cx_requests().len(),
                2,
                "dispatch_payload_with_cx must invoke transport.send_with_cx for each endpoint"
            );
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `WebhookDispatcher::dispatch_with_cx`
    /// (the detection-entry sibling of `dispatch`) must route
    /// through `dispatch_payload_with_cx` so caller cx reaches the
    /// transport. Pins the same cx-forward contract as
    /// `dispatch_payload_with_cx_invokes_transport_cx_path` but
    /// via the higher-level detection surface used by pattern
    /// handlers.
    #[test]
    fn dispatch_with_cx_routes_through_cx_aware_transport() {
        run_async_test(async {
            let transport = MockTransport::success();
            let endpoints = vec![test_endpoint(
                "slack",
                "https://hooks.slack.com/test",
                WebhookTemplate::Slack,
            )];
            let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));

            let cx = crate::cx::for_request();
            let records = dispatcher
                .dispatch_with_cx(&cx, &test_detection(), 7, &test_rendered(), 0)
                .await;

            assert_eq!(records.len(), 1);
            assert!(records[0].accepted);
            assert_eq!(
                transport.cx_requests().len(),
                1,
                "dispatch_with_cx must route through the cx-aware transport path"
            );
        });
    }

    #[test]
    fn precancelled_cx_blocks_transport_and_surfaces_cancellation() {
        run_async_test(async {
            const SECRET: &str = "sentinel-private-webhook-cancellation-reason";
            let transport = CxTestTransport::success();
            let endpoints = vec![test_endpoint(
                "blocked",
                "https://example.invalid/blocked",
                WebhookTemplate::Generic,
            )];
            let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));
            let payload =
                NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);
            let cx = crate::cx::for_request();
            cx.cancel_with(crate::outcome::CancelKind::User, Some(SECRET));

            let delivery = dispatcher.send_with_cx(&cx, &payload).await;

            assert!(!delivery.success);
            assert_eq!(delivery.records.len(), 1);
            assert_eq!(delivery.records[0].target, WEBHOOK_DISPATCH_TARGET);
            assert!(!delivery.records[0].accepted);
            assert_eq!(
                delivery.records[0].error.as_deref(),
                Some(WEBHOOK_CANCELLATION_ERROR)
            );
            assert_eq!(
                delivery.error.as_deref(),
                Some(WEBHOOK_CANCELLATION_ERROR),
                "NotificationDelivery must surface a finite cancellation class"
            );
            assert!(!format!("{delivery:?}").contains(SECRET));
            assert!(
                transport.requests().is_empty(),
                "pre-cancelled dispatch must not enter the transport"
            );
        });
    }

    #[test]
    fn exhausted_poll_budget_is_not_misreported_as_user_cancellation() {
        run_async_test(async {
            let transport = CxTestTransport::success();
            let endpoint = test_endpoint(
                "budget-blocked",
                "https://example.invalid/blocked",
                WebhookTemplate::Generic,
            );
            let dispatcher = WebhookDispatcher::new(vec![endpoint], Box::new(transport.clone()));
            let payload =
                NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);
            let cx =
                crate::cx::Cx::for_request_with_budget(crate::cx::Budget::new().with_poll_quota(0));

            let delivery = dispatcher.send_with_cx(&cx, &payload).await;

            assert!(!delivery.success);
            assert_eq!(delivery.records.len(), 1);
            assert_eq!(delivery.records[0].target, WEBHOOK_DISPATCH_TARGET);
            assert_eq!(
                delivery.records[0].error.as_deref(),
                Some(WEBHOOK_POLL_BUDGET_ERROR)
            );
            assert_eq!(delivery.error.as_deref(), Some(WEBHOOK_POLL_BUDGET_ERROR));
            assert!(
                transport.requests().is_empty(),
                "an exhausted poll budget must stop before transport dispatch"
            );
        });
    }

    #[test]
    fn unknown_checkpoint_error_uses_finite_context_class() {
        let cx = crate::cx::for_request();
        let error = crate::runtime_async::ContextError::new(
            crate::runtime_async::ContextErrorKind::Internal,
        )
        .with_message("sentinel-private-context-detail");

        assert_eq!(
            checkpoint_error_class(&cx, &error),
            DeliveryFailureClass::Context
        );
    }

    #[test]
    fn cancellation_cleanup_timeout_is_not_misreported_as_request_deadline() {
        let cx = crate::cx::for_request();
        let error = crate::runtime_async::ContextError::new(
            crate::runtime_async::ContextErrorKind::CancelTimeout,
        )
        .with_message("sentinel-private-cancellation-cleanup-detail");

        assert_eq!(
            checkpoint_error_class(&cx, &error),
            DeliveryFailureClass::CancellationCleanupTimeout
        );
        assert_eq!(
            checkpoint_error_class(&cx, &error).code(),
            WEBHOOK_CANCELLATION_CLEANUP_TIMEOUT_ERROR
        );
    }

    #[test]
    fn cancelled_checkpoint_preserves_every_root_cancel_class_without_detail_leak() {
        use crate::outcome::CancelKind;

        const SECRET: &str = "sentinel-private-webhook-root-cancel-detail";
        let cases = [
            (CancelKind::Timeout, DeliveryFailureClass::DeadlineExceeded),
            (CancelKind::Deadline, DeliveryFailureClass::DeadlineExceeded),
            (
                CancelKind::PollQuota,
                DeliveryFailureClass::PollBudgetExhausted,
            ),
            (
                CancelKind::CostBudget,
                DeliveryFailureClass::CostBudgetExhausted,
            ),
            (CancelKind::User, DeliveryFailureClass::Cancelled),
            (CancelKind::FailFast, DeliveryFailureClass::Cancelled),
            (CancelKind::RaceLost, DeliveryFailureClass::Cancelled),
            (CancelKind::ParentCancelled, DeliveryFailureClass::Cancelled),
            (
                CancelKind::ResourceUnavailable,
                DeliveryFailureClass::Cancelled,
            ),
            (CancelKind::Shutdown, DeliveryFailureClass::Cancelled),
            (CancelKind::LinkedExit, DeliveryFailureClass::Cancelled),
        ];

        for (kind, expected) in cases {
            let cx = crate::cx::for_request();
            cx.cancel_with(kind, Some(SECRET));
            let error = crate::runtime_async::ContextError::new(
                crate::runtime_async::ContextErrorKind::Cancelled,
            )
            .with_message(SECRET);

            let actual = checkpoint_error_class(&cx, &error);
            assert_eq!(actual, expected);
            assert!(!actual.code().contains(SECRET));
        }
    }

    #[test]
    fn cancellation_during_transport_stops_later_endpoints() {
        run_async_test(async {
            let cx = crate::cx::for_request();
            let transport = CxTestTransport::cancelling();
            let endpoints = vec![
                test_endpoint(
                    "first",
                    "https://example.invalid/first",
                    WebhookTemplate::Generic,
                ),
                test_endpoint(
                    "must-not-run",
                    "https://example.invalid/second",
                    WebhookTemplate::Generic,
                ),
            ];
            let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));
            let payload =
                NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);

            let records = dispatcher.dispatch_payload_with_cx(&cx, &payload).await;

            assert_eq!(
                transport.requests().len(),
                1,
                "cancellation from the first Cx-aware request must stop later fanout"
            );
            assert_eq!(records.len(), 2);
            assert_eq!(records[0].target, "first");
            assert!(
                records[0].accepted,
                "the completed first attempt is retained"
            );
            assert_eq!(records[1].target, WEBHOOK_DISPATCH_TARGET);
            assert!(!records[1].accepted);
            assert_eq!(
                records[1].error.as_deref(),
                Some(WEBHOOK_CANCELLATION_ERROR)
            );
            assert!(records.iter().all(|record| record.target != "must-not-run"));
        });
    }

    #[test]
    fn typed_transport_control_failure_stops_fanout_without_duplicate_record() {
        run_async_test(async {
            let cases = [
                (DeliveryResult::cancelled(), DeliveryFailureClass::Cancelled),
                (
                    DeliveryResult::cancellation_cleanup_timeout(),
                    DeliveryFailureClass::CancellationCleanupTimeout,
                ),
                (
                    DeliveryResult::deadline_exceeded(),
                    DeliveryFailureClass::DeadlineExceeded,
                ),
                (
                    DeliveryResult::poll_budget_exhausted(),
                    DeliveryFailureClass::PollBudgetExhausted,
                ),
                (
                    DeliveryResult::cost_budget_exhausted(),
                    DeliveryFailureClass::CostBudgetExhausted,
                ),
                (
                    DeliveryResult::context_failure(),
                    DeliveryFailureClass::Context,
                ),
            ];

            for (result, expected_class) in cases {
                let transport = MockTransport::responding(result);
                let endpoints = vec![
                    test_endpoint(
                        "control-failed",
                        "https://example.invalid/first",
                        WebhookTemplate::Generic,
                    ),
                    test_endpoint(
                        "must-not-run",
                        "https://example.invalid/second",
                        WebhookTemplate::Generic,
                    ),
                ];
                let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));
                let payload =
                    NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);
                let cx = crate::cx::for_request();

                let records = dispatcher.dispatch_payload_with_cx(&cx, &payload).await;

                assert_eq!(
                    transport.cx_requests().len(),
                    1,
                    "typed {expected_class:?} must stop later endpoint fanout"
                );
                assert_eq!(
                    records.len(),
                    1,
                    "typed {expected_class:?} must not be followed by a duplicate checkpoint record"
                );
                assert_eq!(records[0].target, "control-failed");
                assert_eq!(records[0].error.as_deref(), Some(expected_class.code()));
            }
        });
    }

    #[test]
    fn typed_transport_control_failure_from_final_endpoint_is_retained_exactly() {
        run_async_test(async {
            let cases = [
                (DeliveryResult::cancelled(), DeliveryFailureClass::Cancelled),
                (
                    DeliveryResult::poll_budget_exhausted(),
                    DeliveryFailureClass::PollBudgetExhausted,
                ),
                (
                    DeliveryResult::cost_budget_exhausted(),
                    DeliveryFailureClass::CostBudgetExhausted,
                ),
            ];

            for (result, expected_class) in cases {
                let transport = MockTransport::responding(result);
                let endpoint = test_endpoint(
                    "only",
                    "https://example.invalid/only",
                    WebhookTemplate::Generic,
                );
                let dispatcher = WebhookDispatcher::new(vec![endpoint], Box::new(transport));
                let payload =
                    NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);
                let cx = crate::cx::for_request();

                let delivery = dispatcher.send_with_cx(&cx, &payload).await;

                assert!(!delivery.success);
                assert_eq!(delivery.records.len(), 1);
                assert_eq!(delivery.records[0].target, "only");
                assert_eq!(
                    delivery.records[0].error.as_deref(),
                    Some(expected_class.code())
                );
                assert_eq!(delivery.error.as_deref(), Some(expected_class.code()));
            }
        });
    }

    #[test]
    fn transport_failure_stays_finite_through_notification_delivery() {
        run_async_test(async {
            let transport = MockTransport::responding(DeliveryResult::transport_failure());
            let endpoint = test_endpoint(
                "inconsistent",
                "https://example.invalid/hook",
                WebhookTemplate::Generic,
            );
            let dispatcher = WebhookDispatcher::new(vec![endpoint], Box::new(transport));
            let payload =
                NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);
            let cx = crate::cx::for_request();

            let delivery = dispatcher.send_with_cx(&cx, &payload).await;

            assert!(!delivery.success);
            assert_eq!(delivery.records.len(), 1);
            assert!(!delivery.records[0].accepted);
            assert_eq!(delivery.records[0].status_code, 0);
            assert_eq!(
                delivery.records[0].error.as_deref(),
                Some(WEBHOOK_TRANSPORT_ERROR)
            );
            assert_eq!(delivery.error.as_deref(), Some(WEBHOOK_TRANSPORT_ERROR));
        });
    }

    #[test]
    fn invalid_http_status_uses_invalid_result_class() {
        run_async_test(async {
            let transport = MockTransport::responding(DeliveryResult::from_http_status(u16::MAX));
            let endpoint = test_endpoint(
                "inconsistent-two-xx",
                "https://example.invalid/hook",
                WebhookTemplate::Generic,
            );
            let dispatcher = WebhookDispatcher::new(vec![endpoint], Box::new(transport));
            let payload =
                NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);
            let cx = crate::cx::for_request();

            let delivery = dispatcher.send_with_cx(&cx, &payload).await;

            assert!(!delivery.success);
            assert!(!delivery.records[0].accepted);
            assert_eq!(delivery.records[0].status_code, u16::MAX);
            assert_eq!(
                delivery.records[0].error.as_deref(),
                Some(WEBHOOK_INVALID_TRANSPORT_RESULT)
            );
        });
    }

    #[test]
    fn late_cancellation_does_not_falsify_completed_final_delivery() {
        run_async_test(async {
            let cx = crate::cx::for_request();
            let transport = CxTestTransport::cancelling();
            let endpoints = vec![test_endpoint(
                "only",
                "https://example.invalid/only",
                WebhookTemplate::Generic,
            )];
            let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));
            let payload =
                NotificationPayload::from_detection(&test_detection(), 3, &test_rendered(), 0);

            let delivery = dispatcher.send_with_cx(&cx, &payload).await;

            assert_eq!(transport.requests().len(), 1);
            assert_eq!(delivery.records.len(), 1);
            assert_eq!(delivery.records[0].target, "only");
            assert!(delivery.records[0].accepted);
            assert!(
                delivery.success,
                "a cancellation observed only after the final accepted side effect must not create a false failure"
            );
            assert_eq!(delivery.error, None);
        });
    }

    #[test]
    fn dispatcher_skips_disabled_endpoints() {
        run_async_test(async {
            let transport = MockTransport::success();
            let mut ep = test_endpoint("disabled", "http://localhost", WebhookTemplate::Generic);
            ep.enabled = false;
            let dispatcher = WebhookDispatcher::new(vec![ep], Box::new(transport.clone()));

            let records = dispatcher
                .dispatch(&test_detection(), 1, &test_rendered(), 0)
                .await;

            assert!(records.is_empty());
            assert!(transport.cx_requests().is_empty());
        });
    }

    #[test]
    fn dispatcher_skips_non_matching_endpoints() {
        run_async_test(async {
            let transport = MockTransport::success();
            let mut ep = test_endpoint("gemini-only", "http://localhost", WebhookTemplate::Generic);
            ep.events = vec!["gemini.*".to_string()];
            let dispatcher = WebhookDispatcher::new(vec![ep], Box::new(transport.clone()));

            let records = dispatcher
                .dispatch(&test_detection(), 1, &test_rendered(), 0)
                .await;

            assert!(records.is_empty());
            assert!(transport.cx_requests().is_empty());
        });
    }

    #[test]
    fn dispatcher_records_failures() {
        run_async_test(async {
            let transport = MockTransport::failure(500);
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
            assert_eq!(records[0].error.as_deref(), Some(WEBHOOK_REMOTE_REJECTION));
        });
    }

    #[test]
    fn dispatcher_sends_correct_template_per_endpoint() {
        run_async_test(async {
            let transport = MockTransport::success();
            let endpoints = vec![
                test_endpoint("generic", "http://a.com", WebhookTemplate::Generic),
                test_endpoint("slack", "http://b.com", WebhookTemplate::Slack),
            ];
            let dispatcher = WebhookDispatcher::new(endpoints, Box::new(transport.clone()));

            dispatcher
                .dispatch(&test_detection(), 1, &test_rendered(), 0)
                .await;

            let reqs = transport.cx_requests();
            assert_eq!(reqs.len(), 2);

            // Generic has flat fields
            assert!(reqs[0].body["event_type"].is_string());

            // Slack has blocks
            assert!(reqs[1].body["blocks"].is_array());
        });
    }

    #[test]
    fn dispatcher_passes_custom_headers() {
        run_async_test(async {
            let transport = MockTransport::success();
            let mut ep = test_endpoint("authed", "http://localhost", WebhookTemplate::Generic);
            ep.headers
                .insert("Authorization".to_string(), "Bearer tok123".to_string());
            let dispatcher = WebhookDispatcher::new(vec![ep], Box::new(transport.clone()));

            dispatcher
                .dispatch(&test_detection(), 1, &test_rendered(), 0)
                .await;

            let reqs = transport.cx_requests();
            assert_eq!(
                reqs[0].headers.get("Authorization").unwrap(),
                "Bearer tok123"
            );
        });
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
        let ok = DeliveryResult::from_http_status(200);
        assert!(ok.accepted());
        assert_eq!(ok.status_code(), 200);
        assert_eq!(ok.failure_class(), None);

        let rejected = DeliveryResult::from_http_status(503);
        assert!(!rejected.accepted());
        assert_eq!(rejected.status_code(), 503);
        assert_eq!(
            rejected.failure_class(),
            Some(DeliveryFailureClass::RemoteRejected)
        );
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

    #[test]
    fn pipeline_gate_filters_before_dispatch() {
        run_async_test(async {
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
            assert!(transport.cx_requests().is_empty());
        });
    }

    #[test]
    fn pipeline_gate_allows_and_dispatches() {
        run_async_test(async {
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

            let reqs = transport.cx_requests();
            assert_eq!(reqs.len(), 2);
            // First request: Slack (has blocks)
            assert!(reqs[0].body["blocks"].is_array());
            // Second request: Discord (has embeds)
            assert!(reqs[1].body["embeds"].is_array());
        });
    }

    #[test]
    fn pipeline_dedup_prevents_second_dispatch() {
        run_async_test(async {
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
            assert_eq!(transport.cx_requests().len(), 1);

            // Second identical event — should be deduplicated
            let d2 = gate.should_notify(&d, 3, None);
            assert!(matches!(d2, NotifyDecision::Deduplicated { .. }));
            // No dispatch for deduplicated events
        });
    }

    #[test]
    fn pipeline_severity_filter_blocks_info_events() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn pipeline_per_endpoint_event_filter() {
        run_async_test(async {
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
            assert_eq!(transport.cx_requests().len(), 1);
        });
    }

    #[test]
    fn pipeline_mixed_success_and_failure() {
        run_async_test(async {
            // Two transports with different responses — simulate via
            // a single dispatcher with the same transport returning failure
            let transport = MockTransport::failure(500);
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
        });
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

    // ====================================================================
    // WebhookTemplate tests
    // ====================================================================

    #[test]
    fn webhook_template_default_is_generic() {
        assert_eq!(WebhookTemplate::default(), WebhookTemplate::Generic);
    }

    #[test]
    fn webhook_template_serde_json_roundtrip() {
        for t in [
            WebhookTemplate::Generic,
            WebhookTemplate::Slack,
            WebhookTemplate::Discord,
        ] {
            let json = serde_json::to_string(&t).unwrap();
            let back: WebhookTemplate = serde_json::from_str(&json).unwrap();
            assert_eq!(t, back);
        }
    }

    #[test]
    fn webhook_template_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&WebhookTemplate::Generic).unwrap(),
            "\"generic\""
        );
        assert_eq!(
            serde_json::to_string(&WebhookTemplate::Slack).unwrap(),
            "\"slack\""
        );
        assert_eq!(
            serde_json::to_string(&WebhookTemplate::Discord).unwrap(),
            "\"discord\""
        );
    }

    #[test]
    fn webhook_template_debug() {
        let dbg = format!("{:?}", WebhookTemplate::Slack);
        assert!(dbg.contains("Slack"));
    }

    #[test]
    fn webhook_template_copy() {
        let t = WebhookTemplate::Discord;
        let t2 = t;
        assert_eq!(t, t2); // Copy semantics
    }

    // ====================================================================
    // DeliveryResult additional tests
    // ====================================================================

    #[test]
    fn delivery_result_accepts_201() {
        let result = DeliveryResult::from_http_status(201);
        assert!(result.accepted());
        assert_eq!(result.status_code(), 201);
        assert_eq!(result.failure_class(), None);
    }

    #[test]
    fn delivery_result_zero_status_is_transport_failure() {
        let result = DeliveryResult::from_http_status(0);
        assert!(!result.accepted());
        assert_eq!(result.status_code(), 0);
        assert_eq!(
            result.failure_class(),
            Some(DeliveryFailureClass::Transport)
        );
    }

    #[test]
    fn delivery_result_debug_is_finite() {
        let result = DeliveryResult::deadline_exceeded();
        let dbg = format!("{result:?}");
        assert!(dbg.contains("DeliveryResult"));
        assert!(dbg.contains("DeadlineExceeded"));
        assert!(
            dbg.len() < 128,
            "finite debug output grew unexpectedly: {dbg}"
        );
    }

    #[test]
    fn delivery_result_clone() {
        let result = DeliveryResult::from_http_status(429);
        let cloned = result.clone();
        assert_eq!(cloned, result);
        assert_eq!(cloned.status_code(), 429);
        assert_eq!(
            cloned.failure_class(),
            Some(DeliveryFailureClass::RemoteRejected)
        );
    }

    #[test]
    fn delivery_result_control_failure_constructors_are_distinct() {
        let cases = [
            (DeliveryResult::cancelled(), DeliveryFailureClass::Cancelled),
            (
                DeliveryResult::cancellation_cleanup_timeout(),
                DeliveryFailureClass::CancellationCleanupTimeout,
            ),
            (
                DeliveryResult::deadline_exceeded(),
                DeliveryFailureClass::DeadlineExceeded,
            ),
            (
                DeliveryResult::poll_budget_exhausted(),
                DeliveryFailureClass::PollBudgetExhausted,
            ),
            (
                DeliveryResult::cost_budget_exhausted(),
                DeliveryFailureClass::CostBudgetExhausted,
            ),
            (
                DeliveryResult::context_failure(),
                DeliveryFailureClass::Context,
            ),
            (
                DeliveryResult::transport_failure(),
                DeliveryFailureClass::Transport,
            ),
        ];

        for (result, expected) in cases {
            assert!(!result.accepted());
            assert_eq!(result.status_code(), 0);
            assert_eq!(result.failure_class(), Some(expected));
        }
    }

    #[test]
    fn delivery_result_rejects_non_http_status_values() {
        for status in [1_u16, 99, 600, u16::MAX] {
            let result = DeliveryResult::from_http_status(status);
            assert!(!result.accepted());
            assert_eq!(result.status_code(), status);
            assert_eq!(
                result.failure_class(),
                Some(DeliveryFailureClass::InvalidResult)
            );
        }
    }

    // ====================================================================
    // render_template pure tests (using direct NotificationPayload)
    // ====================================================================

    fn make_payload(
        severity: &str,
        quick_fix: Option<&str>,
        suppressed: u64,
    ) -> NotificationPayload {
        NotificationPayload {
            event_type: "test.rule:event".to_string(),
            pane_id: 42,
            timestamp: "2026-02-14T00:00:00Z".to_string(),
            summary: "Test summary".to_string(),
            description: "Test description".to_string(),
            severity: severity.to_string(),
            agent_type: "codex".to_string(),
            confidence: 0.85,
            quick_fix: quick_fix.map(|s| s.to_string()),
            suppressed_since_last: suppressed,
        }
    }

    #[test]
    fn render_slack_critical_emoji() {
        let p = make_payload("critical", None, 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let text = json["text"].as_str().unwrap();
        assert!(text.contains(":red_circle:"));
    }

    #[test]
    fn render_slack_info_emoji() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let text = json["text"].as_str().unwrap();
        assert!(text.contains(":large_blue_circle:"));
    }

    #[test]
    fn render_slack_warning_emoji() {
        let p = make_payload("warning", None, 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let text = json["text"].as_str().unwrap();
        assert!(text.contains(":large_yellow_circle:"));
    }

    #[test]
    fn render_slack_unknown_severity_uses_blue() {
        let p = make_payload("trace", None, 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let text = json["text"].as_str().unwrap();
        assert!(text.contains(":large_blue_circle:"));
    }

    #[test]
    fn render_slack_with_quick_fix() {
        let p = make_payload("warning", Some("ft restart --pane 42"), 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let blocks = json["blocks"].as_array().unwrap();
        let fields_block = &blocks[1];
        let fields_json = serde_json::to_string(fields_block).unwrap();
        assert!(fields_json.contains("Quick fix"));
        assert!(fields_json.contains("ft restart --pane 42"));
    }

    #[test]
    fn render_slack_without_quick_fix_has_3_fields() {
        let p = make_payload("warning", None, 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let blocks = json["blocks"].as_array().unwrap();
        let fields_block = &blocks[1];
        let fields = fields_block["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 3); // Pane, Severity, Agent
    }

    #[test]
    fn render_slack_with_quick_fix_has_4_fields() {
        let p = make_payload("warning", Some("cmd"), 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let blocks = json["blocks"].as_array().unwrap();
        let fields_block = &blocks[1];
        let fields = fields_block["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 4); // Pane, Severity, Agent, Quick fix
    }

    #[test]
    fn render_slack_has_context_block() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let blocks = json["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 3); // section, section(fields), context
        assert_eq!(blocks[2]["type"], "context");
    }

    #[test]
    fn render_slack_context_has_timestamp() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let ctx = &json["blocks"][2]["elements"][0]["text"];
        assert!(ctx.as_str().unwrap().contains("2026-02-14T00:00:00Z"));
    }

    #[test]
    fn render_slack_suppressed_count_in_text() {
        let p = make_payload("warning", None, 7);
        let json = render_template(WebhookTemplate::Slack, &p);
        let text = json["text"].as_str().unwrap();
        assert!(text.contains("(+7 suppressed)"));
    }

    #[test]
    fn render_discord_info_color_blue() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        assert_eq!(json["embeds"][0]["color"], 0x3498DB);
    }

    #[test]
    fn render_discord_warning_color_amber() {
        let p = make_payload("warning", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        assert_eq!(json["embeds"][0]["color"], 0xFFAA00);
    }

    #[test]
    fn render_discord_critical_color_red() {
        let p = make_payload("critical", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        assert_eq!(json["embeds"][0]["color"], 0xFF0000);
    }

    #[test]
    fn render_discord_unknown_severity_color_blue() {
        let p = make_payload("debug", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        assert_eq!(json["embeds"][0]["color"], 0x3498DB);
    }

    #[test]
    fn render_discord_with_quick_fix() {
        let p = make_payload("warning", Some("ft fix"), 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        let fields = json["embeds"][0]["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 4); // Pane, Severity, Agent, Quick Fix
        assert_eq!(fields[3]["name"], "Quick Fix");
        assert!(fields[3]["value"].as_str().unwrap().contains("ft fix"));
        assert_eq!(fields[3]["inline"], false);
    }

    #[test]
    fn render_discord_without_quick_fix_has_3_fields() {
        let p = make_payload("warning", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        let fields = json["embeds"][0]["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 3);
    }

    #[test]
    fn render_discord_suppressed_in_title() {
        let p = make_payload("warning", None, 12);
        let json = render_template(WebhookTemplate::Discord, &p);
        let title = json["embeds"][0]["title"].as_str().unwrap();
        assert!(title.contains("(+12 suppressed)"));
    }

    #[test]
    fn render_discord_no_suppressed_omits_count() {
        let p = make_payload("warning", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        let title = json["embeds"][0]["title"].as_str().unwrap();
        assert!(!title.contains("suppressed"));
    }

    #[test]
    fn render_discord_has_footer_with_timestamp() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        let footer = json["embeds"][0]["footer"]["text"].as_str().unwrap();
        assert!(footer.contains("2026-02-14T00:00:00Z"));
        assert!(footer.contains("test.rule:event"));
    }

    #[test]
    fn render_discord_has_description() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        assert_eq!(
            json["embeds"][0]["description"].as_str().unwrap(),
            "Test description"
        );
    }

    #[test]
    fn render_discord_content_is_null() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        assert!(json["content"].is_null());
    }

    #[test]
    fn render_generic_preserves_all_fields() {
        let p = make_payload("warning", Some("cmd"), 3);
        let json = render_template(WebhookTemplate::Generic, &p);
        assert_eq!(json["event_type"], "test.rule:event");
        assert_eq!(json["pane_id"], 42);
        assert_eq!(json["severity"], "warning");
        assert_eq!(json["agent_type"], "codex");
        assert_eq!(json["summary"], "Test summary");
        assert_eq!(json["description"], "Test description");
        assert_eq!(json["quick_fix"], "cmd");
        assert_eq!(json["suppressed_since_last"], 3);
        assert!((json["confidence"].as_f64().unwrap() - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn render_generic_omits_quick_fix_when_none() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Generic, &p);
        assert!(json.get("quick_fix").is_none());
    }

    // ====================================================================
    // WebhookEndpointConfig additional tests
    // ====================================================================

    #[test]
    fn endpoint_config_clone() {
        let ep = test_endpoint("x", "http://x.com", WebhookTemplate::Slack);
        let ep2 = ep.clone();
        assert_eq!(ep2.name, "x");
        assert_eq!(ep2.template, WebhookTemplate::Slack);
    }

    #[test]
    fn endpoint_config_debug() {
        const SECRET: &str = "sentinel-webhook-secret-never-log";
        let mut ep = test_endpoint(
            "dbg",
            &format!("https://hooks.example.invalid/services/{SECRET}?token={SECRET}"),
            WebhookTemplate::Discord,
        );
        ep.headers
            .insert("Authorization".to_string(), format!("Bearer {SECRET}"));
        ep.headers
            .insert(format!("X-Secret-{SECRET}"), "opaque".to_string());
        let dbg = format!("{ep:?}");
        assert!(dbg.contains("WebhookEndpointConfig"));
        assert!(dbg.contains("dbg"));
        assert!(dbg.contains("https"));
        assert!(dbg.contains("header_count: 2"));
        assert!(!dbg.contains(SECRET));
        assert!(!dbg.contains("hooks.example.invalid"));
        assert!(!dbg.contains("Authorization"));
    }

    #[test]
    fn webhook_log_scheme_never_exposes_url_credentials() {
        const SECRET: &str = "sentinel-webhook-path-credential";
        let url =
            format!("https://user:{SECRET}@hooks.example.invalid/services/{SECRET}?token={SECRET}");

        let diagnostic = webhook_url_scheme(&url);

        assert_eq!(diagnostic, "https");
        assert!(!diagnostic.contains(SECRET));
        assert!(!diagnostic.contains("hooks.example.invalid"));
        assert_eq!(webhook_url_scheme("HtTpS://example.invalid"), "https");
        assert_eq!(webhook_url_scheme("hTtP://example.invalid"), "http");
        assert_eq!(webhook_url_scheme("1nvalid://example.invalid"), "unknown");
        assert_eq!(webhook_url_scheme("missing-delimiter"), "unknown");

        let enormous_custom_scheme = format!("{}-{SECRET}://example.invalid", "x".repeat(32_768));
        let custom_diagnostic = webhook_url_scheme(&enormous_custom_scheme);
        assert_eq!(custom_diagnostic, "unknown");
        assert!(!custom_diagnostic.contains(SECRET));
    }

    #[test]
    fn matches_event_type_empty_events_matches_all() {
        let ep = test_endpoint("all", "http://a.com", WebhookTemplate::Generic);
        assert!(ep.matches_event_type("anything:goes"));
        assert!(ep.matches_event_type(""));
    }

    #[test]
    fn matches_event_type_specific_pattern() {
        let mut ep = test_endpoint("test", "http://t.com", WebhookTemplate::Generic);
        ep.events = vec!["core.codex:*".to_string()];
        assert!(ep.matches_event_type("core.codex:usage_reached"));
        assert!(!ep.matches_event_type("core.gemini:error"));
    }

    #[test]
    fn matches_event_type_multiple_patterns() {
        let mut ep = test_endpoint("multi", "http://m.com", WebhookTemplate::Generic);
        ep.events = vec!["core.codex:*".to_string(), "core.gemini:*".to_string()];
        assert!(ep.matches_event_type("core.codex:usage"));
        assert!(ep.matches_event_type("core.gemini:error"));
        assert!(!ep.matches_event_type("core.claude:stuck"));
    }

    #[test]
    fn endpoint_disabled_false_in_toml() {
        let toml_str = r#"
name = "off"
url = "http://localhost"
enabled = false
"#;
        let ep: WebhookEndpointConfig = toml::from_str(toml_str).expect("parse");
        assert!(!ep.enabled);
    }

    #[test]
    fn endpoint_config_with_all_fields() {
        let toml_str = r#"
name = "full"
url = "https://example.com/webhook"
template = "discord"
events = ["a.*", "b.*"]
enabled = true

[headers]
X-Custom = "value"
Authorization = "Bearer xyz"
"#;
        let ep: WebhookEndpointConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(ep.name, "full");
        assert_eq!(ep.template, WebhookTemplate::Discord);
        assert_eq!(ep.events.len(), 2);
        assert_eq!(ep.headers.len(), 2);
        assert_eq!(ep.headers["X-Custom"], "value");
    }

    #[test]
    fn endpoint_config_json_roundtrip() {
        let ep = WebhookEndpointConfig {
            name: "json-test".to_string(),
            url: "http://json.test".to_string(),
            template: WebhookTemplate::Slack,
            events: vec!["*.error".to_string()],
            headers: {
                let mut h = HashMap::new();
                h.insert("X-Key".to_string(), "val".to_string());
                h
            },
            enabled: true,
        };
        let json = serde_json::to_string(&ep).unwrap();
        let back: WebhookEndpointConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "json-test");
        assert_eq!(back.template, WebhookTemplate::Slack);
        assert_eq!(back.events, vec!["*.error"]);
        assert_eq!(back.headers["X-Key"], "val");
    }

    // ====================================================================
    // Dispatcher pure tests
    // ====================================================================

    #[test]
    fn dispatcher_zero_endpoints() {
        let d = WebhookDispatcher::new(vec![], Box::new(MockTransport::success()));
        assert_eq!(d.endpoint_count(), 0);
        assert_eq!(d.active_endpoint_count(), 0);
    }

    #[test]
    fn dispatcher_all_disabled() {
        let mut ep1 = test_endpoint("a", "http://a", WebhookTemplate::Generic);
        let mut ep2 = test_endpoint("b", "http://b", WebhookTemplate::Slack);
        ep1.enabled = false;
        ep2.enabled = false;
        let d = WebhookDispatcher::new(vec![ep1, ep2], Box::new(MockTransport::success()));
        assert_eq!(d.endpoint_count(), 2);
        assert_eq!(d.active_endpoint_count(), 0);
    }

    #[test]
    fn render_template_dispatches_to_generic() {
        let p = make_payload("info", None, 0);
        let generic = render_template(WebhookTemplate::Generic, &p);
        // Generic renders the full payload as JSON
        assert!(generic.is_object());
        assert!(generic.get("event_type").is_some());
    }

    #[test]
    fn render_template_dispatches_to_slack() {
        let p = make_payload("info", None, 0);
        let slack = render_template(WebhookTemplate::Slack, &p);
        assert!(slack.get("blocks").is_some());
    }

    #[test]
    fn render_template_dispatches_to_discord() {
        let p = make_payload("info", None, 0);
        let discord = render_template(WebhookTemplate::Discord, &p);
        assert!(discord.get("embeds").is_some());
    }

    #[test]
    fn render_discord_inline_fields() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        let fields = json["embeds"][0]["fields"].as_array().unwrap();
        // First 3 fields (Pane, Severity, Agent) are inline
        assert_eq!(fields[0]["inline"], true);
        assert_eq!(fields[1]["inline"], true);
        assert_eq!(fields[2]["inline"], true);
    }

    #[test]
    fn render_discord_field_names() {
        let p = make_payload("info", None, 0);
        let json = render_template(WebhookTemplate::Discord, &p);
        let fields = json["embeds"][0]["fields"].as_array().unwrap();
        assert_eq!(fields[0]["name"], "Pane");
        assert_eq!(fields[1]["name"], "Severity");
        assert_eq!(fields[2]["name"], "Agent");
    }

    #[test]
    fn render_slack_field_types_are_mrkdwn() {
        let p = make_payload("warning", None, 0);
        let json = render_template(WebhookTemplate::Slack, &p);
        let fields = json["blocks"][1]["fields"].as_array().unwrap();
        for field in fields {
            assert_eq!(field["type"], "mrkdwn");
        }
    }
}
