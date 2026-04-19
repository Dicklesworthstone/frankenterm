//! Criterion benchmarks for homomorphic stream hashing.
//!
//! Bead: wa-283h4.6
//!
//! Performance budgets:
//! - 1 KB hash:   **< 1µs** (≥ 1 GB/s throughput)
//! - 64 KB hash:  **< 64µs**
//! - 1 MB hash:   **< 1ms**
//! - Throughput:   **> 1 GB/s** on modern hardware
//! - Verify/digest compare: **< 10ns**
//! - Combine (homomorphic): **O(1)** regardless of data size

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use frankenterm_core::stream_hash::{IntegrityChecker, StreamDigest, StreamHash};

mod bench_common;

/// Performance budgets for CI regression detection.
#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "stream_hash_1kb",
        budget: "p50 < 1us (1 KB payload, >= 1 GB/s)",
    },
    bench_common::BenchBudget {
        name: "stream_hash_64kb",
        budget: "p50 < 64us (64 KB payload)",
    },
    bench_common::BenchBudget {
        name: "stream_hash_1mb",
        budget: "p50 < 1ms (1 MB payload)",
    },
    bench_common::BenchBudget {
        name: "stream_hash_throughput",
        budget: "> 1 GB/s continuous hashing on modern hardware",
    },
    bench_common::BenchBudget {
        name: "stream_hash_verify",
        budget: "p50 < 10ns (digest comparison)",
    },
    bench_common::BenchBudget {
        name: "stream_hash_combine",
        budget: "O(1) regardless of constituent data sizes",
    },
];

// ── Data generators ────────────────────────────────────────────────────

/// Deterministic pseudo-random data via LCG for reproducibility.
fn pseudo_random(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((state >> 33) as u8);
    }
    out
}

/// Terminal-like output (ANSI escapes + printable ASCII).
fn terminal_data(n: usize) -> Vec<u8> {
    let fragment = b"\x1b[32muser@host\x1b[0m:\x1b[34m~/project\x1b[0m$ cargo test\r\n\
                     running 42 tests\r\ntest foo::bar ... ok\r\n";
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let remaining = n - out.len();
        let chunk = remaining.min(fragment.len());
        out.extend_from_slice(&fragment[..chunk]);
    }
    out
}

/// All zeros (low entropy edge case).
fn constant_data(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

// ── Benchmarks ─────────────────────────────────────────────────────────

/// Benchmark: hash payloads of varying sizes.
/// Targets: 1 KB < 1µs, 64 KB < 64µs, 1 MB < 1ms.
fn bench_hash_payload_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_hash/payload_sizes");

    for (label, size) in [
        ("1KB", 1024),
        ("4KB", 4096),
        ("16KB", 16_384),
        ("64KB", 65_536),
        ("256KB", 262_144),
        ("1MB", 1_048_576),
    ] {
        group.throughput(Throughput::Bytes(size as u64));
        let data = pseudo_random(size);

        group.bench_with_input(
            BenchmarkId::new("random", label),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut h = StreamHash::new();
                    h.update(black_box(data));
                    black_box(h.digest())
                });
            },
        );
    }

    // Also benchmark terminal-like data at 64 KB.
    let terminal = terminal_data(65_536);
    group.throughput(Throughput::Bytes(65_536));
    group.bench_function("terminal_64KB", |b| {
        b.iter(|| {
            let mut h = StreamHash::new();
            h.update(black_box(&terminal));
            black_box(h.digest())
        });
    });

    // Constant data at 64 KB (tests hash with zero entropy).
    let zeros = constant_data(65_536);
    group.bench_function("constant_64KB", |b| {
        b.iter(|| {
            let mut h = StreamHash::new();
            h.update(black_box(&zeros));
            black_box(h.digest())
        });
    });

    group.finish();
}

/// Benchmark: per-byte update throughput.
/// Tests the hot path for byte-at-a-time feeding.
fn bench_single_byte_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_hash/single_byte_update");

    group.bench_function("cold_byte", |b| {
        b.iter_batched(
            StreamHash::new,
            |mut h| {
                h.update_byte(black_box(42));
                h
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("after_warmup", |b| {
        b.iter_batched(
            || {
                let mut h = StreamHash::new();
                h.update(&pseudo_random(1000));
                h
            },
            |mut h| {
                h.update_byte(black_box(99));
                h
            },
            BatchSize::SmallInput,
        );
    });

    // Batch of 256 individual byte updates (simulates byte-at-a-time stream).
    group.bench_function("256_individual_bytes", |b| {
        let bytes: Vec<u8> = (0..=255).collect();
        b.iter(|| {
            let mut h = StreamHash::new();
            for &byte in black_box(&bytes) {
                h.update_byte(byte);
            }
            black_box(h.digest())
        });
    });

    group.finish();
}

/// Benchmark: digest comparison.
/// Target: < 10ns.
fn bench_digest_compare(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_hash/digest_compare");

    let mut h1 = StreamHash::new();
    h1.update(b"matching data stream");
    let d1 = h1.digest();

    let mut h2 = StreamHash::new();
    h2.update(b"matching data stream");
    let d2 = h2.digest();

    let mut h3 = StreamHash::new();
    h3.update(b"different data stream");
    let d3 = h3.digest();

    group.bench_function("equal_digests", |b| {
        b.iter(|| black_box(black_box(&d1).matches(black_box(&d2))));
    });

    group.bench_function("unequal_digests", |b| {
        b.iter(|| black_box(black_box(&d1).matches(black_box(&d3))));
    });

    group.bench_function("digest_hex", |b| {
        b.iter(|| black_box(black_box(&d1).hex()));
    });

    group.bench_function("digest_display", |b| {
        b.iter(|| black_box(format!("{}", black_box(&d1))));
    });

    group.finish();
}

/// Benchmark: homomorphic combine.
/// Target: O(1) regardless of how much data was hashed.
fn bench_combine(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_hash/combine");

    // Combine cost should be constant regardless of data size.
    for (label, size) in [("1KB", 1024), ("64KB", 65_536), ("1MB", 1_048_576)] {
        let data = pseudo_random(size);
        let mid = size / 2;

        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(data, mid),
            |b, (data, mid)| {
                b.iter_batched(
                    || {
                        let mut ha = StreamHash::new();
                        ha.update(&data[..*mid]);
                        let mut hb = StreamHash::new();
                        hb.update(&data[*mid..]);
                        (ha, hb)
                    },
                    |(ha, hb)| black_box(ha.combine(&hb)),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // Combine chain: combine 10 segments.
    group.bench_function("chain_10_segments", |b| {
        b.iter_batched(
            || {
                let data = pseudo_random(10_000);
                let mut segments = Vec::new();
                for chunk in data.chunks(1000) {
                    let mut h = StreamHash::new();
                    h.update(chunk);
                    segments.push(h);
                }
                segments
            },
            |segments| {
                let mut combined = segments[0].clone();
                for seg in &segments[1..] {
                    combined = combined.combine(seg);
                }
                black_box(combined.digest())
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: incremental vs bulk hashing.
/// Verifies that chunk boundaries don't affect performance significantly.
fn bench_incremental_vs_bulk(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_hash/incremental_vs_bulk");
    let data = pseudo_random(65_536);

    group.throughput(Throughput::Bytes(65_536));

    group.bench_function("bulk_64KB", |b| {
        b.iter(|| {
            let mut h = StreamHash::new();
            h.update(black_box(&data));
            black_box(h.digest())
        });
    });

    // 64-byte chunks (typical cache line).
    group.bench_function("64B_chunks", |b| {
        b.iter(|| {
            let mut h = StreamHash::new();
            for chunk in black_box(&data).chunks(64) {
                h.update(chunk);
            }
            black_box(h.digest())
        });
    });

    // 1 KB chunks.
    group.bench_function("1KB_chunks", |b| {
        b.iter(|| {
            let mut h = StreamHash::new();
            for chunk in black_box(&data).chunks(1024) {
                h.update(chunk);
            }
            black_box(h.digest())
        });
    });

    // 4 KB chunks (typical page size).
    group.bench_function("4KB_chunks", |b| {
        b.iter(|| {
            let mut h = StreamHash::new();
            for chunk in black_box(&data).chunks(4096) {
                h.update(chunk);
            }
            black_box(h.digest())
        });
    });

    group.finish();
}

/// Benchmark: IntegrityChecker full pipeline.
/// Simulates producer/consumer verification pattern.
fn bench_integrity_checker(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_hash/integrity_checker");

    // Feed + set remote + check — the full verification pipeline.
    for (label, size) in [("1KB", 1024), ("16KB", 16_384), ("64KB", 65_536)] {
        let data = pseudo_random(size);

        group.bench_with_input(
            BenchmarkId::new("full_pipeline", label),
            &data,
            |b, data| {
                b.iter_batched(
                    || {
                        // Pre-compute the "producer" digest.
                        let mut producer = StreamHash::new();
                        producer.update(data);
                        producer.digest()
                    },
                    |remote_digest| {
                        let mut checker = IntegrityChecker::new();
                        checker.update(black_box(data));
                        checker.set_remote_digest(remote_digest);
                        black_box(checker.check())
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // Just the check() call after data is already fed.
    group.bench_function("check_only", |b| {
        let data = pseudo_random(65_536);
        let mut producer = StreamHash::new();
        producer.update(&data);
        let remote_digest = producer.digest();

        b.iter_batched(
            || {
                let mut checker = IntegrityChecker::new();
                checker.update(&data);
                checker.set_remote_digest(remote_digest);
                checker
            },
            |mut checker| black_box(checker.check()),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: StreamHash reset and reuse.
fn bench_reset(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_hash/reset");
    let data = pseudo_random(4096);

    // Cost of reset after feeding data.
    group.bench_function("reset_after_4KB", |b| {
        b.iter_batched(
            || {
                let mut h = StreamHash::new();
                h.update(&data);
                h
            },
            |mut h| {
                h.reset();
                black_box(h)
            },
            BatchSize::SmallInput,
        );
    });

    // Hash → digest → reset → hash cycle (streaming reuse pattern).
    group.bench_function("hash_reset_cycle", |b| {
        let chunk = pseudo_random(1024);
        b.iter_batched(
            StreamHash::new,
            |mut h| {
                h.update(black_box(&chunk));
                let d = h.digest();
                h.reset();
                h.update(black_box(&chunk));
                black_box((d, h.digest()))
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: StreamDigest serde roundtrip.
fn bench_digest_serde(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_hash/digest_serde");

    let digest = StreamDigest {
        h1: 0x1234_5678_9ABC_DEF0,
        h2: 0xFEDC_BA98_7654_3210,
        len: 100_000,
    };

    group.bench_function("serialize", |b| {
        b.iter(|| black_box(serde_json::to_string(black_box(&digest)).unwrap()));
    });

    let json = serde_json::to_string(&digest).unwrap();
    group.bench_function("deserialize", |b| {
        b.iter(|| black_box(serde_json::from_str::<StreamDigest>(black_box(&json)).unwrap()));
    });

    group.bench_function("roundtrip", |b| {
        b.iter(|| {
            let s = serde_json::to_string(black_box(&digest)).unwrap();
            black_box(serde_json::from_str::<StreamDigest>(&s).unwrap())
        });
    });

    group.finish();
}

// ── Criterion harness ──────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_hash_payload_sizes,
    bench_single_byte_update,
    bench_digest_compare,
    bench_combine,
    bench_incremental_vs_bulk,
    bench_integrity_checker,
    bench_reset,
    bench_digest_serde,
);

criterion_main!(benches);
