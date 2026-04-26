//! Integration coverage for the generated Rust robot SDK transport backend.
//!
//! These tests exercise the compiled `RustSdkTransport` over a real IPC server
//! with a minimal RPC handler so the generated client surface is backed by a
//! concrete control-plane transport rather than a render-time stub.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use frankenterm_core::events::EventBus;
use frankenterm_core::ingest::PaneRegistry;
use frankenterm_core::ipc::{IpcResponse, IpcRpcHandler, IpcServer};
use frankenterm_core::robot_sdk_contracts::{RustSdkTransport, RustSdkTransportError};
use frankenterm_core::robot_types::GetTextData;
use tempfile::TempDir;

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use frankenterm_core::runtime_async::CompatRuntime;

    let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future);
}

async fn send_shutdown(
    shutdown_tx: &frankenterm_core::runtime_async::mpsc::Sender<()>,
) -> Result<(), frankenterm_core::runtime_async::mpsc::SendError<()>> {
    // ft-nm5nc: legacy-tokio fallback branch (`shutdown_tx.send(()).await`
    // with no Cx) retired. asupersync is sole runtime; reserve(&cx) is
    // the canonical pattern.
    let cx = frankenterm_core::cx::for_testing();
    match shutdown_tx.reserve(&cx).await {
        Ok(permit) => {
            permit.send(());
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
#[test]
fn robot_sdk_transport_integration_get_text_roundtrip() {
    run_async_test(async {
        let temp_dir = TempDir::new().expect("create temp dir");
        let socket_path = temp_dir.path().join("robot-sdk.sock");
        let seen_args = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let seen_args_handler = Arc::clone(&seen_args);

        let handler: IpcRpcHandler = Arc::new(move |request| {
            let seen_args = Arc::clone(&seen_args_handler);
            Box::pin(async move {
                seen_args.lock().unwrap().push(request.args);
                IpcResponse::ok_with_data(serde_json::json!({
                    "pane_id": 7,
                    "text": "hello from ipc",
                    "tail_lines": 12,
                    "escapes_included": true,
                    "truncated": false
                }))
            })
        });

        let event_bus = Arc::new(EventBus::new(16));
        let registry = Arc::new(frankenterm_core::runtime_async::RwLock::new(
            PaneRegistry::new(),
        ));
        let server = IpcServer::bind(&socket_path)
            .await
            .expect("bind ipc server");
        let (shutdown_tx, shutdown_rx) = frankenterm_core::runtime_async::mpsc::channel(1);

        let server_handle = frankenterm_core::runtime_async::task::spawn(async move {
            let server_cx = frankenterm_core::cx::for_testing();
            server
                .run_with_registry_auth_and_rpc_with_cx(
                    &server_cx,
                    event_bus,
                    registry,
                    None,
                    Some(handler),
                    shutdown_rx,
                )
                .await;
        });

        frankenterm_core::runtime_async::sleep(Duration::from_millis(10)).await;

        let transport = RustSdkTransport::new(&socket_path);
        let data: GetTextData = transport
            .call(
                "get-text",
                serde_json::json!({
                    "pane_id": 7,
                    "tail_lines": 12,
                    "escapes": true
                }),
            )
            .await
            .expect("transport roundtrip should succeed");

        assert_eq!(data.pane_id, 7);
        assert_eq!(data.text, "hello from ipc");
        assert_eq!(data.tail_lines, 12);
        assert!(data.escapes_included);
        assert!(!data.truncated);
        assert_eq!(
            seen_args.lock().unwrap().as_slice(),
            &[vec![
                "get-text".to_string(),
                "7".to_string(),
                "--tail".to_string(),
                "12".to_string(),
                "--escapes".to_string(),
            ]]
        );

        send_shutdown(&shutdown_tx).await.expect("send shutdown");
        let _ = server_handle.await;
    });
}

#[cfg(unix)]
#[test]
fn robot_sdk_transport_integration_preserves_robot_error_semantics() {
    run_async_test(async {
        let temp_dir = TempDir::new().expect("create temp dir");
        let socket_path = temp_dir.path().join("robot-sdk.sock");
        let seen_args = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let seen_args_handler = Arc::clone(&seen_args);

        let handler: IpcRpcHandler = Arc::new(move |request| {
            let seen_args = Arc::clone(&seen_args_handler);
            Box::pin(async move {
                seen_args.lock().unwrap().push(request.args);
                IpcResponse::error_with_code(
                    "robot.timeout",
                    "pattern not matched before timeout",
                    Some("increase timeout_secs or relax the pattern".to_string()),
                )
            })
        });

        let event_bus = Arc::new(EventBus::new(16));
        let registry = Arc::new(frankenterm_core::runtime_async::RwLock::new(
            PaneRegistry::new(),
        ));
        let server = IpcServer::bind(&socket_path)
            .await
            .expect("bind ipc server");
        let (shutdown_tx, shutdown_rx) = frankenterm_core::runtime_async::mpsc::channel(1);

        let server_handle = frankenterm_core::runtime_async::task::spawn(async move {
            let server_cx = frankenterm_core::cx::for_testing();
            server
                .run_with_registry_auth_and_rpc_with_cx(
                    &server_cx,
                    event_bus,
                    registry,
                    None,
                    Some(handler),
                    shutdown_rx,
                )
                .await;
        });

        frankenterm_core::runtime_async::sleep(Duration::from_millis(10)).await;

        let transport = RustSdkTransport::new(&socket_path);
        let err = transport
            .call_value(
                "search",
                serde_json::json!({
                    "query": "panic",
                    "limit": 5,
                    "pane": 7,
                    "mode": "hybrid",
                    "snippets": false
                }),
            )
            .await
            .expect_err("robot timeout should surface as a robot error");

        match err {
            RustSdkTransportError::Robot(robot_err) => {
                assert_eq!(robot_err.code.as_deref(), Some("robot.timeout"));
                assert_eq!(robot_err.message, "pattern not matched before timeout");
                assert_eq!(
                    robot_err.hint.as_deref(),
                    Some("increase timeout_secs or relax the pattern")
                );
            }
            other => panic!("expected robot error, got {other:?}"),
        }
        assert_eq!(
            seen_args.lock().unwrap().as_slice(),
            &[vec![
                "search".to_string(),
                "panic".to_string(),
                "--limit".to_string(),
                "5".to_string(),
                "--pane".to_string(),
                "7".to_string(),
                "--snippets=false".to_string(),
                "--mode".to_string(),
                "hybrid".to_string(),
            ]]
        );

        send_shutdown(&shutdown_tx).await.expect("send shutdown");
        let _ = server_handle.await;
    });
}
