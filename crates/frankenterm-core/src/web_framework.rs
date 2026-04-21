//! Shared `fastapi` alias surface for the web server module.
//!
//! Keeps framework dependency boundaries explicit and centralized.
//! Re-exports are consumed by web.rs sub-modules during migration.

use crate::{Error, Result};
use asupersync::net::TcpListener;
use asupersync::runtime::{JoinHandle, Runtime};
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

#[doc(hidden)]
pub use fastapi::ResponseBody as FrameworkResponseBody;
#[doc(hidden)]
pub use fastapi::core::{Body as FrameworkRequestBody, TestClient as FrameworkWebTestClient};
#[doc(hidden)]
pub use fastapi::prelude::{
    App as FrameworkApp, Method as FrameworkMethod, Request as FrameworkRequest,
    RequestContext as FrameworkRequestContext, Response as FrameworkResponse,
    StatusCode as FrameworkStatusCode,
};

#[doc(hidden)]
pub type FrameworkServerJoinResult = std::result::Result<(), ServerError>;

/// Framework-owned runtime state for the feature-gated web server.
///
/// This keeps `fastapi` server/app internals inside the framework seam so the
/// outer `web` module can evolve toward a replacement implementation without
/// carrying transport/runtime details in its primary control surface.
#[doc(hidden)]
pub struct FrameworkWebRuntime {
    app: Arc<App>,
    server: Arc<TcpServer>,
    join: JoinHandle<std::result::Result<(), ServerError>>,
}

impl FrameworkWebRuntime {
    #[doc(hidden)]
    pub async fn start(bind_addr: String, app: App) -> Result<(SocketAddr, Self)> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        Self::start_with_cx(&cx, bind_addr, app).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`start`].
    ///
    /// Threads the caller's cx into the accept loop so the
    /// server honors cancellation/budget/virtual-time at the
    /// framework boundary, rather than spawning a fresh
    /// per-request cx. Also adds checkpoint seams before
    /// startup hooks and before the bind call so a cancelled
    /// caller can bail before claiming resources.
    #[doc(hidden)]
    pub async fn start_with_cx(
        cx: &crate::cx::Cx,
        bind_addr: String,
        app: App,
    ) -> Result<(SocketAddr, Self)> {
        cx.checkpoint()
            .map_err(|err| Error::Runtime(format!("web start cancelled: {err}")))?;

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

        cx.checkpoint()
            .map_err(|err| Error::Runtime(format!("web start cancelled before bind: {err}")))?;

        let app = Arc::new(app);
        let listener = TcpListener::bind(bind_addr.clone())
            .await
            .map_err(Error::Io)?;
        let local_addr = listener.local_addr().map_err(Error::Io)?;

        let server = Arc::new(TcpServer::new(ServerConfig::new(bind_addr)));
        let handler: Arc<dyn Handler> = Arc::clone(&app) as Arc<dyn Handler>;
        let runtime_handle = Runtime::current_handle()
            .ok_or_else(|| Error::Runtime("web runtime unavailable during startup".to_string()))?;

        let join = {
            let server = Arc::clone(&server);
            let child_cx = cx.clone();
            runtime_handle
                .spawn(async move { server.serve_on_handler(&child_cx, listener, handler).await })
        };

        Ok((local_addr, Self { app, server, join }))
    }

    #[doc(hidden)]
    pub fn signal_shutdown(&self) {
        self.server.shutdown();
    }

    #[doc(hidden)]
    pub fn join_handle_mut(&mut self) -> &mut JoinHandle<std::result::Result<(), ServerError>> {
        &mut self.join
    }

    #[doc(hidden)]
    pub async fn finish(self, result: FrameworkServerJoinResult) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.finish_with_cx(&cx, result).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`finish`].
    ///
    /// Processes the join result synchronously (zero-cost on the
    /// error-reporting path), then gates the drain + shutdown
    /// hook execution on cx liveness. If the cx is cancelled
    /// after the join result is decoded, the drain and hooks
    /// are still run — they must always execute to avoid
    /// leaking connections, so cancellation only skips waiting
    /// for the hooks to complete in-order by letting the caller
    /// return early after signalling the error.
    #[doc(hidden)]
    pub async fn finish_with_cx(
        self,
        cx: &crate::cx::Cx,
        result: FrameworkServerJoinResult,
    ) -> Result<()> {
        match result {
            Ok(()) => {}
            Err(ServerError::Shutdown) => {}
            Err(err) => {
                return Err(Error::Runtime(format!("web server error: {err}")));
            }
        }

        let forced = self.server.drain().await;
        if forced > 0 {
            warn!(target: "wa.web", forced, "web server forced closed connections");
        }
        self.app.run_shutdown_hooks().await;
        cx.checkpoint()
            .map_err(|err| Error::Runtime(format!("web finish cancelled after shutdown: {err}")))?;
        Ok(())
    }
}

// ── Helper functions ─────────────────────────────────────────────────────

/// Build a JSON response with the given status code.
pub fn json_response_with_status<T: serde::Serialize>(status: StatusCode, payload: &T) -> Response {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    Response::with_status(status)
        .header("content-type", b"application/json".to_vec())
        .body(ResponseBody::Bytes(body))
}

/// Build an SSE streaming response with standard headers.
pub fn sse_stream_response<S>(stream: S) -> Response
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
