//! Criterion benchmarks for the unified scan_pipeline module (ft-2oph2).
//!
//! Measures end-to-end throughput of the 3-stage pipeline (SIMD metrics +
//! Aho-Corasick triggers + zstd compression) in both batch and chunked modes.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::scan_pipeline::{ChunkedPipelineState, ScanPipeline, ScanPipelineConfig};
use std::hint::black_box;

// =============================================================================
// Payload generators
// =============================================================================

fn plain_log_payload(size: usize) -> Vec<u8> {
    let lines = [
        b"2025-01-15T08:30:00Z  INFO  server: request processed in 4ms\n".as_slice(),
        b"2025-01-15T08:30:01Z  DEBUG handler: path=/api/v2/users\n",
        b"2025-01-15T08:30:02Z  INFO  server: response 200 OK\n",
    ];
    let mut buf = Vec::with_capacity(size);
    let mut i = 0;
    while buf.len() < size {
        buf.extend_from_slice(lines[i % lines.len()]);
        i += 1;
    }
    buf.truncate(size);
    buf
}

fn cargo_build_payload(size: usize) -> Vec<u8> {
    let lines = [
        b"   Compiling serde v1.0.200\n".as_slice(),
        b"   Compiling tokio v1.37.0\n",
        b"warning: unused variable `x`\n",
        b"   Compiling frankenterm-core v0.1.0\n",
        b"error[E0308]: mismatched types\n",
        b"    Finished `dev` profile in 12.3s\n",
    ];
    let mut buf = Vec::with_capacity(size);
    let mut i = 0;
    while buf.len() < size {
        buf.extend_from_slice(lines[i % lines.len()]);
        i += 1;
    }
    buf.truncate(size);
    buf
}

fn ansi_heavy_payload(size: usize) -> Vec<u8> {
    let lines = [
        b"\x1b[32m   Compiling\x1b[0m serde v1.0\n".as_slice(),
        b"\x1b[33mwarning\x1b[0m: unused import\n",
        b"\x1b[31mERROR\x1b[0m: build failed\n",
        b"\x1b[36m   Downloading\x1b[0m crates ...\n",
        b"  \x1b[1;34mRunning\x1b[0m tests/integration.rs\n",
        b"test result: \x1b[32mok\x1b[0m. 42 passed; 0 failed\n",
    ];
    let mut buf = Vec::with_capacity(size);
    let mut i = 0;
    while buf.len() < size {
        buf.extend_from_slice(lines[i % lines.len()]);
        i += 1;
    }
    buf.truncate(size);
    buf
}

// =============================================================================
// Benchmark: full pipeline throughput across sizes
// =============================================================================

fn bench_pipeline_sizes(c: &mut Criterion) {
    let pipeline = ScanPipeline::default();
    let sizes: &[usize] = &[1024, 4096, 16_384, 65_536, 262_144, 1_048_576];

    let mut group = c.benchmark_group("scan_pipeline/full/sizes");
    for &size in sizes {
        let payload = cargo_build_payload(size);
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size}B")),
            &payload,
            |b, data| {
                b.iter(|| pipeline.process(black_box(data)));
            },
        );
    }
    group.finish();
}

// =============================================================================
// Benchmark: pipeline stages comparison (which stage dominates?)
// =============================================================================

fn bench_pipeline_stages(c: &mut Criterion) {
    let size = 65_536;
    let payload = cargo_build_payload(size);

    let mut group = c.benchmark_group("scan_pipeline/stages");
    group.throughput(Throughput::Bytes(payload.len() as u64));

    // All stages enabled
    let full = ScanPipeline::default();
    group.bench_function("all_stages", |b| {
        b.iter(|| full.process(black_box(&payload)));
    });

    // Metrics only (no triggers, no compression)
    let metrics_only = ScanPipeline::new(ScanPipelineConfig {
        enable_triggers: false,
        enable_compression: false,
        ..Default::default()
    });
    group.bench_function("metrics_only", |b| {
        b.iter(|| metrics_only.process(black_box(&payload)));
    });

    // Metrics + triggers (no compression)
    let no_compress = ScanPipeline::new(ScanPipelineConfig {
        enable_compression: false,
        ..Default::default()
    });
    group.bench_function("metrics_and_triggers", |b| {
        b.iter(|| no_compress.process(black_box(&payload)));
    });

    // Metrics + compression (no triggers)
    let no_triggers = ScanPipeline::new(ScanPipelineConfig {
        enable_triggers: false,
        ..Default::default()
    });
    group.bench_function("metrics_and_compression", |b| {
        b.iter(|| no_triggers.process(black_box(&payload)));
    });

    group.finish();
}

// =============================================================================
// Benchmark: payload type comparison
// =============================================================================

fn bench_pipeline_payload_types(c: &mut Criterion) {
    let pipeline = ScanPipeline::default();
    let size = 65_536;

    let payloads: Vec<(&str, Vec<u8>)> = vec![
        ("plain_logs", plain_log_payload(size)),
        ("cargo_build", cargo_build_payload(size)),
        ("ansi_heavy", ansi_heavy_payload(size)),
    ];

    let mut group = c.benchmark_group("scan_pipeline/payload_types");
    for (name, payload) in &payloads {
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(BenchmarkId::new("type", name), payload, |b, data| {
            b.iter(|| pipeline.process(black_box(data)));
        });
    }
    group.finish();
}

// =============================================================================
// Benchmark: chunked vs batch mode
// =============================================================================

fn bench_chunked_vs_batch(c: &mut Criterion) {
    let pipeline = ScanPipeline::new(ScanPipelineConfig {
        enable_compression: false,
        ..Default::default()
    });
    let payload = cargo_build_payload(262_144);

    let mut group = c.benchmark_group("scan_pipeline/mode");
    group.throughput(Throughput::Bytes(payload.len() as u64));

    // Batch
    group.bench_function("batch", |b| {
        b.iter(|| pipeline.process(black_box(&payload)));
    });

    // Chunked with 4KB chunks
    let chunks: Vec<&[u8]> = payload.chunks(4096).collect();
    group.bench_function("chunked_4KB", |b| {
        b.iter(|| {
            let mut state = ChunkedPipelineState::new(16_777_216);
            for chunk in &chunks {
                pipeline.process_chunk(black_box(chunk), &mut state);
            }
            pipeline.flush(&mut state)
        });
    });

    // Chunked with 64KB chunks
    let chunks_64k: Vec<&[u8]> = payload.chunks(65_536).collect();
    group.bench_function("chunked_64KB", |b| {
        b.iter(|| {
            let mut state = ChunkedPipelineState::new(16_777_216);
            for chunk in &chunks_64k {
                pipeline.process_chunk(black_box(chunk), &mut state);
            }
            pipeline.flush(&mut state)
        });
    });

    group.finish();
}

// =============================================================================
// Benchmark: compression level impact on pipeline throughput
// =============================================================================

fn bench_compression_levels(c: &mut Criterion) {
    use frankenterm_core::scan_pipeline::CompressionLevelConfig;

    let size = 262_144;
    let payload = cargo_build_payload(size);

    let levels = [
        ("fast", CompressionLevelConfig::Fast),
        ("default", CompressionLevelConfig::Default),
        ("high", CompressionLevelConfig::High),
        ("maximum", CompressionLevelConfig::Maximum),
    ];

    let mut group = c.benchmark_group("scan_pipeline/compression_levels");
    group.throughput(Throughput::Bytes(payload.len() as u64));

    for (name, level) in &levels {
        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            compression_level: *level,
            ..Default::default()
        });
        group.bench_with_input(BenchmarkId::new("level", name), &payload, |b, data| {
            b.iter(|| pipeline.process(black_box(data)));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_pipeline_sizes,
    bench_pipeline_stages,
    bench_pipeline_payload_types,
    bench_chunked_vs_batch,
    bench_compression_levels,
);
criterion_main!(benches);
