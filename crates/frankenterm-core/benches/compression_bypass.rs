//! Benchmarks for vendored mux compression bypass behavior.
//!
//! Required scenarios:
//! - local_socket_overhead_savings
//! - compression_bypass_latency
//! - negotiation_overhead
//! - fallback_detection_latency

use std::path::PathBuf;
use std::time::Duration;

use codec::{
    CODEC_VERSION, CompressionMode, GetCodecVersionResponse, Pdu, UnitResponse, WriteToPane,
};
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use frankenterm_core::config::VendoredCompressionMode;
use frankenterm_core::runtime_compat::timeout;
use frankenterm_core::vendored::{DirectMuxClient, DirectMuxClientConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "local_socket_overhead_savings",
        budget: "throughput for no-compression local mode exceeds compressed mode for 1KB/10KB/100KB PDUs",
    },
    bench_common::BenchBudget {
        name: "compression_bypass_latency",
        budget: "per-PDU latency for local bypass mode is lower on sub-1KB payloads",
    },
    bench_common::BenchBudget {
        name: "negotiation_overhead",
        budget: "local auto-mode handshake overhead remains sub-millisecond scale",
    },
    bench_common::BenchBudget {
        name: "fallback_detection_latency",
        budget: "detect reject + retry fallback path remains a low single-digit millisecond operation",
    },
];

const COMPRESSED_MASK: u64 = 1 << 63;
const BENCH_SERIAL: u64 = 77;
const BENCH_PANE_ID: usize = 11;

fn make_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn make_payload(size: usize) -> Vec<u8> {
    (0..size)
        .map(|idx| (((idx * 31) + 7) % 251) as u8)
        .collect()
}

fn roundtrip_write_to_pane(mode: CompressionMode, payload_size: usize) -> usize {
    let pdu = Pdu::WriteToPane(WriteToPane {
        pane_id: BENCH_PANE_ID,
        data: make_payload(payload_size),
    });
    let mut encoded = Vec::with_capacity(payload_size + 128);
    pdu.encode_with_mode(&mut encoded, BENCH_SERIAL, mode)
        .expect("encode_with_mode");

    let decoded = Pdu::decode(encoded.as_slice()).expect("decode");
    match decoded.pdu {
        Pdu::WriteToPane(write) => write.data.len(),
        other => panic!("unexpected decoded pdu: {}", other.pdu_name()),
    }
}

fn decode_u64_leb128_prefix(bytes: &[u8]) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;

    for (idx, byte) in bytes.iter().copied().enumerate() {
        if idx >= 10 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if (byte & 0x80) == 0 {
            return Some(value);
        }
        shift += 7;
    }

    None
}

fn frame_marked_compressed(bytes: &[u8]) -> Option<bool> {
    decode_u64_leb128_prefix(bytes).map(|length| (length & COMPRESSED_MASK) != 0)
}

async fn serve_handshake(
    stream: &mut tokio::net::UnixStream,
    reject_uncompressed_first_frame: bool,
) {
    let mut read_buf = Vec::new();
    let mut first_frame_checked = false;

    loop {
        let mut temp = vec![0u8; 4096];
        let read = match stream.read(&mut temp).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        read_buf.extend_from_slice(&temp[..read]);

        if !first_frame_checked {
            if let Some(is_compressed) = frame_marked_compressed(&read_buf) {
                first_frame_checked = true;
                if reject_uncompressed_first_frame && !is_compressed {
                    return;
                }
            }
        }

        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
            let pdu = decoded.pdu;
            let is_handshake_complete = matches!(pdu, Pdu::SetClientId(_));
            let response = match pdu {
                Pdu::GetCodecVersion(_) => Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                    codec_vers: CODEC_VERSION,
                    version_string: "compression-bypass-bench".to_string(),
                    executable_path: PathBuf::from("/bin/wezterm"),
                    config_file_path: None,
                }),
                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                _ => continue,
            };

            let mut out = Vec::new();
            if response.encode(&mut out, decoded.serial).is_err() {
                return;
            }
            if stream.write_all(&out).await.is_err() {
                return;
            }
            if is_handshake_complete {
                return;
            }
        }
    }
}

async fn connect_once(mode: VendoredCompressionMode) -> Result<(), String> {
    let temp_dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let socket_path = temp_dir.path().join("compression-bypass-negotiation.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).map_err(|err| err.to_string())?;

    let server = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            serve_handshake(&mut stream, false).await;
        }
    });

    let mut config = DirectMuxClientConfig::default().with_socket_path(socket_path);
    config.compression_mode = mode;
    let client = DirectMuxClient::connect(config)
        .await
        .map_err(|err| err.to_string())?;
    drop(client);

    let _ = timeout(Duration::from_secs(1), server).await;
    Ok(())
}

async fn connect_with_manual_fallback() -> Result<(), String> {
    let temp_dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let socket_path = temp_dir.path().join("compression-bypass-fallback.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).map_err(|err| err.to_string())?;

    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            serve_handshake(&mut stream, attempt == 0).await;
        }
    });

    let auto = DirectMuxClientConfig::default().with_socket_path(socket_path.clone());
    if DirectMuxClient::connect(auto).await.is_ok() {
        return Err("expected first auto-mode attempt to fail".to_string());
    }

    let mut fallback = DirectMuxClientConfig::default().with_socket_path(socket_path);
    fallback.compression_mode = VendoredCompressionMode::Always;
    let client = DirectMuxClient::connect(fallback)
        .await
        .map_err(|err| err.to_string())?;
    drop(client);

    let _ = timeout(Duration::from_secs(1), server).await;
    Ok(())
}

fn bench_local_socket_overhead_savings(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_bypass/local_socket_overhead_savings");

    for &payload_size in &[1024usize, 10 * 1024, 100 * 1024] {
        group.throughput(Throughput::Bytes(payload_size as u64));
        group.bench_with_input(
            BenchmarkId::new("mode_never", payload_size),
            &payload_size,
            |b, &size| {
                b.iter(|| black_box(roundtrip_write_to_pane(CompressionMode::Never, size)));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("mode_always", payload_size),
            &payload_size,
            |b, &size| {
                b.iter(|| black_box(roundtrip_write_to_pane(CompressionMode::Always, size)));
            },
        );
    }

    group.finish();
}

fn bench_compression_bypass_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_bypass/compression_bypass_latency");

    for &payload_size in &[64usize, 256, 768] {
        group.bench_with_input(
            BenchmarkId::new("mode_never", payload_size),
            &payload_size,
            |b, &size| {
                b.iter(|| black_box(roundtrip_write_to_pane(CompressionMode::Never, size)));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("mode_always", payload_size),
            &payload_size,
            |b, &size| {
                b.iter(|| black_box(roundtrip_write_to_pane(CompressionMode::Always, size)));
            },
        );
    }

    group.finish();
}

fn bench_negotiation_overhead(c: &mut Criterion) {
    let rt = make_runtime();
    let mut group = c.benchmark_group("compression_bypass/negotiation_overhead");
    group.sample_size(20);

    group.bench_function("auto_local_connect_handshake", |b| {
        b.to_async(&rt).iter(|| async {
            connect_once(VendoredCompressionMode::Auto)
                .await
                .expect("connect once");
        });
    });

    group.finish();
}

fn bench_fallback_detection_latency(c: &mut Criterion) {
    let rt = make_runtime();
    let mut group = c.benchmark_group("compression_bypass/fallback_detection_latency");
    group.sample_size(20);

    group.bench_function("auto_reject_then_always_retry", |b| {
        b.to_async(&rt).iter(|| async {
            connect_with_manual_fallback()
                .await
                .expect("manual fallback");
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("compression_bypass", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_local_socket_overhead_savings,
        bench_compression_bypass_latency,
        bench_negotiation_overhead,
        bench_fallback_detection_latency
);
criterion_main!(benches);
