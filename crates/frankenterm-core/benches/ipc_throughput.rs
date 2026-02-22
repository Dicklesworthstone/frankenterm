//! Criterion benchmarks for IPC JSON-line throughput and latency.
//!
//! Bead: ft-16hou
//! Required coverage:
//! - single-client JSON-line request/response latency
//! - multi-client requests/sec throughput for N=1,5,10,20
//! - JSON serialization/deserialization overhead
//! - connection establishment latency (accept + handler spawn)
//! - comparison with a tokio-based equivalent server

use std::hint::black_box;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::events::EventBus;
use frankenterm_core::ipc::{IpcClient, IpcRequest, IpcResponse, IpcServer, MAX_MESSAGE_SIZE};
use frankenterm_core::runtime_compat::mpsc;
use frankenterm_core::runtime_compat::unix as compat_unix;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener as TokioUnixListener, UnixStream as TokioUnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "ipc_throughput/roundtrip_latency/compat_server_ping",
        budget: "single-client ping round-trip latency for compat IPC server remains stable",
    },
    bench_common::BenchBudget {
        name: "ipc_throughput/roundtrip_latency/tokio_server_ping",
        budget: "single-client ping round-trip latency for tokio baseline remains stable",
    },
    bench_common::BenchBudget {
        name: "ipc_throughput/multi_client_throughput/compat_server_clients",
        budget: "compat IPC server sustains bounded latency under N={1,5,10,20} clients",
    },
    bench_common::BenchBudget {
        name: "ipc_throughput/multi_client_throughput/tokio_server_clients",
        budget: "tokio baseline sustains bounded latency under N={1,5,10,20} clients",
    },
    bench_common::BenchBudget {
        name: "ipc_throughput/json_serde_overhead/serialize_ping_request",
        budget: "JSON request serialization overhead stays low",
    },
    bench_common::BenchBudget {
        name: "ipc_throughput/json_serde_overhead/deserialize_ok_response",
        budget: "JSON response deserialization overhead stays low",
    },
    bench_common::BenchBudget {
        name: "ipc_throughput/connection_establishment_latency/compat_server_connect_drop",
        budget: "compat IPC connect+accept+spawn path remains bounded",
    },
    bench_common::BenchBudget {
        name: "ipc_throughput/connection_establishment_latency/tokio_server_connect_drop",
        budget: "tokio baseline connect+accept+spawn path remains bounded",
    },
];

const STARTUP_WAIT: Duration = Duration::from_millis(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const REQUESTS_PER_CLIENT_WAVE: usize = 16;
const CLIENT_CONCURRENCY_LEVELS: &[usize] = &[1, 5, 10, 20];

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create tokio runtime")
}

fn socket_path(prefix: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("frankenterm-bench-{prefix}-{ts}.sock"))
}

struct CompatServerHandle {
    shutdown_tx: mpsc::Sender<()>,
    join_handle: frankenterm_core::runtime_compat::task::JoinHandle<()>,
}

async fn start_compat_server(socket_path: &Path) -> io::Result<CompatServerHandle> {
    let server = IpcServer::bind(socket_path).await?;
    let event_bus = Arc::new(EventBus::new(1024));
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
    let join_handle = frankenterm_core::runtime_compat::task::spawn(async move {
        server.run(event_bus, shutdown_rx).await;
    });
    frankenterm_core::runtime_compat::sleep(STARTUP_WAIT).await;
    Ok(CompatServerHandle {
        shutdown_tx,
        join_handle,
    })
}

async fn stop_compat_server(handle: CompatServerHandle) {
    let _ = frankenterm_core::runtime_compat::mpsc_send(&handle.shutdown_tx, ()).await;
    let _ = frankenterm_core::runtime_compat::timeout(SHUTDOWN_TIMEOUT, handle.join_handle).await;
}

struct TokioServerHandle {
    shutdown_tx: watch::Sender<bool>,
    join_handle: tokio::task::JoinHandle<()>,
}

async fn start_tokio_server(socket_path: &Path) -> io::Result<TokioServerHandle> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = TokioUnixListener::bind(socket_path)?;
    let socket_path_for_cleanup = socket_path.to_path_buf();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let join_handle = tokio::spawn(async move {
        let mut connection_tasks = JoinSet::new();

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            connection_tasks.spawn(async move {
                                let _ = handle_tokio_client(stream).await;
                            });
                        }
                        Err(_) => break,
                    }
                }
            }

            while let Some(joined) = connection_tasks.try_join_next() {
                let _ = joined;
            }
        }

        connection_tasks.abort_all();
        while connection_tasks.join_next().await.is_some() {}
        let _ = std::fs::remove_file(socket_path_for_cleanup);
    });

    tokio::time::sleep(STARTUP_WAIT).await;
    Ok(TokioServerHandle {
        shutdown_tx,
        join_handle,
    })
}

async fn stop_tokio_server(handle: TokioServerHandle) {
    let _ = handle.shutdown_tx.send(true);
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, handle.join_handle).await;
}

fn tokio_response_for_request_line(line: &str) -> IpcResponse {
    if line.len() > MAX_MESSAGE_SIZE {
        IpcResponse::error("message too large")
    } else if serde_json::from_str::<serde_json::Value>(line).is_ok() {
        IpcResponse::ok()
    } else {
        IpcResponse::error("invalid request")
    }
}

async fn handle_tokio_client(stream: TokioUnixStream) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    if let Some(line) = lines.next_line().await? {
        let response = tokio_response_for_request_line(&line);
        let response_json = serde_json::to_string(&response)
            .unwrap_or_else(|_| r#"{"ok":false,"error":"serialization failed"}"#.to_string());
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}

async fn ping_once(socket_path: &Path) -> IpcResponse {
    let client = IpcClient::new(socket_path);
    let response = client.ping().await.expect("send ping over IPC");
    assert!(
        response.ok,
        "unexpected IPC error during benchmark: {:?}",
        response.error
    );
    response
}

async fn connect_drop_once(socket_path: &Path) -> io::Result<()> {
    let stream = compat_unix::connect(socket_path).await?;
    drop(stream);
    Ok(())
}

async fn run_concurrent_ping_wave(
    socket_path: &Path,
    concurrent_clients: usize,
    requests_per_client: usize,
) -> usize {
    let mut join_set = JoinSet::new();

    for _ in 0..concurrent_clients {
        let socket_path = socket_path.to_path_buf();
        join_set.spawn(async move {
            let client = IpcClient::new(&socket_path);
            let mut completed = 0usize;
            for _ in 0..requests_per_client {
                let response = client.ping().await.expect("send ping over IPC");
                assert!(response.ok, "IPC ping returned an error payload");
                completed = completed.saturating_add(1);
            }
            completed
        });
    }

    let mut total = 0usize;
    while let Some(joined) = join_set.join_next().await {
        total = total.saturating_add(joined.expect("ping wave task join"));
    }
    total
}

fn bench_roundtrip_latency(c: &mut Criterion) {
    let rt = runtime();
    let compat_socket = socket_path("ipc-roundtrip-compat");
    let tokio_socket = socket_path("ipc-roundtrip-tokio");

    let compat_server = rt
        .block_on(start_compat_server(&compat_socket))
        .expect("start compat IPC server");
    let tokio_server = rt
        .block_on(start_tokio_server(&tokio_socket))
        .expect("start tokio IPC server");

    let mut group = c.benchmark_group("ipc_throughput/roundtrip_latency");
    group.measurement_time(Duration::from_secs(10));

    let compat_socket_for_bench = compat_socket.clone();
    group.bench_function("compat_server_ping", |b| {
        b.to_async(&rt).iter(|| {
            let socket_path = compat_socket_for_bench.clone();
            async move {
                let response = ping_once(&socket_path).await;
                black_box(response.elapsed_ms);
            }
        });
    });

    let tokio_socket_for_bench = tokio_socket.clone();
    group.bench_function("tokio_server_ping", |b| {
        b.to_async(&rt).iter(|| {
            let socket_path = tokio_socket_for_bench.clone();
            async move {
                let response = ping_once(&socket_path).await;
                black_box(response.elapsed_ms);
            }
        });
    });

    group.finish();

    rt.block_on(async {
        stop_compat_server(compat_server).await;
        stop_tokio_server(tokio_server).await;
    });
}

fn bench_multi_client_throughput(c: &mut Criterion) {
    let rt = runtime();
    let compat_socket = socket_path("ipc-throughput-compat");
    let tokio_socket = socket_path("ipc-throughput-tokio");

    let compat_server = rt
        .block_on(start_compat_server(&compat_socket))
        .expect("start compat IPC server");
    let tokio_server = rt
        .block_on(start_tokio_server(&tokio_socket))
        .expect("start tokio IPC server");

    let mut group = c.benchmark_group("ipc_throughput/multi_client_throughput");

    for &clients in CLIENT_CONCURRENCY_LEVELS {
        let total_requests = clients.saturating_mul(REQUESTS_PER_CLIENT_WAVE);
        group.throughput(Throughput::Elements(total_requests as u64));

        let compat_socket_for_bench = compat_socket.clone();
        group.bench_with_input(
            BenchmarkId::new("compat_server_clients", clients),
            &clients,
            |b, &clients| {
                b.to_async(&rt).iter(|| {
                    let socket_path = compat_socket_for_bench.clone();
                    async move {
                        let completed = run_concurrent_ping_wave(
                            &socket_path,
                            clients,
                            REQUESTS_PER_CLIENT_WAVE,
                        )
                        .await;
                        black_box(completed);
                    }
                });
            },
        );

        let tokio_socket_for_bench = tokio_socket.clone();
        group.bench_with_input(
            BenchmarkId::new("tokio_server_clients", clients),
            &clients,
            |b, &clients| {
                b.to_async(&rt).iter(|| {
                    let socket_path = tokio_socket_for_bench.clone();
                    async move {
                        let completed = run_concurrent_ping_wave(
                            &socket_path,
                            clients,
                            REQUESTS_PER_CLIENT_WAVE,
                        )
                        .await;
                        black_box(completed);
                    }
                });
            },
        );
    }

    group.finish();

    rt.block_on(async {
        stop_compat_server(compat_server).await;
        stop_tokio_server(tokio_server).await;
    });
}

fn bench_json_serde_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("ipc_throughput/json_serde_overhead");
    let request = IpcRequest::Ping;
    let response_json = serde_json::to_string(&IpcResponse::ok()).expect("serialize response");

    group.bench_function("serialize_ping_request", |b| {
        b.iter(|| {
            let json = serde_json::to_string(black_box(&request)).expect("serialize request");
            black_box(json);
        });
    });

    group.bench_function("deserialize_ok_response", |b| {
        b.iter(|| {
            let response: IpcResponse =
                serde_json::from_str(black_box(response_json.as_str())).expect("parse response");
            black_box(response);
        });
    });

    group.bench_function("request_response_roundtrip", |b| {
        b.iter(|| {
            let request_json =
                serde_json::to_string(black_box(&request)).expect("serialize request");
            let _parsed_request: serde_json::Value =
                serde_json::from_str(black_box(request_json.as_str())).expect("parse request");
            let response: IpcResponse =
                serde_json::from_str(black_box(response_json.as_str())).expect("parse response");
            black_box(response);
        });
    });

    group.finish();
}

fn bench_connection_establishment_latency(c: &mut Criterion) {
    let rt = runtime();
    let compat_socket = socket_path("ipc-connect-compat");
    let tokio_socket = socket_path("ipc-connect-tokio");

    let compat_server = rt
        .block_on(start_compat_server(&compat_socket))
        .expect("start compat IPC server");
    let tokio_server = rt
        .block_on(start_tokio_server(&tokio_socket))
        .expect("start tokio IPC server");

    let mut group = c.benchmark_group("ipc_throughput/connection_establishment_latency");
    group.measurement_time(Duration::from_secs(8));

    let compat_socket_for_bench = compat_socket.clone();
    group.bench_function("compat_server_connect_drop", |b| {
        b.to_async(&rt).iter(|| {
            let socket_path = compat_socket_for_bench.clone();
            async move {
                connect_drop_once(&socket_path)
                    .await
                    .expect("connect/drop against compat IPC server");
            }
        });
    });

    let tokio_socket_for_bench = tokio_socket.clone();
    group.bench_function("tokio_server_connect_drop", |b| {
        b.to_async(&rt).iter(|| {
            let socket_path = tokio_socket_for_bench.clone();
            async move {
                connect_drop_once(&socket_path)
                    .await
                    .expect("connect/drop against tokio IPC server");
            }
        });
    });

    group.finish();

    rt.block_on(async {
        stop_compat_server(compat_server).await;
        stop_tokio_server(tokio_server).await;
    });
}

fn bench_suite(c: &mut Criterion) {
    bench_roundtrip_latency(c);
    bench_multi_client_throughput(c);
    bench_json_serde_overhead(c);
    bench_connection_establishment_latency(c);
    bench_common::emit_bench_artifacts("ipc_throughput", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
