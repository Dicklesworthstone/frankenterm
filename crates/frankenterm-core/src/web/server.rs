//! Server lifecycle: start, run, shutdown, and signal handling.
//!
//! Extracted from `web.rs` as part of Wave 4B migration (ft-1zej2).

use super::{WebServerConfig, WebServerHandle, build_app};
use crate::runtime_compat::{select, signal};
use crate::web_framework::FrameworkWebRuntime;
use crate::{Error, Result};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;
use tracing::{info, warn};

/// Start the web server and return a handle for shutdown.
///
/// Refuses to bind on non-localhost addresses unless the config was
/// created with [`WebServerConfig::with_dangerous_public_bind`].
pub async fn start_web_server(config: WebServerConfig) -> Result<WebServerHandle> {
    if !config.is_localhost() && !config.allow_public_bind {
        return Err(Error::Runtime(format!(
            "refusing to bind on public address '{}' — \
             use --dangerous-bind-any or with_dangerous_public_bind() to override",
            config.host
        )));
    }
    if !config.is_localhost() {
        warn!(
            target: "wa.web",
            host = %config.host,
            "binding web server on non-localhost address — endpoints may be remotely reachable"
        );
    }
    let bind_addr = config.bind_addr();
    let app = build_app(config.storage, config.event_bus);
    let (local_addr, runtime) = FrameworkWebRuntime::start(bind_addr, app).await?;

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

/// ft-xbnl0.2.3 Cx-first sibling of [`start_web_server`].
///
/// Threads the caller's cx through to
/// `FrameworkWebRuntime::start_with_cx` so the accept loop
/// inherits the caller's capability context (not a fresh
/// per-request one). Cancellation/budget/virtual-time propagate
/// into every accepted connection.
///
/// Tick 101 upgraded this from a simple pre-flight delegate to
/// a true cx-threading entry point.
#[cfg(feature = "asupersync-runtime")]
pub async fn start_web_server_with_cx(
    cx: &crate::cx::Cx,
    config: WebServerConfig,
) -> Result<WebServerHandle> {
    use tracing::info;

    cx.checkpoint()
        .map_err(|err| Error::Runtime(format!("start_web_server cancelled: {err}")))?;

    if !config.is_localhost() && !config.allow_public_bind {
        return Err(Error::Runtime(format!(
            "refusing to bind on public address '{}' — \
             use --dangerous-bind-any or with_dangerous_public_bind() to override",
            config.host
        )));
    }
    if !config.is_localhost() {
        warn!(
            target: "wa.web",
            host = %config.host,
            "binding web server on non-localhost address — endpoints may be remotely reachable"
        );
    }
    let bind_addr = config.bind_addr();
    let app = super::build_app(config.storage, config.event_bus);
    let (local_addr, runtime) =
        crate::web_framework::FrameworkWebRuntime::start_with_cx(cx, bind_addr, app).await?;

    info!(
        target: "wa.web",
        bound_addr = %local_addr,
        "web server listening (cx-first)"
    );

    Ok(WebServerHandle {
        bound_addr: local_addr,
        runtime,
    })
}

/// Run the web server until Ctrl+C, then shut down gracefully.
pub async fn run_web_server(config: WebServerConfig) -> Result<()> {
    let WebServerHandle {
        bound_addr,
        mut runtime,
    } = start_web_server(config).await?;

    println!("ft web listening on http://{bound_addr}");

    select! {
        result = runtime.join_handle_mut() => {
            runtime.finish(result).await?;
        }
        shutdown = wait_for_shutdown_signal() => {
            shutdown?;
            runtime.signal_shutdown();
            poke_listener(bound_addr);
            let result = runtime.join_handle_mut().await;
            runtime.finish(result).await?;
        }
    }

    Ok(())
}

/// ft-xbnl0.2.3 Cx-first sibling of [`run_web_server`].
///
/// Orchestrator-level cx threading: routes the initial bind
/// through `start_web_server_with_cx` so the bind phase honours
/// caller cancellation, and passes the caller's cx into
/// `wait_for_shutdown_signal_with_cx` so the signal wait can be
/// interrupted by cx-cancel as well as by Ctrl+C/SIGTERM. A
/// caller-initiated cx-cancel now triggers the same graceful-
/// shutdown path (`signal_shutdown` + `poke_listener` + drain
/// join handle) that a SIGTERM would.
#[cfg(feature = "asupersync-runtime")]
pub async fn run_web_server_with_cx(cx: &crate::cx::Cx, config: WebServerConfig) -> Result<()> {
    let WebServerHandle {
        bound_addr,
        mut runtime,
    } = start_web_server_with_cx(cx, config).await?;

    println!("ft web listening on http://{bound_addr} (cx-first)");

    select! {
        result = runtime.join_handle_mut() => {
            runtime.finish(result).await?;
        }
        shutdown = wait_for_shutdown_signal_with_cx(cx) => {
            shutdown?;
            runtime.signal_shutdown();
            poke_listener(bound_addr);
            let result = runtime.join_handle_mut().await;
            runtime.finish(result).await?;
        }
    }

    Ok(())
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use crate::runtime_compat::signal::unix::SignalKind;

        let mut term = signal::unix::signal(SignalKind::terminate())
            .map_err(|e| Error::Runtime(format!("SIGTERM handler failed: {e}")))?;

        select! {
            _ = signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        signal::ctrl_c()
            .await
            .map_err(|e| Error::Runtime(format!("Ctrl+C handler failed: {e}")))?;
        Ok(())
    }
}

/// ft-xbnl0.2.3 Cx-first sibling of [`wait_for_shutdown_signal`].
///
/// Races the OS signal futures against `cx` cancellation. On
/// cx-cancel the function returns `Ok(())` — semantically
/// "external shutdown request", indistinguishable from a SIGTERM
/// for the caller's graceful-shutdown logic. That keeps
/// `run_web_server_with_cx`'s shutdown branch simple: whichever
/// of the three (SIGINT, SIGTERM, cx-cancel) fires first wins.
#[cfg(all(unix, feature = "asupersync-runtime"))]
async fn wait_for_shutdown_signal_with_cx(cx: &crate::cx::Cx) -> Result<()> {
    use crate::runtime_compat::signal::unix::SignalKind;

    cx.checkpoint()
        .map_err(|err| Error::Runtime(format!("web shutdown wait cancelled: {err}")))?;

    let mut term = signal::unix::signal(SignalKind::terminate())
        .map_err(|e| Error::Runtime(format!("SIGTERM handler failed: {e}")))?;

    // Tick 194 (ft-xbnl0.2.3): inner poll-sleep now threads the
    // caller's cx via sleep_with_cx. Previously the cancel-watcher
    // loop used ambient `sleep()` which falls back to
    // `Cx::current()` thread-local — if the web server runs under a
    // different thread-local cx than the caller's explicit one, the
    // timer was bound to the wrong cx. Under LabRuntime virtual
    // time that meant cancel could land later than the operator
    // intended.
    let cancel_fut = async {
        loop {
            if cx.is_cancel_requested() {
                return;
            }
            let _ = crate::runtime_compat::sleep_with_cx(cx, std::time::Duration::from_millis(100))
                .await;
        }
    };

    select! {
        _ = signal::ctrl_c() => {}
        _ = term.recv() => {}
        () = cancel_fut => {}
    }
    Ok(())
}

#[cfg(all(not(unix), feature = "asupersync-runtime"))]
async fn wait_for_shutdown_signal_with_cx(cx: &crate::cx::Cx) -> Result<()> {
    cx.checkpoint()
        .map_err(|err| Error::Runtime(format!("web shutdown wait cancelled: {err}")))?;

    // Tick 194 (ft-xbnl0.2.3): non-unix mirror of the poll-sleep
    // cx-threading fix above.
    let cancel_fut = async {
        loop {
            if cx.is_cancel_requested() {
                return;
            }
            let _ = crate::runtime_compat::sleep_with_cx(cx, std::time::Duration::from_millis(100))
                .await;
        }
    };

    select! {
        result = signal::ctrl_c() => {
            result.map_err(|e| Error::Runtime(format!("Ctrl+C handler failed: {e}")))?;
        }
        _ = cancel_fut => {}
    }
    Ok(())
}

pub(super) fn poke_listener(addr: SocketAddr) {
    if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}
