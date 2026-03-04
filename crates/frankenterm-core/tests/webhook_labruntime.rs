//! Webhook notification tests ported to the RuntimeFixture pattern.
//!
//! Ports all 12 `#[tokio::test]` functions from `src/webhook.rs` to use
//! `RuntimeFixture::current_thread().block_on(async { ... })` so they run
//! under the asupersync lab-runtime instead of tokio.
//!
//! Bead: ft-22x4r

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;

use frankenterm_core::event_templates::{RenderedEvent, Suggestion};
use frankenterm_core::events::{EventFilter, NotificationGate, NotifyDecision};
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::webhook::{
    DeliveryResult, WebhookDispatcher, WebhookEndpointConfig, WebhookTemplate,
    WebhookTransport,
};

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

// ============================================================================
// Private test helpers (reimplemented from src/webhook.rs `#[cfg(test)]` block)
// ============================================================================

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct MockRequest {
    url: String,
    headers: HashMap<String, String>,
    body: serde_json::Value,
}

#[derive(Clone)]
struct MockTransport {
    requests: Arc<Mutex<Vec<MockRequest>>>,
    response: DeliveryResult,
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
        suggestions: vec![Suggestion::with_command(
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

// ============================================================================
// Dispatcher tests
// ============================================================================

#[test]
fn dispatcher_sends_to_matching_endpoints() {
    RuntimeFixture::current_thread().block_on(async {
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
    });
}

#[test]
fn dispatcher_skips_disabled_endpoints() {
    RuntimeFixture::current_thread().block_on(async {
        let transport = MockTransport::success();
        let mut ep = test_endpoint("disabled", "http://localhost", WebhookTemplate::Generic);
        ep.enabled = false;
        let dispatcher = WebhookDispatcher::new(vec![ep], Box::new(transport.clone()));

        let records = dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        assert!(records.is_empty());
        assert!(transport.requests().is_empty());
    });
}

#[test]
fn dispatcher_skips_non_matching_endpoints() {
    RuntimeFixture::current_thread().block_on(async {
        let transport = MockTransport::success();
        let mut ep = test_endpoint("gemini-only", "http://localhost", WebhookTemplate::Generic);
        ep.events = vec!["gemini.*".to_string()];
        let dispatcher = WebhookDispatcher::new(vec![ep], Box::new(transport.clone()));

        let records = dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        assert!(records.is_empty());
        assert!(transport.requests().is_empty());
    });
}

#[test]
fn dispatcher_records_failures() {
    RuntimeFixture::current_thread().block_on(async {
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
    });
}

#[test]
fn dispatcher_sends_correct_template_per_endpoint() {
    RuntimeFixture::current_thread().block_on(async {
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
    });
}

#[test]
fn dispatcher_passes_custom_headers() {
    RuntimeFixture::current_thread().block_on(async {
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
    });
}

// ============================================================================
// Pipeline integration tests
// ============================================================================

#[test]
fn pipeline_gate_filters_before_dispatch() {
    RuntimeFixture::current_thread().block_on(async {
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

        let d = test_detection(); // core.codex:usage_reached -- should be excluded
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
    });
}

#[test]
fn pipeline_gate_allows_and_dispatches() {
    RuntimeFixture::current_thread().block_on(async {
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
    });
}

#[test]
fn pipeline_dedup_prevents_second_dispatch() {
    RuntimeFixture::current_thread().block_on(async {
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

        // First event -- should pass through
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

        // Second identical event -- should be deduplicated
        let d2 = gate.should_notify(&d, 3, None);
        assert!(matches!(d2, NotifyDecision::Deduplicated { .. }));
        // No dispatch for deduplicated events
    });
}

#[test]
fn pipeline_severity_filter_blocks_info_events() {
    RuntimeFixture::current_thread().block_on(async {
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
    RuntimeFixture::current_thread().block_on(async {
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

        // Codex event -> only codex-hook receives it
        let records = dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await;

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].target, "codex-hook");
        assert_eq!(transport.requests().len(), 1);
    });
}

#[test]
fn pipeline_mixed_success_and_failure() {
    RuntimeFixture::current_thread().block_on(async {
        // Two transports with different responses -- simulate via
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
    });
}
