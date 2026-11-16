//! Cold-read latency of the durable scrollback store (ft-yccm0.2.1.4 AC2).
//!
//! A row sealed per segment authenticates only with its whole segment, so a
//! cold read, one with no segment open, decrypts the whole segment holding
//! its row: as the store's writer cuts them, up to 1024 rows or 128 KiB of
//! payload (or one larger row). The sink keeps the last segment it opened,
//! so a read of a row in that segment opens nothing. For each shape this
//! measures, on the largest segment (by payload), at its first row and at
//! its tail row (whose reader scans back across the whole segment):
//!
//! - `store_row_cold`: `load_scrollback_line` on a live durable store in a
//!   temporary directory, written as a pane's scrollback reaches it (row 0
//!   alone, then one full 4096-row commit window) and published, with no
//!   segment kept open (dropped untimed before each read). This is
//!   everything a cold read pays: the state checks, the ledger reads, the
//!   segment scan, the AEAD and the row decode. The OS page cache is warm;
//!   "cold" means nothing is open in the process.
//! - `store_row_kept`: the same read with the row's segment kept open.
//! - `store_viewport24_cold`: a cold 24-row `load_scrollback_lines` from the
//!   same row, as a scrolled viewport loads (the tail row's viewport reaches
//!   into the next segment, when there is one).
//! - `cipher_segment_row` and `cipher_compact_row`: the AEAD and the row
//!   decode alone, for the same row sealed in its segment, and alone as a
//!   compact (v4) row, as rows were stored before option A.
//!
//! And per shape, `store_sweep_kept` and `store_sweep_cold`: every row, in
//! ranged loads of at most 32 rows as Screen's cold index and search read
//! them, with the sink keeping the last segment open, and with every load
//! starting cold (as each did before the sink kept one). Throughput is rows.
//!
//! Shapes: `t0_corpus` (the realistic T0 rows; segments end at 128 KiB),
//! `ascii91` (segments end at 128 KiB) and `ascii9` (segments end at 1024
//! rows). Before timing, each shape prints its segments.

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_mux_server_impl::scrollback_record_bench::{
    ColdReadStore, FIRST_SEGMENT_ROW, Rows, Sealer,
};
use std::hint::black_box;

/// Row 0, then one full commit window.
const STORED_ROWS: usize = FIRST_SEGMENT_ROW as usize + 4096;
const VIEWPORT_ROWS: u64 = 24;

fn cold_read(c: &mut Criterion) {
    let sealer = Sealer::new();
    let mut group = c.benchmark_group("scrollback_cold_read");
    let shapes = [
        ("t0_corpus", Rows::t0_corpus(STORED_ROWS, 7)),
        ("ascii91", Rows::printable(STORED_ROWS, 91)),
        ("ascii9", Rows::printable(STORED_ROWS, 9)),
    ];
    for (shape, rows) in &shapes {
        let dir = tempfile::tempdir().expect("bench store directory");
        let store = ColdReadStore::open(dir.path(), rows);
        let window = sealer.seal_window(rows);
        let (largest, payload_bytes) = store
            .segments()
            .iter()
            .max_by_key(|(rows, payload_bytes)| (*payload_bytes, std::cmp::Reverse(rows.start)))
            .cloned()
            .expect("the bench store holds a segment");
        eprintln!(
            "scrollback_cold_read {shape}: {} segments over {STORED_ROWS} rows; largest rows \
             {largest:?} ({} rows, {payload_bytes} payload bytes)",
            store.segments().len(),
            largest.end - largest.start,
        );
        let viewport_rows = |row: u64| VIEWPORT_ROWS.min(STORED_ROWS as u64 - row);
        group.throughput(Throughput::Elements(1));
        for (probe, row) in [("first", largest.start), ("tail", largest.end - 1)] {
            let id = |name: &str| BenchmarkId::new(name, format!("{shape}_{probe}"));
            group.bench_function(id("store_row_cold"), |bench| {
                bench.iter_batched(
                    || store.forget_open_segment(),
                    |()| black_box(store.load(black_box(row))),
                    BatchSize::PerIteration,
                );
            });
            group.bench_function(id("store_row_kept"), |bench| {
                bench.iter(|| black_box(store.load(black_box(row))));
            });
            group.bench_function(id("store_viewport24_cold"), |bench| {
                bench.iter_batched(
                    || store.forget_open_segment(),
                    |()| black_box(store.load_viewport(black_box(row), viewport_rows(row))),
                    BatchSize::PerIteration,
                );
            });
            group.bench_function(id("cipher_segment_row"), |bench| {
                bench.iter(|| black_box(sealer.open_segment_row(&window, black_box(row))));
            });
            group.bench_function(id("cipher_compact_row"), |bench| {
                bench.iter(|| black_box(sealer.open_compact_row(&window, black_box(row))));
            });
        }
        group.throughput(Throughput::Elements(STORED_ROWS as u64));
        for (name, keep_open) in [("store_sweep_kept", true), ("store_sweep_cold", false)] {
            group.bench_function(BenchmarkId::new(name, shape), |bench| {
                bench.iter_batched(
                    || store.forget_open_segment(),
                    |()| black_box(store.sweep(keep_open)),
                    BatchSize::PerIteration,
                );
            });
        }
    }
    group.finish();
}

criterion_group!(benches, cold_read);
criterion_main!(benches);
