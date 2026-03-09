#![cfg(feature = "distributed")]

//! Criterion benchmarks for wa-agent distributed streaming hot paths.
//!
//! Bead: ft-nu4.4.3.2
//! Required benchmark families:
//! - bench_agent_capture_cpu
//! - bench_agent_capture_memory
//! - bench_agent_delta_serialization
//! - bench_agent_network_send

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::ingest::{DeltaResult, extract_delta};
use frankenterm_core::wire_protocol::{PaneDelta, WireEnvelope, WirePayload};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "wa_agent_streaming/bench_agent_capture_cpu",
        budget: "capture loop overhead remains bounded across idle/moderate/heavy pane output rates",
    },
    bench_common::BenchBudget {
        name: "wa_agent_streaming/bench_agent_capture_memory",
        budget: "steady-state memory scales approximately linearly with monitored pane count",
    },
    bench_common::BenchBudget {
        name: "wa_agent_streaming/bench_agent_delta_serialization",
        budget: "pane delta envelope serialization latency remains bounded at 256B/4KB/64KB payload sizes",
    },
    bench_common::BenchBudget {
        name: "wa_agent_streaming/bench_agent_network_send",
        budget: "localhost capture->aggregator send latency remains stable for newline-delimited envelope frames",
    },
];

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create tokio runtime")
}

fn capture_cycle_for_profile(pane_count: usize, append_bytes: usize, rounds: usize) -> usize {
    let payload = "x".repeat(append_bytes);
    let mut previous: Vec<String> = vec![String::new(); pane_count];
    let mut extracted_bytes = 0usize;

    for _ in 0..rounds {
        for snapshot in &mut previous {
            let current = if append_bytes == 0 {
                snapshot.clone()
            } else {
                let mut next = String::with_capacity(snapshot.len().saturating_add(payload.len()));
                next.push_str(snapshot);
                next.push_str(&payload);
                next
            };

            let delta = extract_delta(snapshot, &current, 4096);
            match delta {
                DeltaResult::Content(content) => {
                    extracted_bytes = extracted_bytes.saturating_add(content.len());
                }
                DeltaResult::Gap { content, .. } => {
                    extracted_bytes = extracted_bytes.saturating_add(content.len());
                }
                DeltaResult::NoChange => {}
            }
            *snapshot = current;
        }
    }

    extracted_bytes
}

fn modeled_pane_bytes(pane_count: usize, steady_state_bytes_per_pane: usize) -> usize {
    let buffers: Vec<String> = (0..pane_count)
        .map(|idx| format!("pane-{idx}:{}", "x".repeat(steady_state_bytes_per_pane)))
        .collect();
    buffers.iter().map(std::string::String::len).sum()
}

fn envelope_for_payload_size(payload_size: usize) -> WireEnvelope {
    let content = "d".repeat(payload_size);
    WireEnvelope::new(
        1,
        "wa-agent-bench",
        WirePayload::PaneDelta(PaneDelta {
            pane_id: 7,
            seq: 42,
            content_len: content.len(),
            content,
            captured_at_ms: 1_700_000_000_000,
        }),
    )
}

async fn send_frame_over_loopback(bytes: Vec<u8>) -> std::io::Result<usize> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = Vec::new();
        let size = reader.read_until(b'\n', &mut line).await?;
        Ok::<usize, std::io::Error>(size)
    });

    let mut client = TcpStream::connect(addr).await?;
    client.write_all(&bytes).await?;
    client.write_all(b"\n").await?;
    client.flush().await?;

    server.await.expect("loopback server task join")
}

fn bench_agent_capture_cpu(c: &mut Criterion) {
    let mut group = c.benchmark_group("wa_agent_streaming/bench_agent_capture_cpu");
    group.measurement_time(Duration::from_secs(8));

    let scenarios = [
        ("idle", 0usize),
        ("moderate_1kb", 1024usize),
        ("heavy_1mb", 1024usize * 1024usize),
    ];

    for (label, append_bytes) in scenarios {
        group.throughput(Throughput::Bytes(append_bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("profile", label),
            &append_bytes,
            |b, &append_bytes| {
                b.iter(|| {
                    let bytes = capture_cycle_for_profile(1, append_bytes, 1);
                    black_box(bytes);
                });
            },
        );
    }

    group.finish();
}

fn bench_agent_capture_memory(c: &mut Criterion) {
    let mut group = c.benchmark_group("wa_agent_streaming/bench_agent_capture_memory");
    group.measurement_time(Duration::from_secs(8));

    for pane_count in [1usize, 10, 50] {
        let steady_state_bytes = 16 * 1024usize;
        group.throughput(Throughput::Bytes((pane_count * steady_state_bytes) as u64));
        group.bench_with_input(
            BenchmarkId::new("panes", pane_count),
            &pane_count,
            |b, &pane_count| {
                b.iter(|| {
                    let modeled = modeled_pane_bytes(pane_count, steady_state_bytes);
                    black_box(modeled);
                });
            },
        );
    }

    group.finish();
}

fn bench_agent_delta_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("wa_agent_streaming/bench_agent_delta_serialization");
    group.measurement_time(Duration::from_secs(8));

    for payload_size in [256usize, 4 * 1024usize, 64 * 1024usize] {
        group.throughput(Throughput::Bytes(payload_size as u64));
        group.bench_with_input(
            BenchmarkId::new("payload_bytes", payload_size),
            &payload_size,
            |b, &payload_size| {
                let envelope = envelope_for_payload_size(payload_size);
                b.iter(|| {
                    let encoded = envelope.to_json().expect("serialize pane delta envelope");
                    black_box(encoded.len());
                });
            },
        );
    }

    group.finish();
}

fn bench_agent_network_send(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("wa_agent_streaming/bench_agent_network_send");
    group.measurement_time(Duration::from_secs(8));

    for payload_size in [256usize, 4 * 1024usize, 64 * 1024usize] {
        group.throughput(Throughput::Bytes(payload_size as u64));
        group.bench_with_input(
            BenchmarkId::new("payload_bytes", payload_size),
            &payload_size,
            |b, &payload_size| {
                let frame = envelope_for_payload_size(payload_size)
                    .to_json()
                    .expect("serialize frame");
                b.to_async(&rt).iter(|| async {
                    let sent = send_frame_over_loopback(frame.clone())
                        .await
                        .expect("send frame");
                    black_box(sent);
                });
            },
        );
    }

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_agent_capture_cpu(c);
    bench_agent_capture_memory(c);
    bench_agent_delta_serialization(c);
    bench_agent_network_send(c);
    bench_common::emit_bench_artifacts("wa_agent_streaming", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
