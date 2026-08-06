//! Shared `fastapi` alias surface for the web server module.
//!
//! Keeps framework dependency boundaries explicit and centralized.
//! Re-exports are consumed by web.rs sub-modules during migration.

use crate::{Error, Result};
use asupersync::net::TcpListener;
use asupersync::runtime::{JoinHandle, Runtime};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

#[allow(unused_imports)]
pub(crate) use fastapi::core::{BoxFuture, ControlFlow, Cx, Middleware, StartupOutcome};
#[allow(unused_imports)]
pub(crate) use fastapi::http::QueryString;
#[allow(unused_imports)]
pub(crate) use fastapi::prelude::{App, Method, Request, RequestContext, Response, StatusCode};
#[allow(unused_imports)]
pub(crate) use fastapi::{ResponseBody, ServerConfig, ServerError, TcpServer};

#[doc(hidden)]
pub use fastapi::ResponseBody as FrameworkResponseBody;
#[doc(hidden)]
pub use fastapi::core::StartupHookError as FrameworkStartupHookError;
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

/// Idle request reads are deliberately short for the localhost-only control
/// plane. Besides releasing abandoned sockets promptly, this leaves enough
/// time inside the connection-drain window for a stalled read to terminate.
const WEB_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(1);

/// FastAPI applies this bound while its concurrent accept loop drains spawned
/// connection tasks. FrankenTerm applies the same bound once more after the
/// server task joins, covering fatal accept-loop exits that bypass FastAPI's
/// internal drain branch. Active-connection waiting is therefore bounded by
/// at most twice this duration; async application shutdown hooks are separate
/// user code and cannot be forcibly time-bounded at this seam.
const WEB_CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const WEB_CONNECTION_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Framework-owned runtime state for the feature-gated web server.
///
/// This keeps `fastapi` server/app internals inside the framework seam so the
/// outer `web` module can evolve toward a replacement implementation without
/// carrying transport/runtime details in its primary control surface.
///
/// The server uses FastAPI's App-aware concurrent accept loop so an idle or slow
/// connection cannot serialize unrelated control-plane requests. Ownership is
/// fail-safe: dropping an unfinished runtime synchronously signals shutdown and
/// wakes the listener. The detached server task then gets a bounded opportunity
/// to drain its connection tasks, but Drop cannot await that drain or lifecycle
/// hooks. Full graceful cleanup still requires [`Self::finish_with_cx`].
///
/// FastAPI keeps spawned per-connection handles private. FrankenTerm can stop
/// and join the accept task, close its listener, and wait on FastAPI's public
/// active-connection counter, but it cannot abort or join an arbitrary handler
/// that ignores its Cx. If such a handler survives both drain bounds,
/// [`Self::finish_with_cx`] returns a truthful error after containing every
/// resource directly owned by this wrapper; the upstream-owned connection task
/// may remain scheduled until that handler cooperatively returns.
#[doc(hidden)]
pub struct FrameworkWebRuntime {
    app: Arc<App>,
    server: Arc<TcpServer>,
    join: JoinHandle<std::result::Result<(), ServerError>>,
    bound_addr: SocketAddr,
    cleanup_complete: bool,
}

async fn rollback_started_app<T>(app: &App, primary_error: Error) -> Result<T> {
    // FastAPI shutdown hooks are infallible at this API boundary: individual
    // hook failures are logged internally. Preserve the primary startup error
    // while unconditionally pairing any attempted startup lifecycle with its
    // registered shutdown hooks.
    app.run_shutdown_hooks().await;
    Err(primary_error)
}

fn listener_wake_addr(addr: SocketAddr) -> SocketAddr {
    let ip = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, addr.port())
}

fn validate_unauthenticated_bound_addr(bound_addr: SocketAddr) -> Result<()> {
    if bound_addr.ip().is_loopback() {
        return Ok(());
    }

    Err(Error::runtime_backend(
        "web bind validation",
        format!(
            "refusing resolved non-loopback web listener {bound_addr}: the current web API has no authentication boundary"
        ),
    ))
}

fn wake_listener(addr: SocketAddr) {
    let wake_addr = listener_wake_addr(addr);
    if let Ok(stream) = TcpStream::connect_timeout(&wake_addr, Duration::from_millis(200)) {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ConnectionDrainOutcome {
    Drained,
    TimedOut {
        remaining_connections: u64,
    },
    TimerFailed {
        remaining_connections: u64,
        error: String,
    },
}

async fn wait_for_connections_to_drain_with<C, W, WaitFuture>(
    mut current_connections: C,
    timeout: Duration,
    poll_interval: Duration,
    mut wait: W,
) -> ConnectionDrainOutcome
where
    C: FnMut() -> u64,
    W: FnMut(Duration) -> WaitFuture,
    WaitFuture: Future<Output = std::result::Result<(), String>>,
{
    let mut remaining = timeout;
    loop {
        let remaining_connections = current_connections();
        if remaining_connections == 0 {
            return ConnectionDrainOutcome::Drained;
        }
        if remaining.is_zero() {
            return ConnectionDrainOutcome::TimedOut {
                remaining_connections,
            };
        }
        if poll_interval.is_zero() {
            return ConnectionDrainOutcome::TimerFailed {
                remaining_connections,
                error: "connection-drain poll interval must be non-zero".to_string(),
            };
        }
        let wait_duration = poll_interval.min(remaining);
        if let Err(error) = wait(wait_duration).await {
            return ConnectionDrainOutcome::TimerFailed {
                remaining_connections,
                error,
            };
        }
        remaining = remaining.saturating_sub(wait_duration);
    }
}

async fn wait_for_connections_to_drain(
    cx: &crate::cx::Cx,
    server: &TcpServer,
) -> ConnectionDrainOutcome {
    wait_for_connections_to_drain_with(
        || server.current_connections(),
        WEB_CONNECTION_DRAIN_TIMEOUT,
        WEB_CONNECTION_DRAIN_POLL_INTERVAL,
        // `sleep_with_cx` deliberately ignores direct cancellation and only
        // respects the supplied runtime budget, so this bounded cleanup wait
        // completes after caller cancellation without minting a fresh
        // full-capability test context. An exhausted budget is reported as a
        // truthful timer failure instead of escalating authority.
        |duration| crate::runtime_async::sleep_with_cx(cx, duration),
    )
    .await
}

impl Drop for FrameworkWebRuntime {
    fn drop(&mut self) {
        if self.cleanup_complete {
            return;
        }

        // Cleanup signalling is the safety action; emit telemetry only after
        // it has been requested so diagnostics can never precede containment.
        self.signal_shutdown();
        warn!(
            target: "wa.web",
            bound_addr = %self.bound_addr,
            "unfinished web runtime dropped; signalling shutdown and waking listener"
        );
    }
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
    /// concurrent server honors cancellation/budget/virtual-time at the
    /// framework boundary, rather than spawning a fresh
    /// per-request cx. Also adds checkpoint seams before
    /// startup hooks and around bind/spawn so a cancelled caller
    /// cannot silently leave resources behind. Once startup hooks
    /// have begun, every later returned failure runs shutdown hooks before
    /// returning. The primary startup error is preserved; a rare
    /// post-spawn rollback error is emitted as secondary telemetry.
    ///
    /// Rust has no async Drop. If this startup future itself is dropped while
    /// a startup or rollback hook is pending, the remaining async lifecycle
    /// work cannot be resumed safely. Callers that acquire resources in hooks
    /// must await startup to completion. Once the runtime below is constructed,
    /// its Drop guard can at least signal shutdown and wake the accept loop.
    #[doc(hidden)]
    pub async fn start_with_cx(
        cx: &crate::cx::Cx,
        bind_addr: String,
        app: App,
    ) -> Result<(SocketAddr, Self)> {
        cx.checkpoint()
            .map_err(|err| Error::runtime_cancelled("web start", err.to_string()))?;

        match app.run_startup_hooks().await {
            StartupOutcome::Success => {}
            StartupOutcome::PartialSuccess { warnings } => {
                warn!(target: "wa.web", warnings, "web startup hooks had warnings");
            }
            StartupOutcome::Aborted(err) => {
                let primary_error = Error::runtime_backend(
                    "web startup hooks",
                    format!("web startup aborted: {err}"),
                );
                return rollback_started_app(&app, primary_error).await;
            }
        }

        if let Err(err) = cx.checkpoint() {
            let primary_error =
                Error::runtime_cancelled("web start before bind", err.to_string());
            return rollback_started_app(&app, primary_error).await;
        }

        let app = Arc::new(app);
        let listener = match TcpListener::bind(bind_addr.clone()).await {
            Ok(listener) => listener,
            Err(err) => return rollback_started_app(app.as_ref(), Error::Io(err)).await,
        };
        let local_addr = match listener.local_addr() {
            Ok(local_addr) => local_addr,
            Err(err) => {
                drop(listener);
                return rollback_started_app(app.as_ref(), Error::Io(err)).await;
            }
        };

        // The configured `localhost` name is only a pre-bind candidate. Check
        // the listener's resolved address before spawning the accept task so a
        // surprising resolver entry never creates even a transient public
        // unauthenticated listener.
        if let Err(primary_error) = validate_unauthenticated_bound_addr(local_addr) {
            drop(listener);
            return rollback_started_app(app.as_ref(), primary_error).await;
        }

        if let Err(err) = cx.checkpoint() {
            drop(listener);
            let primary_error = Error::runtime_cancelled("web start after bind", err.to_string());
            return rollback_started_app(app.as_ref(), primary_error).await;
        }

        let server_config = ServerConfig::new(bind_addr)
            .with_keep_alive_timeout(WEB_KEEP_ALIVE_TIMEOUT)
            .with_drain_timeout(WEB_CONNECTION_DRAIN_TIMEOUT);
        let server = Arc::new(TcpServer::new(server_config));
        let Some(runtime_handle) = Runtime::current_handle() else {
            drop(listener);
            let primary_error =
                Error::runtime_backend("web runtime", "unavailable during startup");
            return rollback_started_app(app.as_ref(), primary_error).await;
        };

        if let Err(err) = cx.checkpoint() {
            drop(listener);
            let primary_error =
                Error::runtime_cancelled("web start before serve", err.to_string());
            return rollback_started_app(app.as_ref(), primary_error).await;
        }

        let join = {
            let serve_server = Arc::clone(&server);
            let serve_app = Arc::clone(&app);
            let child_cx = cx.clone();
            let serve = async move {
                serve_server
                    .serve_on_app_concurrent(&child_cx, listener, serve_app)
                    .await
            };
            match runtime_handle.try_spawn(serve) {
                Ok(join) => join,
                Err(err) => {
                    let primary_error = Error::runtime_backend(
                        "web runtime spawn",
                        format!("server task admission failed: {err}"),
                    );
                    return rollback_started_app(app.as_ref(), primary_error).await;
                }
            }
        };

        let mut runtime = Self {
            app,
            server,
            join,
            bound_addr: local_addr,
            cleanup_complete: false,
        };

        if let Err(err) = cx.checkpoint() {
            let primary_error = Error::runtime_cancelled("web start after spawn", err.to_string());
            runtime.signal_shutdown();
            let join_result = runtime.join_handle_mut().await;
            if let Err(cleanup_error) = runtime.finish_with_cx(cx, join_result).await {
                warn!(
                    target: "wa.web",
                    error = %cleanup_error,
                    primary_error = %primary_error,
                    "web startup rollback cleanup failed; preserving primary startup error"
                );
            }
            return Err(primary_error);
        }

        Ok((local_addr, runtime))
    }

    #[doc(hidden)]
    /// Signal server shutdown and wake a potentially blocked accept loop.
    pub fn signal_shutdown(&self) {
        self.server.shutdown();
        if !self.join.is_finished() {
            wake_listener(self.bound_addr);
        }
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
    /// Defers any non-shutdown join error until after a bounded connection
    /// drain and framework shutdown hooks. Cleanup is unconditional. Error
    /// precedence is server join failure, then a drain-timer failure or drain
    /// timeout with live connections, then cleanup-context cancellation.
    /// Only connection draining is bounded: pinned FastAPI exposes shutdown
    /// hooks as one opaque future, so an arbitrary async hook can still wait
    /// indefinitely. Dropping that future on a timeout would silently discard
    /// later cleanup hooks, while detaching it would lose structured ownership;
    /// ft-interactive-systems-performance-4tenz.33.2 tracks the required owned
    /// hook-runner API. Do not describe this method as an end-to-end bounded
    /// lifecycle boundary until that blocker closes.
    ///
    /// Dropping this future before cleanup completes invokes the runtime's
    /// synchronous Drop fallback, which signals shutdown and wakes the listener.
    /// Rust Drop cannot await the join, drain, or async hooks, so callers that
    /// require full cleanup must await this method to completion.
    #[doc(hidden)]
    pub async fn finish_with_cx(
        mut self,
        cx: &crate::cx::Cx,
        result: FrameworkServerJoinResult,
    ) -> Result<()> {
        let deferred_join_error = match result {
            Ok(()) | Err(ServerError::Shutdown) => None,
            Err(err) => Some(Error::runtime_backend("web server", err.to_string())),
        };

        // The App-aware concurrent accept loop performs its own bounded drain
        // on ordinary shutdown. A fatal accept error can bypass that branch,
        // so perform one additional non-blocking bounded wait here. Do not use
        // FastAPI's public `drain()` helper: it sleeps the executor thread and
        // would prevent connection tasks from making progress on a
        // current-thread runtime.
        self.server.shutdown();
        let deferred_drain_error = match wait_for_connections_to_drain(cx, &self.server).await {
            ConnectionDrainOutcome::Drained => None,
            ConnectionDrainOutcome::TimedOut {
                remaining_connections,
            } => {
                let drain_timeout_ms = WEB_CONNECTION_DRAIN_TIMEOUT.as_millis();
                warn!(
                    target: "wa.web",
                    active_connections = remaining_connections,
                    post_join_drain_timeout_ms = %drain_timeout_ms,
                    "web server drain timed out with active connections"
                );
                Some(Error::runtime_backend(
                    "web server drain",
                    format!(
                        "{remaining_connections} active connection(s) survived the \
                         {drain_timeout_ms} ms post-join drain; FastAPI exposes no \
                         public handle to cancel or join the surviving task(s)"
                    ),
                ))
            }
            ConnectionDrainOutcome::TimerFailed {
                remaining_connections,
                error,
            } => {
                warn!(
                    target: "wa.web",
                    active_connections = remaining_connections,
                    error = %error,
                    "web server connection-drain timer failed"
                );
                Some(Error::runtime_backend(
                    "web server drain timer",
                    format!(
                        "timer failed with {remaining_connections} active connection(s): {error}"
                    ),
                ))
            }
        };
        self.app.run_shutdown_hooks().await;
        self.cleanup_complete = true;

        if let Some(error) = deferred_join_error {
            return Err(error);
        }
        if let Some(error) = deferred_drain_error {
            return Err(error);
        }

        cx.checkpoint().map_err(|err| {
            Error::runtime_cancelled("web finish after shutdown", err.to_string())
        })?;
        Ok(())
    }
}

// ── Helper functions ─────────────────────────────────────────────────────

/// Build a JSON response with the given status code.
pub fn json_response_with_status<T: serde::Serialize>(status: StatusCode, payload: &T) -> Response {
    let (status, body) = match serde_json::to_vec(payload) {
        Ok(body) => (status, body),
        Err(_) => {
            warn!(
                target: "wa.web",
                "JSON response serialization failed; returning finite internal error"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                br#"{"error":"response_serialization_failed"}"#.to_vec(),
            )
        }
    };
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

#[cfg(test)]
mod tests {
    use super::{
        ConnectionDrainOutcome, ResponseBody, StatusCode, json_response_with_status,
        listener_wake_addr, validate_unauthenticated_bound_addr,
        wait_for_connections_to_drain_with,
    };
    use crate::runtime_async::{CompatRuntime, RuntimeBuilder};
    use std::cell::Cell;
    use std::net::SocketAddr;
    use std::time::Duration;

    struct SerializationFailure;

    impl serde::Serialize for SerializationFailure {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(<S::Error as serde::ser::Error>::custom(
                "credential-bearing serializer failure",
            ))
        }
    }

    #[test]
    fn json_serialization_failure_returns_finite_valid_error_response() {
        let response = json_response_with_status(StatusCode::OK, &SerializationFailure);

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        match response.body_ref() {
            ResponseBody::Bytes(body) => {
                assert_eq!(
                    body.as_slice(),
                    br#"{"error":"response_serialization_failed"}"#
                );
                assert!(!body
                    .windows(b"credential-bearing".len())
                    .any(|window| window == b"credential-bearing"));
            }
            other => panic!("expected finite JSON error body, got {other:?}"),
        }
    }

    #[test]
    fn wildcard_listener_wake_addresses_use_matching_loopback_family() {
        let wildcard_v4: SocketAddr = "0.0.0.0:4321".parse().expect("valid IPv4 address");
        let wildcard_v6: SocketAddr = "[::]:8765".parse().expect("valid IPv6 address");
        let concrete_v4: SocketAddr = "192.0.2.1:9999".parse().expect("valid IPv4 address");

        assert_eq!(
            listener_wake_addr(wildcard_v4),
            "127.0.0.1:4321".parse().expect("valid loopback address")
        );
        assert_eq!(
            listener_wake_addr(wildcard_v6),
            "[::1]:8765".parse().expect("valid loopback address")
        );
        assert_eq!(listener_wake_addr(concrete_v4), concrete_v4);
    }

    #[test]
    fn unauthenticated_listener_rejects_resolved_non_loopback_before_serve() {
        let loopback: SocketAddr = "127.0.0.1:8080".parse().expect("valid loopback address");
        let public: SocketAddr = "192.0.2.1:8080".parse().expect("valid public address");

        assert!(validate_unauthenticated_bound_addr(loopback).is_ok());
        let error = validate_unauthenticated_bound_addr(public)
            .expect_err("resolved non-loopback listener must fail closed")
            .to_string();
        assert!(error.contains("192.0.2.1:8080"));
        assert!(error.contains("no authentication boundary"));
    }

    #[test]
    fn connection_drain_timer_failure_is_terminal_and_explicit() {
        let connection_checks = Cell::new(0_u32);
        let timer_calls = Cell::new(0_u32);
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let outcome = runtime.block_on(wait_for_connections_to_drain_with(
            || {
                connection_checks.set(connection_checks.get() + 1);
                3
            },
            Duration::from_secs(1),
            Duration::from_millis(10),
            |_| {
                timer_calls.set(timer_calls.get() + 1);
                std::future::ready(Err("drain-timer-sentinel".to_string()))
            },
        ));

        assert_eq!(
            outcome,
            ConnectionDrainOutcome::TimerFailed {
                remaining_connections: 3,
                error: "drain-timer-sentinel".to_string(),
            }
        );
        assert_eq!(connection_checks.get(), 1);
        assert_eq!(timer_calls.get(), 1, "timer failure must not spin or retry");
    }

    #[test]
    fn zero_connection_drain_poll_interval_is_rejected_without_spinning() {
        let timer_calls = Cell::new(0_u32);
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let outcome = runtime.block_on(wait_for_connections_to_drain_with(
            || 2,
            Duration::from_secs(1),
            Duration::ZERO,
            |_| {
                timer_calls.set(timer_calls.get() + 1);
                std::future::ready(Ok(()))
            },
        ));

        assert_eq!(
            outcome,
            ConnectionDrainOutcome::TimerFailed {
                remaining_connections: 2,
                error: "connection-drain poll interval must be non-zero".to_string(),
            }
        );
        assert_eq!(timer_calls.get(), 0);
    }
}
