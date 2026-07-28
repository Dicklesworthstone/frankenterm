#![cfg(unix)]

use base64::Engine as _;
use frankenterm_core::native_events::{
    NativeEvent, NativeEventError, NativeEventListener, NativePaneState, WireEvent, WirePaneState,
    native_output_truncation_gap_reason,
};
use frankenterm_core::runtime_async::mpsc;
use frankenterm_core::runtime_async::unix::{self as event_socket, AsyncWriteExt};
use frankenterm_core::runtime_async::{self, CompatRuntime, RuntimeBuilder, task};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MAX_EVENT_LINE_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("failed to build runtime for native event integration tests");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(future);
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        frankenterm_core::runtime_async::clear_runtime_handle();
    }));
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn fail(message: impl Into<String>) -> ! {
    std::panic::panic_any(message.into())
}

async fn recv_next<T>(rx: &mut mpsc::Receiver<T>) -> Option<T> {
    let cx = frankenterm_core::cx::for_testing();
    rx.recv(&cx).await.ok()
}

async fn recv_event(
    event_rx: &mut mpsc::Receiver<NativeEvent>,
    timeout: Duration,
    label: &'static str,
) -> NativeEvent {
    runtime_async::timeout(timeout, recv_next(event_rx))
        .await
        .expect("native event receive timed out")
        .expect(label)
}

async fn write_line(socket_path: &std::path::Path, line: &str) {
    let mut stream = event_socket::connect(socket_path)
        .await
        .expect("connect native event socket");
    stream.write_all(line.as_bytes()).await.expect("write line");
    stream.write_all(b"\n").await.expect("write newline");
    stream.flush().await.expect("flush native event payload");
}

#[test]
fn listener_decodes_wire_events_and_ignores_hello() {
    run_async_test(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("native-events-decode.sock");
        let listener = NativeEventListener::bind(socket_path.clone())
            .await
            .expect("bind listener");
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

        let mut stream = event_socket::connect(&socket_path)
            .await
            .expect("connect native event socket");
        let lines = [
            r#"{"type":"hello","proto":1,"wezterm_version":"FrankenTerm","ts":1}"#,
            r#"{"type":"pane_output","pane_id":7,"data_b64":"aGV5","ts":42}"#,
            r#"{"type":"state_change","pane_id":8,"state":{"title":"zsh","rows":24,"cols":80,"is_alt_screen":false,"cursor_row":1,"cursor_col":2},"ts":43}"#,
        ];
        for line in lines {
            stream
                .write_all(line.as_bytes())
                .await
                .expect("write event");
            stream.write_all(b"\n").await.expect("write newline");
        }
        stream.flush().await.expect("flush native event payload");

        match recv_event(&mut event_rx, Duration::from_secs(2), "pane output").await {
            NativeEvent::PaneOutput {
                pane_id,
                data,
                timestamp_ms,
                dropped_bytes,
            } => {
                assert_eq!(pane_id, 7);
                assert_eq!(data, b"hey");
                assert_eq!(timestamp_ms, 42);
                assert_eq!(dropped_bytes, 0);
            }
            other => fail(format!("unexpected first event: {other:?}")),
        }

        match recv_event(&mut event_rx, Duration::from_secs(2), "state change").await {
            NativeEvent::StateChange {
                pane_id,
                state,
                timestamp_ms,
            } => {
                assert_eq!(pane_id, 8);
                assert_eq!(state.title, "zsh");
                assert_eq!(state.rows, 24);
                assert_eq!(state.cols, 80);
                assert_eq!(state.cursor_row, 1);
                assert_eq!(state.cursor_col, 2);
                assert_eq!(timestamp_ms, 43);
            }
            other => fail(format!("unexpected second event: {other:?}")),
        }

        drop(stream);
        shutdown.store(true, Ordering::SeqCst);
        let _ = runtime_async::timeout(Duration::from_secs(2), handle)
            .await
            .expect("listener shutdown timed out");
    });
}

#[test]
fn listener_reports_truncated_output_as_recoverable_gap() {
    run_async_test(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("native-events-truncate.sock");
        let listener = NativeEventListener::bind(socket_path.clone())
            .await
            .expect("bind listener");
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

        let raw = vec![b'a'; MAX_OUTPUT_BYTES + 1234];
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let payload =
            format!(r#"{{"type":"pane_output","pane_id":9,"data_b64":"{data_b64}","ts":7}}"#);
        assert!(payload.len() < MAX_EVENT_LINE_BYTES);
        write_line(&socket_path, &payload).await;

        match recv_event(&mut event_rx, Duration::from_secs(2), "truncated output").await {
            NativeEvent::PaneOutput {
                data,
                dropped_bytes,
                ..
            } => {
                assert_eq!(data.len(), MAX_OUTPUT_BYTES);
                assert_eq!(dropped_bytes, 1234);
                assert_eq!(
                    native_output_truncation_gap_reason(dropped_bytes),
                    "native_output_truncated:dropped_bytes=1234"
                );
            }
            other => fail(format!("unexpected truncated event: {other:?}")),
        }

        shutdown.store(true, Ordering::SeqCst);
        let _ = runtime_async::timeout(Duration::from_secs(2), handle)
            .await
            .expect("listener shutdown timed out");
    });
}

#[test]
fn listener_skips_invalid_and_oversized_lines_then_recovers() {
    run_async_test(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("native-events-recover.sock");
        let listener = NativeEventListener::bind(socket_path.clone())
            .await
            .expect("bind listener");
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

        let mut stream = event_socket::connect(&socket_path)
            .await
            .expect("connect native event socket");
        stream
            .write_all(b"not json\n")
            .await
            .expect("write invalid line");
        let oversized = "x".repeat(MAX_EVENT_LINE_BYTES + 1);
        stream
            .write_all(oversized.as_bytes())
            .await
            .expect("write oversized line");
        stream.write_all(b"\n").await.expect("write newline");
        stream
            .write_all(r#"{"type":"pane_destroyed","pane_id":42,"ts":999}"#.as_bytes())
            .await
            .expect("write valid line");
        stream.write_all(b"\n").await.expect("write newline");
        stream.flush().await.expect("flush native event payload");

        assert!(matches!(
            recv_event(&mut event_rx, Duration::from_secs(2), "recovered event").await,
            NativeEvent::PaneDestroyed {
                pane_id: 42,
                timestamp_ms: 999
            }
        ));

        drop(stream);
        shutdown.store(true, Ordering::SeqCst);
        let _ = runtime_async::timeout(Duration::from_secs(2), handle)
            .await
            .expect("listener shutdown timed out");
    });
}

#[test]
fn listener_accepts_reconnect_and_rapid_events() {
    run_async_test(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("native-events-reconnect.sock");
        let listener = NativeEventListener::bind(socket_path.clone())
            .await
            .expect("bind listener");
        let (event_tx, mut event_rx) = mpsc::channel(512);
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

        write_line(
            &socket_path,
            r#"{"type":"pane_destroyed","pane_id":41,"ts":100}"#,
        )
        .await;
        assert!(matches!(
            recv_event(&mut event_rx, Duration::from_secs(2), "first event").await,
            NativeEvent::PaneDestroyed {
                pane_id: 41,
                timestamp_ms: 100
            }
        ));

        let mut stream = event_socket::connect(&socket_path)
            .await
            .expect("connect second stream");
        let event_count = 256u64;
        for i in 0..event_count {
            let line = format!(
                r#"{{"type":"pane_destroyed","pane_id":{},"ts":{}}}"#,
                i,
                i * 10
            );
            stream
                .write_all(line.as_bytes())
                .await
                .expect("write event");
            stream.write_all(b"\n").await.expect("write newline");
        }
        stream.flush().await.expect("flush native event payload");

        let mut received = 0u64;
        while received < event_count {
            match runtime_async::timeout(Duration::from_secs(5), recv_next(&mut event_rx)).await {
                Ok(Some(_)) => received += 1,
                Ok(None) => break,
                Err(elapsed) => fail(format!(
                    "timeout after {elapsed}: received {received}/{event_count} events"
                )),
            }
        }
        assert_eq!(received, event_count);

        drop(stream);
        shutdown.store(true, Ordering::SeqCst);
        let _ = runtime_async::timeout(Duration::from_secs(2), handle)
            .await
            .expect("listener shutdown timed out");
    });
}

#[test]
fn bind_errors_and_drop_cleanup_are_public_contracts() {
    run_async_test(async {
        let empty = NativeEventListener::bind(std::path::PathBuf::from("")).await;
        assert!(matches!(empty, Err(NativeEventError::EmptySocketPath)));

        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("exists.sock");
        std::fs::write(&file_path, b"").expect("create collision file");
        let collision = NativeEventListener::bind(file_path).await;
        assert!(matches!(
            collision,
            Err(NativeEventError::SocketAlreadyExists(_))
        ));

        let socket_path = dir.path().join("drop-cleanup.sock");
        let listener = NativeEventListener::bind(socket_path.clone())
            .await
            .expect("bind cleanup listener");
        assert!(socket_path.exists(), "socket should exist after bind");
        drop(listener);
        assert!(
            !socket_path.exists(),
            "socket path should be removed after listener drop"
        );
    });
}

#[test]
fn wire_types_roundtrip_and_public_variants_remain_stable() {
    let state = NativePaneState {
        title: "nvim main.rs".to_string(),
        rows: 50,
        cols: 200,
        is_alt_screen: true,
        cursor_row: 25,
        cursor_col: 42,
    };
    let event = NativeEvent::StateChange {
        pane_id: 7,
        state: state.clone(),
        timestamp_ms: 555,
    };
    assert!(format!("{event:?}").contains("StateChange"));
    assert_eq!(state.title, "nvim main.rs");

    let events = vec![
        WireEvent::Hello {
            proto: Some(1),
            wezterm_version: Some("v1".into()),
            ts: Some(100),
        },
        WireEvent::PaneOutput {
            pane_id: 1,
            data_b64: base64::engine::general_purpose::STANDARD.encode(b"test"),
            ts: 200,
        },
        WireEvent::StateChange {
            pane_id: 2,
            state: WirePaneState {
                title: "zsh".into(),
                rows: 24,
                cols: 80,
                is_alt_screen: false,
                cursor_row: 0,
                cursor_col: 0,
            },
            ts: 300,
        },
        WireEvent::UserVar {
            pane_id: 3,
            name: "k".into(),
            value: "v".into(),
            ts: 400,
        },
        WireEvent::PaneCreated {
            pane_id: 4,
            domain: "local".into(),
            cwd: Some("/tmp".into()),
            ts: 500,
        },
        WireEvent::PaneDestroyed {
            pane_id: 5,
            ts: 600,
        },
    ];

    for wire in events {
        let json = serde_json::to_string(&wire).expect("serialize wire event");
        let parsed: WireEvent = serde_json::from_str(&json).expect("deserialize wire event");
        let reparsed = serde_json::to_string(&parsed).expect("reserialize wire event");
        assert_eq!(json, reparsed, "wire event roundtrip should be stable");
    }
}
