//! Benchmarks for native WezTerm event ingress latency.
//!
//! This benchmark focuses on the wa-jgqs acceptance gap:
//! - native listener first-message latency
//! - steady-state native event throughput
//! - legacy Lua->CLI process-spawn baseline (approximation)

use std::hint::black_box;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::native_events::{NativeEvent, NativeEventListener};
use frankenterm_core::runtime_compat::mpsc;
use frankenterm_core::runtime_compat::unix::{self as compat_unix, AsyncWriteExt};
use frankenterm_core::runtime_compat::{
    CompatRuntime, Runtime, RuntimeBuilder, mpsc_recv_option, task, timeout,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "native_events_latency/bench_native_first_message_latency",
        budget: "native socket listener to first event delivery latency",
    },
    bench_common::BenchBudget {
        name: "native_events_latency/bench_native_batch_throughput",
        budget: "native socket listener sustained event delivery throughput",
    },
    bench_common::BenchBudget {
        name: "native_events_latency/bench_native_parse_dispatch_latency",
        budget: "native socket parse+dispatch latency with immediate consume",
    },
    bench_common::BenchBudget {
        name: "native_events_latency/bench_native_backpressure_impact",
        budget: "native socket throughput under bounded-channel backpressure",
    },
    bench_common::BenchBudget {
        name: "native_events_latency/bench_legacy_process_spawn_baseline",
        budget: "legacy Lua->CLI style per-event process spawn baseline",
    },
];

const PANE_OUTPUT_LINE: &str = r#"{"type":"pane_output","pane_id":1,"data_b64":"aGVsbG8=","ts":1}"#;
const NATIVE_EVENT_TIMEOUT: Duration = Duration::from_secs(2);
const BACKPRESSURE_EVENTS_PER_BATCH: u64 = 256;
const BACKPRESSURE_DRAIN_DELAY: Duration = Duration::from_micros(250);

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("create compat runtime")
}

async fn recv_event_or_panic(event_rx: &mut mpsc::Receiver<NativeEvent>) -> NativeEvent {
    timeout(NATIVE_EVENT_TIMEOUT, mpsc_recv_option(event_rx))
        .await
        .expect("timeout waiting for native event")
        .expect("event channel closed unexpectedly")
}

fn bench_native_first_message_latency(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("native_events_latency");

    group.bench_function("bench_native_first_message_latency", |b| {
        b.iter(|| {
            rt.block_on(async {
                let dir = tempfile::tempdir().expect("tempdir");
                let socket_path = dir.path().join("native-first.sock");

                let listener = NativeEventListener::bind(socket_path.clone())
                    .await
                    .expect("bind native listener");
                let (event_tx, mut event_rx) = mpsc::channel(32);
                let shutdown = Arc::new(AtomicBool::new(false));

                let shutdown_for_task = Arc::clone(&shutdown);
                let listener_task = task::spawn(listener.run(event_tx, shutdown_for_task));

                let mut stream = compat_unix::connect(socket_path)
                    .await
                    .expect("connect to native socket");
                stream
                    .write_all(format!("{PANE_OUTPUT_LINE}\n").as_bytes())
                    .await
                    .expect("write pane output event");

                let event = recv_event_or_panic(&mut event_rx).await;
                black_box(event);

                shutdown.store(true, Ordering::SeqCst);
                let _ = timeout(NATIVE_EVENT_TIMEOUT, listener_task).await;
            });
        });
    });

    group.finish();
}

fn bench_native_batch_throughput(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("native_events_latency");

    for events_per_batch in [100_u64, 1000, 10_000] {
        group.throughput(Throughput::Elements(events_per_batch));
        group.bench_with_input(
            BenchmarkId::new("bench_native_batch_throughput", events_per_batch),
            &events_per_batch,
            |b, events_per_batch| {
                let events_per_batch = *events_per_batch;
                b.iter(|| {
                    rt.block_on(async move {
                        let dir = tempfile::tempdir().expect("tempdir");
                        let socket_path = dir.path().join("native-batch.sock");

                        let listener = NativeEventListener::bind(socket_path.clone())
                            .await
                            .expect("bind native listener");
                        let (event_tx, mut event_rx) = mpsc::channel(2048);
                        let shutdown = Arc::new(AtomicBool::new(false));

                        let shutdown_for_task = Arc::clone(&shutdown);
                        let listener_task = task::spawn(listener.run(event_tx, shutdown_for_task));

                        let mut stream = compat_unix::connect(socket_path)
                            .await
                            .expect("connect to native socket");

                        for i in 0..events_per_batch {
                            let line = format!(
                                r#"{{"type":"pane_output","pane_id":1,"data_b64":"aGVsbG8=","ts":{i}}}"#
                            );
                            stream
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .expect("write pane output event");
                        }

                        for _ in 0..events_per_batch {
                            let event = recv_event_or_panic(&mut event_rx).await;
                            match event {
                                NativeEvent::PaneOutput { .. } => {}
                                other => {
                                    panic!(
                                        "unexpected event while benchmarking throughput: {other:?}"
                                    )
                                }
                            }
                        }

                        shutdown.store(true, Ordering::SeqCst);
                        let _ = timeout(NATIVE_EVENT_TIMEOUT, listener_task).await;
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_native_parse_dispatch_latency(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("native_events_latency");

    for events_per_batch in [100_u64, 1000, 10_000] {
        group.throughput(Throughput::Elements(events_per_batch));
        group.bench_with_input(
            BenchmarkId::new("bench_native_parse_dispatch_latency", events_per_batch),
            &events_per_batch,
            |b, events_per_batch| {
                let events_per_batch = *events_per_batch;
                b.iter(|| {
                    rt.block_on(async move {
                        let dir = tempfile::tempdir().expect("tempdir");
                        let socket_path = dir.path().join("native-parse.sock");

                        let listener = NativeEventListener::bind(socket_path.clone())
                            .await
                            .expect("bind native listener");
                        let (event_tx, mut event_rx) = mpsc::channel(512);
                        let shutdown = Arc::new(AtomicBool::new(false));

                        let shutdown_for_task = Arc::clone(&shutdown);
                        let listener_task = task::spawn(listener.run(event_tx, shutdown_for_task));

                        let mut stream = compat_unix::connect(socket_path)
                            .await
                            .expect("connect to native socket");

                        for i in 0..events_per_batch {
                            let line = format!(
                                r#"{{"type":"pane_output","pane_id":9,"data_b64":"aGVsbG8=","ts":{i}}}"#
                            );
                            stream
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .expect("write pane output event");
                            let event = recv_event_or_panic(&mut event_rx).await;
                            black_box(event);
                        }

                        shutdown.store(true, Ordering::SeqCst);
                        let _ = timeout(NATIVE_EVENT_TIMEOUT, listener_task).await;
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_native_backpressure_impact(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("native_events_latency");

    for channel_capacity in [1_usize, 2, 8, 32] {
        group.throughput(Throughput::Elements(BACKPRESSURE_EVENTS_PER_BATCH));
        group.bench_with_input(
            BenchmarkId::new("bench_native_backpressure_impact", channel_capacity),
            &channel_capacity,
            |b, channel_capacity| {
                let channel_capacity = *channel_capacity;
                b.iter(|| {
                    rt.block_on(async move {
                        let dir = tempfile::tempdir().expect("tempdir");
                        let socket_path = dir.path().join("native-backpressure.sock");

                        let listener = NativeEventListener::bind(socket_path.clone())
                            .await
                            .expect("bind native listener");
                        let (event_tx, mut event_rx) = mpsc::channel(channel_capacity);
                        let shutdown = Arc::new(AtomicBool::new(false));

                        let shutdown_for_task = Arc::clone(&shutdown);
                        let listener_task = task::spawn(listener.run(event_tx, shutdown_for_task));

                        let consumer = task::spawn(async move {
                            let mut received = 0_u64;
                            while let Ok(Some(_event)) =
                                timeout(Duration::from_millis(250), mpsc_recv_option(&mut event_rx))
                                    .await
                            {
                                received += 1;
                                frankenterm_core::runtime_compat::sleep(BACKPRESSURE_DRAIN_DELAY)
                                    .await;
                                if received >= BACKPRESSURE_EVENTS_PER_BATCH {
                                    break;
                                }
                            }
                            received
                        });

                        let mut stream = compat_unix::connect(socket_path)
                            .await
                            .expect("connect to native socket");

                        for i in 0..BACKPRESSURE_EVENTS_PER_BATCH {
                            let line = format!(
                                r#"{{"type":"pane_output","pane_id":11,"data_b64":"aGVsbG8=","ts":{i}}}"#
                            );
                            stream
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .expect("write pane output event");
                        }

                        shutdown.store(true, Ordering::SeqCst);
                        let _ = timeout(NATIVE_EVENT_TIMEOUT, listener_task).await;
                        let drained_events = match timeout(NATIVE_EVENT_TIMEOUT, consumer).await {
                            Ok(Ok(count)) => count,
                            _ => 0,
                        };
                        black_box(drained_events);
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_legacy_process_spawn_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("native_events_latency");
    group.bench_function("bench_legacy_process_spawn_baseline", |b| {
        b.iter(|| {
            // Approximate the legacy Lua->CLI path cost by measuring per-event process spawn.
            let status = Command::new("true").status().expect("spawn no-op command");
            black_box(status.success());
        });
    });
    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_native_first_message_latency(c);
    bench_native_batch_throughput(c);
    bench_native_parse_dispatch_latency(c);
    bench_native_backpressure_impact(c);
    bench_legacy_process_spawn_baseline(c);
    bench_common::emit_bench_artifacts("native_events_latency", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
