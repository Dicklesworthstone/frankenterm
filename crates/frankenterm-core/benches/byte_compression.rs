//! Benchmarks for zstd byte-level compression (`byte_compression` module).
//!
//! Measures:
//! - Compression throughput at various levels (Fast/Default/High)
//! - Decompression throughput
//! - Dictionary vs no-dictionary comparison
//! - Batch compression overhead
//! - Various payload types (plain text, ANSI-heavy, repetitive)

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::byte_compression::{
    ByteCompressor, CompressionLevel, terminal_dictionary_seeds, train_dictionary,
};

mod bench_common;

// =============================================================================
// Payload generators
// =============================================================================

/// Plain text: shell-like output with line numbers.
fn plain_text_payload(size: usize) -> Vec<u8> {
    let line = b"$ cargo build --release 2>&1 | head -20\n";
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        buf.extend_from_slice(line);
    }
    buf.truncate(size);
    buf
}

/// ANSI-heavy: colored build output with escape sequences.
fn ansi_payload(size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size);
    let mut i = 0u32;
    while buf.len() < size {
        let line = format!(
            "\x1b[38;5;{}m   Compiling\x1b[0m crate-{} v0.1.{} (/path/to/crate-{})\n",
            (i % 256),
            i,
            i % 100,
            i
        );
        buf.extend_from_slice(line.as_bytes());
        i += 1;
    }
    buf.truncate(size);
    buf
}

/// Repetitive: progress counter output (highly compressible).
fn repetitive_payload(size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size);
    let mut i = 0u64;
    while buf.len() < size {
        let line = format!(
            "Downloading artifacts... {i}/10000 ({:.1}%)\n",
            i as f64 / 100.0
        );
        buf.extend_from_slice(line.as_bytes());
        i += 1;
    }
    buf.truncate(size);
    buf
}

/// Mixed: alternating plain, ANSI, and binary-ish content.
fn mixed_payload(size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size);
    let mut i = 0u32;
    while buf.len() < size {
        match i % 3 {
            0 => {
                let line = format!("$ echo 'step {i}'\nstep {i}\n");
                buf.extend_from_slice(line.as_bytes());
            }
            1 => {
                let line = format!("\x1b[32m  OK\x1b[0m test_{i} passed in {}ms\n", i * 3 + 7);
                buf.extend_from_slice(line.as_bytes());
            }
            _ => {
                // Binary-ish: raw bytes
                for j in 0..40u8 {
                    buf.push(j.wrapping_add(i as u8));
                }
                buf.push(b'\n');
            }
        }
        i += 1;
    }
    buf.truncate(size);
    buf
}

// =============================================================================
// Benchmarks
// =============================================================================

fn bench_compression_levels(c: &mut Criterion) {
    let mut group = c.benchmark_group("byte_compression_levels");

    let payload = plain_text_payload(1024 * 1024); // 1 MiB
    group.throughput(Throughput::Bytes(payload.len() as u64));

    for (label, level) in [
        ("fast", CompressionLevel::Fast),
        ("default", CompressionLevel::Default),
        ("high", CompressionLevel::High),
    ] {
        let compressor = ByteCompressor::new(level);

        group.bench_with_input(BenchmarkId::new("compress", label), &payload, |b, data| {
            b.iter(|| black_box(compressor.compress(black_box(data))));
        });
    }

    // Decompression (always fast — level doesn't matter)
    let compressor = ByteCompressor::new(CompressionLevel::Default);
    let compressed = compressor.compress(&payload);
    group.bench_function("decompress_default", |b| {
        b.iter(|| black_box(compressor.decompress(black_box(&compressed)).unwrap()));
    });

    group.finish();
}

fn bench_compression_payload_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("byte_compression_sizes");

    for size in [1024, 64 * 1024, 1024 * 1024, 4 * 1024 * 1024] {
        let payload = plain_text_payload(size);
        let label = match size {
            1024 => "1KB",
            65536 => "64KB",
            1048576 => "1MB",
            4194304 => "4MB",
            _ => "other",
        };

        group.throughput(Throughput::Bytes(payload.len() as u64));

        let compressor = ByteCompressor::new(CompressionLevel::Default);

        group.bench_with_input(BenchmarkId::new("compress", label), &payload, |b, data| {
            b.iter(|| black_box(compressor.compress(black_box(data))))
        });

        let compressed = compressor.compress(&payload);
        group.bench_with_input(
            BenchmarkId::new("decompress", label),
            &compressed,
            |b, data| b.iter(|| black_box(compressor.decompress(black_box(data)).unwrap())),
        );
    }

    group.finish();
}

fn bench_compression_payload_types(c: &mut Criterion) {
    let mut group = c.benchmark_group("byte_compression_payload_types");

    let size = 1024 * 1024; // 1 MiB

    for (label, payload) in [
        ("plain_text", plain_text_payload(size)),
        ("ansi_heavy", ansi_payload(size)),
        ("repetitive", repetitive_payload(size)),
        ("mixed", mixed_payload(size)),
    ] {
        group.throughput(Throughput::Bytes(payload.len() as u64));

        let compressor = ByteCompressor::new(CompressionLevel::Default);

        group.bench_with_input(BenchmarkId::new("compress", label), &payload, |b, data| {
            b.iter(|| black_box(compressor.compress(black_box(data))))
        });

        let compressed = compressor.compress(&payload);
        let ratio = payload.len() as f64 / compressed.len() as f64;

        group.bench_with_input(
            BenchmarkId::new(format!("decompress_ratio_{ratio:.1}x"), label),
            &compressed,
            |b, data| b.iter(|| black_box(compressor.decompress(black_box(data)).unwrap())),
        );
    }

    group.finish();
}

fn bench_dictionary_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("byte_compression_dictionary");

    // Train a dictionary from terminal output seeds
    let seeds = terminal_dictionary_seeds();
    let samples: Vec<&[u8]> = seeds.iter().map(|s| s.as_slice()).collect();

    let dict = match train_dictionary(&samples, 8192) {
        Ok(d) => d,
        Err(_) => {
            // Dictionary training can fail with limited samples; skip this benchmark
            group.finish();
            return;
        }
    };

    // Test with small payloads (where dictionary helps most)
    for size in [256, 1024, 4096] {
        let payload = ansi_payload(size);
        let label = match size {
            256 => "256B",
            1024 => "1KB",
            4096 => "4KB",
            _ => "other",
        };

        group.throughput(Throughput::Bytes(payload.len() as u64));

        let no_dict = ByteCompressor::new(CompressionLevel::Default);
        let with_dict =
            ByteCompressor::new(CompressionLevel::Default).with_dictionary(dict.clone());

        group.bench_with_input(BenchmarkId::new("no_dict", label), &payload, |b, data| {
            b.iter(|| black_box(no_dict.compress(black_box(data))))
        });

        group.bench_with_input(BenchmarkId::new("with_dict", label), &payload, |b, data| {
            b.iter(|| black_box(with_dict.compress(black_box(data))))
        });
    }

    group.finish();
}

fn bench_batch_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("byte_compression_batch");

    // Simulate 10 pane outputs
    let pane_outputs: Vec<Vec<u8>> = (0..10)
        .map(|i| {
            let mut buf = Vec::new();
            for j in 0..100 {
                let line =
                    format!("\x1b[32m[pane-{i}]\x1b[0m Output line {j}: processing task...\n");
                buf.extend_from_slice(line.as_bytes());
            }
            buf
        })
        .collect();

    let total_bytes: u64 = pane_outputs.iter().map(|b| b.len() as u64).sum();
    group.throughput(Throughput::Bytes(total_bytes));

    let compressor = ByteCompressor::new(CompressionLevel::Default);

    // Batch compression
    let refs: Vec<&[u8]> = pane_outputs.iter().map(|b| b.as_slice()).collect();
    group.bench_function("batch_10_panes", |b| {
        b.iter(|| black_box(compressor.compress_batch(black_box(&refs))));
    });

    // Individual compression for comparison
    group.bench_function("individual_10_panes", |b| {
        b.iter(|| {
            for pane in &pane_outputs {
                black_box(compressor.compress(black_box(pane)));
            }
        });
    });

    // Batch decompression
    let (batch, _stats) = compressor.compress_batch(&refs);
    group.bench_function("batch_decompress_10_panes", |b| {
        b.iter(|| black_box(compressor.decompress_batch(black_box(&batch)).unwrap()));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_compression_levels,
    bench_compression_payload_sizes,
    bench_compression_payload_types,
    bench_dictionary_comparison,
    bench_batch_compression,
);
criterion_main!(benches);
