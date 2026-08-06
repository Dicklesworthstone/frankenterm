//! Server lifecycle: start, run, shutdown, and signal handling.
//!
//! Extracted from `web.rs` as part of Wave 4B migration (ft-1zej2).

use super::{WebServerConfig, WebServerHandle, build_app};
use crate::runtime_async::signal;
use crate::web_framework::{FrameworkWebRuntime, web_cx_error};
use crate::{Error, Result};
use std::io::Write;
use std::net::SocketAddr;
use tracing::{info, warn};

/// Start the web server and return a handle for shutdown.
///
/// Refuses to bind on non-localhost addresses because the current web API has
/// no authentication boundary.
pub async fn start_web_server(config: WebServerConfig) -> Result<WebServerHandle> {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    start_web_server_with_cx(&cx, config).await
}

/// ft-xbnl0.2.3 Cx-first sibling of [`start_web_server`].
///
/// Threads the caller's cx through to
/// `FrameworkWebRuntime::start_with_cx` so the accept loop
/// inherits the caller's capability context (not a fresh
/// per-request one). The framework receives that context for accepted
/// connections; complete per-connection task ownership and settlement remain
/// separately tracked by the web-lifecycle campaign.
///
/// Tick 101 upgraded this from a simple pre-flight delegate to
/// a true cx-threading entry point.
pub async fn start_web_server_with_cx(
    cx: &crate::cx::Cx,
    config: WebServerConfig,
) -> Result<WebServerHandle> {
    cx.checkpoint()
        .map_err(|err| web_cx_error(cx, "start_web_server", &err))?;

    validate_bind_config(&config)?;
    let bind_addr = config.bind_addr();
    let runtime_limits = config.runtime_limits;
    let app = build_app(config.storage, config.event_bus, runtime_limits);
    let (local_addr, runtime) = FrameworkWebRuntime::start_with_cx(cx, bind_addr, app).await?;

    info!(
        target: "wa.web",
        bound_addr = %local_addr,
        "web server listening"
    );

    Ok(WebServerHandle {
        bound_addr: local_addr,
        runtime,
    })
}

/// Run the web server until Ctrl+C, then shut down gracefully.
pub async fn run_web_server(config: WebServerConfig) -> Result<()> {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    run_web_server_with_cx(&cx, config).await
}

fn validate_bind_config(config: &WebServerConfig) -> Result<()> {
    if config.is_localhost() {
        return Ok(());
    }

    let override_note = if config.allow_public_bind {
        " even with dangerous public-bind opt-in"
    } else {
        ""
    };
    Err(Error::runtime_backend(
        "web bind validation",
        format!(
            "refusing to bind web server on non-localhost address '{}'{}: the current web API has no authentication boundary; bind to 127.0.0.1, ::1, or localhost until auth middleware lands",
            config.host, override_note
        ),
    ))
}

fn write_listening_announcement(
    mut writer: impl Write,
    bound_addr: SocketAddr,
) -> std::io::Result<()> {
    writeln!(writer, "ft web listening on http://{bound_addr}")
}

/// ft-xbnl0.2.3 Cx-first sibling of [`run_web_server`].
///
/// Routes the initial bind through [`start_web_server_with_cx`] so startup
/// honors caller cancellation. After startup succeeds, every normally awaited
/// exit path retains the server until it has joined, attempted its bounded
/// drain, and run framework shutdown hooks.
/// Caller cancellation triggers graceful shutdown, but is surfaced only after
/// the cleanup attempt. The cleanup path reuses the caller's runtime drivers
/// and capabilities; a pre-cancelled or exhausted context can make its timer
/// wait fail, which is reported rather than replaced with freshly minted
/// authority. A server-join or cleanup failure takes precedence over the
/// shutdown-wait or caller-cancellation error.
/// Dropping the run future invokes the runtime's synchronous signal-and-wake
/// fallback; full drain and async hooks require awaiting this function.
pub async fn run_web_server_with_cx(cx: &crate::cx::Cx, config: WebServerConfig) -> Result<()> {
    let WebServerHandle {
        bound_addr,
        mut runtime,
    } = start_web_server_with_cx(cx, config).await?;

    if let Err(error) = write_listening_announcement(std::io::stdout().lock(), bound_addr) {
        warn!(
            target: "wa.web",
            error = %error,
            "unable to write web-listener announcement to stdout; server remains active"
        );
    }

    let (join_result, shutdown_result) = crate::runtime_async::select! {
        result = runtime.join_handle_mut() => {
            (result, None)
        }
        shutdown = wait_for_shutdown_signal_with_cx(cx) => {
            runtime.signal_shutdown();
            let result = runtime.join_handle_mut().await;
            (result, Some(shutdown))
        }
    };

    // Once start succeeds, cleanup is mandatory. `finish_with_cx` always runs
    // shutdown hooks after its drain attempt. A cancelled or exhausted caller
    // can make the drain timer fail, but cannot skip the hook invocation and is
    // never replaced with freshly minted authority.
    runtime.finish_with_cx(cx, join_result).await?;

    if let Some(result) = shutdown_result {
        result?;
    }

    Ok(())
}

/// Poll until caller cancellation or budget exhaustion is observed.
async fn wait_for_cx_cancellation(cx: &crate::cx::Cx) -> Result<()> {
    loop {
        cx.checkpoint()
            .map_err(|err| web_cx_error(cx, "web shutdown wait", &err))?;
        if crate::runtime_async::sleep_with_cx(cx, std::time::Duration::from_millis(100))
            .await
            .is_err()
        {
            if let Err(cancel_error) = cx.checkpoint() {
                return Err(web_cx_error(cx, "web shutdown wait", &cancel_error));
            }
            // Runtime timer detail is intentionally discarded: the caller gets
            // a finite class, and the unused raw string cannot leak capability
            // or backend data through this control-plane boundary.
            return Err(Error::runtime_backend(
                "web shutdown wait sleep",
                "web_shutdown_wait_sleep_failed",
            ));
        }
    }
}

/// Wait for an OS shutdown signal or caller cancellation under an explicit Cx.
///
/// Races the OS signal futures against `cx` cancellation. OS signals return
/// `Ok(())`; caller cancellation returns a typed runtime-cancellation error.
/// The caller retains that result while it completes mandatory cleanup, then
/// surfaces it to its caller.
#[cfg(unix)]
async fn wait_for_shutdown_signal_with_cx(cx: &crate::cx::Cx) -> Result<()> {
    use crate::runtime_async::signal::unix::SignalKind;
    use futures::future::{Either, select};
    use futures::pin_mut;

    cx.checkpoint()
        .map_err(|err| web_cx_error(cx, "web shutdown wait", &err))?;

    let mut term = signal::unix::signal(SignalKind::terminate())
        .map_err(|e| Error::runtime_backend("web sigterm handler", e.to_string()))?;

    let cancel_fut = wait_for_cx_cancellation(cx);
    let ctrl_c = signal::ctrl_c();
    let term_recv = term.recv();
    pin_mut!(ctrl_c, term_recv, cancel_fut);

    match select(select(ctrl_c, term_recv), cancel_fut).await {
        Either::Left((Either::Left((result, _)), _)) => {
            result.map_err(|e| Error::runtime_backend("web ctrl_c handler", e.to_string()))?;
        }
        Either::Left((Either::Right((term, _)), _)) => {
            if term.is_none() {
                return Err(Error::runtime_backend(
                    "web sigterm handler",
                    "signal stream closed without a termination signal",
                ));
            }
        }
        Either::Right((result, _)) => return result,
    }
    Ok(())
}

/// Non-Unix shutdown-signal race; see the Unix sibling for semantics.
#[cfg(not(unix))]
async fn wait_for_shutdown_signal_with_cx(cx: &crate::cx::Cx) -> Result<()> {
    use futures::future::{Either, select};
    use futures::pin_mut;

    cx.checkpoint()
        .map_err(|err| web_cx_error(cx, "web shutdown wait", &err))?;

    let cancel_fut = wait_for_cx_cancellation(cx);
    let ctrl_c = signal::ctrl_c();
    pin_mut!(ctrl_c, cancel_fut);

    match select(ctrl_c, cancel_fut).await {
        Either::Left((result, _)) => {
            result.map_err(|e| Error::runtime_backend("web ctrl_c handler", e.to_string()))?;
        }
        Either::Right((result, _)) => return result,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_bind_config, write_listening_announcement};
    use crate::web::WebServerConfig;
    use std::io::{self, Write};
    use std::net::SocketAddr;

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout-closed-sentinel",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn validate_bind_config_allows_localhost() {
        let config = WebServerConfig::default();
        assert!(validate_bind_config(&config).is_ok());
    }

    #[test]
    fn validate_bind_config_allows_noncanonical_ipv4_loopback() {
        let config = WebServerConfig::new(8080).with_host("127.0.0.2");
        assert!(validate_bind_config(&config).is_ok());
    }

    #[test]
    fn listener_announcement_write_failure_is_returned_without_panicking() {
        let bound_addr: SocketAddr = "127.0.0.1:8000".parse().expect("valid loopback address");
        let error = write_listening_announcement(BrokenPipeWriter, bound_addr)
            .expect_err("closed stdout must be reported as an ordinary I/O error");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn validate_bind_config_rejects_public_bind_without_auth_boundary() {
        let config = WebServerConfig::new(8080)
            .with_host("0.0.0.0")
            .with_dangerous_public_bind();
        let err = validate_bind_config(&config)
            .expect_err("public web bind must fail closed without auth")
            .to_string();
        assert!(err.contains("0.0.0.0"));
        assert!(err.contains("no authentication boundary"));
        assert!(err.contains("dangerous public-bind opt-in"));
    }

    #[test]
    fn validate_bind_config_rejects_public_ipv6_without_auth_boundary() {
        let config = WebServerConfig::new(8080).with_host("2001:db8::1");
        let err = validate_bind_config(&config)
            .expect_err("public IPv6 bind must fail closed until web auth lands")
            .to_string();
        assert!(err.contains("2001:db8::1"));
        assert!(err.contains("no authentication boundary"));
    }
}
