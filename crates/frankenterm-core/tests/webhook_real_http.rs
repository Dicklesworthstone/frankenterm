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
//! and connection-failure paths. The transport is std-net-only, no new
//! workspace dependencies.
//!
//! Coverage:
//! - real_http_dispatch_succeeds_against_local_server: 200 OK on real
//!   POST, body+headers captured by the server
//! - real_http_dispatch_records_failure_on_500: server returns 500,
//!   `DeliveryRecord::accepted == false`
//! - real_http_dispatch_records_failure_on_connection_drop: server
//!   resets the connection mid-response
//! - real_http_dispatch_records_failure_on_unreachable_port: no
//!   listener at all → connect() fails, surfaces as DeliveryResult::err
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
use std::io::{Read, Write};
use std::pin::Pin;
use std::time::Duration;

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
// Uses std::net::TcpStream synchronously inside the async `send` future.
// The dispatcher polls this future on whichever runtime drives the test;
// blocking on a 127.0.0.1 socket inside a single-threaded test runtime
// is acceptable here because the local server replies in <50ms.
struct RealHttpTransport;

impl WebhookTransport for RealHttpTransport {
    fn send<'a>(
        &'a self,
        url: &'a str,
        headers: &'a HashMap<String, String>,
        body: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = DeliveryResult> + Send + 'a>> {
        let url = url.to_string();
        let headers = headers.clone();
        let body = body.clone();
        Box::pin(async move {
            match send_blocking(&url, &headers, &body) {
                // Mirror the convention used by `MockTransport::failure(500, ..)`
                // in webhook_labruntime.rs: 4xx/5xx are NOT accepted. A real
                // HTTP client without this mapping would silently swallow
                // server errors, which is the very class of bug the no-mocks
                // suite exists to catch.
                Ok(status) if (200..=299).contains(&status) => DeliveryResult::ok(status),
                Ok(status) => DeliveryResult::err(status, format!("HTTP {status}")),
                Err(err) => DeliveryResult::err(0, err),
            }
        })
    }
}

fn send_blocking(
    url: &str,
    headers: &HashMap<String, String>,
    body: &serde_json::Value,
) -> Result<u16, String> {
    let (host_port, path) = parse_url(url)?;
    let body_bytes = serde_json::to_vec(body).map_err(|e| format!("serialize body: {e}"))?;

    let mut stream = std::net::TcpStream::connect_timeout(
        &host_port
            .parse()
            .map_err(|e| format!("parse host_port {host_port}: {e}"))?,
        Duration::from_secs(2),
    )
    .map_err(|e| format!("connect {host_port}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok();

    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n",
        len = body_bytes.len()
    );
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write headers: {e}"))?;
    stream
        .write_all(&body_bytes)
        .map_err(|e| format!("write body: {e}"))?;
    stream.flush().ok();

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("read response: {e}"))?;
    parse_status(&response)
}

fn parse_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// supported, got {url}"))?;
    if let Some(slash) = rest.find('/') {
        let (host_port, path) = rest.split_at(slash);
        Ok((host_port.to_string(), path.to_string()))
    } else {
        Ok((rest.to_string(), "/".to_string()))
    }
}

fn parse_status(response: &[u8]) -> Result<u16, String> {
    let head = std::str::from_utf8(response).map_err(|e| format!("non-utf8 response: {e}"))?;
    let first_line = head.lines().next().ok_or("empty response")?;
    let parts: Vec<&str> = first_line.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return Err(format!("malformed status line: {first_line}"));
    }
    parts[1]
        .parse::<u16>()
        .map_err(|e| format!("parse status code {}: {e}", parts[1]))
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
    let server =
        LocalHttpServer::start_with_responses(vec![CannedResponse::Status(200, "OK")]).expect(
            "bind local HTTP server",
        );
    log("succ", "started", serde_json::json!({"url": server.url()}));

    let url = server.url_path("/hooks/test");
    let endpoints = vec![test_endpoint(
        "test-endpoint",
        &url,
        WebhookTemplate::Generic,
    )];
    let dispatcher = WebhookDispatcher::new(endpoints, Box::new(RealHttpTransport));

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
            .any(|(k, v)| k.eq_ignore_ascii_case("Content-Type")
                && v.contains("application/json")),
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
    let dispatcher = WebhookDispatcher::new(endpoints, Box::new(RealHttpTransport));

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
    let dispatcher = WebhookDispatcher::new(endpoints, Box::new(RealHttpTransport));

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
    // response). The transport returns DeliveryResult::err with
    // status_code=0; the dispatcher must record accepted=false.
    assert_eq!(records.len(), 1);
    assert!(!records[0].accepted, "dropped connection must mark accepted=false");
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
    // surfaces it as DeliveryResult::err.
    let dead_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("temporary bind");
        listener.local_addr().expect("local_addr").port()
    };
    let url = format!("http://127.0.0.1:{dead_port}/hooks/dead");
    let endpoints = vec![test_endpoint("dead", &url, WebhookTemplate::Generic)];
    let dispatcher = WebhookDispatcher::new(endpoints, Box::new(RealHttpTransport));

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
