//! Server lifecycle: start, run, shutdown, and signal handling.
//!
//! Extracted from `web.rs` as part of Wave 4B migration (ft-1zej2).

use super::{WebServerConfig, WebServerHandle, build_app};
use crate::runtime_async::signal;
use crate::web_framework::FrameworkWebRuntime;
use crate::{Error, Result};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;
use tracing::info;

/// Start the web server and return a handle for shutdown.
///
/// Refuses to bind on non-localhost addresses because the current web API has
/// no authentication boundary.
pub async fn start_web_server(config: WebServerConfig) -> Result<WebServerHandle> {
    validate_bind_config(&config)?;
    let bind_addr = config.bind_addr();
    let runtime_limits = config.runtime_limits;
    let app = build_app(config.storage, config.event_bus, runtime_limits);
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
pub async fn start_web_server_with_cx(
    cx: &crate::cx::Cx,
    config: WebServerConfig,
) -> Result<WebServerHandle> {
    use tracing::info;

    cx.checkpoint()
        .map_err(|err| Error::runtime_cancelled("start_web_server", err.to_string()))?;

    validate_bind_config(&config)?;
    let bind_addr = config.bind_addr();
    let runtime_limits = config.runtime_limits;
    let app = super::build_app(config.storage, config.event_bus, runtime_limits);
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

    crate::runtime_async::select! {
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
pub async fn run_web_server_with_cx(cx: &crate::cx::Cx, config: WebServerConfig) -> Result<()> {
    let WebServerHandle {
        bound_addr,
        mut runtime,
    } = start_web_server_with_cx(cx, config).await?;

    println!("ft web listening on http://{bound_addr} (cx-first)");

    crate::runtime_async::select! {
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
        use crate::runtime_async::signal::unix::SignalKind;

        let mut term = signal::unix::signal(SignalKind::terminate())
            .map_err(|e| Error::runtime_backend("web sigterm handler", e.to_string()))?;

        crate::runtime_async::select! {
            _ = signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        signal::ctrl_c()
            .await
            .map_err(|e| Error::runtime_backend("web ctrl_c handler", e.to_string()))?;
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
#[cfg(unix)]
async fn wait_for_shutdown_signal_with_cx(cx: &crate::cx::Cx) -> Result<()> {
    use crate::runtime_async::signal::unix::SignalKind;
    use futures::future::{Either, select};
    use futures::pin_mut;

    cx.checkpoint()
        .map_err(|err| Error::runtime_cancelled("web shutdown wait", err.to_string()))?;

    let mut term = signal::unix::signal(SignalKind::terminate())
        .map_err(|e| Error::runtime_backend("web sigterm handler", e.to_string()))?;

    let cancel_fut = async {
        loop {
            if cx.is_cancel_requested() {
                return;
            }
            let _ = crate::runtime_async::sleep_with_cx(cx, std::time::Duration::from_millis(100))
                .await;
        }
    };
    let ctrl_c = signal::ctrl_c();
    let term_recv = term.recv();
    pin_mut!(ctrl_c, term_recv, cancel_fut);

    match select(select(ctrl_c, term_recv), cancel_fut).await {
        Either::Left((Either::Left((result, _)), _)) => {
            result.map_err(|e| Error::runtime_backend("web ctrl_c handler", e.to_string()))?;
        }
        Either::Left((Either::Right((_term, _)), _)) => {}
        Either::Right(((), _)) => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal_with_cx(cx: &crate::cx::Cx) -> Result<()> {
    use futures::future::{Either, select};
    use futures::pin_mut;

    cx.checkpoint()
        .map_err(|err| Error::runtime_cancelled("web shutdown wait", err.to_string()))?;

    let cancel_fut = async {
        loop {
            if cx.is_cancel_requested() {
                return;
            }
            let _ = crate::runtime_async::sleep_with_cx(cx, std::time::Duration::from_millis(100))
                .await;
        }
    };
    let ctrl_c = signal::ctrl_c();
    pin_mut!(ctrl_c, cancel_fut);

    match select(ctrl_c, cancel_fut).await {
        Either::Left((result, _)) => {
            result.map_err(|e| Error::runtime_backend("web ctrl_c handler", e.to_string()))?;
        }
        Either::Right(((), _)) => {}
    }
    Ok(())
}

pub(super) fn poke_listener(addr: SocketAddr) {
    if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::validate_bind_config;
    use crate::web::WebServerConfig;

    #[test]
    fn validate_bind_config_allows_localhost() {
        let config = WebServerConfig::default();
        assert!(validate_bind_config(&config).is_ok());
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
