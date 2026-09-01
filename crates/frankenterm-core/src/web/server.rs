//! Server lifecycle: start, run, shutdown, and signal handling.
//!
//! Extracted from `web.rs` as part of Wave 4B migration (ft-1zej2).

use super::{StorageEventTail, WebServerConfig, WebServerHandle, build_app};
use crate::events::{Event, EventBus};
use crate::patterns::{AgentType, Detection, Severity};
use crate::runtime_async::signal;
use crate::storage::{EventQuery, EventStreamQuery, StorageHandle, StoredEvent};
use crate::web_framework::{FrameworkWebRuntime, web_cx_error};
use crate::{Error, Result};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// How often the storage tail polls for new events when it is caught up.
const STORAGE_TAIL_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Rows fetched per poll; a full batch means "poll again immediately".
const STORAGE_TAIL_BATCH: usize = 256;

/// Rebuild the bus event the watcher would have published for a stored
/// detection. Unknown agent/severity labels fall back to the least-privileged
/// variants instead of dropping the event, so a malformed row is still
/// visible to stream consumers.
fn stored_event_to_bus_event(stored: StoredEvent) -> Event {
    let agent_type =
        serde_json::from_value(serde_json::Value::String(stored.agent_type.clone()))
            .unwrap_or(AgentType::Unknown);
    let severity = serde_json::from_value(serde_json::Value::String(stored.severity.clone()))
        .unwrap_or(Severity::Info);
    let detection = Detection {
        rule_id: stored.rule_id,
        agent_type,
        event_type: stored.event_type,
        severity,
        confidence: stored.confidence,
        extracted: stored.extracted.unwrap_or(serde_json::Value::Null),
        matched_text: stored.matched_text.unwrap_or_default(),
        span: (0, 0),
    };
    Event::PatternDetected {
        pane_id: stored.pane_id,
        pane_uuid: None,
        detection: crate::runtime::redact_detection(&detection),
        event_id: Some(stored.id),
    }
}

/// Follow newly persisted detection events from `storage` and republish them
/// on `bus`, so a standalone `ft web` streams live detections without running
/// a capture pipeline of its own (ft-zeo5o).
///
/// Starts after the newest persisted event so a server restart never replays
/// history through the bus. Rows are delivered in ascending id order; the
/// cursor only moves forward, so a retention sweep that removes old rows
/// cannot cause redelivery.
pub(super) async fn spawn_storage_event_tail(
    cx: &crate::cx::Cx,
    storage: StorageHandle,
    bus: Arc<EventBus>,
    poll_interval: Duration,
    batch: usize,
) -> StorageEventTail {
    let (stop, stop_rx) = crate::runtime_async::watch::channel(false);
    let mut cursor = storage
        .get_events(EventQuery {
            limit: Some(1),
            ..EventQuery::default()
        })
        .await
        .ok()
        .and_then(|events| events.first().map(|event| event.id))
        .unwrap_or(0);
    let batch = batch.max(1);

    let task = crate::runtime_async::task::spawn_with_cx(cx, move |child_cx| async move {
        info!(target: "wa.web", start_after_event_id = cursor, "storage event tail started");
        loop {
            if *stop_rx.borrow() || child_cx.checkpoint().is_err() {
                break;
            }
            let query = EventStreamQuery {
                after_id: Some(cursor),
                limit: Some(batch),
                ..EventStreamQuery::default()
            };
            let drained = match storage.get_events_stream(query).await {
                Ok(events) => {
                    let drained = events.len();
                    for stored in events {
                        cursor = cursor.max(stored.id);
                        // A zero-subscriber publish is normal here (no SSE
                        // client connected yet); the cursor still advances so
                        // late subscribers do not receive stale history.
                        let _ = bus.publish(stored_event_to_bus_event(stored));
                    }
                    drained
                }
                Err(error) => {
                    warn!(
                        target: "wa.web",
                        error = %error,
                        "storage event tail query failed; retrying after the poll interval"
                    );
                    0
                }
            };
            if drained >= batch {
                // More rows may be waiting; yield and poll again immediately.
                crate::runtime_async::task::yield_now().await;
                continue;
            }
            if crate::runtime_async::sleep_with_cx(&child_cx, poll_interval)
                .await
                .is_err()
            {
                break;
            }
        }
        info!(target: "wa.web", last_event_id = cursor, "storage event tail stopped");
    });

    StorageEventTail::new(stop, task)
}

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
    let tail_inputs = if config.storage_event_tail_enabled() {
        config.storage.clone().zip(config.event_bus.clone())
    } else {
        None
    };
    let app = build_app(config.storage, config.event_bus, runtime_limits);
    let (local_addr, runtime) = FrameworkWebRuntime::start_with_cx(cx, bind_addr, app).await?;

    let storage_tail = match tail_inputs {
        Some((storage, bus)) => Some(
            spawn_storage_event_tail(cx, storage, bus, STORAGE_TAIL_POLL_INTERVAL, STORAGE_TAIL_BATCH)
                .await,
        ),
        None => None,
    };

    info!(
        target: "wa.web",
        bound_addr = %local_addr,
        event_source = if storage_tail.is_some() { "storage_tail" } else { "bus_only" },
        "web server listening"
    );

    Ok(WebServerHandle {
        bound_addr: local_addr,
        runtime,
        storage_tail,
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
        storage_tail,
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
    if let Some(tail) = storage_tail {
        tail.stop();
    }

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
