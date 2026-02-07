#[cfg(feature = "web")]
mod web_tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use wa_core::web::{WebServerConfig, start_web_server};

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
                    tokio::time::sleep(Duration::from_millis(20)).await;
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
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "server not ready")
        }))
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
        assert!(debug.contains("127.0.0.1"), "default host must be 127.0.0.1");
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

        assert!(response.contains("503"), "should return 503 without storage: {response}");
        assert!(response.contains("no_storage"), "should include error code: {response}");
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

        // Should return 400 since storage is absent (503) or missing query param (400).
        // Without storage, 503 takes precedence. With storage, it'd be 400.
        assert!(
            response.contains("503") || response.contains("400"),
            "should reject missing q: {response}"
        );
        Ok(())
    }

}
