#[cfg(feature = "web")]
mod web_tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use frankenterm_core::events::{Event, EventBus};
    use frankenterm_core::patterns::{AgentType, Detection, Severity};
    use frankenterm_core::runtime_compat::{sleep, timeout};
    use frankenterm_core::storage::{PaneRecord, StorageHandle};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use frankenterm_core::web::{WebServerConfig, start_web_server};

    /// Extract the HTTP response body from a raw HTTP response string.
    fn extract_body(raw: &str) -> &str {
        raw.split("\r\n\r\n").nth(1).unwrap_or("")
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
        let mut last_err = None;

        for _ in 0..50 {
            match TcpStream::connect(addr).await {
                Ok(mut stream) => {
                    stream.write_all(request).await?;
                    let mut buf = Vec::new();
                    stream.read_to_end(&mut buf).await?;
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

    async fn fetch_raw(addr: SocketAddr, raw_request: &[u8]) -> std::io::Result<String> {
        let mut last_err = None;
        for _ in 0..50 {
            match TcpStream::connect(addr).await {
                Ok(mut stream) => {
                    stream.write_all(raw_request).await?;
                    let mut buf = Vec::new();
                    stream.read_to_end(&mut buf).await?;
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

    async fn fetch_stream_prefix(
        addr: SocketAddr,
        raw_request: &[u8],
        timeout: Duration,
        min_bytes: usize,
    ) -> std::io::Result<String> {
        let mut last_err = None;
        for _ in 0..50 {
            match TcpStream::connect(addr).await {
                Ok(mut stream) => {
                    stream.write_all(raw_request).await?;
                    let deadline = Instant::now() + timeout;
                    let mut buf = Vec::new();
                    while buf.len() < min_bytes {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let mut chunk = [0_u8; 2048];
                        match timeout(deadline - now, stream.read(&mut chunk)).await {
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

    #[tokio::test]
    async fn web_health_ephemeral_port() -> Result<(), Box<dyn std::error::Error>> {
        let server = start_web_server(WebServerConfig::default().with_port(0)).await?;
        let addr = server.bound_addr();

        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

        let response = fetch_health(addr).await;
        let shutdown = server.shutdown().await;

        let response = response?;
        shutdown?;

        assert!(response.contains("200"));
        assert!(response.contains("\"ok\":true"));
        Ok(())
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

    #[tokio::test]
    async fn public_bind_rejected_without_opt_in() {
        let config = WebServerConfig::new(0).with_host("0.0.0.0");
        let result = start_web_server(config).await;
        assert!(result.is_err(), "public bind should be rejected by default");
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("refusing to bind") || err_msg.contains("dangerous"),
            "error should mention public bind safety: {err_msg}"
        );
    }

    #[tokio::test]
    async fn public_bind_allowed_with_explicit_opt_in() {
        // We use port 0 so this doesn't actually need a specific interface
        let config = WebServerConfig::new(0)
            .with_host("0.0.0.0")
            .with_dangerous_public_bind();
        let result = start_web_server(config).await;
        assert!(result.is_ok(), "public bind should succeed with opt-in");
        if let Ok(server) = result {
            let _ = server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn panes_returns_503_without_storage() -> Result<(), Box<dyn std::error::Error>> {
        let server = start_web_server(WebServerConfig::default().with_port(0)).await?;
        let addr = server.bound_addr();

        let req = b"GET /panes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let response = fetch_raw(addr, req).await;
        let shutdown = server.shutdown().await;

        let response = response?;
        shutdown?;

        assert!(
            response.contains("503"),
            "should return 503 without storage: {response}"
        );
        assert!(
            response.contains("no_storage"),
            "should include error code: {response}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn search_requires_query_param() -> Result<(), Box<dyn std::error::Error>> {
        let server = start_web_server(WebServerConfig::default().with_port(0)).await?;
        let addr = server.bound_addr();

        // Request /search without ?q= parameter
        let req = b"GET /search HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let response = fetch_raw(addr, req).await;
        let shutdown = server.shutdown().await;

        let response = response?;
        shutdown?;

        // Without storage, 503 takes precedence over the missing-query 400.
        assert!(
            response.contains("503") || response.contains("400"),
            "should reject missing q: {response}"
        );
        Ok(())
    }

    // =========================================================================
    // Schema / contract tests (wa-nu4.3.6.4)
    // =========================================================================

    #[tokio::test]
    async fn health_schema_parseable() -> Result<(), Box<dyn std::error::Error>> {
        let server = start_web_server(WebServerConfig::default().with_port(0)).await?;
        let addr = server.bound_addr();

        let response = fetch_health(addr).await;
        let shutdown = server.shutdown().await;
        let response = response?;
        shutdown?;

        let body = extract_body(&response);
        let json: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("health response not valid JSON: {e}\nbody: {body}"));

        assert_eq!(json["ok"], true, "health.ok should be true");
        assert!(
            json["version"].is_string(),
            "health.version should be a string"
        );
        Ok(())
    }

    #[tokio::test]
    async fn events_returns_503_without_storage() -> Result<(), Box<dyn std::error::Error>> {
        let server = start_web_server(WebServerConfig::default().with_port(0)).await?;
        let addr = server.bound_addr();

        let req = b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let response = fetch_raw(addr, req).await;
        let shutdown = server.shutdown().await;
        let response = response?;
        shutdown?;

        assert_eq!(extract_status(&response), 503);

        // Response body should be valid JSON with the error envelope
        let body = extract_body(&response);
        let json: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("events 503 not valid JSON: {e}\nbody: {body}"));
        assert_eq!(json["ok"], false, "error response ok should be false");
        assert!(json["error_code"].is_string(), "should include error_code");
        Ok(())
    }

    #[tokio::test]
    async fn panes_503_has_json_envelope() -> Result<(), Box<dyn std::error::Error>> {
        let server = start_web_server(WebServerConfig::default().with_port(0)).await?;
        let addr = server.bound_addr();

        let req = b"GET /panes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let response = fetch_raw(addr, req).await;
        let shutdown = server.shutdown().await;
        let response = response?;
        shutdown?;

        let body = extract_body(&response);
        let json: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("panes 503 not valid JSON: {e}\nbody: {body}"));
        assert_eq!(json["ok"], false);
        assert_eq!(json["error_code"], "no_storage");
        assert!(
            json["version"].is_string(),
            "envelope should include version"
        );
        Ok(())
    }

    #[tokio::test]
    async fn unknown_route_returns_404() -> Result<(), Box<dyn std::error::Error>> {
        let server = start_web_server(WebServerConfig::default().with_port(0)).await?;
        let addr = server.bound_addr();

        let req = b"GET /not-a-route HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let response = fetch_raw(addr, req).await;
        let shutdown = server.shutdown().await;
        let response = response?;
        shutdown?;

        assert_eq!(
            extract_status(&response),
            404,
            "unknown route should 404: {response}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn post_method_not_allowed() -> Result<(), Box<dyn std::error::Error>> {
        let server = start_web_server(WebServerConfig::default().with_port(0)).await?;
        let addr = server.bound_addr();

        let req = b"POST /health HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let response = fetch_raw(addr, req).await;
        let shutdown = server.shutdown().await;
        let response = response?;
        shutdown?;

        let status = extract_status(&response);
        assert!(
            status == 404 || status == 405,
            "POST should be rejected (404 or 405), got {status}: {response}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stream_deltas_emits_schema_and_redacts_content()
    -> Result<(), Box<dyn std::error::Error>> {
        let (storage, _tmp) = create_test_storage_with_pane(7).await?;
        let event_bus = Arc::new(EventBus::new(64));
        let server = start_web_server(
            WebServerConfig::default()
                .with_port(0)
                .with_storage(storage.clone())
                .with_event_bus(Arc::clone(&event_bus)),
        )
        .await?;
        let addr = server.bound_addr();

        let req = b"GET /stream/deltas?pane_id=7&max_hz=200 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let fetch_task = tokio::spawn(async move {
            fetch_stream_prefix(addr, req, Duration::from_secs(3), 512).await
        });

        sleep(Duration::from_millis(80)).await;
        let secret = "auth token sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let seg = storage.append_segment(7, secret, None).await?;
        let _ = event_bus.publish(Event::SegmentCaptured {
            pane_id: 7,
            seq: seg.seq,
            content_len: seg.content_len,
        });

        let response = fetch_task.await??;
        server.shutdown().await?;

        assert!(
            response.contains("text/event-stream"),
            "SSE content-type header expected: {response}"
        );

        let delta_data = response
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .find(|line| line.contains("\"kind\":\"delta\""))
            .ok_or("missing delta frame in SSE response")?;
        let frame: serde_json::Value = serde_json::from_str(delta_data)?;

        assert_eq!(frame["schema"], "ft.stream.v1");
        assert_eq!(frame["stream"], "deltas");
        assert_eq!(frame["kind"], "delta");
        assert_eq!(frame["data"]["pane_id"], 7);

        let redacted = frame["data"]["content"]
            .as_str()
            .ok_or("delta content should be a string")?;
        assert!(redacted.contains("[REDACTED]"), "content must be redacted");
        assert!(
            !redacted.contains("sk-aaaaaaaa"),
            "raw secret should not appear"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stream_events_reports_lag_and_releases_subscriber()
    -> Result<(), Box<dyn std::error::Error>> {
        let event_bus = Arc::new(EventBus::new(2));
        let server = start_web_server(
            WebServerConfig::default()
                .with_port(0)
                .with_event_bus(Arc::clone(&event_bus)),
        )
        .await?;
        let addr = server.bound_addr();

        let req = b"GET /stream/events?channel=detections&max_hz=1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let fetch_task = tokio::spawn(async move {
            fetch_stream_prefix(addr, req, Duration::from_secs(4), 640).await
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

        let response = fetch_task.await??;
        server.shutdown().await?;

        assert!(
            response.contains("event: lag") || response.contains("\"kind\":\"lag\""),
            "expected lag event in stream output: {response}"
        );

        for _ in 0..20 {
            if event_bus.subscriber_count() == 0 {
                return Ok(());
            }
            sleep(Duration::from_millis(25)).await;
        }

        assert_eq!(
            event_bus.subscriber_count(),
            0,
            "subscriber should be released after disconnect"
        );
        Ok(())
    }
}
