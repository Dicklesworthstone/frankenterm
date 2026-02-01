//! Web server scaffolding for wa (feature-gated: web).
//!
//! Provides a minimal `wa web` HTTP server with /health and lifecycle
//! management for graceful shutdown.

use crate::{Error, Result, VERSION};
use asupersync::net::TcpListener;
use fastapi::core::{ControlFlow, Cx, Handler, Middleware, StartupOutcome};
use fastapi::prelude::{App, Method, Request, RequestContext, Response};
use fastapi::{ServerConfig, ServerError, TcpServer};
use serde::Serialize;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8000;

/// Configuration for the web server.
#[derive(Debug, Clone)]
pub struct WebServerConfig {
    host: String,
    port: u16,
}

impl WebServerConfig {
    /// Create a new config with the default localhost host.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port,
        }
    }

    /// Override the port.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Override the bind host.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    #[must_use]
    fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl Default for WebServerConfig {
    fn default() -> Self {
        Self::new(DEFAULT_PORT)
    }
}

/// Handle to a running web server.
pub struct WebServerHandle {
    bound_addr: SocketAddr,
    server: Arc<TcpServer>,
    app: Arc<App>,
    join: tokio::task::JoinHandle<std::result::Result<(), ServerError>>,
}

impl WebServerHandle {
    /// The address the server actually bound to.
    #[must_use]
    pub fn bound_addr(&self) -> SocketAddr {
        self.bound_addr
    }

    /// Trigger graceful shutdown and wait for completion.
    pub async fn shutdown(self) -> Result<()> {
        self.server.shutdown();
        poke_listener(self.bound_addr);
        handle_server_exit(self.join.await, &self.server, &self.app).await
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestStart(Instant);

#[derive(Debug, Clone, Default)]
struct RequestSpanLogger;

impl Middleware for RequestSpanLogger {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a mut Request,
    ) -> fastapi::core::BoxFuture<'a, ControlFlow> {
        req.insert_extension(RequestStart(Instant::now()));
        Box::pin(async { ControlFlow::Continue })
    }

    fn after<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a Request,
        response: Response,
    ) -> fastapi::core::BoxFuture<'a, Response> {
        let start = req
            .get_extension::<RequestStart>()
            .map(|s| s.0)
            .unwrap_or_else(Instant::now);
        let duration = start.elapsed();
        let method = req.method();
        let path = req.path();
        let status = response.status().as_u16();

        info!(
            target: "wa.web",
            method = %method,
            path = %path,
            status,
            duration_ms = duration.as_millis(),
            "web request"
        );

        Box::pin(async move { response })
    }

    fn name(&self) -> &'static str {
        "RequestSpanLogger"
    }
}

fn build_app() -> App {
    App::builder()
        .middleware(RequestSpanLogger::default())
        .route(
            "/health",
            Method::Get,
            |_ctx: &RequestContext, _req: &mut Request| async { health_response() },
        )
        .build()
}

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
    version: &'static str,
}

fn health_response() -> Response {
    let payload = HealthResponse {
        ok: true,
        version: VERSION,
    };
    Response::json(&payload).unwrap_or_else(|_| Response::internal_error())
}

/// Start the web server and return a handle for shutdown.
pub async fn start_web_server(config: WebServerConfig) -> Result<WebServerHandle> {
    let app = build_app();

    match app.run_startup_hooks().await {
        StartupOutcome::Success => {}
        StartupOutcome::PartialSuccess { warnings } => {
            warn!(target: "wa.web", warnings, "web startup hooks had warnings");
        }
        StartupOutcome::Aborted(err) => {
            return Err(Error::Runtime(format!(
                "web startup aborted: {}",
                err.message
            )));
        }
    }

    let app = Arc::new(app);
    let bind_addr = config.bind_addr();
    let listener = TcpListener::bind(bind_addr.clone())
        .await
        .map_err(Error::Io)?;
    let local_addr = listener.local_addr().map_err(Error::Io)?;

    let server = Arc::new(TcpServer::new(ServerConfig::new(bind_addr)));
    let handler: Arc<dyn Handler> = Arc::clone(&app) as Arc<dyn Handler>;

    let server_task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            let cx = Cx::for_testing();
            server.serve_on_handler(&cx, listener, handler).await
        })
    };

    info!(
        target: "wa.web",
        bound_addr = %local_addr,
        "web server listening"
    );

    Ok(WebServerHandle {
        bound_addr: local_addr,
        server,
        app,
        join: server_task,
    })
}

/// Run the web server until Ctrl+C, then shut down gracefully.
pub async fn run_web_server(config: WebServerConfig) -> Result<()> {
    let WebServerHandle {
        bound_addr,
        server,
        app,
        mut join,
    } = start_web_server(config).await?;

    println!("wa web listening on http://{bound_addr}");

    tokio::select! {
        result = &mut join => {
            handle_server_exit(result, &server, &app).await?;
        }
        shutdown = wait_for_shutdown_signal() => {
            shutdown?;
            server.shutdown();
            poke_listener(bound_addr);
            handle_server_exit(join.await, &server, &app).await?;
        }
    }

    Ok(())
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut term = signal(SignalKind::terminate())
            .map_err(|e| Error::Runtime(format!("SIGTERM handler failed: {e}")))?;

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| Error::Runtime(format!("Ctrl+C handler failed: {e}")))?;
        Ok(())
    }
}

async fn handle_server_exit(
    result: std::result::Result<std::result::Result<(), ServerError>, tokio::task::JoinError>,
    server: &Arc<TcpServer>,
    app: &Arc<App>,
) -> Result<()> {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(ServerError::Shutdown)) => {}
        Ok(Err(err)) => {
            return Err(Error::Runtime(format!("web server error: {err}")));
        }
        Err(err) => {
            return Err(Error::Runtime(format!("web server join error: {err}")));
        }
    }

    let forced = server.drain().await;
    if forced > 0 {
        warn!(target: "wa.web", forced, "web server forced closed connections");
    }
    app.run_shutdown_hooks().await;
    Ok(())
}

fn poke_listener(addr: SocketAddr) {
    let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
}
