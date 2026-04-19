//! Criterion benchmarks for persistent immutable data structures.
//!
//! Bead: wa-283h4.7
//!
//! Performance budgets:
//! - Persistent insert vs mutable: **< 2x** std::HashMap wall time
//! - Persistent clone vs deep copy: **> 100x** faster for n > 1000
//! - Version access: **O(1)** regardless of version count
//! - Structural sharing memory: **< 2x** single version for 1000 versions
//! - Diff two versions: proportional to changes, not total size

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use std::collections::HashMap;
use std::hint::black_box;

use frankenterm_core::persistent_ds::{PersistentMap, PersistentVec, VersionedStore};

mod bench_common;

#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "persistent_insert_vs_mutable",
        budget: "< 2x std::HashMap wall time for 10K inserts",
    },
    bench_common::BenchBudget {
        name: "persistent_clone_vs_deep_copy",
        budget: "> 100x faster for n > 1000",
    },
    bench_common::BenchBudget {
        name: "version_access",
        budget: "O(1) regardless of K or N",
    },
    bench_common::BenchBudget {
        name: "structural_sharing_memory",
        budget: "< 2x single version size for 1000 versions",
    },
    bench_common::BenchBudget {
        name: "diff_two_versions",
        budget: "proportional to changes, not total size",
    },
];

// ── Data generators ────────────────────────────────────────────────────

/// Generate deterministic key-value pairs for map benchmarks.
fn generate_kv_pairs(n: usize) -> Vec<(String, i64)> {
    (0..n).map(|i| (format!("key_{:06}", i), i as i64)).collect()
}

/// Pre-build a PersistentMap with `n` entries.
fn build_persistent_map(n: usize) -> PersistentMap<String, i64> {
    let mut m = PersistentMap::new();
    for (k, v) in generate_kv_pairs(n) {
        m = m.insert(k, v);
    }
    m
}

/// Pre-build a std::HashMap with `n` entries.
fn build_std_hashmap(n: usize) -> HashMap<String, i64> {
    generate_kv_pairs(n).into_iter().collect()
}

// ── Benchmarks ─────────────────────────────────────────────────────────

/// Benchmark: insert N entries into PersistentMap vs std::HashMap.
/// Target: PersistentMap < 2x std::HashMap wall time.
fn bench_persistent_insert_vs_mutable(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_ds/insert_vs_mutable");

    for n in [100, 1000, 10_000] {
        let pairs = generate_kv_pairs(n);

        group.bench_with_input(
            BenchmarkId::new("persistent_map", n),
            &pairs,
            |b, pairs| {
                b.iter(|| {
                    let mut m = PersistentMap::new();
                    for (k, v) in pairs {
                        m = m.insert(k.clone(), *v);
                    }
                    black_box(m)
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("std_hashmap", n),
            &pairs,
            |b, pairs| {
                b.iter(|| {
                    let mut m = HashMap::new();
                    for (k, v) in pairs {
                        m.insert(k.clone(), *v);
                    }
                    black_box(m)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: clone persistent (O(1) Arc clone) vs deep copy of std::HashMap.
/// Target: > 100x faster for n > 1000.
fn bench_persistent_clone_vs_deep_copy(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_ds/clone_vs_deep_copy");

    for n in [100, 1000, 5000] {
        group.bench_with_input(
            BenchmarkId::new("persistent_clone", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || build_persistent_map(n),
                    |m| black_box(m.clone()),
                    BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("std_hashmap_clone", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || build_std_hashmap(n),
                    |m| black_box(m.clone()),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Benchmark: access version K out of N versions.
/// Target: O(1) regardless of K or N — it's a Vec index lookup.
fn bench_version_access(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_ds/version_access");

    for total_versions in [100, 1000, 10_000] {
        // Pre-build a VersionedStore with many versions.
        let mut store = VersionedStore::new(0i64, 0);
        for i in 1..total_versions {
            store.push(i as i64, i as u64 * 100);
        }

        // Access early version.
        group.bench_with_input(
            BenchmarkId::new("access_first", total_versions),
            &store,
            |b, store| {
                b.iter(|| black_box(store.at_version(0)));
            },
        );

        // Access middle version.
        group.bench_with_input(
            BenchmarkId::new("access_middle", total_versions),
            &store,
            |b, store| {
                b.iter(|| black_box(store.at_version(total_versions / 2)));
            },
        );

        // Access last version.
        group.bench_with_input(
            BenchmarkId::new("access_last", total_versions),
            &store,
            |b, store| {
                b.iter(|| black_box(store.at_version(total_versions - 1)));
            },
        );

        // Access by timestamp (binary search).
        group.bench_with_input(
            BenchmarkId::new("access_by_timestamp", total_versions),
            &store,
            |b, store| {
                let target_ts = (total_versions as u64 / 2) * 100;
                b.iter(|| black_box(store.at_timestamp(target_ts)));
            },
        );
    }

    group.finish();
}

/// Benchmark: create 1000 versions of a PersistentMap with 1 mutation each.
/// Measures the overhead of structural sharing.
/// Target: total memory < 2x single version size.
fn bench_structural_sharing_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_ds/structural_sharing");

    // Measure the cost of creating N versions with 1 mutation each.
    for (base_size, mutation_count) in [(100, 1000), (1000, 1000)] {
        group.bench_with_input(
            BenchmarkId::new(
                "create_versions",
                format!("base{}_mutations{}", base_size, mutation_count),
            ),
            &(base_size, mutation_count),
            |b, &(base_size, mutation_count)| {
                b.iter_batched(
                    || build_persistent_map(base_size),
                    |base_map| {
                        let mut versions = Vec::with_capacity(mutation_count);
                        let mut current = base_map;
                        for i in 0..mutation_count {
                            current = current.insert(format!("mutated_{}", i), i as i64);
                            versions.push(current.clone());
                        }
                        black_box(versions)
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // Measure single mutation cost on maps of different sizes.
    for n in [100, 1000, 5000] {
        group.bench_with_input(
            BenchmarkId::new("single_mutation", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || build_persistent_map(n),
                    |m| black_box(m.insert("new_key".to_string(), 42)),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Benchmark: diff between two PersistentMap versions.
/// Target: proportional to changes, not total size.
fn bench_diff_two_versions(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_ds/diff");

    // Diff with varying numbers of changes on a fixed-size base.
    let base_size = 1000;
    for change_count in [1, 10, 100, 500] {
        group.bench_with_input(
            BenchmarkId::new(
                "changes_on_1k_base",
                format!("{}_changes", change_count),
            ),
            &change_count,
            |b, &change_count| {
                b.iter_batched(
                    || {
                        let base = build_persistent_map(base_size);
                        let mut modified = base.clone();
                        for i in 0..change_count {
                            modified =
                                modified.insert(format!("key_{:06}", i), (i as i64) * 1000);
                        }
                        (base, modified)
                    },
                    |(base, modified)| black_box(base.diff(&modified)),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // Diff between identical maps (should be fast — no changes).
    group.bench_function("no_changes_1k", |b| {
        b.iter_batched(
            || {
                let m = build_persistent_map(1000);
                (m.clone(), m)
            },
            |(a, b_map)| black_box(a.diff(&b_map)),
            BatchSize::SmallInput,
        );
    });

    // Diff between completely different maps.
    group.bench_function("all_different_1k", |b| {
        b.iter_batched(
            || {
                let m1 = build_persistent_map(1000);
                let mut m2 = PersistentMap::new();
                for i in 0..1000 {
                    m2 = m2.insert(format!("other_{:06}", i), i as i64);
                }
                (m1, m2)
            },
            |(a, b_map)| black_box(a.diff(&b_map)),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: PersistentVec operations (push, get, set) for comparison.
fn bench_persistent_vec_ops(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_ds/vec_ops");

    // Push throughput.
    for n in [100, 1000, 10_000] {
        group.bench_with_input(BenchmarkId::new("push", n), &n, |b, &n| {
            b.iter(|| {
                let mut v = PersistentVec::new();
                for i in 0..n {
                    v = v.push(i as i64);
                }
                black_box(v)
            });
        });

        group.bench_with_input(BenchmarkId::new("std_vec_push", n), &n, |b, &n| {
            b.iter(|| {
                let mut v = Vec::new();
                for i in 0..n {
                    v.push(i as i64);
                }
                black_box(v)
            });
        });
    }

    // Random access on a pre-built vector.
    for n in [100, 1000, 10_000] {
        group.bench_with_input(BenchmarkId::new("get", n), &n, |b, &n| {
            b.iter_batched(
                || PersistentVec::from_iter(0..n as i64),
                |v| {
                    let mut sum = 0i64;
                    for i in 0..v.len() {
                        sum = sum.wrapping_add(*v.get(i).unwrap());
                    }
                    black_box(sum)
                },
                BatchSize::SmallInput,
            );
        });
    }

    // Set (copy-on-write) on a pre-built vector.
    for n in [100, 1000, 10_000] {
        group.bench_with_input(BenchmarkId::new("set_middle", n), &n, |b, &n| {
            b.iter_batched(
                || PersistentVec::from_iter(0..n as i64),
                |v| black_box(v.set(n / 2, 999)),
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// Benchmark: PersistentMap lookup throughput.
fn bench_persistent_map_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_ds/map_lookup");

    for n in [100, 1000, 5000] {
        group.bench_with_input(
            BenchmarkId::new("persistent_get", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || {
                        let m = build_persistent_map(n);
                        let keys: Vec<_> = (0..n).map(|i| format!("key_{:06}", i)).collect();
                        (m, keys)
                    },
                    |(m, keys)| {
                        let mut found = 0u64;
                        for k in &keys {
                            if m.get(k).is_some() {
                                found += 1;
                            }
                        }
                        black_box(found)
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("std_hashmap_get", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || {
                        let m = build_std_hashmap(n);
                        let keys: Vec<_> = (0..n).map(|i| format!("key_{:06}", i)).collect();
                        (m, keys)
                    },
                    |(m, keys)| {
                        let mut found = 0u64;
                        for k in &keys {
                            if m.get(k).is_some() {
                                found += 1;
                            }
                        }
                        black_box(found)
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Benchmark: VersionedStore eviction latency.
fn bench_versioned_eviction(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_ds/versioned_eviction");

    for version_count in [100, 1000, 5000] {
        group.bench_with_input(
            BenchmarkId::new("evict_half", version_count),
            &version_count,
            |b, &count| {
                b.iter_batched(
                    || {
                        let mut store = VersionedStore::new(0i64, 0);
                        for i in 1..count {
                            store.push(i as i64, i as u64 * 100);
                        }
                        store
                    },
                    |mut store| {
                        let mid_ts = (count as u64 / 2) * 100;
                        store.evict_before(mid_ts);
                        black_box(store)
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

// ── Criterion harness ──────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_persistent_insert_vs_mutable,
    bench_persistent_clone_vs_deep_copy,
    bench_version_access,
    bench_structural_sharing_overhead,
    bench_diff_two_versions,
    bench_persistent_vec_ops,
    bench_persistent_map_lookup,
    bench_versioned_eviction,
);

criterion_main!(benches);
