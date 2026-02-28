//! Server lifecycle: start, run, shutdown, and signal handling.
//!
//! Extracted from `web.rs` as part of Wave 4B migration (ft-1zej2).

use super::*;

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

    let server_task = {
        let server = Arc::clone(&server);
        task::spawn(async move {
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

    println!("ft web listening on http://{bound_addr}");

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

pub(super) async fn handle_server_exit(
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

pub(super) fn poke_listener(addr: SocketAddr) {
    if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}
