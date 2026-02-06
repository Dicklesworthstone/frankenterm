//! Storage + FTS p95 regression guard benchmarks.
//!
//! These benchmarks isolate **write-path latency** so CI can catch regressions.
//!
//! Performance budgets:
//! - **append_segment p95 < 2ms** (single insert, DB up to 100K segments)
//! - **batch append throughput > 500 segments/sec** (sustained, 10K batch)
//! - **FTS search p95 < 15ms** (common query, DB ~100K segments)
//! - **upsert_pane p95 < 1ms** (metadata write)

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use wa_core::storage::{PaneRecord, SearchOptions, StorageHandle};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "append_segment_p95",
        budget: "p95 < 2ms (single insert, DB up to 100K segments)",
    },
    bench_common::BenchBudget {
        name: "batch_append_throughput",
        budget: "> 500 segments/sec (sustained, 10K batch)",
    },
    bench_common::BenchBudget {
        name: "fts_search_p95",
        budget: "p95 < 15ms (common query, DB ~100K segments)",
    },
    bench_common::BenchBudget {
        name: "upsert_pane_p95",
        budget: "p95 < 1ms (metadata write)",
    },
];

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir
        .path()
        .join("regression.db")
        .to_string_lossy()
        .to_string();
    (dir, path)
}

fn test_pane(pane_id: u64) -> PaneRecord {
    let now = now_ms();
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: Some(1),
        tab_id: Some(1),
        title: Some(format!("bench-pane-{pane_id}")),
        cwd: Some("/tmp/bench".to_string()),
        tty_name: None,
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

/// Generate varied terminal content (avg ~200 bytes).
fn gen_content(i: usize) -> String {
    match i % 6 {
        0 => format!(
            "$ cargo build\n   Compiling crate-{i} v0.1.0\n    Finished dev in 2.{0}s\n",
            i % 10
        ),
        1 => format!(
            "error[E0308]: mismatched types\n --> src/main.rs:{i}:5\n  expected `i32`, found `String`\n"
        ),
        2 => format!("test test_{i} ... ok\ntest result: ok. 1 passed; 0 failed\n"),
        3 => format!(
            "$ git diff --stat\n src/lib.rs | {0} ++--\n 1 file changed, {0} insertions\n",
            10 + i % 50
        ),
        4 => format!(
            "Processing batch {i}... done ({0}ms)\nItems: {1}\n",
            100 + (i * 7) % 900,
            i * 100
        ),
        _ => format!(
            "$ ls -la\n-rw-r--r-- 1 user staff {0} file_{i}.txt\ndrwxr-xr-x 5 user staff 160 src\n",
            1000 + i * 10
        ),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

/// Pre-populate a DB with `n` segments on pane 1.
async fn seed_db(storage: &StorageHandle, n: usize) {
    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");
    for i in 0..n {
        storage
            .append_segment(1, &gen_content(i), None)
            .await
            .expect("append segment");
    }
}

// ---------------------------------------------------------------------------
// Group 1: Single append_segment latency at varying DB sizes
// ---------------------------------------------------------------------------

fn bench_append_single(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("storage_append_single");
    group.sample_size(100);

    // Budget: p95 < 2ms per insert
    for (db_size, label) in [(1_000, "1K"), (10_000, "10K"), (100_000, "100K")] {
        let (_dir, db_path) = temp_db();
        let storage = rt.block_on(async {
            let s = StorageHandle::new(&db_path).await.expect("create storage");
            seed_db(&s, db_size).await;
            s
        });

        let counter = std::sync::atomic::AtomicUsize::new(db_size);
        group.bench_function(BenchmarkId::new("latency", label), |b| {
            b.to_async(&rt).iter(|| {
                let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let content = gen_content(idx);
                let s = &storage;
                async move { s.append_segment(1, &content, None).await }
            });
        });

        rt.block_on(storage.shutdown()).expect("shutdown");
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 2: Batch append throughput
// ---------------------------------------------------------------------------

fn bench_append_batch(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("storage_append_batch");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));

    // Budget: > 500 segments/sec sustained
    let batch_size: u64 = 1_000;
    group.throughput(Throughput::Elements(batch_size));

    group.bench_function("1K_batch_on_empty", |b| {
        b.to_async(&rt).iter(|| async {
            let (_dir, db_path) = temp_db();
            let storage = StorageHandle::new(&db_path).await.expect("create storage");
            storage
                .upsert_pane(test_pane(1))
                .await
                .expect("upsert pane");
            for i in 0..batch_size as usize {
                storage
                    .append_segment(1, &gen_content(i), None)
                    .await
                    .expect("append");
            }
            storage.shutdown().await.expect("shutdown");
        });
    });

    // Batch on pre-populated DB (10K existing)
    group.bench_function("1K_batch_on_10K", |b| {
        let (_dir, db_path) = temp_db();
        let storage = rt.block_on(async {
            let s = StorageHandle::new(&db_path).await.expect("create storage");
            seed_db(&s, 10_000).await;
            s
        });

        let counter = std::sync::atomic::AtomicUsize::new(10_000);
        b.to_async(&rt).iter(|| {
            let base = counter.fetch_add(batch_size as usize, std::sync::atomic::Ordering::Relaxed);
            let s = &storage;
            async move {
                for i in 0..batch_size as usize {
                    s.append_segment(1, &gen_content(base + i), None)
                        .await
                        .expect("append");
                }
            }
        });

        rt.block_on(storage.shutdown()).expect("shutdown");
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 3: FTS search p95 regression guard at scale
// ---------------------------------------------------------------------------

fn bench_fts_regression(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("storage_fts_p95");
    group.sample_size(100);

    // Budget: p95 < 15ms at 100K segments
    let (_dir, db_path) = temp_db();
    let storage = rt.block_on(async {
        let s = StorageHandle::new(&db_path).await.expect("create storage");
        seed_db(&s, 100_000).await;
        s
    });

    let opts = SearchOptions {
        limit: Some(10),
        ..Default::default()
    };

    group.bench_function("simple_term_100K", |b| {
        b.to_async(&rt)
            .iter(|| async { storage.search_with_options("cargo", opts.clone()).await });
    });

    group.bench_function("phrase_100K", |b| {
        b.to_async(&rt).iter(|| async {
            storage
                .search_with_options("\"mismatched types\"", opts.clone())
                .await
        });
    });

    group.bench_function("prefix_100K", |b| {
        b.to_async(&rt)
            .iter(|| async { storage.search_with_options("compil*", opts.clone()).await });
    });

    group.bench_function("boolean_100K", |b| {
        b.to_async(&rt).iter(|| async {
            storage
                .search_with_options("error AND types", opts.clone())
                .await
        });
    });

    group.bench_function("no_match_100K", |b| {
        b.to_async(&rt).iter(|| async {
            storage
                .search_with_options("zzz_nonexistent_zzz", opts.clone())
                .await
        });
    });

    // High-limit query regression guard
    let opts_100 = SearchOptions {
        limit: Some(100),
        ..Default::default()
    };
    group.bench_function("high_limit_100K", |b| {
        b.to_async(&rt)
            .iter(|| async { storage.search_with_options("test", opts_100.clone()).await });
    });

    rt.block_on(storage.shutdown()).expect("shutdown");
    group.finish();
}

// ---------------------------------------------------------------------------
// Group 4: upsert_pane latency
// ---------------------------------------------------------------------------

fn bench_upsert_pane(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("storage_upsert_pane");
    group.sample_size(100);

    // Budget: p95 < 1ms
    let (_dir, db_path) = temp_db();
    let storage = rt.block_on(async {
        let s = StorageHandle::new(&db_path).await.expect("create storage");
        // Pre-populate some panes
        for id in 0..50 {
            s.upsert_pane(test_pane(id)).await.expect("upsert");
        }
        s
    });

    // Update existing pane (hot path)
    group.bench_function("update_existing", |b| {
        b.to_async(&rt).iter(|| async {
            let mut pane = test_pane(1);
            pane.last_seen_at = now_ms();
            storage.upsert_pane(pane).await
        });
    });

    // Insert new pane
    let counter = std::sync::atomic::AtomicU64::new(1000);
    group.bench_function("insert_new", |b| {
        b.to_async(&rt).iter(|| {
            let id = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let s = &storage;
            async move { s.upsert_pane(test_pane(id)).await }
        });
    });

    rt.block_on(storage.shutdown()).expect("shutdown");
    group.finish();
}

// ---------------------------------------------------------------------------
// Group 5: Append latency scaling (regression detection)
// ---------------------------------------------------------------------------

fn bench_append_scaling(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("storage_append_scaling");
    group.sample_size(50);

    // Measure how append latency degrades as DB grows.
    // If ratio 100K/1K > 3x, it's a regression signal.
    for (pre_pop, label) in [
        (100, "100"),
        (1_000, "1K"),
        (10_000, "10K"),
        (50_000, "50K"),
    ] {
        let (_dir, db_path) = temp_db();
        let storage = rt.block_on(async {
            let s = StorageHandle::new(&db_path).await.expect("create storage");
            seed_db(&s, pre_pop).await;
            s
        });

        let counter = std::sync::atomic::AtomicUsize::new(pre_pop);
        group.bench_function(BenchmarkId::new("at", label), |b| {
            b.to_async(&rt).iter(|| {
                let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let content = gen_content(idx);
                let s = &storage;
                async move { s.append_segment(1, &content, None).await }
            });
        });

        rt.block_on(storage.shutdown()).expect("shutdown");
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("storage_regression", BUDGETS);
    Criterion::default()
        .configure_from_args()
        .measurement_time(Duration::from_secs(10))
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_append_single,
        bench_append_batch,
        bench_fts_regression,
        bench_upsert_pane,
        bench_append_scaling
);
criterion_main!(benches);
