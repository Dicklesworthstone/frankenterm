//! Criterion benchmarks for WAL engine throughput.
//!
//! Bead: ft-283h4.2
//!
//! Targets:
//! - >100K mutations/sec for in-memory WAL
//! - Checkpoint cost <1μs
//! - Disk WAL append with/without fsync

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use frankenterm_core::wal_engine::{
    DiskWal, DiskWalConfig, MuxMutation, WalConfig, WalEngine, replay_mutations,
};

// ── In-Memory WAL Benchmarks ─────────────────────────────────────

fn bench_append_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_append");
    group.sample_size(50);

    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::new("in_memory", count), &count, |b, &count| {
            b.iter(|| {
                let mut wal = WalEngine::new(WalConfig {
                    compaction_threshold: count + 1,
                    max_retained_entries: count,
                });
                for i in 0..count {
                    wal.append(
                        MuxMutation::PaneOutput {
                            pane_id: (i % 50) as u64,
                            data: vec![0x41; 64],
                        },
                        i as u64,
                    );
                }
            });
        });
    }

    group.finish();
}

fn bench_checkpoint_cost(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_checkpoint");
    group.sample_size(100);

    group.bench_function("checkpoint_cost", |b| {
        let mut wal = WalEngine::new(WalConfig::default());
        // Pre-fill with some entries
        for i in 0..100 {
            wal.append(
                MuxMutation::PaneOutput {
                    pane_id: i,
                    data: vec![0x41; 32],
                },
                i,
            );
        }
        let mut ts = 1000u64;
        b.iter(|| {
            ts += 1;
            wal.checkpoint(ts)
        });
    });

    group.finish();
}

fn bench_replay(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_replay");
    group.sample_size(30);

    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::new("replay", count), &count, |b, &count| {
            let mut wal = WalEngine::new(WalConfig {
                compaction_threshold: count + 1,
                max_retained_entries: count,
            });
            for i in 0..count {
                wal.append(
                    MuxMutation::FocusChanged {
                        pane_id: (i % 50) as u64,
                    },
                    i as u64,
                );
                if i % 100 == 0 {
                    wal.checkpoint(i as u64);
                }
            }
            b.iter(|| {
                let replayed = replay_mutations(&wal, 0, u64::MAX);
                std::hint::black_box(replayed.len())
            });
        });
    }

    group.finish();
}

fn bench_compact(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_compact");
    group.sample_size(30);

    for &count in &[100, 1_000, 5_000] {
        group.bench_with_input(BenchmarkId::new("compact", count), &count, |b, &count| {
            b.iter_batched(
                || {
                    let mut wal = WalEngine::new(WalConfig {
                        compaction_threshold: 10,
                        max_retained_entries: count / 2,
                    });
                    for i in 0..count {
                        wal.append(
                            MuxMutation::PaneOutput {
                                pane_id: (i % 50) as u64,
                                data: vec![0x41; 32],
                            },
                            i as u64,
                        );
                    }
                    wal.checkpoint(count as u64);
                    wal.append(MuxMutation::FocusChanged { pane_id: 1 }, count as u64 + 1);
                    wal
                },
                |mut wal| {
                    wal.compact();
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

// ── Disk WAL Benchmarks ──────────────────────────────────────────

fn bench_disk_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_disk_append");
    group.sample_size(20);

    for &fsync in &[false, true] {
        let label = if fsync { "fsync" } else { "no_fsync" };
        group.bench_function(BenchmarkId::new("append_1000", label), |b| {
            b.iter_batched(
                || {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("bench.wal");
                    let config = DiskWalConfig {
                        wal_config: WalConfig {
                            compaction_threshold: 100_000,
                            max_retained_entries: 50_000,
                        },
                        fsync_on_write: fsync,
                        max_file_size: 100 * 1024 * 1024,
                    };
                    let (wal, _) = DiskWal::<MuxMutation>::open(&path, config).unwrap();
                    (wal, dir) // keep dir alive
                },
                |(mut wal, _dir)| {
                    for i in 0..1_000u64 {
                        wal.append(
                            MuxMutation::PaneOutput {
                                pane_id: i % 50,
                                data: vec![0x41; 64],
                            },
                            i,
                        )
                        .unwrap();
                    }
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

fn bench_disk_reload(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_disk_reload");
    group.sample_size(20);

    for &count in &[100, 1_000, 5_000] {
        group.bench_with_input(BenchmarkId::new("reload", count), &count, |b, &count| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("bench.wal");
            let config = DiskWalConfig::default();

            // Pre-write entries
            {
                let (mut wal, _) = DiskWal::<MuxMutation>::open(&path, config.clone()).unwrap();
                for i in 0..count {
                    wal.append(
                        MuxMutation::PaneOutput {
                            pane_id: (i % 50) as u64,
                            data: vec![0x41; 64],
                        },
                        i as u64,
                    )
                    .unwrap();
                }
            }

            b.iter(|| {
                let (wal, result) = DiskWal::<MuxMutation>::open(&path, config.clone()).unwrap();
                std::hint::black_box((wal.engine().len(), result.entries_loaded))
            });
        });
    }

    group.finish();
}

fn bench_disk_compact_rewrite(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_disk_compact");
    group.sample_size(10);

    group.bench_function("compact_rewrite_1000", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("bench.wal");
                let config = DiskWalConfig {
                    wal_config: WalConfig {
                        compaction_threshold: 100,
                        max_retained_entries: 200,
                    },
                    fsync_on_write: false,
                    max_file_size: 100 * 1024 * 1024,
                };

                let (mut wal, _) = DiskWal::<MuxMutation>::open(&path, config).unwrap();
                for i in 0..1_000u64 {
                    wal.append(
                        MuxMutation::PaneOutput {
                            pane_id: i % 50,
                            data: vec![0x41; 64],
                        },
                        i,
                    )
                    .unwrap();
                }
                wal.checkpoint(1000).unwrap();
                wal.append(MuxMutation::FocusChanged { pane_id: 1 }, 1001)
                    .unwrap();
                (wal, dir)
            },
            |(mut wal, _dir)| {
                wal.compact_and_rewrite().unwrap();
            },
            criterion::BatchSize::PerIteration,
        );
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_append_throughput,
    bench_checkpoint_cost,
    bench_replay,
    bench_compact,
    bench_disk_append,
    bench_disk_reload,
    bench_disk_compact_rewrite,
);
criterion_main!(benches);
