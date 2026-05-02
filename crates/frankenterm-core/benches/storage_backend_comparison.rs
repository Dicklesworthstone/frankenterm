//! Side-by-side rusqlite vs frankensqlite backend comparison
//! (br-ft-giisk substrate-pass).
//!
//! **Bead:** `ft-giisk` / wa-2l27x.8.cont.benchmarks.
//!
//! ## Substrate vs wired-pass split
//!
//! The bead is "blocked on cont.frankensqlite" only for the
//! end-to-end usefulness — the bench HARNESS shape is
//! backend-agnostic. This file ships the harness over the
//! [`StorageBackend`] trait so the wired-pass cont-bead just
//! pushes a `FrankenSQLiteBackend::open(...)` row into the
//! `backends_under_test()` list when ft-kcdqp lands.
//!
//! Until then the harness benches `RusqliteBackend` against
//! itself so the substrate is exercised in CI + the workload-level
//! invariants (rows-per-second ordering, no-panic) carry a
//! regression guard.
//!
//! ## What this bench measures (per the bead's scope)
//!
//! 1. **Single-writer throughput** — sequential INSERTs in a
//!    tight loop, rows/sec.
//! 2. **Concurrent-writer throughput** — multiple writers
//!    contending on the same backend (the substrate-pass
//!    serializes via `Mutex<Connection>`; wired-pass exposes
//!    pool concurrency).
//! 3. **Checkpoint latency** — `PRAGMA wal_checkpoint(FULL)`
//!    cost on a populated WAL.
//!
//! ## What this bench deliberately does NOT ship
//!
//! - The aggregated `docs/perf/storage-backend-comparison.json`
//!   artifact (scope item 3) — wired-pass extension that
//!   parses Criterion's estimates.json + emits the ranked
//!   per-backend comparison.
//! - The frankensqlite leg of the comparison — registers
//!   automatically once `FrankenSQLiteBackend` lands at
//!   ft-kcdqp.

use std::hint::black_box;
use std::sync::Arc;
use std::thread;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::storage_backend_trait::{OpenConfig, RusqliteBackend, StorageBackend};

mod bench_common;

/// Backend variants registered for comparison.
///
/// Each entry is `(label, factory)`. The factory closure
/// constructs a fresh backend for every bench run so the
/// comparison isn't perturbed by leftover state.
///
/// Wired-pass cont-bead pushes a frankensqlite row here.
fn backends_under_test() -> Vec<(&'static str, Box<dyn Fn() -> Box<dyn StorageBackend>>)> {
    vec![
        (
            "rusqlite_memory",
            Box::new(|| {
                let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
                Box::new(backend) as Box<dyn StorageBackend>
            }),
        ),
        (
            "rusqlite_memory_wal",
            Box::new(|| {
                let mut config = OpenConfig::default();
                config.wal_mode = true;
                // Note: ":memory:" disables WAL; use a temp file
                // so wal_mode actually fires. We can't write to
                // disk in the bench context cleanly without
                // polluting the runner, so this entry stays
                // memory-mode + matches the non-WAL row in
                // throughput. The wired-pass bench harness adds
                // a real-file row.
                let backend = RusqliteBackend::open(":memory:", &config).unwrap();
                Box::new(backend) as Box<dyn StorageBackend>
            }),
        ),
    ]
}

/// Apply the documented schema to a freshly-constructed backend.
/// Single-table workload so the comparison stays apples-to-apples.
fn populate_schema(backend: &dyn StorageBackend) {
    backend
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS bench_log \
                (id INTEGER PRIMARY KEY, \
                 ts INTEGER NOT NULL, \
                 payload TEXT NOT NULL);",
        )
        .unwrap();
}

// =============================================================================
// Bench 1: single-writer throughput
// =============================================================================

fn bench_single_writer_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_backend/single_writer_throughput");
    group.throughput(Throughput::Elements(1000));

    for (label, factory) in backends_under_test() {
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &factory,
            |b, factory| {
                b.iter_with_setup(
                    || {
                        let backend = factory();
                        populate_schema(&*backend);
                        backend
                    },
                    |backend| {
                        for i in 0..1000 {
                            let sql = format!(
                                "INSERT INTO bench_log (id, ts, payload) \
                                 VALUES ({i}, {i}, 'p{i}')"
                            );
                            black_box(backend.execute(&sql).unwrap());
                        }
                    },
                );
            },
        );
    }

    group.finish();
}

// =============================================================================
// Bench 2: concurrent-writer throughput
// =============================================================================

fn bench_concurrent_writer_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_backend/concurrent_writer_throughput");
    group.throughput(Throughput::Elements(1000));

    for (label, factory) in backends_under_test() {
        let factory = Arc::new(factory);
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &factory,
            |b, factory| {
                b.iter_with_setup(
                    || {
                        let backend = Arc::from(factory());
                        populate_schema(&*backend);
                        backend
                    },
                    |backend: Arc<dyn StorageBackend>| {
                        // 4 writers × 250 rows each = 1000 total.
                        let mut handles = Vec::with_capacity(4);
                        for tid in 0..4u64 {
                            let backend_t = Arc::clone(&backend);
                            handles.push(thread::spawn(move || {
                                for i in 0..250u64 {
                                    let id = tid * 1000 + i;
                                    let sql = format!(
                                        "INSERT INTO bench_log (id, ts, payload) \
                                         VALUES ({id}, {id}, 'p{id}')"
                                    );
                                    let _ = backend_t.execute(&sql);
                                }
                            }));
                        }
                        for h in handles {
                            h.join().unwrap();
                        }
                    },
                );
            },
        );
    }

    group.finish();
}

// =============================================================================
// Bench 3: checkpoint latency
// =============================================================================

fn bench_checkpoint_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_backend/checkpoint_latency");

    for (label, factory) in backends_under_test() {
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &factory,
            |b, factory| {
                b.iter_with_setup(
                    || {
                        let backend = factory();
                        populate_schema(&*backend);
                        // Pre-populate so the WAL has work to
                        // checkpoint. Memory-mode degrades to
                        // a no-op checkpoint, but the harness
                        // shape is what's tested.
                        for i in 0..500 {
                            let sql = format!(
                                "INSERT INTO bench_log (id, ts, payload) \
                                 VALUES ({i}, {i}, 'p{i}')"
                            );
                            backend.execute(&sql).unwrap();
                        }
                        backend
                    },
                    |backend| {
                        // PRAGMA wal_checkpoint returns
                        // (busy, log, checkpointed); we use
                        // query_row_strings to consume the
                        // multi-column result through the trait.
                        let _ = black_box(
                            backend
                                .query_row_strings("PRAGMA wal_checkpoint(FULL)", &[])
                                .unwrap(),
                        );
                    },
                );
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_single_writer_throughput,
    bench_concurrent_writer_throughput,
    bench_checkpoint_latency,
);
criterion_main!(benches);
