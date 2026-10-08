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
//!
//! ft-yccm0.2.1.4 AC1: the `_clustered` shapes are the same rows in the
//! clustered storage Screen hands the store. For them it compares, in one
//! run, the cell-vector schemas every row sealed before (a payload vector and
//! a record string allocated per row: `seal_compact_cell_schemas`) with
//! schema 3 through the reusable encode arena (`seal_compact_arena`).
//!
//! `emoji_colors80` is synthetic: two alternating emoji with arithmetic
//! colors, whose repetition favors the cell vector after zstd. `t0_corpus`
//! is the realistic T0 shape (the M.1 generator's random emoji and colors
//! through a terminal, as Screen hands the rows over); judge T0 by it.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_mux_server_impl::scrollback_record_bench::{Rows, Sealer, WriterStore};
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
    let clustered = [
        (
            "ascii9_clustered",
            Rows::printable(WINDOW_ROWS, 9).compressed_for_scrollback(),
        ),
        (
            "ascii91_clustered",
            Rows::printable(WINDOW_ROWS, 91).compressed_for_scrollback(),
        ),
        (
            "emoji_colors80_clustered",
            Rows::emoji_colors(WINDOW_ROWS, 80).compressed_for_scrollback(),
        ),
        // The realistic T0 rows: the M.1 generator through a terminal, as
        // Screen hands them over (some stay in vector storage).
        ("t0_corpus", Rows::t0_corpus(WINDOW_ROWS, 7)),
    ];
    let mut stream = sealer.compact_stream();
    for (shape, rows) in &clustered {
        let clustered_rows = rows
            .lines()
            .iter()
            .filter(|line| line.has_clustered_storage())
            .count();
        eprintln!("scrollback_record {shape}: {clustered_rows} of {WINDOW_ROWS} rows clustered");
        let per_row = |bytes: usize| bytes as f64 / WINDOW_ROWS as f64;
        let (cell_plaintext, cell_payload) = sealer.serialize_and_compress_cell_schemas(rows);
        let cell_records = sealer.seal_compact_cell_schemas(rows, &mut Vec::new());
        let (plaintext, payload) = sealer.serialize_and_compress(rows);
        let records = sealer.seal_compact_arena(rows, &mut stream);
        let segment_records = sealer.seal_segments_arena(rows, &mut stream);
        eprintln!(
            "scrollback_record {shape}: per row, cell schemas {:.1} plaintext, {:.1} payload, \
             {:.1} compact record bytes; clustered schema 3 {:.1} plaintext, {:.1} payload, \
             {:.1} compact record bytes ({:.1}x the row text); per segment {:.1} record bytes \
             ({:.1}x)",
            per_row(cell_plaintext),
            per_row(cell_payload),
            per_row(cell_records),
            per_row(plaintext),
            per_row(payload),
            per_row(records),
            records as f64 / rows.text_bytes() as f64,
            per_row(segment_records),
            segment_records as f64 / rows.text_bytes() as f64,
        );
        group.throughput(Throughput::Bytes(rows.text_bytes()));
        let mut scratch = Vec::new();
        group.bench_with_input(
            BenchmarkId::new("seal_compact_cell_schemas", shape),
            rows,
            |bench, rows| {
                bench.iter(|| {
                    black_box(sealer.seal_compact_cell_schemas(black_box(rows), &mut scratch))
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("seal_compact_arena", shape),
            rows,
            |bench, rows| {
                bench.iter(|| black_box(sealer.seal_compact_arena(black_box(rows), &mut stream)));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("seal_segments_arena", shape),
            rows,
            |bench, rows| {
                bench.iter(|| black_box(sealer.seal_segments_arena(black_box(rows), &mut stream)));
            },
        );
    }
    group.finish();
}

/// ft-y0gy9: the durability writer's two largest costs on T0 rows, each
/// with its alternatives in one run. Serialization: varbincode (through
/// `&mut dyn Write`) against `exact_varbincode` (the same bytes). Per-row
/// compression at zstd level 1 (the store's) and at negative levels, which
/// leave literals uncompressed and decode with the same decoder. Whole
/// segments as one frame are measured for the owner's format decision only.
/// Before timing, each prints its bytes per row.
fn writer_stages(c: &mut Criterion) {
    let sealer = Sealer::new();
    let rows = Rows::t0_corpus(WINDOW_ROWS, 7);
    let plaintexts = sealer.plaintexts(&rows);
    let plaintext_bytes: usize = plaintexts.iter().map(Vec::len).sum();
    let per_row = |bytes: usize| bytes as f64 / WINDOW_ROWS as f64;
    let mut group = c.benchmark_group("scrollback_writer");
    group.throughput(Throughput::Bytes(rows.text_bytes()));
    let mut out = Vec::new();
    for (name, direct) in [("serialize_varbincode", false), ("serialize_direct", true)] {
        let bytes = sealer.serialize_through(&rows, direct, &mut out);
        eprintln!(
            "scrollback_writer t0_corpus {name}: {:.1} plaintext bytes per row",
            per_row(bytes)
        );
        group.bench_function(BenchmarkId::new(name, "t0_corpus"), |bench| {
            bench.iter(|| black_box(sealer.serialize_through(black_box(&rows), direct, &mut out)));
        });
    }
    let clustered = rows
        .lines()
        .iter()
        .filter(|line| line.has_clustered_storage())
        .count();
    eprintln!("scrollback_writer t0_corpus charge_clustered: {clustered} of {WINDOW_ROWS} rows");
    group.bench_function(BenchmarkId::new("charge_clustered", "t0_corpus"), |bench| {
        bench.iter(|| black_box(sealer.charge_clustered(black_box(&rows))));
    });
    for level in [1, -1, -3, -7] {
        let bytes = sealer.compress_rows_at(&plaintexts, level);
        eprintln!(
            "scrollback_writer t0_corpus compress_rows level {level}: {:.1} payload bytes per row \
             ({:.3} of {:.1} plaintext)",
            per_row(bytes),
            bytes as f64 / plaintext_bytes as f64,
            per_row(plaintext_bytes),
        );
        group.bench_with_input(
            BenchmarkId::new("compress_rows", format!("t0_corpus_level{level}")),
            &level,
            |bench, &level| {
                bench.iter(|| black_box(sealer.compress_rows_at(black_box(&plaintexts), level)));
            },
        );
    }
    for level in [1, -1] {
        let bytes = sealer.compress_segments_at(&plaintexts, 1024, level);
        eprintln!(
            "scrollback_writer t0_corpus compress_segments level {level}: {:.1} bytes per row \
             ({:.3} of plaintext)",
            per_row(bytes),
            bytes as f64 / plaintext_bytes as f64,
        );
        group.bench_with_input(
            BenchmarkId::new("compress_segments", format!("t0_corpus_level{level}")),
            &level,
            |bench, &level| {
                bench.iter(|| {
                    black_box(sealer.compress_segments_at(black_box(&plaintexts), 1024, level))
                });
            },
        );
    }
    group.finish();
}

/// ft-y0gy9: the durability writer's whole CPU per T0 row: 1024-row windows
/// into a live store under a 2048-row retention (every window evicts), the
/// tail published every second window. It includes encoding, sealing, the
/// ledger chain, eviction reads, appends and publications, but not the
/// deferred queue. Run as is, and with the switches
/// (FT_SCROLLBACK_DIRECT_SERIALIZE=0, FT_SCROLLBACK_ROW_ZSTD_LEVEL=1,
/// FT_STORE_BLOCK_READS=0) to see each change's share.
fn writer_store(c: &mut Criterion) {
    const WINDOW: usize = 1024;
    let rows = Rows::t0_corpus(WINDOW, 7);
    let dir = tempfile::tempdir().expect("bench store directory");
    let mut store = WriterStore::open(dir.path(), &rows, 2 * WINDOW, 2);
    // Fill the retention, so every measured window evicts.
    for _ in 0..3 {
        store.store_window(&rows);
    }
    let mut group = c.benchmark_group("scrollback_writer");
    group.throughput(Throughput::Elements(WINDOW as u64));
    group.bench_function(BenchmarkId::new("store_window", "t0_corpus"), |bench| {
        bench.iter(|| black_box(store.store_window(black_box(&rows))));
    });
    group.finish();
}

criterion_group!(benches, record_path, writer_stages, writer_store);
criterion_main!(benches);
