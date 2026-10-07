//! Writer CPU of the durable scrollback record path (ft-yccm0.2.1.4).
//!
//! Each benchmark runs one 4096-row commit window, the largest batch the store
//! accepts, through the per-row stages the writer thread runs: the exact
//! semantic plaintext alone, plaintext plus compression, the full v3 record
//! (serialize, compress, seal, encode) as one String per row, the same
//! records sealed in place into one reused batch string, and the compact v4
//! record batches now store (same payload, nonce from the segment stream,
//! no per-row header). Rows are printable
//! with one attribute run, at the two line lengths the amplification targets
//! name (9 and 91 bytes), and T0-like rows of wide emoji with per-cell palette
//! colors. Throughput is the rows' UTF-8 text bytes, so "MiB/s" is MiB of
//! visible pane text the writer can make durable per second of one core.
//! Before timing, each shape prints its per-row plaintext, sealed payload and
//! record bytes.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_mux_server_impl::scrollback_record_bench::{Rows, Sealer};
use std::hint::black_box;

const WINDOW_ROWS: usize = 4096;

fn record_path(c: &mut Criterion) {
    let sealer = Sealer::new();
    let mut group = c.benchmark_group("scrollback_record");
    let shapes = [
        ("ascii9", Rows::printable(WINDOW_ROWS, 9)),
        ("ascii91", Rows::printable(WINDOW_ROWS, 91)),
        ("emoji_colors80", Rows::emoji_colors(WINDOW_ROWS, 80)),
    ];
    for (shape, rows) in &shapes {
        let record_bytes = sealer.seal(rows);
        let compact_bytes = sealer.seal_compact(rows, &mut Vec::new());
        let (plaintext_bytes, payload_bytes) = sealer.serialize_and_compress(rows);
        let per_row = |bytes: usize| bytes as f64 / WINDOW_ROWS as f64;
        eprintln!(
            "scrollback_record {shape}: per row {:.1} plaintext, {:.1} sealed payload, \
             {:.1} v3 record bytes ({:.1}x the row text), {:.1} compact record bytes \
             ({:.1}x)",
            per_row(plaintext_bytes),
            per_row(payload_bytes),
            per_row(record_bytes),
            record_bytes as f64 / rows.text_bytes() as f64,
            per_row(compact_bytes),
            compact_bytes as f64 / rows.text_bytes() as f64,
        );
        group.throughput(Throughput::Bytes(rows.text_bytes()));
        group.bench_with_input(BenchmarkId::new("serialize", shape), rows, |bench, rows| {
            bench.iter(|| black_box(sealer.serialize(black_box(rows))));
        });
        group.bench_with_input(
            BenchmarkId::new("serialize_compress", shape),
            rows,
            |bench, rows| {
                bench.iter(|| black_box(sealer.serialize_and_compress(black_box(rows))));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("seal_records", shape),
            rows,
            |bench, rows| {
                bench.iter(|| black_box(sealer.seal(black_box(rows))));
            },
        );
        let (mut scratch, mut records) = (Vec::new(), String::new());
        group.bench_with_input(
            BenchmarkId::new("seal_into_batch", shape),
            rows,
            |bench, rows| {
                bench.iter(|| {
                    black_box(sealer.seal_into(black_box(rows), &mut scratch, &mut records))
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("seal_compact_records", shape),
            rows,
            |bench, rows| {
                bench.iter(|| black_box(sealer.seal_compact(black_box(rows), &mut scratch)));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, record_path);
criterion_main!(benches);
