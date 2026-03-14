//! Shared `fastapi` alias surface for the web server module.
//!
//! Keeps framework dependency boundaries explicit and centralized.
//! Re-exports are consumed by web.rs sub-modules during migration.

use crate::runtime_compat::task;
use crate::{Error, Result};
use asupersync::net::TcpListener;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::warn;

#[allow(unused_imports)]
pub(crate) use fastapi::core::{BoxFuture, ControlFlow, Cx, Handler, Middleware, StartupOutcome};
#[allow(unused_imports)]
pub(crate) use fastapi::http::QueryString;
#[allow(unused_imports)]
pub(crate) use fastapi::prelude::{App, Method, Request, RequestContext, Response, StatusCode};
#[allow(unused_imports)]
pub(crate) use fastapi::{ResponseBody, ServerConfig, ServerError, TcpServer};

pub(crate) type FrameworkServerJoinResult =
    std::result::Result<std::result::Result<(), ServerError>, task::JoinError>;

/// Framework-owned runtime state for the feature-gated web server.
///
/// This keeps `fastapi` server/app internals inside the framework seam so the
/// outer `web` module can evolve toward a replacement implementation without
/// carrying transport/runtime details in its primary control surface.
pub(crate) struct FrameworkWebRuntime {
    app: Arc<App>,
    server: Arc<TcpServer>,
    join: task::JoinHandle<std::result::Result<(), ServerError>>,
}

impl FrameworkWebRuntime {
    pub(crate) async fn start(bind_addr: String, app: App) -> Result<(SocketAddr, Self)> {
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
        let listener = TcpListener::bind(bind_addr.clone())
            .await
            .map_err(Error::Io)?;
        let local_addr = listener.local_addr().map_err(Error::Io)?;

        let server = Arc::new(TcpServer::new(ServerConfig::new(bind_addr)));
        let handler: Arc<dyn Handler> = Arc::clone(&app) as Arc<dyn Handler>;

        let join = {
            let server = Arc::clone(&server);
            task::spawn(async move {
                let cx = crate::cx::for_request();
                server.serve_on_handler(&cx, listener, handler).await
            })
        };

        Ok((local_addr, Self { app, server, join }))
    }

    pub(crate) fn signal_shutdown(&self) {
        self.server.shutdown();
    }

    pub(crate) fn join_handle_mut(
        &mut self,
    ) -> &mut task::JoinHandle<std::result::Result<(), ServerError>> {
        &mut self.join
    }

    pub(crate) async fn finish(self, result: FrameworkServerJoinResult) -> Result<()> {
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

        let forced = self.server.drain().await;
        if forced > 0 {
            warn!(target: "wa.web", forced, "web server forced closed connections");
        }
        self.app.run_shutdown_hooks().await;
        Ok(())
    }
}

// ── Helper functions ─────────────────────────────────────────────────────

/// Build a JSON response with the given status code.
pub(crate) fn json_response_with_status<T: serde::Serialize>(
    status: StatusCode,
    payload: &T,
) -> Response {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    Response::with_status(status)
        .header("content-type", b"application/json".to_vec())
        .body(ResponseBody::Bytes(body))
}

/// Build an SSE streaming response with standard headers.
pub(crate) fn sse_stream_response<S>(stream: S) -> Response
where
    S: asupersync::stream::Stream<Item = Vec<u8>> + Send + 'static,
{
    Response::with_status(StatusCode::OK)
        .header("content-type", b"text/event-stream".to_vec())
        .header("cache-control", b"no-cache".to_vec())
        .header("connection", b"keep-alive".to_vec())
        .header("x-accel-buffering", b"no".to_vec())
        .body(ResponseBody::stream(stream))
}
