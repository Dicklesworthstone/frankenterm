//! Benchmarks for SIMD-friendly output scanning (`simd_scan` module).
//!
//! Focus:
//! - newline scanning throughput across payload sizes
//! - ANSI-heavy workloads (escape-sequence dense)
//! - fast path vs scalar reference implementation

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::simd_scan::{
    OutputScanMetrics, OutputScanState, scan_newlines_and_ansi, scan_newlines_and_ansi_with_state,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "simd_scan_newline",
        budget: "fast path should outperform scalar across 1KB..16MB buffers",
    },
    bench_common::BenchBudget {
        name: "simd_scan_ansi_heavy",
        budget: "ANSI-heavy throughput should avoid scalar-regression pathologies",
    },
    bench_common::BenchBudget {
        name: "simd_scan_mixed_payload",
        budget: "stable scan throughput across plain/ansi/binary payload classes",
    },
    bench_common::BenchBudget {
        name: "simd_scan_chunked_stateful",
        budget: "stateful chunked SIMD scan should beat scalar by >2x on dense ASCII logs",
    },
];

fn payload_plain(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut i = 0usize;
    while out.len() < size {
        if i % 80 == 79 {
            out.push(b'\n');
        } else {
            out.push(b'a' + (i % 26) as u8);
        }
        i += 1;
    }
    out
}

fn payload_ansi_heavy(size: usize) -> Vec<u8> {
    // ESC[38;5;196mERR_ESC_SEQ_ESC[0m\n (31 bytes)
    const CHUNK: &[u8] = b"\x1b[38;5;196mERR_ESC_SEQ\x1b[0m\n";
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        let remaining = size - out.len();
        let copy_len = CHUNK.len().min(remaining);
        out.extend_from_slice(&CHUNK[..copy_len]);
    }
    out
}

fn payload_binary_like(size: usize) -> Vec<u8> {
    // Deterministic pseudo-random bytes with sparse newline/ESC injection.
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut out = Vec::with_capacity(size);
    for i in 0..size {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let mut b = (state >> 56) as u8;
        if i % 97 == 0 {
            b = b'\n';
        } else if i % 211 == 0 {
            b = 0x1b;
        }
        out.push(b);
    }
    out
}

fn payload_dense_logs(size: usize) -> Vec<u8> {
    // Mostly-ASCII operator logs with frequent newlines and sparse ANSI tags.
    const LINE_PREFIX: &[u8] = b"2026-02-24T09:15:42.123Z INFO pane=42 ";
    const BODY: &[u8] = b"worker heartbeat accepted request_id=abc123 latency_ms=17";
    const ANSI_WARN: &[u8] = b"\x1b[33mWARN\x1b[0m ";

    let mut out = Vec::with_capacity(size);
    let mut line = 0usize;
    while out.len() < size {
        out.extend_from_slice(LINE_PREFIX);
        if line % 64 == 0 {
            out.extend_from_slice(ANSI_WARN);
        }
        out.extend_from_slice(BODY);
        out.push(b'\n');
        line += 1;
    }
    out.truncate(size);
    out
}

fn scalar_scan_reference(bytes: &[u8]) -> OutputScanMetrics {
    let mut newline_count = 0usize;
    let mut ansi_byte_count = 0usize;
    let mut in_escape = false;

    for &b in bytes {
        if b == b'\n' {
            newline_count += 1;
        }

        if b == 0x1b {
            in_escape = true;
            ansi_byte_count += 1;
        } else if in_escape {
            ansi_byte_count += 1;
            if (0x40..=0x7E).contains(&b) && b != b'[' {
                in_escape = false;
            }
        }
    }

    OutputScanMetrics {
        newline_count,
        ansi_byte_count,
    }
}

#[derive(Default)]
struct ScalarState {
    in_escape: bool,
    pending_utf8_continuations: u8,
}

fn scalar_scan_reference_with_state(bytes: &[u8], state: &mut ScalarState) -> OutputScanMetrics {
    let mut newline_count = 0usize;
    let mut ansi_byte_count = 0usize;
    let mut in_escape = state.in_escape;
    let mut pending_utf8 = state.pending_utf8_continuations;

    for &b in bytes {
        if b == b'\n' {
            newline_count += 1;
        }

        if b == 0x1b {
            in_escape = true;
            ansi_byte_count += 1;
        } else if in_escape {
            ansi_byte_count += 1;
            if (0x40..=0x7E).contains(&b) && b != b'[' {
                in_escape = false;
            }
        }

        if pending_utf8 > 0 && (b & 0b1100_0000) == 0b1000_0000 {
            pending_utf8 -= 1;
            continue;
        }

        pending_utf8 = match b {
            0xC2..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF4 => 3,
            _ => 0,
        };
    }

    state.in_escape = in_escape;
    state.pending_utf8_continuations = pending_utf8;

    OutputScanMetrics {
        newline_count,
        ansi_byte_count,
    }
}

fn bench_newline_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("simd_scan_newline");

    for size in [1024usize, 64 * 1024, 1024 * 1024, 16 * 1024 * 1024] {
        let payload = payload_plain(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("fast", size), &payload, |b, data| {
            b.iter(|| {
                let metrics = scan_newlines_and_ansi(black_box(data));
                black_box(metrics.newline_count)
            });
        });

        group.bench_with_input(BenchmarkId::new("scalar", size), &payload, |b, data| {
            b.iter(|| {
                let metrics = scalar_scan_reference(black_box(data));
                black_box(metrics.newline_count)
            });
        });
    }

    group.finish();
}

fn bench_ansi_heavy_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("simd_scan_ansi_heavy");

    for size in [1024usize, 64 * 1024, 1024 * 1024] {
        let payload = payload_ansi_heavy(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("fast", size), &payload, |b, data| {
            b.iter(|| {
                let metrics = scan_newlines_and_ansi(black_box(data));
                black_box(metrics.ansi_byte_count)
            });
        });

        group.bench_with_input(BenchmarkId::new("scalar", size), &payload, |b, data| {
            b.iter(|| {
                let metrics = scalar_scan_reference(black_box(data));
                black_box(metrics.ansi_byte_count)
            });
        });
    }

    group.finish();
}

fn bench_mixed_payload_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("simd_scan_mixed_payload");
    let cases = [
        ("plain_1m", payload_plain(1024 * 1024)),
        ("ansi_1m", payload_ansi_heavy(1024 * 1024)),
        ("binary_1m", payload_binary_like(1024 * 1024)),
    ];

    for (name, payload) in &cases {
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(BenchmarkId::new("fast", name), payload, |b, data| {
            b.iter(|| black_box(scan_newlines_and_ansi(black_box(data))));
        });
    }

    group.finish();
}

fn bench_chunked_stateful_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("simd_scan_chunked_stateful");
    let payload = payload_dense_logs(2 * 1024 * 1024);

    for chunk_size in [16usize, 32, 64, 128, 256] {
        group.throughput(Throughput::Bytes(payload.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("fast_stateful", chunk_size),
            &chunk_size,
            |b, &chunk| {
                b.iter(|| {
                    let mut state = OutputScanState::default();
                    let mut total = OutputScanMetrics::default();
                    for piece in payload.chunks(chunk) {
                        let m = scan_newlines_and_ansi_with_state(black_box(piece), &mut state);
                        total.newline_count += m.newline_count;
                        total.ansi_byte_count += m.ansi_byte_count;
                    }
                    black_box(total);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("scalar_stateful", chunk_size),
            &chunk_size,
            |b, &chunk| {
                b.iter(|| {
                    let mut state = ScalarState::default();
                    let mut total = OutputScanMetrics::default();
                    for piece in payload.chunks(chunk) {
                        let m = scalar_scan_reference_with_state(black_box(piece), &mut state);
                        total.newline_count += m.newline_count;
                        total.ansi_byte_count += m.ansi_byte_count;
                    }
                    black_box(total);
                });
            },
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("simd_scan", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_newline_scan, bench_ansi_heavy_scan, bench_mixed_payload_scan, bench_chunked_stateful_scan
);
criterion_main!(benches);
