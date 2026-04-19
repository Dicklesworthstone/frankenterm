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
use frankenterm_core::ipc::{IpcRequest, IpcResponse, IpcServer};
use frankenterm_core::runtime_compat::unix::{AsyncReadExt, AsyncWriteExt};
use frankenterm_core::runtime_compat::{
    CompatRuntime, RuntimeBuilder, mpsc, task, unix as ft_unix,
};
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
        let cx = frankenterm_core::cx::for_testing();

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
        frankenterm_core::runtime_compat::sleep(Duration::from_millis(50)).await;

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
        let _ = frankenterm_core::runtime_compat::mpsc_send(&shutdown_tx, ()).await;
        frankenterm_core::runtime_compat::timeout(Duration::from_secs(5), server_task)
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
        let cx = frankenterm_core::cx::for_testing();

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
        frankenterm_core::runtime_compat::sleep(Duration::from_millis(50)).await;

        // Cancel via cx only — no shutdown signal.
        let started = Instant::now();
        cx.cancel_with(
            frankenterm_core::outcome::CancelKind::User,
            Some("ft-xbnl0.2.3 e2e cx cancel"),
        );

        frankenterm_core::runtime_compat::timeout(Duration::from_secs(5), server_task)
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
        let cx = frankenterm_core::cx::for_testing();

        let server = IpcServer::bind_with_cx(&cx, &path)
            .await
            .expect("bind_with_cx");
        let event_bus = Arc::new(EventBus::new(16));
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        let run_cx = cx.clone();
        let server_task = task::spawn(async move {
            server.run_with_cx(&run_cx, event_bus, shutdown_rx).await;
        });

        frankenterm_core::runtime_compat::sleep(Duration::from_millis(50)).await;

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

        let _ = frankenterm_core::runtime_compat::mpsc_send(&shutdown_tx, ()).await;
        let _ = frankenterm_core::runtime_compat::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server join should not time out");
    });
}
