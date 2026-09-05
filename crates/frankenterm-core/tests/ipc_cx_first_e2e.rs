//! End-to-end Cx-first transport integration tests for the
//! `ft-xbnl0.2.3` (IPC + local control-plane) lane.
//!
//! Exercises the full Cx-first pipeline landed in ticks 102+103:
//! 1. `IpcServer::bind_with_cx` — threaded filesystem setup.
//! 2. `IpcServer::run_with_cx` → `run_with_context_with_cx` — cx
//!    threaded into the accept loop + shutdown-signal polling.
//! 3. Graceful shutdown via the shutdown channel (legacy surface).
//! 4. Cancellation via the caller's cx alone (Cx-first surface).
//!
//! These tests serve as completion evidence for ft-xbnl0.2.3 acceptance
//! criteria #3 (tests cover concurrent clients, disconnects, shutdown)
//! and supplement existing unit-level cx tests with end-to-end transport
//! stack coverage.

#![cfg(all(unix, feature = "asupersync-runtime"))]

use frankenterm_core::events::EventBus;
use frankenterm_core::ipc::{
    IpcRequest, IpcResponse, IpcRuntimeLimits, IpcServer, MAX_MESSAGE_SIZE,
};
use frankenterm_core::runtime_async::unix::{AsyncReadExt, AsyncWriteExt};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder, mpsc, task, unix as ft_unix};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn socket_path(test_name: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name_hash = test_name.bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(16777619) ^ u32::from(byte)
    });
    let suffix = ts & 0xffff_ffff;
    std::path::PathBuf::from(format!("/tmp/ft-{name_hash:08x}-{suffix:x}.sock"))
}

fn run_async_test<F>(future: F)
where
    F: Future<Output = ()>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(future);
}

fn runtime_ipc_cx() -> frankenterm_core::cx::Cx {
    // Match the watcher: accepted sockets must register with this runtime's
    // reactor. A detached test Cx has no drivers and makes idle socket I/O
    // immediately self-wake on every poll instead of waiting for readiness.
    let cx = frankenterm_core::cx::Cx::current().expect("runtime-owned IPC context");
    assert!(cx.io_driver_handle().is_some(), "real socket I/O driver");
    assert!(cx.timer_driver().is_some(), "real deadline timer driver");
    cx
}

async fn send_ping_request(socket: &std::path::Path) -> std::io::Result<IpcResponse> {
    let mut stream = ft_unix::connect(socket).await?;
    let request = IpcRequest::Ping;
    let mut line = serde_json::to_vec(&request).expect("serialize request");
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;

    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
    }

    let response: IpcResponse =
        serde_json::from_slice(&buf).map_err(|e| std::io::Error::other(format!("parse: {e}")))?;
    Ok(response)
}

/// ft-xbnl0.2.3 E2E: full cx-first transport pipeline happy path.
///
/// Binds via `bind_with_cx`, spawns `run_with_cx`, connects a
/// client, verifies a Ping/Pong roundtrip, then shuts down via the
/// shutdown channel. Confirms the cx-first pipeline is
/// protocol-compatible with legacy IPC.
#[test]
fn ipc_cx_first_bind_run_ping_shutdown_roundtrip() {
    run_async_test(async {
        let path = socket_path("cx-first-bind-run");
        let cx = runtime_ipc_cx();

        let server = IpcServer::bind_with_cx(&cx, &path)
            .await
            .expect("bind_with_cx");
        let event_bus = Arc::new(EventBus::new(16));
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        let run_cx = cx.clone();
        let server_task = task::spawn(async move {
            server.run_with_cx(&run_cx, event_bus, shutdown_rx).await;
        });

        // Let the accept loop arm itself.
        frankenterm_core::runtime_async::sleep(Duration::from_millis(50)).await;

        // Client ping → server ok roundtrip (Pong is represented
        // as an ok-shaped IpcResponse with no error).
        let response = send_ping_request(&path).await.expect("ping");
        assert!(
            response.ok,
            "ping response should have ok=true, got: {response:?}"
        );
        assert!(
            response.error.is_none(),
            "ping response should have no error: {response:?}"
        );

        // Shutdown via the legacy shutdown channel — must be honored
        // even though the accept loop is cx-driven.
        let _ = frankenterm_core::runtime_async::mpsc_send(&shutdown_tx, ()).await;
        frankenterm_core::runtime_async::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server join should not time out")
            .expect("server task");

        assert!(
            !path.exists(),
            "socket file should be removed after shutdown"
        );
    });
}

/// ft-xbnl0.2.3 E2E: cx cancellation alone (no shutdown signal)
/// must tear down the full transport stack.
///
/// This demonstrates the behavioral upgrade from a pure-signal-based
/// shutdown to cx-first cancellation. A cancelled caller cx must
/// propagate into `shutdown_signal_pending_with_cx` and cause the
/// accept loop to exit, cleaning up the socket file.
#[test]
fn ipc_cx_first_cx_cancel_alone_tears_down_server() {
    run_async_test(async {
        let path = socket_path("cx-first-cancel-teardown");
        let cx = runtime_ipc_cx();

        let server = IpcServer::bind_with_cx(&cx, &path)
            .await
            .expect("bind_with_cx");
        let event_bus = Arc::new(EventBus::new(16));
        // Create a shutdown channel but never send on it — cancel via cx only.
        let (_shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        let run_cx = cx.clone();
        let server_task = task::spawn(async move {
            server.run_with_cx(&run_cx, event_bus, shutdown_rx).await;
        });

        // Let the accept loop settle.
        frankenterm_core::runtime_async::sleep(Duration::from_millis(50)).await;

        // Cancel via cx only — no shutdown signal.
        let started = Instant::now();
        cx.cancel_with(
            frankenterm_core::outcome::CancelKind::User,
            Some("ft-xbnl0.2.3 e2e cx cancel"),
        );

        frankenterm_core::runtime_async::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server join should not time out after cx cancel")
            .expect("server task");
        let elapsed = started.elapsed();

        // Cx cancel must propagate into accept loop shutdown-poll; the
        // accept poll interval is a low multiple of hundreds of ms.
        assert!(
            elapsed < Duration::from_secs(2),
            "cx cancel should tear down accept loop within 2s, took {elapsed:?}"
        );
        assert!(
            !path.exists(),
            "socket file should be removed after cx-only teardown"
        );
    });
}

/// ft-xbnl0.2.3 E2E: multiple concurrent client connections
/// survive the cx-first accept loop without interference.
///
/// Spawns 4 concurrent clients each making a Ping/Pong roundtrip.
/// Verifies all succeed — pins that the cx-threaded accept loop
/// does not serialize connections or drop any under concurrent load.
#[test]
fn ipc_cx_first_multiple_concurrent_clients() {
    run_async_test(async {
        let path = socket_path("cx-first-concurrent");
        let cx = runtime_ipc_cx();

        let server = IpcServer::bind_with_cx(&cx, &path)
            .await
            .expect("bind_with_cx");
        let event_bus = Arc::new(EventBus::new(16));
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        let run_cx = cx.clone();
        let server_task = task::spawn(async move {
            server.run_with_cx(&run_cx, event_bus, shutdown_rx).await;
        });

        frankenterm_core::runtime_async::sleep(Duration::from_millis(50)).await;

        // Fire 4 clients concurrently.
        let client_count: usize = 4;
        let mut handles = Vec::with_capacity(client_count);
        for _ in 0..client_count {
            let p = path.clone();
            handles.push(task::spawn(async move { send_ping_request(&p).await }));
        }

        let mut succeeded = 0;
        for h in handles {
            let res = h.await.expect("join client task");
            if let Ok(resp) = res {
                if resp.ok {
                    succeeded += 1;
                }
            }
        }
        assert_eq!(
            succeeded, client_count,
            "all concurrent clients should complete a Ping/Pong roundtrip"
        );

        let _ = frankenterm_core::runtime_async::mpsc_send(&shutdown_tx, ()).await;
        let _ = frankenterm_core::runtime_async::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server join should not time out");
    });
}

#[test]
fn accepted_idle_partial_and_subscribed_clients_stop_without_rpc_settlement_grace() {
    use tracing::instrument::WithSubscriber;

    run_async_test(async {
        for mode in ["empty", "partial", "subscription"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("stop.sock");
            let cx = runtime_ipc_cx();
            let limits = IpcRuntimeLimits {
                max_message_size: MAX_MESSAGE_SIZE,
                accept_poll_interval_ms: 10,
                max_concurrent_connections: 1,
                initial_request_timeout_ms: 5_000,
                io_timeout_ms: 1_000,
            };
            let server = IpcServer::bind_with_permissions_and_limits_with_cx(
                &cx,
                &path,
                Some(0o600),
                limits,
            )
            .await
            .unwrap();
            let event_bus = Arc::new(EventBus::new(16));
            let server_bus = Arc::clone(&event_bus);
            let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
            let run_cx = cx.clone();
            let server_task = task::spawn(async move {
                server
                    .run_with_cx(&run_cx, server_bus, shutdown_rx)
                    .with_subscriber(
                        tracing_subscriber::fmt()
                            .with_max_level(tracing::Level::DEBUG)
                            .with_test_writer()
                            .finish(),
                    )
                    .await;
            });

            let mut client = ft_unix::connect(&path).await.unwrap();
            match mode {
                "partial" => client.write_all(br#"{"type":"ping""#).await.unwrap(),
                "subscription" => {
                    let mut request = serde_json::to_vec(&IpcRequest::SubscribeEvents {
                        pane: None,
                        severity: None,
                        rule_id: None,
                        heartbeat_interval_ms: 0,
                    })
                    .unwrap();
                    request.push(b'\n');
                    client.write_all(&request).await.unwrap();
                    frankenterm_core::runtime_async::timeout_with_cx(
                        &cx,
                        Duration::from_millis(500),
                        async {
                            while event_bus.subscriber_count() == 0 {
                                frankenterm_core::runtime_async::sleep_with_cx(
                                    &cx,
                                    Duration::from_millis(1),
                                )
                                .await
                                .unwrap();
                            }
                        },
                    )
                    .await
                    .expect("real event subscription must be installed before shutdown");
                }
                _ => {}
            }

            // The second socket can reach EOF this quickly only after the
            // first has actually occupied the sole connection slot. This
            // prevents shutdown-before-accept from making the test pass.
            let mut capacity_probe = ft_unix::connect(&path).await.unwrap();
            let mut byte = [0u8; 1];
            let count = frankenterm_core::runtime_async::timeout_with_cx(
                &cx,
                Duration::from_millis(500),
                capacity_probe.read(&mut byte),
            )
            .await
            .unwrap_or_else(|error| {
                let mut probe_byte = [0_u8; 1];
                let mut client_byte = [0_u8; 1];
                let probe_state =
                    std::io::Read::read(&mut capacity_probe.as_std(), &mut probe_byte);
                let client_state = std::io::Read::read(&mut client.as_std(), &mut client_byte);
                panic!(
                    "capacity probe must reach the real accept loop: mode={mode}, error={error}, probe={probe_state:?}, client={client_state:?}, server_finished={}",
                    server_task.is_finished()
                );
            })
            .expect("capacity rejection must close the probe socket");
            assert_eq!(count, 0, "first {mode} connection must already be accepted");

            frankenterm_core::runtime_async::mpsc_send(&shutdown_tx, ())
                .await
                .unwrap();
            frankenterm_core::runtime_async::timeout_with_cx(
                &cx,
                Duration::from_millis(500),
                server_task,
            )
            .await
            .unwrap_or_else(|_| panic!("accepted {mode} client exhausted idle shutdown budget"))
            .expect("server task must settle successfully");
            assert!(!path.exists(), "owned socket must be retired for {mode}");
            assert_eq!(
                event_bus.subscriber_count(),
                0,
                "subscription must be dropped"
            );
        }
    });
}
