//! Criterion benchmarks for Unix socket throughput and framing overhead.
//!
//! Bead: wa-q8vj3

use std::hint::black_box;
use std::io;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::runtime_compat::unix;
use unix::{AsyncReadExt, AsyncWriteExt};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "socket_throughput/roundtrip_latency",
        budget: "round-trip latency for 64B..1MB payloads stays bounded and monotonic",
    },
    bench_common::BenchBudget {
        name: "socket_throughput/streaming_throughput",
        budget: "sustained unix-stream throughput remains stable across frame sizes",
    },
    bench_common::BenchBudget {
        name: "socket_throughput/pdu_framing_overhead",
        budget: "4-byte framing overhead remains low vs raw payload throughput",
    },
    bench_common::BenchBudget {
        name: "socket_throughput/connect_accept_latency",
        budget: "connect+accept stays in low-millisecond range on local unix sockets",
    },
];

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create tokio runtime")
}

fn socket_path(prefix: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("frankenterm-bench-{prefix}-{ts}.sock"))
}

async fn roundtrip_once(payload_size: usize) -> io::Result<usize> {
    let socket_path = socket_path("roundtrip");
    let listener = unix::bind(&socket_path).await?;
    let payload = vec![0x5Au8; payload_size];

    let server_payload_size = payload_size;
    let server = tokio::spawn(async move {
        let (mut stream, _addr) = listener.accept().await?;
        let mut inbound = vec![0_u8; server_payload_size];
        stream.read_exact(&mut inbound).await?;
        stream.write_all(&inbound).await?;
        Ok::<usize, io::Error>(inbound.len())
    });

    let mut stream = unix::connect(&socket_path).await?;
    stream.write_all(&payload).await?;
    let mut echoed = vec![0_u8; payload_size];
    stream.read_exact(&mut echoed).await?;
    debug_assert_eq!(echoed, payload);

    let server_len = server.await.expect("server join")?;
    Ok(server_len)
}

async fn connect_accept_once() -> io::Result<()> {
    let socket_path = socket_path("connect");
    let listener = unix::bind(&socket_path).await?;

    let server = tokio::spawn(async move {
        let (_stream, _addr) = listener.accept().await?;
        Ok::<(), io::Error>(())
    });

    let _client = unix::connect(&socket_path).await?;
    server.await.expect("server join")?;
    Ok(())
}

async fn stream_throughput_once(
    payload_size: usize,
    frame_count: usize,
    use_framing: bool,
) -> io::Result<usize> {
    let socket_path = socket_path("stream");
    let listener = unix::bind(&socket_path).await?;
    let (writer_res, accept_res) = tokio::join!(unix::connect(&socket_path), listener.accept());
    let mut writer = writer_res?;
    let (mut reader, _addr) = accept_res?;
    let payload = vec![0xA5u8; payload_size];

    let read_task = tokio::spawn(async move {
        let mut bytes_read = 0usize;
        for _ in 0..frame_count {
            if use_framing {
                let mut len_buf = [0_u8; 4];
                reader.read_exact(&mut len_buf).await?;
                let frame_len = usize::try_from(u32::from_be_bytes(len_buf)).unwrap_or(0);
                let mut buf = vec![0_u8; frame_len];
                reader.read_exact(&mut buf).await?;
                bytes_read = bytes_read.saturating_add(frame_len);
            } else {
                let mut buf = vec![0_u8; payload_size];
                reader.read_exact(&mut buf).await?;
                bytes_read = bytes_read.saturating_add(payload_size);
            }
        }
        Ok::<usize, io::Error>(bytes_read)
    });

    for _ in 0..frame_count {
        if use_framing {
            let len = u32::try_from(payload_size)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "payload too large"))?;
            writer.write_all(&len.to_be_bytes()).await?;
        }
        writer.write_all(&payload).await?;
    }

    read_task.await.expect("reader join")
}

fn bench_roundtrip_latency(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("socket_throughput/roundtrip_latency");

    for &payload_size in &[64usize, 1024, 64 * 1024, 1024 * 1024] {
        group.throughput(Throughput::Bytes(payload_size as u64));
        group.bench_with_input(
            BenchmarkId::new("unix_roundtrip", payload_size),
            &payload_size,
            |b, &size| {
                b.to_async(&rt).iter(|| async move {
                    let echoed = roundtrip_once(size).await.expect("roundtrip");
                    black_box(echoed);
                });
            },
        );
    }

    group.finish();
}

fn bench_streaming_throughput(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("socket_throughput/streaming_throughput");

    for &payload_size in &[256usize, 4 * 1024, 64 * 1024] {
        let frames = 128usize;
        group.throughput(Throughput::Bytes((payload_size * frames) as u64));
        group.bench_with_input(
            BenchmarkId::new("raw_stream", payload_size),
            &payload_size,
            |b, &size| {
                b.to_async(&rt).iter(|| async move {
                    let bytes = stream_throughput_once(size, frames, false)
                        .await
                        .expect("raw stream");
                    black_box(bytes);
                });
            },
        );
    }

    group.finish();
}

fn bench_pdu_framing_overhead(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("socket_throughput/pdu_framing_overhead");

    for &payload_size in &[256usize, 4 * 1024, 64 * 1024] {
        let frames = 128usize;
        group.throughput(Throughput::Bytes((payload_size * frames) as u64));
        group.bench_with_input(
            BenchmarkId::new("framed_stream", payload_size),
            &payload_size,
            |b, &size| {
                b.to_async(&rt).iter(|| async move {
                    let bytes = stream_throughput_once(size, frames, true)
                        .await
                        .expect("framed stream");
                    black_box(bytes);
                });
            },
        );
    }

    group.finish();
}

fn bench_connect_accept_latency(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("socket_throughput/connect_accept_latency");
    group.measurement_time(Duration::from_secs(8));

    group.bench_function("connect_accept", |b| {
        b.to_async(&rt).iter(|| async {
            connect_accept_once().await.expect("connect+accept");
        });
    });

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_roundtrip_latency(c);
    bench_streaming_throughput(c);
    bench_pdu_framing_overhead(c);
    bench_connect_accept_latency(c);
    bench_common::emit_bench_artifacts("socket_throughput", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
