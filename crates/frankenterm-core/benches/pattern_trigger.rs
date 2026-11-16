//! Criterion benchmarks for pattern_trigger module (ft-2oph2).
//!
//! Measures throughput of Aho-Corasick pattern scanning across different
//! payload sizes, pattern counts, and scan modes (count vs. locate).

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::pattern_trigger::{TriggerCategory, TriggerPattern, TriggerScanner};
use std::hint::black_box;

// =============================================================================
// Payload generators
// =============================================================================

/// Plain log lines with occasional matches.
fn plain_log_payload(size: usize) -> Vec<u8> {
    let lines = [
        b"2025-01-15T08:30:00Z  INFO  server: request processed in 4ms\n".as_slice(),
        b"2025-01-15T08:30:01Z  DEBUG handler: path=/api/v2/users\n",
        b"2025-01-15T08:30:02Z  INFO  server: request processed in 2ms\n",
        b"2025-01-15T08:30:03Z  WARN  pool: connection reused after timeout\n",
        b"2025-01-15T08:30:04Z  INFO  server: request processed in 6ms\n",
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

/// Cargo build output with many progress and completion markers.
fn cargo_build_payload(size: usize) -> Vec<u8> {
    let lines = [
        b"   Compiling serde v1.0.200\n".as_slice(),
        b"   Compiling tokio v1.37.0\n",
        b"   Compiling rand v0.8.5\n",
        b"warning: unused variable `x`\n",
        b"   Compiling frankenterm-core v0.1.0\n",
        b"   Compiling regex v1.10.4\n",
        b"   Building [=====>      ] 50%\n",
        b"error[E0308]: mismatched types\n",
        b"   Compiling futures v0.3.30\n",
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

/// Dense error output (worst case for match density).
fn error_dense_payload(size: usize) -> Vec<u8> {
    let lines = [
        b"ERROR: connection refused\n".as_slice(),
        b"FATAL: database unreachable\n",
        b"error: aborting due to previous error\n",
        b"FAILED: test_integration_auth\n",
        b"panic at 'index out of bounds'\n",
        b"SIGSEGV: segmentation fault\n",
        b"Traceback (most recent call last)\n",
        b"error[E0425]: cannot find value\n",
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

/// ANSI-colored output with embedded trigger patterns.
fn ansi_colored_payload(size: usize) -> Vec<u8> {
    let lines = [
        b"\x1b[32m   Compiling\x1b[0m serde v1.0\n".as_slice(),
        b"\x1b[33mwarning\x1b[0m: unused import\n",
        b"\x1b[31mERROR\x1b[0m: build failed\n",
        b"\x1b[32m    Finished\x1b[0m `dev` profile in 3s\n",
        b"\x1b[36m   Downloading\x1b[0m crates ...\n",
        b"  \x1b[1mRunning\x1b[0m tests/integration.rs\n",
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

/// No-match payload (pure ASCII noise, no keywords).
fn no_match_payload(size: usize) -> Vec<u8> {
    let line = b"abcdefghijklmnopqrstuvwxyz 0123456789 _ - . : ; , /\n";
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        buf.extend_from_slice(line);
    }
    buf.truncate(size);
    buf
}

// =============================================================================
// Benchmark: scan_counts across payload sizes
// =============================================================================

fn bench_scan_counts_sizes(c: &mut Criterion) {
    let scanner = TriggerScanner::default();
    let sizes: &[usize] = &[1024, 4096, 16_384, 65_536, 262_144, 1_048_576];

    let mut group = c.benchmark_group("pattern_trigger/scan_counts/sizes");
    for &size in sizes {
        let payload = cargo_build_payload(size);
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size}B")),
            &payload,
            |b, data| {
                b.iter(|| scanner.scan_counts(black_box(data)));
            },
        );
    }
    group.finish();
}

// =============================================================================
// Benchmark: scan_counts across payload types
// =============================================================================

fn bench_scan_counts_types(c: &mut Criterion) {
    let scanner = TriggerScanner::default();
    let size = 65_536;

    let payloads: Vec<(&str, Vec<u8>)> = vec![
        ("plain_logs", plain_log_payload(size)),
        ("cargo_build", cargo_build_payload(size)),
        ("error_dense", error_dense_payload(size)),
        ("ansi_colored", ansi_colored_payload(size)),
        ("no_match", no_match_payload(size)),
    ];

    let mut group = c.benchmark_group("pattern_trigger/scan_counts/types");
    for (name, payload) in &payloads {
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(BenchmarkId::new("type", name), payload, |b, data| {
            b.iter(|| scanner.scan_counts(black_box(data)));
        });
    }
    group.finish();
}

// =============================================================================
// Benchmark: scan_locate vs scan_counts
// =============================================================================

fn bench_locate_vs_counts(c: &mut Criterion) {
    let scanner = TriggerScanner::default();
    let payload = cargo_build_payload(65_536);

    let mut group = c.benchmark_group("pattern_trigger/mode_comparison");
    group.throughput(Throughput::Bytes(payload.len() as u64));

    group.bench_function("scan_counts", |b| {
        b.iter(|| scanner.scan_counts(black_box(&payload)));
    });
    group.bench_function("scan_locate", |b| {
        b.iter(|| scanner.scan_locate(black_box(&payload)));
    });

    group.finish();
}

// =============================================================================
// Benchmark: varying pattern count
// =============================================================================

fn bench_pattern_count_scaling(c: &mut Criterion) {
    let payload = cargo_build_payload(65_536);

    let pattern_counts: &[usize] = &[1, 5, 10, 25, 50, 100];
    let base_patterns = [
        "ERROR",
        "FATAL",
        "FAILED",
        "panic",
        "segfault",
        "error[E",
        "error:",
        "SIGSEGV",
        "SIGABRT",
        "Traceback",
        "WARNING",
        "WARN",
        "warning:",
        "deprecated",
        "Finished",
        "Complete",
        "Done",
        "PASSED",
        "test result:",
        "Compiling",
        "Downloading",
        "Building",
        "Installing",
        "Resolving",
        "tests passed",
    ];

    let mut group = c.benchmark_group("pattern_trigger/pattern_count");
    group.throughput(Throughput::Bytes(payload.len() as u64));

    for &count in pattern_counts {
        let patterns: Vec<TriggerPattern> = (0..count)
            .map(|i| {
                let pat = base_patterns[i % base_patterns.len()];
                if i < base_patterns.len() {
                    TriggerPattern::new(pat, TriggerCategory::Custom)
                } else {
                    // Generate synthetic patterns for high counts
                    TriggerPattern::new(
                        &format!("{pat}_{}", i / base_patterns.len()),
                        TriggerCategory::Custom,
                    )
                }
            })
            .collect();
        let scanner = TriggerScanner::new(patterns);

        group.bench_with_input(BenchmarkId::from_parameter(count), &payload, |b, data| {
            b.iter(|| scanner.scan_counts(black_box(data)));
        });
    }
    group.finish();
}

// =============================================================================
// Benchmark: case-insensitive overhead
// =============================================================================

fn bench_case_insensitive_overhead(c: &mut Criterion) {
    let payload = cargo_build_payload(65_536);

    let exact_patterns: Vec<TriggerPattern> = vec![
        TriggerPattern::new("ERROR", TriggerCategory::Error),
        TriggerPattern::new("WARNING", TriggerCategory::Warning),
        TriggerPattern::new("Compiling", TriggerCategory::Progress),
        TriggerPattern::new("Finished", TriggerCategory::Completion),
    ];
    let nocase_patterns: Vec<TriggerPattern> = vec![
        TriggerPattern::case_insensitive("error", TriggerCategory::Error),
        TriggerPattern::case_insensitive("warning", TriggerCategory::Warning),
        TriggerPattern::case_insensitive("compiling", TriggerCategory::Progress),
        TriggerPattern::case_insensitive("finished", TriggerCategory::Completion),
    ];

    let exact_scanner = TriggerScanner::new(exact_patterns);
    let nocase_scanner = TriggerScanner::new(nocase_patterns);

    let mut group = c.benchmark_group("pattern_trigger/case_sensitivity");
    group.throughput(Throughput::Bytes(payload.len() as u64));

    group.bench_function("case_sensitive", |b| {
        b.iter(|| exact_scanner.scan_counts(black_box(&payload)));
    });
    group.bench_function("case_insensitive", |b| {
        b.iter(|| nocase_scanner.scan_counts(black_box(&payload)));
    });

    group.finish();
}

// =============================================================================
// Benchmark: empty scanner (baseline overhead)
// =============================================================================

fn bench_empty_scanner(c: &mut Criterion) {
    let scanner = TriggerScanner::new(Vec::new());
    let sizes: &[usize] = &[1024, 65_536, 1_048_576];

    let mut group = c.benchmark_group("pattern_trigger/empty_scanner");
    for &size in sizes {
        let payload = no_match_payload(size);
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size}B")),
            &payload,
            |b, data| {
                b.iter(|| scanner.scan_counts(black_box(data)));
            },
        );
    }
    group.finish();
}

// =============================================================================
// Benchmark: scanner construction time
// =============================================================================

fn bench_scanner_construction(c: &mut Criterion) {
    use frankenterm_core::pattern_trigger::all_default_patterns;

    let mut group = c.benchmark_group("pattern_trigger/construction");

    group.bench_function("default_patterns", |b| {
        b.iter(|| {
            let patterns = all_default_patterns();
            let _scanner = TriggerScanner::new(black_box(patterns));
        });
    });

    group.bench_function("100_patterns", |b| {
        b.iter(|| {
            let patterns: Vec<TriggerPattern> = (0..100)
                .map(|i| TriggerPattern::new(&format!("PATTERN_{i:03}"), TriggerCategory::Custom))
                .collect();
            let _scanner = TriggerScanner::new(black_box(patterns));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_scan_counts_sizes,
    bench_scan_counts_types,
    bench_locate_vs_counts,
    bench_pattern_count_scaling,
    bench_case_insensitive_overhead,
    bench_empty_scanner,
    bench_scanner_construction,
);
criterion_main!(benches);
