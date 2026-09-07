// The Send proof for this file's async fixtures walks the storage-open chain
// and, on nightly-2026-08-31 on macOS, exhausts the default 128-step budget
// ("overflow evaluating the requirement ... : Send"). The same code type-checks
// on the Linux proof workers, so this is the solver's depth budget, not a cycle;
// frankenterm-core's lib crate has carried the same raise for a while.
#![recursion_limit = "256"]

#[cfg(feature = "web")]
#[path = "common/fixtures.rs"]
mod runtime_fixtures;

#[cfg(feature = "web")]
mod web_tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    #[cfg(feature = "asupersync-runtime")]
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use frankenterm_core::events::{Event, EventBus};
    use frankenterm_core::patterns::{AgentType, Detection, Severity};
    use frankenterm_core::runtime_async::{
        io::{AsyncReadExt, AsyncWriteExt, read},
        net::{TcpListener, TcpStream},
        sleep, task, timeout,
    };
    use frankenterm_core::storage::{EventQuery, PaneRecord, StorageHandle, StoredEvent};

    use frankenterm_core::web::{WebRuntimeLimits, WebServerConfig, start_web_server};
    #[cfg(feature = "asupersync-runtime")]
    use frankenterm_core::web::{run_web_server_with_cx, start_web_server_with_cx};
    #[cfg(feature = "asupersync-runtime")]
    use frankenterm_core::web_framework::{
        FrameworkApp, FrameworkResponse, FrameworkResponseBody, FrameworkStartupHookError,
        FrameworkWebRuntime,
    };

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        // The shared fixture restores the ambient runtime handle even when
        // an assertion unwinds. Do not retain a strong TLS handle into the
        // next test's runtime or silently discard a runtime teardown panic.
        crate::runtime_fixtures::RuntimeFixture::current_thread().block_on(future);
    }

    #[cfg(feature = "asupersync-runtime")]
    fn run_async_test_with_cx<F>(
        test: impl FnOnce(frankenterm_core::cx::Cx, frankenterm_core::cx::Cx) -> F,
    ) where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_fixtures::RuntimeFixture::current_thread();
        // Pinned Asupersync 0.3.10 does not expose request_cx_with_budget.
        // Its block_on installs a fresh context backed by this runtime's
        // drivers. Retain that context for the server, then enter a separate
        // block_on for the client/cleanup driver: cancelling the server must
        // not cancel the test that observes whether cleanup really happened.
        let server_cx = runtime.block_on(async {
            frankenterm_core::cx::Cx::current().expect("runtime-owned server context")
        });
        let cleanup_cx = runtime.block_on(async {
            frankenterm_core::cx::Cx::current().expect("runtime-owned cleanup context")
        });
        runtime.block_on(async move {
            let observer_cx =
                frankenterm_core::cx::Cx::current().expect("runtime-owned observer context");
            assert_ne!(server_cx.task_id(), observer_cx.task_id());
            assert_ne!(cleanup_cx.task_id(), observer_cx.task_id());
            assert_ne!(server_cx.task_id(), cleanup_cx.task_id());
            test(server_cx, cleanup_cx).await;
            observer_cx
                .checkpoint()
                .expect("server cancellation must not cancel the fixture observer");
        });
    }

    /// Extract the HTTP response body from a raw HTTP response string.
    fn extract_body(raw: &str) -> &str {
        raw.split_once("\r\n\r\n").map_or("", |(_, body)| body)
    }

    #[test]
    fn web_openapi_discovery_http_fixture_preserves_body() {
        assert_eq!(
            extract_body("HTTP/1.1 200 OK\r\n\r\nfirst\r\n\r\nsecond"),
            "first\r\n\r\nsecond"
        );
        assert_eq!(extract_body("HTTP/1.1 200 OK\r\n\r\n"), "");
        assert_eq!(extract_body("incomplete headers"), "");
    }

    /// Extract the HTTP status code from a raw HTTP response string.
    fn extract_status(raw: &str) -> u16 {
        raw.split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    async fn fetch_health(addr: SocketAddr) -> std::io::Result<String> {
        let request = b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        fetch_raw(addr, request).await
    }

    async fn fetch_raw(addr: SocketAddr, raw_request: &[u8]) -> std::io::Result<String> {
        // These are finite JSON/error responses, not SSE. Bound the entire
        // exchange (including connect/write/EOF), not just connection retries.
        const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
        timeout(Duration::from_secs(5), async {
            let mut last_err = None;
            for _ in 0..50 {
                match TcpStream::connect(addr).await {
                    Ok(mut stream) => {
                        stream.write_all(raw_request).await?;
                        let mut buf = Vec::new();
                        let mut chunk = [0_u8; 4096];
                        loop {
                            let count = read(&mut stream, &mut chunk).await?;
                            if count == 0 {
                                return String::from_utf8(buf).map_err(|_| {
                                    std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        "HTTP fixture response is not UTF-8",
                                    )
                                });
                            }
                            if count > MAX_RESPONSE_BYTES - buf.len() {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "HTTP fixture response exceeds 1 MiB limit",
                                ));
                            }
                            buf.extend_from_slice(&chunk[..count]);
                        }
                    }
                    Err(err) => {
                        last_err = Some(err);
                        sleep(Duration::from_millis(20)).await;
                    }
                }
            }
            Err(last_err.unwrap_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "server not ready")
            }))
        })
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "HTTP fixture exchange timed out",
            )
        })?
    }

    async fn fetch_stream_prefix(
        addr: SocketAddr,
        raw_request: &[u8],
        read_timeout: Duration,
        min_bytes: usize,
    ) -> std::io::Result<String> {
        let mut last_err = None;
        for _ in 0..50 {
            match TcpStream::connect(addr).await {
                Ok(mut stream) => {
                    stream.write_all(raw_request).await?;
                    let deadline = Instant::now() + read_timeout;
                    let mut buf = Vec::new();
                    while buf.len() < min_bytes {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let mut chunk = [0_u8; 2048];
                        match timeout(deadline - now, read(&mut stream, &mut chunk)).await {
                            Ok(Ok(0)) => break,
                            Ok(Ok(n)) => {
                                buf.extend_from_slice(&chunk[..n]);
                            }
                            Ok(Err(err)) => return Err(err),
                            Err(_) => break,
                        }
                    }
                    return Ok(String::from_utf8_lossy(&buf).to_string());
                }
                Err(err) => {
                    last_err = Some(err);
                    sleep(Duration::from_millis(20)).await;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "server not ready")
        }))
    }

    fn stream_frame_kind_count(raw: &str, kind: &str) -> usize {
        raw.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|frame| frame["kind"] == kind)
            .count()
    }

    async fn fetch_stream_until_kind_count(
        addr: SocketAddr,
        raw_request: &[u8],
        read_timeout: Duration,
        kind: &str,
        expected_count: usize,
    ) -> std::io::Result<String> {
        let mut last_err = None;
        for _ in 0..50 {
            match TcpStream::connect(addr).await {
                Ok(mut stream) => {
                    stream.write_all(raw_request).await?;
                    let deadline = Instant::now() + read_timeout;
                    let mut buf = Vec::new();
                    loop {
                        let raw = String::from_utf8_lossy(&buf);
                        if stream_frame_kind_count(&raw, kind) >= expected_count {
                            return Ok(raw.to_string());
                        }

                        let now = Instant::now();
                        if now >= deadline {
                            return Ok(raw.to_string());
                        }

                        let mut chunk = [0_u8; 2048];
                        match timeout(deadline - now, read(&mut stream, &mut chunk)).await {
                            Ok(Ok(0)) => return Ok(raw.to_string()),
                            Ok(Ok(n)) => {
                                drop(raw);
                                buf.extend_from_slice(&chunk[..n]);
                            }
                            Ok(Err(err)) => return Err(err),
                            Err(_) => {
                                let raw = String::from_utf8_lossy(&buf);
                                return Ok(raw.to_string());
                            }
                        }
                    }
                }
                Err(err) => {
                    last_err = Some(err);
                    sleep(Duration::from_millis(20)).await;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "server not ready")
        }))
    }

    async fn fetch_stream_until_delta_count(
        addr: SocketAddr,
        raw_request: &[u8],
        read_timeout: Duration,
        expected_deltas: usize,
    ) -> std::io::Result<String> {
        fetch_stream_until_kind_count(addr, raw_request, read_timeout, "delta", expected_deltas)
            .await
    }

    fn epoch_ms_now() -> i64 {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        i64::try_from(ts.as_millis()).unwrap_or(0)
    }

    async fn create_test_storage_with_pane(
        pane_id: u64,
    ) -> Result<(StorageHandle, tempfile::TempDir), Box<dyn std::error::Error>> {
        let tempdir = tempfile::tempdir()?;
        let db_path = tempdir.path().join("web_stream_test.db");
        let db_path_str = db_path.to_string_lossy().to_string();
        let storage = StorageHandle::new(&db_path_str).await?;
        storage
            .upsert_pane(PaneRecord {
                pane_id,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("test-pane".to_string()),
                cwd: Some("/tmp".to_string()),
                tty_name: None,
                first_seen_at: epoch_ms_now(),
                last_seen_at: epoch_ms_now(),
                observed: true,
                ignore_reason: None,
                last_decision_at: Some(epoch_ms_now()),
            })
            .await?;
        Ok((storage, tempdir))
    }

    #[test]
    fn web_health_ephemeral_port() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .unwrap();
            let addr = server.bound_addr();

            assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

            let response = fetch_health(addr).await;
            let shutdown = server.shutdown().await;

            let response = response.unwrap();
            shutdown.unwrap();

            assert!(response.contains("200"));
            assert!(response.contains("\"ok\":true"));
        });
    }

    #[test]
    fn web_openapi_discovery_invokes_advertised_health() {
        use anyhow::{Context, ensure};

        run_async_test(async {
            let started = Instant::now();
            let server = timeout(
                Duration::from_secs(5),
                start_web_server(WebServerConfig::default().with_port(0)),
            )
            .await
            .expect("owned loopback startup exceeded its deadline")
            .expect("owned loopback server must start");
            let addr = server.bound_addr();
            let scenario = timeout(Duration::from_secs(5), async {
                // Return failures instead of panicking while the server is
                // live: both failed and successful scenarios attempt cleanup.
                ensure!(addr.ip().is_loopback(), "server must remain loopback-only");
                let raw = fetch_raw(
                    addr,
                    b"GET /openapi.json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .await
                .context("live discovery request must succeed")?;
                ensure!(
                    extract_status(&raw) == 200,
                    "discovery must return HTTP 200"
                );
                let (headers, document) = raw.split_once("\r\n\r\n").context("response headers")?;
                ensure!(
                    headers.lines().any(|line| {
                        line.split_once(':').is_some_and(|(name, value)| {
                            name.eq_ignore_ascii_case("content-type")
                                && value.trim().eq_ignore_ascii_case("application/json")
                        })
                    }),
                    "discovery must have JSON content type"
                );
                let spec: serde_json::Value =
                    serde_json::from_str(document).context("live document must be JSON")?;
                ensure!(spec["openapi"] == "3.1.0", "OpenAPI version must match");
                ensure!(
                    spec["info"]["version"] == frankenterm_core::VERSION,
                    "application version must match"
                );
                let paths = spec["paths"].as_object().context("live paths object")?;
                let expected: std::collections::BTreeSet<_> = [
                    "/health",
                    "/panes",
                    "/events",
                    "/search",
                    "/bookmarks",
                    "/ruleset-profile",
                    "/saved-searches",
                    "/stream/events",
                    "/stream/deltas",
                ]
                .into_iter()
                .map(|path| (path, "get"))
                .collect();
                let mut documented = std::collections::BTreeSet::new();
                let mut operation_ids = std::collections::BTreeSet::new();
                for (path, item) in paths {
                    for (method, operation) in item.as_object().context("path item")? {
                        documented.insert((path.as_str(), method.as_str()));
                        let id = operation["operationId"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                            .context("nonempty operation ID")?;
                        ensure!(operation_ids.insert(id), "operation IDs must be unique");
                        ensure!(
                            operation.get("security").is_none(),
                            "auth is not implemented yet"
                        );
                    }
                }
                ensure!(
                    documented == expected,
                    "live document must describe exactly nine GET routes"
                );
                ensure!(
                    spec.get("security").is_none(),
                    "global auth is not implemented yet"
                );
                ensure!(
                    spec["components"].get("securitySchemes").is_none(),
                    "no unwired auth schemes"
                );
                let (health_path, _) = paths
                    .iter()
                    .find(|(_, item)| item["get"]["operationId"] == "get_health")
                    .context("health operation must be discoverable")?;
                ensure!(
                    health_path == "/health",
                    "health operation must map to /health"
                );
                ensure!(
                    paths.len() == 9,
                    "review missing or newly exposed operations"
                );
                ensure!(
                    !paths.contains_key("/openapi.json"),
                    "document route must not describe itself"
                );
                let request = format!(
                    "GET {health_path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                );
                let health = fetch_raw(addr, request.as_bytes())
                    .await
                    .context("advertised route must answer")?;
                ensure!(
                    extract_status(&health) == 200,
                    "health must return HTTP 200"
                );
                let body: serde_json::Value = serde_json::from_str(extract_body(&health))
                    .context("health response must be JSON")?;
                ensure!(body["ok"] == true, "health must report success");
                Ok::<_, anyhow::Error>((paths.len(), document.len()))
            })
            .await;
            let shutdown = timeout(Duration::from_secs(5), server.shutdown()).await;
            let (routes, document_bytes) = scenario
                .expect("discovery scenario exceeded its deadline")
                .expect("live discovery/health assertions must pass");
            shutdown
                .expect("owned server shutdown exceeded its deadline")
                .expect("owned server must shut down cleanly");
            eprintln!(
                "{}",
                serde_json::json!({
                    "bead": "ft-xxfwy.55.2.1",
                    "scenario": "live_route_then_health",
                    "selected": 1, "executed": 1, "passed": 1, "failed": 0, "skipped": 0,
                    "routes": routes, "document_bytes": document_bytes,
                    "elapsed_ms": started.elapsed().as_millis(),
                    "teardown": "clean", "status": "passed",
                })
            );
        });
    }

    /// These synthetic TCP responses test the client fixture's bounds, not
    /// FrankenTerm's production HTTP parser or request-body admission policy.
    #[test]
    fn web_openapi_discovery_http_fixture_response_limits() {
        const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";

        run_async_test(async {
            for (name, response, expected_error) in [
                ("at_cap", vec![b'a'; 1024 * 1024], None),
                (
                    "over_cap",
                    vec![b'a'; 1024 * 1024 + 1],
                    Some("HTTP fixture response exceeds 1 MiB limit"),
                ),
                (
                    "invalid_utf8",
                    vec![0xff],
                    Some("HTTP fixture response is not UTF-8"),
                ),
            ] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let response_len = response.len();
                let server_task = task::spawn(async move {
                    timeout(Duration::from_secs(5), async {
                        let (mut stream, _) = listener.accept().await?;
                        let mut request = [0_u8; REQUEST.len()];
                        stream.read_exact(&mut request).await?;
                        stream.write_all(&response).await
                    })
                    .await
                });
                let result = fetch_raw(addr, REQUEST).await;
                // Join the owned peer before assertions, including failure cases.
                let peer = timeout(Duration::from_secs(5), server_task).await;
                peer.expect("fixture peer join deadline")
                    .expect("fixture peer task")
                    .expect("fixture peer exchange deadline")
                    .expect("fixture peer must send the complete response");
                match expected_error {
                    Some(message) => {
                        let error = result.expect_err(name);
                        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{name}");
                        assert_eq!(error.to_string(), message, "{name}");
                    }
                    None => assert_eq!(result.expect(name).len(), response_len, "{name}"),
                }
            }
        });
    }

    #[test]
    fn web_redaction_depth_limit_protects_stored_events() {
        use anyhow::{Context, ensure};

        run_async_test(async {
            let (storage, _directory) =
                timeout(Duration::from_secs(5), create_test_storage_with_pane(71))
                    .await
                    .expect("owned database startup deadline")
                    .expect("owned database starts");
            let startup = timeout(
                Duration::from_secs(5),
                start_web_server(
                    WebServerConfig::default()
                        .with_port(0)
                        .with_storage(storage.clone())
                        .with_storage_event_tail(false),
                ),
            )
            .await;
            if !matches!(&startup, Ok(Ok(_))) {
                let _ = timeout(Duration::from_secs(5), storage.shutdown()).await;
            }
            let server = startup
                .expect("owned HTTP server startup deadline")
                .expect("owned HTTP server starts");
            let addr = server.bound_addr();
            let scenario = timeout(Duration::from_secs(5), async {
                let canary = "sk-livetest1234567890abcdefghij";
                let mut nested = serde_json::Value::String(canary.to_owned());
                for _ in 0..70 {
                    nested = serde_json::Value::Array(vec![nested]);
                }
                // Seed a real legacy/unredacted row. This deliberately tests
                // read-side protection, independently of capture redaction.
                let event_id = storage.record_event(StoredEvent {
                    id: 0, pane_id: 71,
                    rule_id: "test.web.depth".to_owned(),
                    agent_type: "codex".to_owned(),
                    event_type: "depth_control".to_owned(),
                    severity: "info".to_owned(), confidence: 1.0,
                    extracted: Some(serde_json::json!({ "safe": "keep-me", "nested": nested })),
                    matched_text: Some("ordinary event".to_owned()),
                    segment_id: None, detected_at: epoch_ms_now(), dedupe_key: None,
                    handled_at: None, handled_by_workflow_id: None, handled_status: None,
                }).await.context("seed legacy event")?;
                let rows = storage.get_events(EventQuery {
                    pane_id: Some(71), limit: Some(1), ..EventQuery::default()
                }).await.context("verify real stored precondition")?;
                ensure!(rows.len() == 1 && rows[0].id == event_id, "real row must exist");
                ensure!(
                    serde_json::to_string(&rows[0].extracted)?.contains(canary),
                    "precondition must contain the canary before HTTP redaction"
                );
                let raw = fetch_raw(
                    addr,
                    b"GET /events?pane_id=71 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                ).await.context("live event read")?;
                ensure!(extract_status(&raw) == 200, "event read must succeed");
                ensure!(!raw.contains(canary), "HTTP response must not disclose deep secret");
                let body: serde_json::Value = serde_json::from_str(extract_body(&raw))?;
                ensure!(body["data"]["total"] == 1, "must return the actual event");
                let event = &body["data"]["events"][0];
                ensure!(event["id"] == event_id, "must return the seeded event ID");
                ensure!(event["extracted"]["safe"] == "keep-me", "shallow safe data must survive");
                ensure!(event["matched_text"] == "ordinary event", "ordinary text must survive");
                ensure!(raw.contains("[REDACTED: depth limit]"), "omitted subtree must be explicit");
                Ok::<_, anyhow::Error>(raw.len())
            }).await;
            let http_shutdown = timeout(Duration::from_secs(5), server.shutdown()).await;
            let storage_shutdown = timeout(Duration::from_secs(5), storage.shutdown()).await;
            let response_bytes = scenario
                .expect("HTTP scenario deadline")
                .expect("HTTP redaction assertions");
            http_shutdown
                .expect("HTTP teardown deadline")
                .expect("HTTP teardown");
            storage_shutdown
                .expect("database teardown deadline")
                .expect("database teardown");
            eprintln!(
                "{}",
                serde_json::json!({
                    "bead": "ft-xxfwy.55.21", "scenario": "stored_event_http_depth",
                    "selected": 1, "executed": 1, "passed": 1, "failed": 0, "skipped": 0,
                    "response_bytes": response_bytes, "teardown": "clean",
                })
            );
        });
    }

    #[test]
    fn web_redaction_depth_limit_protects_live_sse() {
        use anyhow::{Context, ensure};
        use base64::Engine as _;

        run_async_test(async {
            let event_bus = Arc::new(EventBus::new(8));
            let server = timeout(
                Duration::from_secs(5),
                start_web_server(
                    WebServerConfig::default()
                        .with_port(0)
                        .with_event_bus(Arc::clone(&event_bus)),
                ),
            )
            .await
            .expect("owned SSE server startup deadline")
            .expect("owned SSE server starts");
            let addr = server.bound_addr();
            let scenario = timeout(Duration::from_secs(5), async {
                let mut stream = TcpStream::connect(addr).await?;
                stream.write_all(b"GET /stream/events?channel=all&pane_id=72&max_hz=100 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await?;
                let canary = "sk-livetest1234567890abcdefghij";
                let mut response = Vec::new();
                let mut sent = false;
                loop {
                    let mut chunk = [0_u8; 2048];
                    let count = read(&mut stream, &mut chunk).await?;
                    ensure!(count != 0, "SSE must not close before delivering the event");
                    ensure!(response.len() + count <= 64 * 1024, "SSE proof byte budget");
                    response.extend_from_slice(&chunk[..count]);
                    let raw = std::str::from_utf8(&response).context("ASCII synthetic SSE response")?;
                    // The actual ready frame, not a timing sleep, proves that
                    // this stream subscribed before its two synthetic events.
                    if !sent && stream_frame_kind_count(raw, "ready") == 1 {
                        ensure!(extract_status(raw) == 200, "SSE must return HTTP 200");
                        ensure!(raw.contains("text/event-stream"), "SSE content type");
                        let mut nested = serde_json::Value::String(canary.to_owned());
                        for _ in 0..70 {
                            nested = serde_json::Value::Array(vec![nested]);
                        }
                        let delivered = event_bus.publish(Event::PatternDetected {
                            pane_id: 72, pane_uuid: None, event_id: None,
                            detection: Detection {
                                rule_id: "test.web.depth".to_owned(),
                                agent_type: AgentType::Codex,
                                event_type: "depth_control".to_owned(),
                                severity: Severity::Info, confidence: 1.0,
                                extracted: serde_json::json!({ "safe": "keep-me", "nested": nested }),
                                matched_text: "ordinary event".to_owned(), span: (0, 0),
                            },
                        });
                        ensure!(delivered == 1, "one owned subscriber must receive the detection");
                        let decoded = serde_json::json!({
                            "safe": "keep-me",
                            "secret": canary,
                            "padding": "x".repeat(13_000),
                        });
                        let source = serde_json::to_string(&decoded)?
                            .replace("sk-", r"\u0073\u006b-");
                        ensure!(!source.contains(canary), "raw JSON source hides the token from text-only matching");
                        ensure!(serde_json::from_str::<serde_json::Value>(&source)?["secret"] == canary,
                            "the browser-visible parsed transport must contain the canary before redaction");
                        let encoded = base64::engine::general_purpose::STANDARD.encode(source);
                        ensure!(encoded.len() > 16_384, "encoded precondition exceeds the old shortcut");
                        let delivered = event_bus.publish(Event::UserVarReceived {
                            pane_id: 72,
                            name: "FT_EVENT".to_owned(),
                            payload: frankenterm_core::events::UserVarPayload {
                                value: encoded,
                                event_type: Some("encoded_control".to_owned()),
                                event_data: Some(decoded),
                            },
                        });
                        ensure!(delivered == 1, "one owned subscriber must receive the user-var");
                        sent = true;
                    }
                    if stream_frame_kind_count(raw, "event") == 2 {
                        ensure!(sent, "only the published events may satisfy the oracle");
                        ensure!(!raw.contains(canary), "SSE must not disclose deep secret");
                        ensure!(raw.contains("[REDACTED: depth limit]"), "SSE truncation marker");
                        ensure!(raw.contains("keep-me") && raw.contains("ordinary event"), "safe event fields survive");
                        let user_var = raw.lines()
                            .filter_map(|line| line.strip_prefix("data: "))
                            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                            .find(|frame| frame["data"]["event"]["type"] == "user_var_received")
                            .context("the actual user-var event must be delivered")?;
                        let payload = &user_var["data"]["event"]["payload"];
                        let encoded = payload["value"].as_str().context("encoded value stays a string")?;
                        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
                        let decoded: serde_json::Value = serde_json::from_slice(&decoded)?;
                        ensure!(decoded["safe"] == "keep-me", "encoded safe data survives");
                        ensure!(decoded["secret"] == "[REDACTED]", "encoded secret must be redacted");
                        ensure!(decoded["padding"] == "x".repeat(13_000), "large benign transport data must not be lost");
                        ensure!(payload["event_data"]["safe"] == "keep-me", "decoded safe data survives");
                        ensure!(payload["event_data"]["secret"] == "[REDACTED]", "decoded view is redacted too");
                        return Ok::<_, anyhow::Error>(response.len());
                    }
                }
            }).await;
            let shutdown = timeout(Duration::from_secs(5), async {
                server.shutdown().await?;
                // Receiver drop wakes the independently scheduled producer;
                // keep its release inside the same teardown deadline.
                while event_bus.subscriber_count() != 0 {
                    sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;
            let bytes = scenario
                .expect("SSE scenario deadline")
                .expect("SSE redaction assertions");
            shutdown
                .expect("SSE teardown deadline")
                .expect("SSE teardown");
            assert_eq!(
                event_bus.subscriber_count(),
                0,
                "owned subscription released"
            );
            eprintln!(
                "{}",
                serde_json::json!({
                    "bead": "ft-xxfwy.55.21", "scenario": "live_event_sse_depth",
                    "selected": 1, "executed": 1, "passed": 1, "failed": 0, "skipped": 0,
                    "response_bytes": bytes, "teardown": "clean",
                })
            );
        });
    }

    /// Dropping an idle live handle cannot await graceful cleanup, but its
    /// owned runtime guard must synchronously signal shutdown and wake the
    /// listener so the accept task releases its socket.
    #[test]
    fn dropped_idle_web_server_handle_releases_listener() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .expect("server should start");
            let addr = server.bound_addr();
            let response = fetch_health(addr)
                .await
                .expect("server should answer before handle drop");
            assert!(response.contains("200"));

            drop(server);

            let mut listener_closed = false;
            for _ in 0..50 {
                match TcpStream::connect(addr).await {
                    Ok(stream) => {
                        drop(stream);
                        sleep(Duration::from_millis(20)).await;
                    }
                    Err(_) => {
                        listener_closed = true;
                        break;
                    }
                }
            }
            assert!(
                listener_closed,
                "Drop fallback must stop the serve task and release {addr}"
            );
        });
    }

    /// Keep both real SSE response bodies parked beyond the entire shutdown
    /// budget. Shutdown must wake them; an eventual keepalive is not cleanup.
    #[test]
    fn idle_event_and_delta_streams_shutdown_without_keepalive() {
        run_async_test(async {
            let (storage, _tmp) = create_test_storage_with_pane(7).await.unwrap();
            let event_bus = Arc::new(EventBus::new(64));
            let limits = WebRuntimeLimits {
                stream_keepalive_secs: 60,
                ..frankenterm_core::web::resolve_runtime_limits(None)
            };
            let server = start_web_server(
                WebServerConfig::default()
                    .with_port(0)
                    .with_storage(storage)
                    .with_event_bus(Arc::clone(&event_bus))
                    .with_storage_event_tail(false)
                    .with_runtime_limits(limits),
            )
            .await
            .expect("idle-stream server starts");
            let addr = server.bound_addr();
            let mut clients = Vec::new();
            let mut response_bytes = 0_usize;
            for path in ["/stream/events", "/stream/deltas"] {
                let client = timeout(Duration::from_secs(2), async {
                    let mut client = TcpStream::connect(addr).await.unwrap();
                    let request = format!(
                        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                    );
                    client.write_all(request.as_bytes()).await.unwrap();
                    let mut received = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        let count = read(&mut client, &mut chunk).await.unwrap();
                        assert_ne!(count, 0, "stream ended before readiness");
                        assert!(received.len() + count <= 4096, "bounded ready frame");
                        received.extend_from_slice(&chunk[..count]);
                        let raw = String::from_utf8_lossy(&received);
                        if raw.contains("event: ready") && raw.contains("\"kind\":\"ready\"") {
                            assert_eq!(extract_status(&raw), 200);
                            response_bytes += received.len();
                            break;
                        }
                    }
                    client
                })
                .await
                .expect("real SSE response must reach ready before shutdown");
                clients.push(client);
            }
            assert_eq!(event_bus.subscriber_count(), 2, "both streams are live");

            let started = Instant::now();
            let shutdown = timeout(Duration::from_secs(5), server.shutdown()).await;
            let elapsed = started.elapsed();
            shutdown
                .expect("shutdown must finish before the outer watchdog")
                .expect("idle streams must not survive either drain phase");
            assert_eq!(
                event_bus.subscriber_count(),
                0,
                "subscriptions settle before return"
            );
            for mut client in clients {
                timeout(Duration::from_secs(1), async {
                    let mut trailing_bytes = 0_usize;
                    let mut chunk = [0_u8; 1024];
                    loop {
                        let count = read(&mut client, &mut chunk).await.unwrap();
                        if count == 0 {
                            break;
                        }
                        trailing_bytes += count;
                        assert!(trailing_bytes <= 4096, "bounded terminal stream bytes");
                    }
                    response_bytes += trailing_bytes;
                })
                .await
                .expect("both real clients must observe EOF after shutdown");
            }
            eprintln!(
                "{}",
                serde_json::json!({
                    "bead": "ft-xxfwy.55.13.1", "scenario": "idle_events_and_deltas_drain",
                    "selected": 1, "executed": 1, "passed": 1, "failed": 0, "skipped": 0,
                    "streams": 2, "subscribers_after": 0, "response_bytes": response_bytes,
                    "shutdown_ms": elapsed.as_millis(), "keepalive_secs": 60, "teardown": "clean",
                })
            );
        });
    }

    /// A connection that stalls mid-request must not serialize the accept
    /// loop. A later health request should complete while the first connection
    /// is still waiting for the rest of its request bytes.
    #[test]
    fn web_server_serves_concurrent_connection_while_prior_read_is_stalled() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .expect("server should start");
            let addr = server.bound_addr();
            let mut stalled = TcpStream::connect(addr)
                .await
                .expect("stalled connection should connect");
            stalled
                .write_all(b"G")
                .await
                .expect("partial request byte should write");

            // Let the accept loop hand the partial request to a connection
            // task before opening the independent health request.
            sleep(Duration::from_millis(50)).await;
            let health = timeout(Duration::from_millis(750), fetch_health(addr))
                .await
                .expect("health request must not wait for the stalled connection")
                .expect("health request should succeed");
            assert!(health.contains("200"));

            drop(stalled);
            server.shutdown().await.expect("server should shut down");
        });
    }

    /// Graceful shutdown must be bounded even when a client leaves an HTTP
    /// request incomplete. The configured idle-read timeout closes the stuck
    /// connection inside the server's bounded drain window.
    #[test]
    fn web_shutdown_bounds_stalled_connection_read() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .expect("server should start");
            let addr = server.bound_addr();
            let mut stalled = TcpStream::connect(addr)
                .await
                .expect("stalled connection should connect");
            stalled
                .write_all(b"G")
                .await
                .expect("partial request byte should write");
            sleep(Duration::from_millis(50)).await;

            let started = Instant::now();
            let shutdown = timeout(Duration::from_secs(5), server.shutdown())
                .await
                .expect("bounded web shutdown must not hit the outer timeout");
            let elapsed = started.elapsed();
            shutdown.expect("stalled idle read should drain within the configured bound");
            assert!(
                elapsed < Duration::from_secs(3),
                "stalled-read shutdown exceeded its drain bound: {elapsed:?}"
            );

            let mut byte = [0_u8; 1];
            match timeout(Duration::from_secs(1), stalled.read(&mut byte)).await {
                Ok(Ok(0) | Err(_)) => {}
                Ok(Ok(count)) => panic!(
                    "stalled connection should close without response bytes, read {count} byte(s)"
                ),
                Err(error) => panic!("stalled connection remained open after shutdown: {error}"),
            }
        });
    }

    /// ft-xbnl0.2.3 Cx-first: verify
    /// `start_web_server_with_cx` + `shutdown_with_cx` roundtrip
    /// on an ephemeral port, identically to the legacy pair. An
    /// uncancelled cx must not affect the happy path.
    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn web_health_with_cx_ephemeral_port() {
        run_async_test_with_cx(|cx, _| async move {
            let server = start_web_server_with_cx(&cx, WebServerConfig::default().with_port(0))
                .await
                .unwrap();
            let addr = server.bound_addr();

            assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

            let response = fetch_health(addr).await;
            let shutdown = server.shutdown_with_cx(&cx).await;

            let response = response.unwrap();
            shutdown.unwrap();

            assert!(response.contains("200"));
            assert!(response.contains("\"ok\":true"));
        });
    }

    /// Cancellation requested by a completed startup hook is a post-startup
    /// failure: shutdown hooks must run before the cancellation is returned.
    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn web_startup_cancellation_rolls_back_shutdown_hooks() {
        run_async_test_with_cx(|cx, _| async move {
            let startup_count = Arc::new(AtomicUsize::new(0));
            let shutdown_count = Arc::new(AtomicUsize::new(0));
            let startup_count_for_hook = Arc::clone(&startup_count);
            let shutdown_count_for_hook = Arc::clone(&shutdown_count);
            let cx_for_hook = cx.clone();
            let app = FrameworkApp::builder()
                .on_startup(move || {
                    startup_count_for_hook.fetch_add(1, Ordering::SeqCst);
                    cx_for_hook.cancel_with(
                        frankenterm_core::outcome::CancelKind::User,
                        Some("cancel from web startup hook rollback test"),
                    );
                    Ok(())
                })
                .on_shutdown(move || {
                    shutdown_count_for_hook.fetch_add(1, Ordering::SeqCst);
                })
                .build();

            let result =
                FrameworkWebRuntime::start_with_cx(&cx, "127.0.0.1:0".to_string(), app).await;

            let error = result
                .err()
                .expect("post-hook cancellation must fail startup");
            assert!(
                error.to_string().contains("cancelled"),
                "primary cancellation must survive startup rollback: {error}"
            );
            assert_eq!(startup_count.load(Ordering::SeqCst), 1);
            assert_eq!(
                shutdown_count.load(Ordering::SeqCst),
                1,
                "post-startup cancellation must run shutdown hooks exactly once"
            );
        });
    }

    /// A fatal startup hook can abort after earlier hooks acquired resources;
    /// shutdown hooks must roll those resources back before the fatal hook
    /// error is returned.
    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn web_startup_abort_rolls_back_shutdown_hooks() {
        run_async_test_with_cx(|cx, _| async move {
            const PRIVATE_HOOK_DETAIL: &str = "fatal-startup-private-sentinel";
            let startup_count = Arc::new(AtomicUsize::new(0));
            let later_startup_count = Arc::new(AtomicUsize::new(0));
            let shutdown_count = Arc::new(AtomicUsize::new(0));
            let startup_count_for_hook = Arc::clone(&startup_count);
            let later_startup_count_for_hook = Arc::clone(&later_startup_count);
            let shutdown_count_for_hook = Arc::clone(&shutdown_count);
            let app = FrameworkApp::builder()
                .on_startup(move || {
                    startup_count_for_hook.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .on_startup(|| Err(FrameworkStartupHookError::new(PRIVATE_HOOK_DETAIL)))
                .on_startup(move || {
                    later_startup_count_for_hook.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .on_shutdown(move || {
                    shutdown_count_for_hook.fetch_add(1, Ordering::SeqCst);
                })
                .build();
            // An invalid bind address is a competing later failure: the exact
            // startup error below proves we never reached bind resolution.
            let result =
                FrameworkWebRuntime::start_with_cx(&cx, "not-a-socket-address".to_string(), app)
                    .await;

            let error = result.err().expect("fatal startup hook must abort startup");
            assert!(
                matches!(
                    &error,
                    frankenterm_core::Error::RuntimeOperation {
                        operation,
                        source: frankenterm_core::error::RuntimeOperationSource::Backend(detail),
                    } if *operation == "web startup hooks" && detail == "web_startup_hook_aborted"
                ),
                "the finite startup failure must remain primary: {error}"
            );
            assert!(!error.to_string().contains(PRIVATE_HOOK_DETAIL));
            assert!(!format!("{error:?}").contains(PRIVATE_HOOK_DETAIL));
            assert_eq!(startup_count.load(Ordering::SeqCst), 1);
            assert_eq!(later_startup_count.load(Ordering::SeqCst), 0);
            assert_eq!(
                shutdown_count.load(Ordering::SeqCst),
                1,
                "startup abort must run shutdown hooks exactly once"
            );
        });
    }

    /// A bind failure after successful startup hooks must also roll back the
    /// hook lifecycle, while preserving the bind error as the primary result.
    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn web_bind_failure_rolls_back_shutdown_hooks() {
        run_async_test_with_cx(|cx, _| async move {
            let occupied = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("port reservation should bind");
            let occupied_addr = occupied
                .local_addr()
                .expect("port reservation should expose its address");
            let startup_count = Arc::new(AtomicUsize::new(0));
            let shutdown_count = Arc::new(AtomicUsize::new(0));
            let startup_count_for_hook = Arc::clone(&startup_count);
            let shutdown_count_for_hook = Arc::clone(&shutdown_count);
            let app = FrameworkApp::builder()
                .on_startup(move || {
                    startup_count_for_hook.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .on_shutdown(move || {
                    shutdown_count_for_hook.fetch_add(1, Ordering::SeqCst);
                })
                .build();
            let result =
                FrameworkWebRuntime::start_with_cx(&cx, occupied_addr.to_string(), app).await;

            let error = result.err().expect("occupied address must fail startup");
            let error_message = error.to_string();
            assert!(
                error_message.contains("I/O")
                    || error_message.contains("address")
                    || error_message.contains("Address"),
                "primary bind failure must survive startup rollback: {error_message}"
            );
            assert_eq!(startup_count.load(Ordering::SeqCst), 1);
            assert_eq!(
                shutdown_count.load(Ordering::SeqCst),
                1,
                "bind failure must run shutdown hooks exactly once"
            );
            drop(occupied);
        });
    }

    /// A caller that is already cancelled when it consumes a server handle
    /// must still release the listener and complete graceful cleanup before
    /// receiving the cancellation error.
    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn web_shutdown_with_precancelled_cx_cleans_before_error() {
        run_async_test_with_cx(|start_cx, shutdown_cx| async move {
            let server =
                start_web_server_with_cx(&start_cx, WebServerConfig::default().with_port(0))
                    .await
                    .expect("server should start with a live Cx");
            let addr = server.bound_addr();
            let response = fetch_health(addr)
                .await
                .expect("server should answer before shutdown");
            assert!(response.contains("200"));

            shutdown_cx.cancel_with(
                frankenterm_core::outcome::CancelKind::User,
                Some("pre-cancelled WebServerHandle shutdown test"),
            );

            let error = server
                .shutdown_with_cx(&shutdown_cx)
                .await
                .expect_err("shutdown must surface cancellation after cleanup");
            assert!(
                error.to_string().contains("cancelled"),
                "shutdown should return the caller cancellation: {error}"
            );

            let reconnect = timeout(Duration::from_secs(1), TcpStream::connect(addr)).await;
            assert!(
                matches!(reconnect, Ok(Err(_))),
                "listener must be closed before shutdown returns: {reconnect:?}"
            );
        });
    }

    /// Framework cleanup must run registered shutdown hooks before it observes
    /// cancellation of the context supplied to the finish phase.
    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn web_finish_with_precancelled_cx_runs_shutdown_hooks() {
        run_async_test_with_cx(|start_cx, finish_cx| async move {
            let shutdown_count = Arc::new(AtomicUsize::new(0));
            let shutdown_count_for_hook = Arc::clone(&shutdown_count);
            let app = FrameworkApp::builder()
                .get("/health", |_, _| async { FrameworkResponse::ok() })
                .on_shutdown(move || {
                    shutdown_count_for_hook.fetch_add(1, Ordering::SeqCst);
                })
                .build();
            let (addr, mut runtime) =
                FrameworkWebRuntime::start_with_cx(&start_cx, "127.0.0.1:0".to_string(), app)
                    .await
                    .expect("framework server should start");
            let response = fetch_health(addr)
                .await
                .expect("framework server should answer before shutdown");
            assert!(response.contains("200"));

            runtime.signal_shutdown();
            let join_result = runtime.join_handle_mut().await;
            finish_cx.cancel_with(
                frankenterm_core::outcome::CancelKind::User,
                Some("pre-cancelled framework finish test"),
            );

            let error = runtime
                .finish_with_cx(&finish_cx, join_result)
                .await
                .expect_err("finish must surface cancellation after shutdown hooks");
            assert!(
                error.to_string().contains("cancelled"),
                "finish should return the cleanup-context cancellation: {error}"
            );
            assert_eq!(
                shutdown_count.load(Ordering::SeqCst),
                1,
                "finish must run shutdown hooks exactly once before returning cancellation"
            );
        });
    }

    /// A handler that outlives both bounded drain phases must produce a
    /// truthful drain error rather than a false successful-shutdown result.
    /// The test releases the synthetic handler afterward so no task is left
    /// behind in the test runtime.
    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn web_finish_reports_connection_that_survives_bounded_drain() {
        run_async_test_with_cx(|cx, _| async move {
            let handler_started = Arc::new(AtomicUsize::new(0));
            let release_handler = Arc::new(AtomicUsize::new(0));
            let started_for_handler = Arc::clone(&handler_started);
            let release_for_handler = Arc::clone(&release_handler);
            let app = FrameworkApp::builder()
                .get("/stuck", move |_, _| {
                    let started = Arc::clone(&started_for_handler);
                    let release = Arc::clone(&release_for_handler);
                    async move {
                        started.store(1, Ordering::SeqCst);
                        while release.load(Ordering::SeqCst) == 0 {
                            sleep(Duration::from_millis(10)).await;
                        }
                        FrameworkResponse::ok()
                            .body(FrameworkResponseBody::Bytes(b"released".to_vec()))
                    }
                })
                .build();
            let (addr, mut runtime) =
                FrameworkWebRuntime::start_with_cx(&cx, "127.0.0.1:0".to_string(), app)
                    .await
                    .expect("framework server should start");
            let mut client = TcpStream::connect(addr)
                .await
                .expect("stuck-handler client should connect");
            client
                .write_all(b"GET /stuck HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .expect("stuck-handler request should write");

            let handler_start = timeout(Duration::from_secs(1), async {
                while handler_started.load(Ordering::SeqCst) == 0 {
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            if let Err(error) = handler_start {
                release_handler.store(1, Ordering::SeqCst);
                panic!("stuck handler should start: {error}");
            }

            let started = Instant::now();
            let finish_result = timeout(Duration::from_secs(6), async move {
                runtime.signal_shutdown();
                let join_result = runtime.join_handle_mut().await;
                runtime.finish_with_cx(&cx, join_result).await
            })
            .await;
            let elapsed = started.elapsed();
            release_handler.store(1, Ordering::SeqCst);
            let error = finish_result
                .expect("two bounded drain phases must not hit the outer timeout")
                .expect_err("a connection beyond both drain bounds must be reported");
            let message = error.to_string();
            assert!(
                message.contains("active connection") && message.contains("drain"),
                "bounded-drain failure must report the live connection: {message}"
            );
            assert!(
                elapsed < Duration::from_secs(5),
                "two bounded drain phases exceeded their combined limit: {elapsed:?}"
            );

            let mut response = Vec::new();
            timeout(Duration::from_secs(1), client.read_to_end(&mut response))
                .await
                .expect("released synthetic connection should close")
                .expect("released synthetic response should read");
            assert!(
                String::from_utf8_lossy(&response).contains("released"),
                "released handler should complete its response"
            );
            task::yield_now().await;
        });
    }

    /// ft-xbnl0.2.4 tick 323: Pre-cancelled cx must refuse to bind.
    ///
    /// `start_web_server_with_cx` has a pre-flight `cx.checkpoint()` at
    /// the top of the function. A cx that is already cancelled when the
    /// call is made must cause the function to return `Err` *before*
    /// any TCP bind is attempted — an operator who has abandoned the
    /// request should not leave a socket in LISTEN state.
    ///
    /// Complements:
    /// - tick 322: metrics server mid-flight cancel stops accept loop
    ///   (parallel service-boundary contract on a different server)
    /// - `web_health_with_cx_ephemeral_port`: happy path (live cx binds)
    ///
    /// Together they pin three timings of the cx signal: pre-start (this
    /// tick), mid-start (metrics), and happy (pre-existing). ft-xbnl0.2.4
    /// acceptance criterion 1 covers "TCP, TLS, and HTTP client or
    /// service boundaries"; this is a service-boundary test.
    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn web_server_with_cx_pre_cancelled_refuses_to_bind() {
        run_async_test_with_cx(|cx, _| async move {
            cx.cancel_with(
                frankenterm_core::outcome::CancelKind::User,
                Some("pre-cancel web_server_with_cx test"),
            );

            let result =
                start_web_server_with_cx(&cx, WebServerConfig::default().with_port(0)).await;

            assert!(
                result.is_err(),
                "pre-cancelled cx must cause start_web_server_with_cx to fail (WebServerHandle does not impl Debug so result value is elided)"
            );
            let err_msg = result.err().unwrap().to_string();
            assert!(
                err_msg.contains("cancelled") || err_msg.contains("start_web_server cancelled"),
                "error should surface cancellation; got: {err_msg}"
            );
        });
    }

    /// ft-xbnl0.2.4 tick 417: `run_web_server_with_cx` orchestrator-level
    /// mid-flight cx-cancel completes graceful cleanup and then surfaces the
    /// cancellation.
    ///
    /// Complements `web_server_with_cx_pre_cancelled_refuses_to_bind` (tick 323),
    /// which pins the pre-start timing. This test pins the mid-flight
    /// timing: cx is live when `run_web_server_with_cx` is called, the
    /// server binds and enters its `select! { join | shutdown_signal }`
    /// orchestration, then the test confirms a health response before
    /// cancelling the caller Cx.
    ///
    /// `wait_for_shutdown_signal_with_cx` checks the caller context every
    /// 100 ms via `sleep_with_cx`, so cx-cancel must wake the shutdown branch,
    /// which then runs `signal_shutdown` (including listener wake), followed by
    /// drain and hooks before checking cancellation at the finish boundary.
    /// Only after that cleanup completes may the outer future return the
    /// cancellation error.
    ///
    /// This pins the orchestrator-level cx→shutdown wiring that was
    /// previously only covered by the pre-cancel pre-bind path.
    #[cfg(feature = "asupersync-runtime")]
    #[test]
    fn web_server_with_cx_mid_flight_cancel_surfaces_after_cleanup() {
        run_async_test_with_cx(|cx, _| async move {
            let cx_for_server = cx.clone();

            // Reserve a currently free port so the test can prove the server
            // completed startup before cancellation. There is an unavoidable
            // small release/rebind race, but readiness is verified below.
            let port_probe = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("ephemeral-port probe should bind");
            let addr = port_probe
                .local_addr()
                .expect("ephemeral-port probe should expose its address");
            drop(port_probe);

            let server_task = task::spawn(async move {
                run_web_server_with_cx(
                    &cx_for_server,
                    WebServerConfig::default().with_port(addr.port()),
                )
                .await
            });

            let response = fetch_health(addr)
                .await
                .expect("server must start successfully before mid-flight cancellation");
            assert!(response.contains("200"));

            cx.cancel_with(
                frankenterm_core::outcome::CancelKind::User,
                Some("mid-flight cancel run_web_server_with_cx test"),
            );

            // The orchestrator should exit within one or two 100ms poll cycles
            // plus graceful shutdown drain time.
            let started = Instant::now();
            let outer = timeout(Duration::from_secs(10), server_task).await;
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(5),
                "run_web_server_with_cx should exit within 5s of cx-cancel; took {elapsed:?}"
            );
            let join = outer.expect("outer timeout should not fire");
            let inner = join.expect("run_web_server_with_cx task must not panic");
            let error = inner.expect_err(
                "run_web_server_with_cx must surface caller cancellation after cleanup",
            );
            let message = error.to_string();
            assert!(
                message.contains("cancelled") || message.contains("shutdown wait"),
                "returned error must identify cancellation: {message}"
            );
        });
    }

    // =========================================================================
    // Hardening tests (wa-nu4.3.6.3)
    // =========================================================================

    #[test]
    fn default_config_binds_localhost() {
        let config = WebServerConfig::default();
        // Default should produce 127.0.0.1:8000
        let debug = format!("{config:?}");
        assert!(
            debug.contains("127.0.0.1"),
            "default host must be 127.0.0.1"
        );
    }

    #[test]
    fn public_bind_rejected_without_opt_in() {
        run_async_test(async {
            let config = WebServerConfig::new(0).with_host("0.0.0.0");
            let result = start_web_server(config).await;
            assert!(result.is_err(), "public bind should be rejected by default");
            let err_msg = result.err().unwrap().to_string();
            assert!(
                err_msg.contains("refusing to bind")
                    && err_msg.contains("no authentication boundary"),
                "error should explain localhost-only auth boundary: {err_msg}"
            );
        });
    }

    #[test]
    fn public_bind_rejected_even_with_explicit_opt_in_without_auth() {
        run_async_test(async {
            let config = WebServerConfig::new(0)
                .with_host("0.0.0.0")
                .with_dangerous_public_bind();
            let result = start_web_server(config).await;
            assert!(
                result.is_err(),
                "public bind should fail closed until auth exists"
            );
            let err_msg = result.err().unwrap().to_string();
            assert!(
                err_msg.contains("dangerous public-bind opt-in")
                    && err_msg.contains("no authentication boundary"),
                "error should explain why opt-in is insufficient: {err_msg}"
            );
        });
    }

    #[test]
    fn panes_returns_503_without_storage() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .unwrap();
            let addr = server.bound_addr();

            let req = b"GET /panes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let response = fetch_raw(addr, req).await;
            let shutdown = server.shutdown().await;

            let response = response.unwrap();
            shutdown.unwrap();

            assert!(
                response.contains("503"),
                "should return 503 without storage: {response}"
            );
            assert!(
                response.contains("no_storage"),
                "should include error code: {response}"
            );
        });
    }

    #[test]
    fn search_requires_query_param() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .unwrap();
            let addr = server.bound_addr();

            // Request /search without ?q= parameter
            let req = b"GET /search HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let response = fetch_raw(addr, req).await;
            let shutdown = server.shutdown().await;

            let response = response.unwrap();
            shutdown.unwrap();

            // Without storage, 503 takes precedence over the missing-query 400.
            assert!(
                response.contains("503") || response.contains("400"),
                "should reject missing q: {response}"
            );
        });
    }

    // =========================================================================
    // Schema / contract tests (wa-nu4.3.6.4)
    // =========================================================================

    #[test]
    fn health_schema_parseable() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .unwrap();
            let addr = server.bound_addr();

            let response = fetch_health(addr).await;
            let shutdown = server.shutdown().await;
            let response = response.unwrap();
            shutdown.unwrap();

            let body = extract_body(&response);
            let json: serde_json::Value = serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("health response not valid JSON: {e}\nbody: {body}"));

            assert_eq!(json["ok"], true, "health.ok should be true");
            assert!(
                json["version"].is_string(),
                "health.version should be a string"
            );
        });
    }

    #[test]
    fn events_returns_503_without_storage() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .unwrap();
            let addr = server.bound_addr();

            let req = b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let response = fetch_raw(addr, req).await;
            let shutdown = server.shutdown().await;
            let response = response.unwrap();
            shutdown.unwrap();

            assert_eq!(extract_status(&response), 503);

            // Response body should be valid JSON with the error envelope
            let body = extract_body(&response);
            let json: serde_json::Value = serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("events 503 not valid JSON: {e}\nbody: {body}"));
            assert_eq!(json["ok"], false, "error response ok should be false");
            assert!(json["error_code"].is_string(), "should include error_code");
        });
    }

    #[test]
    fn panes_503_has_json_envelope() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .unwrap();
            let addr = server.bound_addr();

            let req = b"GET /panes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let response = fetch_raw(addr, req).await;
            let shutdown = server.shutdown().await;
            let response = response.unwrap();
            shutdown.unwrap();

            let body = extract_body(&response);
            let json: serde_json::Value = serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("panes 503 not valid JSON: {e}\nbody: {body}"));
            assert_eq!(json["ok"], false);
            assert_eq!(json["error_code"], "no_storage");
            assert!(
                json["version"].is_string(),
                "envelope should include version"
            );
        });
    }

    #[test]
    fn unknown_route_returns_404() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .unwrap();
            let addr = server.bound_addr();

            let req = b"GET /not-a-route HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let response = fetch_raw(addr, req).await;
            let shutdown = server.shutdown().await;
            let response = response.unwrap();
            shutdown.unwrap();

            assert_eq!(
                extract_status(&response),
                404,
                "unknown route should 404: {response}"
            );
        });
    }

    #[test]
    fn post_method_not_allowed() {
        run_async_test(async {
            let server = start_web_server(WebServerConfig::default().with_port(0))
                .await
                .unwrap();
            let addr = server.bound_addr();

            let req = b"POST /health HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let response = fetch_raw(addr, req).await;
            let shutdown = server.shutdown().await;
            let response = response.unwrap();
            shutdown.unwrap();

            let status = extract_status(&response);
            assert!(
                status == 404 || status == 405,
                "POST should be rejected (404 or 405), got {status}: {response}"
            );
        });
    }

    #[test]
    fn stream_fetch_prefix_times_out_on_stalled_body() {
        run_async_test(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let server_task = task::spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut req_buf = [0_u8; 512];
                let _ = timeout(Duration::from_millis(250), read(&mut stream, &mut req_buf)).await;
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                    )
                    .await?;
                sleep(Duration::from_secs(1)).await;
                Ok::<(), std::io::Error>(())
            });

            let req = b"GET /stream/events?channel=detections&max_hz=1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let start = Instant::now();
            let response = fetch_stream_prefix(addr, req, Duration::from_millis(120), 256)
                .await
                .unwrap();
            let elapsed = start.elapsed();

            assert!(
                elapsed < Duration::from_millis(700),
                "fetch should return after read timeout, elapsed={elapsed:?}"
            );
            assert!(
                response.contains("HTTP/1.1 200 OK"),
                "expected partial HTTP response headers: {response}"
            );

            let _ = timeout(Duration::from_secs(2), server_task).await;
        });
    }

    #[test]
    fn stream_deltas_emits_schema_and_redacts_content() {
        run_async_test(async {
            let (storage, _tmp) = create_test_storage_with_pane(7).await.unwrap();
            let event_bus = Arc::new(EventBus::new(64));
            let runtime_limits = WebRuntimeLimits {
                max_list_limit: 50,
                default_list_limit: 10,
                max_request_body_bytes: 1024,
                stream_default_max_hz: 200,
                stream_max_max_hz: 200,
                stream_keepalive_secs: 1,
                stream_scan_limit: 4,
                stream_scan_max_pages: 2,
            };
            let server = start_web_server(
                WebServerConfig::default()
                    .with_port(0)
                    .with_runtime_limits(runtime_limits)
                    .with_storage(storage.clone())
                    .with_event_bus(Arc::clone(&event_bus)),
            )
            .await
            .unwrap();
            let addr = server.bound_addr();

            let req = b"GET /stream/deltas?pane_id=7&max_hz=200 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let fetch_task = task::spawn(async move {
                fetch_stream_until_delta_count(addr, req, Duration::from_secs(3), 1).await
            });

            sleep(Duration::from_millis(80)).await;
            let secret = "sk-abc123456789012345678901234567890123456789012345678901";
            let content = format!("auth {secret} ! done");
            let seg = storage.append_segment(7, &content, None).await.unwrap();
            let _ = event_bus.publish(Event::SegmentCaptured {
                pane_id: 7,
                seq: seg.seq,
                content_len: seg.content_len,
            });

            let response = fetch_task.await.unwrap().unwrap();
            server.shutdown().await.unwrap();

            assert!(
                response.contains("text/event-stream"),
                "SSE content-type header expected: {response}"
            );

            let delta_data = response
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .find(|line| line.contains("\"kind\":\"delta\""))
                .expect("missing delta frame in SSE response");
            let frame: serde_json::Value = serde_json::from_str(delta_data).unwrap();

            assert_eq!(frame["schema"], "ft.stream.v1");
            assert_eq!(frame["stream"], "deltas");
            assert_eq!(frame["kind"], "delta");
            assert_eq!(frame["data"]["pane_id"], 7);

            let redacted = frame["data"]["content"]
                .as_str()
                .expect("delta content should be a string");
            assert!(
                redacted.contains("[REDACTED"),
                "content must be redacted: {redacted:?}"
            );
            assert!(
                !redacted.contains("sk-abc123"),
                "raw secret should not appear"
            );
        });
    }

    #[test]
    fn stream_deltas_drains_backlog_beyond_single_scan_window() {
        run_async_test(async {
            let (storage, _tmp) = create_test_storage_with_pane(7).await.unwrap();
            let event_bus = Arc::new(EventBus::new(64));
            let runtime_limits = WebRuntimeLimits {
                max_list_limit: 50,
                default_list_limit: 10,
                max_request_body_bytes: 1024,
                stream_default_max_hz: 1000,
                stream_max_max_hz: 1000,
                stream_keepalive_secs: 1,
                stream_scan_limit: 2,
                stream_scan_max_pages: 1,
            };
            let server = start_web_server(
                WebServerConfig::default()
                    .with_port(0)
                    .with_runtime_limits(runtime_limits)
                    .with_storage(storage.clone())
                    .with_event_bus(Arc::clone(&event_bus)),
            )
            .await
            .unwrap();
            let addr = server.bound_addr();

            let req = b"GET /stream/deltas?pane_id=7&max_hz=1000 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let fetch_task = task::spawn(async move {
                fetch_stream_until_delta_count(addr, req, Duration::from_secs(3), 5).await
            });

            sleep(Duration::from_millis(80)).await;
            let mut expected_segment_ids = Vec::new();
            let mut last_seq = 0_u64;
            let mut last_content_len = 0_usize;
            for i in 0..5_u64 {
                let content = format!("delta backlog payload {i}: {}", "x".repeat(96));
                let seg = storage.append_segment(7, &content, None).await.unwrap();
                expected_segment_ids.push(seg.id);
                last_seq = seg.seq;
                last_content_len = seg.content_len;
            }
            let _ = event_bus.publish(Event::SegmentCaptured {
                pane_id: 7,
                seq: last_seq,
                content_len: last_content_len,
            });

            let response = fetch_task.await.unwrap().unwrap();
            server.shutdown().await.unwrap();

            let delta_segment_ids: Vec<i64> = response
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|frame| frame["kind"] == "delta")
                .map(|frame| {
                    frame["data"]["segment_id"]
                        .as_i64()
                        .expect("delta segment_id should be an integer")
                })
                .collect();

            assert_eq!(
                delta_segment_ids, expected_segment_ids,
                "SSE delta stream should drain all stored backlog pages: {response}"
            );
        });
    }

    #[test]
    fn stream_events_reports_lag_and_releases_subscriber() {
        run_async_test(async {
            let event_bus = Arc::new(EventBus::new(2));
            let server = start_web_server(
                WebServerConfig::default()
                    .with_port(0)
                    .with_event_bus(Arc::clone(&event_bus)),
            )
            .await
            .unwrap();
            let addr = server.bound_addr();

            let req = b"GET /stream/events?channel=detections&max_hz=1000 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let fetch_task = task::spawn(async move {
                fetch_stream_until_kind_count(addr, req, Duration::from_secs(4), "lag", 1).await
            });

            sleep(Duration::from_millis(80)).await;
            for i in 0..64_u64 {
                let detection = Detection {
                    rule_id: format!("core.test:{i}"),
                    agent_type: AgentType::Codex,
                    event_type: "usage_reached".to_string(),
                    severity: Severity::Warning,
                    confidence: 1.0,
                    extracted: serde_json::json!({
                        "token": "sk-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "index": i
                    }),
                    matched_text: format!(
                        "usage reached with secret sk-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb #{i}"
                    ),
                    span: (0, 0),
                };
                let _ = event_bus.publish(Event::PatternDetected {
                    pane_id: 11,
                    pane_uuid: None,
                    detection,
                    event_id: None,
                });
            }

            let response = fetch_task.await.unwrap().unwrap();
            server.shutdown().await.unwrap();

            assert!(
                response.contains("event: lag") || response.contains("\"kind\":\"lag\""),
                "expected lag event in stream output: {response}"
            );

            for _ in 0..20 {
                if event_bus.subscriber_count() == 0 {
                    return;
                }
                sleep(Duration::from_millis(25)).await;
            }

            assert_eq!(
                event_bus.subscriber_count(),
                0,
                "subscriber should be released after disconnect"
            );
        });
    }
}
