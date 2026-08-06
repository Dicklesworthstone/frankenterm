//! No-mocks webhook tests built on a hermetic local HTTP server
//! (`tests/common/local_http_server.rs`). Drives `WebhookDispatcher`
//! against a real TCP listener that speaks HTTP/1.1, instead of
//! `MockTransport` returning canned struct values.
//!
//! Bead: ft-geg03.
//!
//! Architectural note. `WebhookTransport` is a pub trait with no
//! concrete real-HTTP implementer in `frankenterm-core/src/` — production
//! code that uses `WebhookDispatcher` injects its own transport. This
//! suite ships a small `RealHttpTransport` IN the test crate so we can
//! exercise the trait against actual HTTP framing, header propagation,
//! caller-Cx propagation, and connection-failure paths. It uses the same
//! asupersync HTTP client surface as the production adapter.
//!
//! Coverage:
//! - real_http_dispatch_succeeds_against_local_server: 200 OK on real
//!   POST, body+headers captured by the server
//! - real_http_dispatch_records_failure_on_500: server returns 500,
//!   `DeliveryRecord::accepted == false`
//! - real_http_dispatch_records_failure_on_connection_drop: server
//!   resets the connection mid-response
//! - real_http_dispatch_records_failure_on_unreachable_port: no
//!   listener at all → connect() fails, surfaces as a finite transport failure
//!
//! Gated on `FT_REAL_HTTP_TESTS=1` so default cargo test runs (and CI
//! lanes that block outbound TCP) skip cleanly. Each test binds to
//! 127.0.0.1:0; no real network egress.

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use common::local_http_server::{CannedResponse, LocalHttpServer};

use frankenterm_core::event_templates::{RenderedEvent, Suggestion};
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::webhook::{
    DeliveryResult, WebhookDispatcher, WebhookEndpointConfig, WebhookTemplate, WebhookTransport,
};

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

fn should_run() -> bool {
    std::env::var("FT_REAL_HTTP_TESTS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Structured JSON-line trace per the no-mocks-and-logging skill.
fn log(test: &str, phase: &str, body: serde_json::Value) {
    let line = serde_json::json!({
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "suite": "webhook_real_http",
        "test": test,
        "phase": phase,
        "data": body,
    });
    eprintln!("{line}");
}

// ── Real HTTP transport (test-only) ──────────────────────────────────────────
//
// Uses the canonical runtime HTTP client surface so the exact caller Cx reaches
// DNS/connect/I/O rather than being reduced to a preflight-only check.
struct RealHttpTransport {
    client: frankenterm_core::runtime_async::http::HttpClient,
}

impl RealHttpTransport {
    fn new() -> Self {
        Self {
            client: frankenterm_core::runtime_async::http::HttpClient::new(),
        }
    }

    fn checkpoint_failure(
        cx: &frankenterm_core::cx::Cx,
        error: &frankenterm_core::runtime_async::ContextError,
    ) -> DeliveryResult {
        use frankenterm_core::runtime_async::ContextErrorKind;

        match error.kind() {
            ContextErrorKind::DeadlineExceeded => DeliveryResult::deadline_exceeded(),
            ContextErrorKind::CancelTimeout => DeliveryResult::cancellation_cleanup_timeout(),
            ContextErrorKind::PollQuotaExhausted => DeliveryResult::poll_budget_exhausted(),
            ContextErrorKind::CostQuotaExhausted => DeliveryResult::cost_budget_exhausted(),
            ContextErrorKind::Cancelled => {
                finite_capability_failure(cx).unwrap_or_else(DeliveryResult::cancelled)
            }
            _ => DeliveryResult::context_failure(),
        }
    }

    fn client_failure(
        cx: &frankenterm_core::cx::Cx,
        error: &frankenterm_core::runtime_async::http::ClientError,
    ) -> DeliveryResult {
        use frankenterm_core::runtime_async::http::ClientError;

        match error {
            ClientError::DeadlineExceeded => DeliveryResult::deadline_exceeded(),
            ClientError::Cancelled => match cx.checkpoint() {
                Ok(()) => finite_capability_failure(cx).unwrap_or_else(DeliveryResult::cancelled),
                Err(error) => Self::checkpoint_failure(cx, &error),
            },
            ClientError::InvalidUrl(_)
            | ClientError::DnsError(_)
            | ClientError::ConnectError(_)
            | ClientError::TlsError(_)
            | ClientError::HttpError(_)
            | ClientError::TooManyRedirects { .. }
            | ClientError::Io(_)
            | ClientError::ConnectTunnelRefused { .. }
            | ClientError::InvalidConnectInput(_)
            | ClientError::ProxyError(_)
            | ClientError::PoolExhausted { .. } => DeliveryResult::transport_failure(),
        }
    }
}

impl WebhookTransport for RealHttpTransport {
    fn send_with_cx<'a>(
        &'a self,
        cx: &'a frankenterm_core::cx::Cx,
        url: &'a str,
        headers: &'a HashMap<String, String>,
        body: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = DeliveryResult> + Send + 'a>> {
        Box::pin(async move {
            if let Err(error) = cx.checkpoint() {
                return Self::checkpoint_failure(cx, &error);
            }

            let body = match serde_json::to_vec(body) {
                Ok(body) => body,
                Err(_) => return DeliveryResult::context_failure(),
            };
            let mut request_headers = Vec::with_capacity(headers.len() + 1);
            request_headers.push(("Content-Type".to_string(), "application/json".to_string()));
            request_headers.extend(headers.iter().map(|(key, value)| {
                (key.clone(), value.clone())
            }));

            if let Err(error) = cx.checkpoint() {
                return Self::checkpoint_failure(cx, &error);
            }

            match self
                .client
                .request(
                    cx,
                    frankenterm_core::runtime_async::http::Method::Post,
                    url,
                    request_headers,
                    body,
                )
                .await
            {
                Ok(response) => DeliveryResult::from_http_status(response.status),
                // Client errors may contain credential-bearing URLs or remote
                // text. Classify by discriminant and discard every payload.
                Err(error) => Self::client_failure(cx, &error),
            }
        })
    }
}

fn finite_capability_failure(cx: &frankenterm_core::cx::Cx) -> Option<DeliveryResult> {
    use frankenterm_core::outcome::CancelKind;

    if let Some(reason) = cx.root_cancel_cause() {
        return Some(match reason.kind {
            CancelKind::Timeout | CancelKind::Deadline => DeliveryResult::deadline_exceeded(),
            CancelKind::PollQuota => DeliveryResult::poll_budget_exhausted(),
            CancelKind::CostBudget => DeliveryResult::cost_budget_exhausted(),
            CancelKind::User
            | CancelKind::FailFast
            | CancelKind::RaceLost
            | CancelKind::ParentCancelled
            | CancelKind::ResourceUnavailable
            | CancelKind::Shutdown
            | CancelKind::LinkedExit => DeliveryResult::cancelled(),
        });
    }

    let budget = cx.budget_stats();
    if budget.deadline.at.is_some() && budget.deadline.remaining.is_none() {
        Some(DeliveryResult::deadline_exceeded())
    } else if budget.polls.remaining == Some(0) {
        Some(DeliveryResult::poll_budget_exhausted())
    } else if budget.cost.remaining == Some(0) {
        Some(DeliveryResult::cost_budget_exhausted())
    } else {
        None
    }
}

// ── Test fixtures (mirrored from webhook_labruntime.rs) ──────────────────────

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

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn real_http_dispatch_succeeds_against_local_server() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_HTTP_TESTS=1 to run real-HTTP webhook tests");
        return;
    }
    let server = LocalHttpServer::start_with_responses(vec![CannedResponse::Status(200, "OK")])
        .expect("bind local HTTP server");
    log("succ", "started", serde_json::json!({"url": server.url()}));

    let url = server.url_path("/hooks/test");
    let endpoints = vec![test_endpoint(
        "test-endpoint",
        &url,
        WebhookTemplate::Generic,
    )];
    let dispatcher = WebhookDispatcher::new(endpoints, Box::new(RealHttpTransport::new()));

    let records = RuntimeFixture::current_thread().block_on(async {
        dispatcher
            .dispatch(&test_detection(), 3, &test_rendered(), 0)
            .await
    });

    log(
        "succ",
        "dispatched",
        serde_json::json!({
            "n_records": records.len(),
            "all_accepted": records.iter().all(|r| r.accepted),
        }),
    );
    assert_eq!(records.len(), 1);
    assert!(records[0].accepted, "200 OK must mark accepted=true");

    // Server captured the request shape that the dispatcher actually
    // emitted — proves we exercise the real HTTP framing path.
    let captured = server.captured();
    assert_eq!(captured.len(), 1, "server received exactly one request");
    let req = &captured[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/hooks/test");
    let body_str = String::from_utf8_lossy(&req.body);
    assert!(
        body_str.contains("Codex hit usage limit"),
        "body should contain rendered summary; got: {body_str}"
    );
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("Content-Type") && v.contains("application/json")),
        "Content-Type header must be application/json"
    );
}

#[test]
fn real_http_dispatch_records_failure_on_500() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_HTTP_TESTS=1 to run real-HTTP webhook tests");
        return;
    }
    let server = LocalHttpServer::start_with_responses(vec![CannedResponse::Status(
        500,
        "Internal Server Error",
    )])
    .expect("bind local HTTP server");

    let url = server.url_path("/hooks/error");
    let endpoints = vec![test_endpoint("err", &url, WebhookTemplate::Generic)];
    let dispatcher = WebhookDispatcher::new(endpoints, Box::new(RealHttpTransport::new()));

    let records = RuntimeFixture::current_thread().block_on(async {
        dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await
    });

    log(
        "fail_500",
        "dispatched",
        serde_json::json!({
            "accepted": records[0].accepted,
            "status_code": records[0].status_code,
        }),
    );
    assert_eq!(records.len(), 1);
    assert!(!records[0].accepted, "500 must mark accepted=false");
    assert_eq!(records[0].status_code, 500);
    // Even on failure, the request body landed on the server — proves
    // the transport actually sent it (vs. a mock that just returns
    // failure without ever writing bytes to the wire).
    assert_eq!(server.captured().len(), 1);
}

#[test]
fn real_http_dispatch_records_failure_on_connection_drop() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_HTTP_TESTS=1 to run real-HTTP webhook tests");
        return;
    }
    let server = LocalHttpServer::start_with_responses(vec![CannedResponse::DropConnection])
        .expect("bind local HTTP server");

    let url = server.url_path("/hooks/drop");
    let endpoints = vec![test_endpoint("drop", &url, WebhookTemplate::Generic)];
    let dispatcher = WebhookDispatcher::new(endpoints, Box::new(RealHttpTransport::new()));

    let records = RuntimeFixture::current_thread().block_on(async {
        dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await
    });

    log(
        "drop",
        "dispatched",
        serde_json::json!({
            "accepted": records[0].accepted,
            "status_code": records[0].status_code,
        }),
    );
    // A reset connection causes parse_status to fail (empty / truncated
    // response). The transport returns a finite transport failure with
    // status_code=0; the dispatcher must record accepted=false.
    assert_eq!(records.len(), 1);
    assert!(
        !records[0].accepted,
        "dropped connection must mark accepted=false"
    );
}

#[test]
fn real_http_dispatch_records_failure_on_unreachable_port() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_HTTP_TESTS=1 to run real-HTTP webhook tests");
        return;
    }
    // Bind a listener, capture its port, then drop the listener so the
    // port is unbound. The dispatcher attempts to connect to a known-
    // dead port — connect() returns ECONNREFUSED and the transport
    // surfaces it as a finite transport failure.
    let dead_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("temporary bind");
        listener.local_addr().expect("local_addr").port()
    };
    let url = format!("http://127.0.0.1:{dead_port}/hooks/dead");
    let endpoints = vec![test_endpoint("dead", &url, WebhookTemplate::Generic)];
    let dispatcher = WebhookDispatcher::new(endpoints, Box::new(RealHttpTransport::new()));

    let records = RuntimeFixture::current_thread().block_on(async {
        dispatcher
            .dispatch(&test_detection(), 1, &test_rendered(), 0)
            .await
    });

    log(
        "unreachable",
        "dispatched",
        serde_json::json!({
            "accepted": records[0].accepted,
            "url": url,
        }),
    );
    assert_eq!(records.len(), 1);
    assert!(
        !records[0].accepted,
        "unreachable port must mark accepted=false"
    );
}
