//! Benchmarks for vendored `MuxPool` scaling behavior.
//!
//! Required scenarios:
//! - acquire + release latency across pool sizes
//! - health-check overhead per acquire
//! - throughput scaling with concurrent tasks
//! - idle eviction scan time by pool size
//! - connection factory (`DirectMuxClient::connect`) overhead

use std::collections::HashMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codec::{CODEC_VERSION, GetCodecVersionResponse, ListPanesResponse, Pdu, UnitResponse};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::pool::PoolConfig;
use frankenterm_core::runtime_compat::sleep;
use frankenterm_core::vendored::{DirectMuxClient, DirectMuxClientConfig, MuxPool, MuxPoolConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "acquire_release_cycle",
        budget: "p50 acquire+release latency should stay sub-ms across max_size 1..32",
    },
    bench_common::BenchBudget {
        name: "health_check_overhead",
        budget: "health_check overhead should be close to list_panes round-trip",
    },
    bench_common::BenchBudget {
        name: "throughput_scaling",
        budget: "throughput should increase with concurrency until saturation",
    },
    bench_common::BenchBudget {
        name: "idle_eviction_scan",
        budget: "idle eviction should scale near-linearly with idle connection count",
    },
    bench_common::BenchBudget {
        name: "connection_factory_overhead",
        budget: "connect latency should remain bounded for local unix sockets",
    },
];

fn make_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

async fn spawn_mock_server(temp_dir: &tempfile::TempDir, response_delay: Duration) -> PathBuf {
    let socket_path = temp_dir.path().join("mux-pool-scaling.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind mock listener");

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };

            tokio::spawn(async move {
                let mut read_buf = Vec::new();
                loop {
                    let mut temp = vec![0u8; 4096];
                    let read = match stream.read(&mut temp).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    read_buf.extend_from_slice(&temp[..read]);

                    let mut responses = Vec::new();
                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "mux-pool-scaling-bench".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::ListPanes(_) => Pdu::ListPanesResponse(ListPanesResponse {
                                tabs: Vec::new(),
                                tab_titles: Vec::new(),
                                window_titles: HashMap::new(),
                            }),
                            Pdu::WriteToPane(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::SendPaste(_) => Pdu::UnitResponse(UnitResponse {}),
                            _ => continue,
                        };
                        responses.push((decoded.serial, response));
                    }

                    if !responses.is_empty() && !response_delay.is_zero() {
                        sleep(response_delay).await;
                    }

                    for (serial, pdu) in responses {
                        let mut out = Vec::new();
                        pdu.encode(&mut out, serial).expect("encode response");
                        if stream.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });

    socket_path
}

fn mux_pool_config(socket_path: PathBuf, max_size: usize, idle_timeout: Duration) -> MuxPoolConfig {
    MuxPoolConfig {
        pool: PoolConfig {
            max_size,
            idle_timeout,
            acquire_timeout: Duration::from_secs(2),
        },
        mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
        ..MuxPoolConfig::default()
    }
}

async fn prime_connections(pool: Arc<MuxPool>, concurrent: usize) {
    let mut joins = Vec::with_capacity(concurrent);
    for _ in 0..concurrent {
        let pool = Arc::clone(&pool);
        joins.push(tokio::spawn(async move {
            pool.list_panes().await.expect("prime list_panes");
        }));
    }
    for join in joins {
        join.await.expect("prime task join");
    }
}

fn bench_acquire_release_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("mux_pool_scaling/acquire_release_cycle");
    let rt = make_runtime();

    for &max_size in &[1usize, 2, 4, 8, 16, 32] {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = rt.block_on(spawn_mock_server(&temp_dir, Duration::from_millis(0)));
        let pool = Arc::new(MuxPool::new(mux_pool_config(
            socket_path,
            max_size,
            Duration::from_secs(60),
        )));

        group.bench_with_input(BenchmarkId::from_parameter(max_size), &max_size, |b, _| {
            let pool = Arc::clone(&pool);
            b.to_async(&rt).iter(|| {
                let pool = Arc::clone(&pool);
                async move {
                    pool.write_to_pane(1, b"echo hi\n".to_vec())
                        .await
                        .expect("write_to_pane");
                }
            });
        });
    }

    group.finish();
}

fn bench_health_check_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("mux_pool_scaling/health_check_overhead");
    let rt = make_runtime();

    for &max_size in &[1usize, 8, 32] {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = rt.block_on(spawn_mock_server(&temp_dir, Duration::from_millis(0)));
        let pool = Arc::new(MuxPool::new(mux_pool_config(
            socket_path,
            max_size,
            Duration::from_secs(60),
        )));

        group.bench_with_input(BenchmarkId::from_parameter(max_size), &max_size, |b, _| {
            let pool = Arc::clone(&pool);
            b.to_async(&rt).iter(|| {
                let pool = Arc::clone(&pool);
                async move {
                    pool.health_check().await.expect("health_check");
                }
            });
        });
    }

    group.finish();
}

fn bench_throughput_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("mux_pool_scaling/throughput_scaling");
    let rt = make_runtime();

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let socket_path = rt.block_on(spawn_mock_server(&temp_dir, Duration::from_millis(1)));
    let pool = Arc::new(MuxPool::new(mux_pool_config(
        socket_path,
        32,
        Duration::from_secs(60),
    )));

    for &concurrency in &[1usize, 2, 4, 8, 16, 32] {
        group.throughput(Throughput::Elements(concurrency as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &n| {
                let pool = Arc::clone(&pool);
                b.to_async(&rt).iter(|| {
                    let pool = Arc::clone(&pool);
                    async move {
                        let mut joins = Vec::with_capacity(n);
                        for _ in 0..n {
                            let pool = Arc::clone(&pool);
                            joins.push(tokio::spawn(async move {
                                pool.list_panes().await.expect("list_panes");
                            }));
                        }
                        for join in joins {
                            join.await.expect("throughput join");
                        }
                    }
                });
            },
        );
    }

    group.finish();
}

fn bench_idle_eviction_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("mux_pool_scaling/idle_eviction_scan");
    let rt = make_runtime();

    for &pool_size in &[1usize, 2, 4, 8, 16, 32] {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = rt.block_on(spawn_mock_server(&temp_dir, Duration::from_millis(2)));

        group.throughput(Throughput::Elements(pool_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pool_size),
            &pool_size,
            |b, &size| {
                let socket_path = socket_path.clone();
                b.to_async(&rt).iter(|| {
                    let socket_path = socket_path.clone();
                    async move {
                        let pool = Arc::new(MuxPool::new(mux_pool_config(
                            socket_path,
                            size,
                            Duration::ZERO,
                        )));
                        prime_connections(Arc::clone(&pool), size).await;
                        let evicted = pool.evict_idle().await;
                        black_box(evicted);
                    }
                });
            },
        );
    }

    group.finish();
}

fn bench_connection_factory_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("mux_pool_scaling/connection_factory_overhead");
    let rt = make_runtime();

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let socket_path = rt.block_on(spawn_mock_server(&temp_dir, Duration::from_millis(0)));
    let config = DirectMuxClientConfig::default().with_socket_path(socket_path);

    group.bench_function("direct_connect", |b| {
        b.to_async(&rt).iter(|| {
            let config = config.clone();
            async move {
                let client = DirectMuxClient::connect(config)
                    .await
                    .expect("direct mux connect");
                black_box(client);
            }
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("mux_pool_scaling", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_acquire_release_cycle,
        bench_health_check_overhead,
        bench_throughput_scaling,
        bench_idle_eviction_scan,
        bench_connection_factory_overhead
);
criterion_main!(benches);
