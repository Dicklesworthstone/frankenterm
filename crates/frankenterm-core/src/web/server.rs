//! Server lifecycle: start, run, shutdown, and signal handling.
//!
//! Extracted from `web.rs` as part of Wave 4B migration (ft-1zej2).

#[allow(clippy::wildcard_imports)]
use super::*;
use crate::web_framework::FrameworkWebRuntime;

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

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use super::signal::unix::SignalKind;

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

pub(super) fn poke_listener(addr: SocketAddr) {
    if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}
