//! Criterion benchmarks comparing tokio vs asupersync for spike patterns.

use std::future::ready;
use std::hint::black_box;
use std::pin::Pin;
use std::time::Duration;

use asupersync::channel::mpsc as asup_mpsc;
use asupersync::combinator::Select;
use asupersync::io::{AsyncReadExt as AsupAsyncReadExt, AsyncWriteExt as AsupAsyncWriteExt};
use asupersync::net::unix::UnixStream as AsupUnixStream;
use asupersync::runtime::RuntimeBuilder as AsupRuntimeBuilder;
use asupersync::sync::{Mutex as AsupMutex, Semaphore as AsupSemaphore};
use asupersync::{Budget, CancelKind, Cx, LabConfig, LabRuntime};
use criterion::{Criterion, criterion_group, criterion_main};
use tokio::io::{AsyncReadExt as TokioAsyncReadExt, AsyncWriteExt as TokioAsyncWriteExt};
use tokio::sync::{Mutex as TokioMutex, Semaphore as TokioSemaphore, mpsc as tokio_mpsc};

mod bench_common;

const PAYLOAD: &[u8] = b"ft-asupersync-spike-payload";
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "spike_comparison/unix_pdu/tokio",
        budget: "tokio unix socket pdu baseline",
    },
    bench_common::BenchBudget {
        name: "spike_comparison/unix_pdu/asupersync",
        budget: "asupersync unix socket pdu baseline",
    },
    bench_common::BenchBudget {
        name: "spike_comparison/two_phase_send/tokio",
        budget: "tokio mpsc send/recv baseline",
    },
    bench_common::BenchBudget {
        name: "spike_comparison/two_phase_send/asupersync",
        budget: "asupersync reserve/send/recv baseline",
    },
    bench_common::BenchBudget {
        name: "spike_comparison/pool_pattern/tokio",
        budget: "tokio semaphore+mutex+timeout baseline",
    },
    bench_common::BenchBudget {
        name: "spike_comparison/pool_pattern/asupersync",
        budget: "asupersync semaphore+mutex+budget-timeout baseline",
    },
    bench_common::BenchBudget {
        name: "spike_comparison/lab_oracle/tokio",
        budget: "tokio no-op control for deterministic harness work",
    },
    bench_common::BenchBudget {
        name: "spike_comparison/lab_oracle/asupersync",
        budget: "asupersync LabRuntime run+oracle report",
    },
    bench_common::BenchBudget {
        name: "spike_comparison/select_race/tokio",
        budget: "tokio select first-completion baseline",
    },
    bench_common::BenchBudget {
        name: "spike_comparison/select_race/asupersync",
        budget: "asupersync Select + Cx::race baseline",
    },
];

async fn tokio_write_pdu(stream: &mut tokio::net::UnixStream, payload: &[u8]) {
    let len = u32::try_from(payload.len()).expect("payload length fits");
    stream
        .write_all(&len.to_be_bytes())
        .await
        .expect("write header");
    stream.write_all(payload).await.expect("write payload");
}

async fn tokio_read_pdu(stream: &mut tokio::net::UnixStream) -> Vec<u8> {
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header).await.expect("read header");
    let len = u32::from_be_bytes(header) as usize;
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).await.expect("read payload");
    payload
}

async fn asup_write_pdu(stream: &mut AsupUnixStream, payload: &[u8]) {
    let len = u32::try_from(payload.len()).expect("payload length fits");
    stream
        .write_all(&len.to_be_bytes())
        .await
        .expect("write header");
    stream.write_all(payload).await.expect("write payload");
}

async fn asup_read_pdu(stream: &mut AsupUnixStream) -> Vec<u8> {
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header).await.expect("read header");
    let len = u32::from_be_bytes(header) as usize;
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).await.expect("read payload");
    payload
}

fn bench_unixstream_pdu(c: &mut Criterion) {
    let mut group = c.benchmark_group("spike_comparison/unix_pdu");
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let asup_rt = AsupRuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");

    group.bench_function("tokio", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                let (mut a, mut b) = tokio::net::UnixStream::pair().expect("tokio stream pair");
                tokio_write_pdu(&mut a, PAYLOAD).await;
                let got = tokio_read_pdu(&mut b).await;
                black_box(got);
            });
        });
    });

    group.bench_function("asupersync", |b| {
        b.iter(|| {
            asup_rt.block_on(async {
                let (mut a, mut b) = AsupUnixStream::pair().expect("asupersync stream pair");
                asup_write_pdu(&mut a, PAYLOAD).await;
                let got = asup_read_pdu(&mut b).await;
                black_box(got);
            });
        });
    });

    group.finish();
}

fn bench_two_phase_send(c: &mut Criterion) {
    let mut group = c.benchmark_group("spike_comparison/two_phase_send");
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let asup_rt = AsupRuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");

    group.bench_function("tokio", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                let (tx, mut rx) = tokio_mpsc::channel(1);
                tx.send(7_u32).await.expect("send");
                let got = rx.recv().await.expect("recv");
                black_box(got);
            });
        });
    });

    group.bench_function("asupersync", |b| {
        b.iter(|| {
            asup_rt.block_on(async {
                let cx = Cx::for_testing();
                let (tx, rx) = asup_mpsc::channel(1);
                let permit = tx.reserve(&cx).await.expect("reserve");
                permit.send(7_u32);
                let got = rx.recv(&cx).await.expect("recv");
                black_box(got);
            });
        });
    });

    group.finish();
}

fn bench_pool_pattern(c: &mut Criterion) {
    let mut group = c.benchmark_group("spike_comparison/pool_pattern");
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let asup_rt = AsupRuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");

    group.bench_function("tokio", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                let sem = TokioSemaphore::new(1);
                let pool = TokioMutex::new(vec![1_u32]);
                let permit = sem.acquire().await.expect("acquire");
                {
                    let mut entries = pool.lock().await;
                    entries.push(2_u32);
                    black_box(entries.len());
                }
                let timed = tokio::time::timeout(Duration::from_nanos(1), sem.acquire()).await;
                black_box(timed.is_err());
                drop(permit);
            });
        });
    });

    group.bench_function("asupersync", |b| {
        b.iter(|| {
            asup_rt.block_on(async {
                let cx = Cx::for_testing();
                let sem = AsupSemaphore::new(1);
                let pool = AsupMutex::new(vec![1_u32]);
                let permit = sem.acquire(&cx, 1).await.expect("acquire");
                {
                    let mut entries = pool.lock(&cx).await.expect("lock");
                    entries.push(2_u32);
                    black_box(entries.len());
                }
                let exhausted = Cx::for_testing_with_budget(Budget::new().with_poll_quota(0));
                exhausted.cancel_with(CancelKind::Timeout, Some("benchmark timeout probe"));
                black_box(sem.acquire(&exhausted, 1).await.is_err());
                drop(permit);
            });
        });
    });

    group.finish();
}

fn bench_lab_runtime_oracle(c: &mut Criterion) {
    let mut group = c.benchmark_group("spike_comparison/lab_oracle");
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    group.bench_function("tokio", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                black_box(1_u8);
            });
        });
    });

    group.bench_function("asupersync", |b| {
        b.iter(|| {
            let mut lab = LabRuntime::new(LabConfig::new(7).worker_count(2).max_steps(1_000));
            let report = lab.run_until_quiescent_with_report();
            black_box(report.oracle_report.all_passed());
        });
    });

    group.finish();
}

fn bench_select_race(c: &mut Criterion) {
    let mut group = c.benchmark_group("spike_comparison/select_race");
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let asup_rt = AsupRuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");

    group.bench_function("tokio", |b| {
        b.iter(|| {
            let winner = tokio_rt.block_on(async {
                tokio::select! {
                    left = async { 1_u8 } => left,
                    right = async { 2_u8 } => right,
                }
            });
            black_box(winner);
        });
    });

    group.bench_function("asupersync", |b| {
        b.iter(|| {
            let selected = asup_rt.block_on(async { Select::new(ready(1_u8), ready(2_u8)).await });
            black_box(selected.is_left());

            let cx = Cx::for_testing();
            let futures: Vec<Pin<Box<dyn std::future::Future<Output = u8> + Send>>> =
                vec![Box::pin(async { 1_u8 }), Box::pin(async { 2_u8 })];
            let raced = asup_rt.block_on(async { cx.race(futures).await.expect("race") });
            black_box(raced);
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("spike_comparison", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_unixstream_pdu,
        bench_two_phase_send,
        bench_pool_pattern,
        bench_lab_runtime_oracle,
        bench_select_race
);
criterion_main!(benches);
