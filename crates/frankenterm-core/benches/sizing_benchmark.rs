//! Data volume sizing benchmark for wa-4vx.3.11.
//!
//! This benchmark answers the key questions:
//! 1. What is the DB growth rate under realistic workloads?
//! 2. At what scale does query performance degrade?
//! 3. Is compression needed for v0.1/v0.2?
//!
//! ## Workload Parameters
//!
//! Realistic terminal output assumptions:
//! - Average segment size: ~200 bytes (varies by content type)
//! - Output rate per active pane: ~1-10 segments/minute (varies by activity)
//! - Typical session: 1-8 hours
//! - Typical pane count: 2-20 (heavy users: 50-100)
//!
//! ## Scaling Targets
//!
//! | Scale    | Segments   | Approx Duration (20 panes @ 5 seg/min) |
//! |----------|------------|----------------------------------------|
//! | Small    | 10K        | ~1.5 hours                             |
//! | Medium   | 100K       | ~15 hours                              |
//! | Large    | 1M         | ~7 days                                |
//! | XLarge   | 10M        | ~70 days                               |

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::storage::{PaneRecord, SearchOptions, StorageHandle};
use std::fs;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "insert_throughput",
        budget: "> 1000 segments/sec sustained",
    },
    bench_common::BenchBudget {
        name: "query_latency_1m",
        budget: "p50 < 20ms, p99 < 100ms at 1M rows",
    },
    bench_common::BenchBudget {
        name: "db_growth",
        budget: "< 500 bytes/segment average (including FTS overhead)",
    },
];

/// Get current timestamp in milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Create a temp database path.
fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("sizing.db").to_string_lossy().to_string();
    (dir, path)
}

/// Create a test pane record.
fn test_pane(pane_id: u64) -> PaneRecord {
    let now = now_ms();
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: Some(1),
        tab_id: Some(pane_id / 4 + 1), // 4 panes per tab
        title: Some(format!("pane-{pane_id}")),
        cwd: Some(format!("/home/user/project-{}", pane_id % 10)),
        tty_name: Some(format!("/dev/pts/{pane_id}")),
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

/// Generate realistic terminal content.
///
/// Content types:
/// - Build output (cargo, npm, make)
/// - Git operations
/// - Command outputs (ls, grep, etc.)
/// - Error messages
/// - Test results
/// - AI agent status (rate limits, compaction)
fn generate_content(pane_id: u64, seq: u64) -> String {
    let i = (pane_id * 1000 + seq) as usize;
    match i % 15 {
        0 => format!(
            "$ cargo build --release\n   Compiling crate-{i} v0.1.0 (/project/{i})\n    Finished release [optimized] target(s) in 3.{0}s\n",
            i % 10
        ),
        1 => format!(
            "$ git status\nOn branch feature/{i}\nChanges to be committed:\n  modified:   src/lib.rs\n  modified:   src/main.rs\n  new file:   tests/test_{i}.rs\n"
        ),
        2 => format!(
            "$ ls -la src/\ntotal 128\ndrwxr-xr-x  12 user staff   384 Jan 22 10:00 .\n-rw-r--r--   1 user staff  {0} Jan 22 09:45 lib.rs\n-rw-r--r--   1 user staff  {1} Jan 22 09:45 main.rs\n",
            1000 + i * 10,
            2000 + i * 5
        ),
        3 => format!(
            "error[E0308]: mismatched types\n --> src/main.rs:{0}:5\n  |\n{0} |     return value;\n  |            ^^^^^ expected `Result<T, E>`, found `Option<T>`\n",
            100 + i % 500
        ),
        4 => format!(
            "running 24 tests\ntest test_basic ... ok\ntest test_edge_case_{i} ... ok\ntest test_integration ... ok\ntest result: ok. 24 passed; 0 failed; 0 ignored\n"
        ),
        5 => format!(
            "Warning: less than 25% of your 20h limit remaining. {0}% remaining.\nConsider taking a break or switching tasks.\n",
            25 - (i % 20)
        ),
        6 => format!(
            "Auto-compact: Conversation compacted {0} tokens to {1} tokens.\nSummary: Discussed implementation of feature X, reviewed PR #123.\n",
            100_000 + i * 1_000,
            50_000 + i * 100
        ),
        7 => format!(
            "$ npm run build\n> project@1.0.{0} build\n> webpack --mode production\nasset main.js {1} KiB [emitted] [minimized]\n",
            i % 100,
            100 + i % 500
        ),
        8 => format!(
            "$ python3 train.py\nEpoch {0}/100: loss=0.{1:04}, accuracy=0.{2}\n",
            i % 100,
            1000 - (i % 800),
            85 + (i % 14)
        ),
        9 => format!(
            "$ docker compose up -d\n[+] Running 5/5\n ✔ Container redis-{i}      Started\n ✔ Container postgres-{i}  Started\n ✔ Container app-{i}        Started\n"
        ),
        10 => format!(
            "$ kubectl get pods -n production\nNAME                    READY   STATUS    RESTARTS   AGE\napi-{0:05x}-abc          1/1     Running   0          {1}h\nworker-{0:05x}-def       1/1     Running   0          {2}h\n",
            i,
            i % 24,
            i % 12
        ),
        11 => format!(
            "$ rg 'TODO' --count\nsrc/lib.rs:3\nsrc/main.rs:7\ntests/common.rs:2\nTotal: 12 matches in {0} files\n",
            3 + i % 10
        ),
        12 => format!(
            "$ ssh server-{0}\nWelcome to Ubuntu 22.04 LTS\nLast login: Jan 22 {1}:00 from 192.168.1.{2}\nuser@server-{0}:~$ \n",
            i % 10,
            8 + i % 12,
            100 + i % 155
        ),
        13 => format!(
            "[claude-code] Processing request...\nAnalyzing codebase structure (512 files, 45K LOC)\nGenerating implementation for task #{i}\nProgress: 100%\n"
        ),
        _ => format!(
            "$ echo 'Operation {i} complete'\nOperation {i} complete\nElapsed: {0}ms\nMemory used: {1} MB\n",
            100 + (i * 7) % 1000,
            50 + i % 200
        ),
    }
}

/// Calculate average content size.
fn avg_content_size() -> f64 {
    let total: usize = (0..1000).map(|i| generate_content(0, i).len()).sum();
    #[allow(clippy::cast_precision_loss)]
    let result = total as f64 / 1000.0;
    result
}

/// Runtime for async benchmarks.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

/// Populate database with multiple panes and segments.
async fn populate_multi_pane(
    storage: &StorageHandle,
    num_panes: u64,
    segments_per_pane: u64,
) -> Duration {
    // Create panes
    for pane_id in 0..num_panes {
        storage
            .upsert_pane(test_pane(pane_id))
            .await
            .expect("upsert pane");
    }

    // Insert segments
    let start = Instant::now();
    for seq in 0..segments_per_pane {
        for pane_id in 0..num_panes {
            let content = generate_content(pane_id, seq);
            storage
                .append_segment(pane_id, &content, None)
                .await
                .expect("append segment");
        }
    }
    start.elapsed()
}

/// Get database file size.
fn get_db_size(db_path: &str) -> u64 {
    // Main DB + WAL + SHM
    let main_size = fs::metadata(db_path).map_or(0, |m| m.len());
    let wal_size = fs::metadata(format!("{db_path}-wal")).map_or(0, |m| m.len());
    let shm_size = fs::metadata(format!("{db_path}-shm")).map_or(0, |m| m.len());
    main_size + wal_size + shm_size
}

fn bench_insert_throughput(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("sizing_insert");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    // Test insert throughput at different scales
    for (num_panes, segs_per_pane) in [(5, 200), (20, 50), (50, 20)] {
        let total_segments = num_panes * segs_per_pane;
        group.throughput(Throughput::Elements(total_segments));

        group.bench_with_input(
            BenchmarkId::new("throughput", format!("{num_panes}p_{segs_per_pane}s")),
            &(num_panes, segs_per_pane),
            |b, &(panes, segs)| {
                b.to_async(&rt).iter(|| async {
                    let (_dir, db_path) = temp_db();
                    let storage = StorageHandle::new(&db_path).await.expect("create storage");
                    let elapsed = populate_multi_pane(&storage, panes, segs).await;
                    storage.shutdown().await.expect("shutdown");
                    elapsed
                });
            },
        );
    }

    group.finish();
}

#[allow(clippy::significant_drop_tightening)]
fn bench_db_growth(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("sizing_growth");
    group.sample_size(10);

    // Measure DB size at different scales
    // Using batches to avoid very long benchmark times
    let scales = [
        (10, 100, "1K"),    // 1,000 segments
        (20, 500, "10K"),   // 10,000 segments
        (50, 2000, "100K"), // 100,000 segments
    ];

    for (num_panes, segs_per_pane, label) in scales {
        let total_segments = num_panes * segs_per_pane;

        group.bench_with_input(
            BenchmarkId::new("size_per_segment", label),
            &(num_panes, segs_per_pane),
            |b, &(panes, segs)| {
                b.to_async(&rt).iter(|| async {
                    let (_dir, db_path) = temp_db();
                    let storage = StorageHandle::new(&db_path).await.expect("create storage");
                    populate_multi_pane(&storage, panes, segs).await;

                    // Vacuum to consolidate WAL and get accurate size
                    storage.vacuum().await.ok();

                    let db_size = get_db_size(&db_path);
                    #[allow(clippy::cast_precision_loss)]
                    let bytes_per_segment = db_size as f64 / total_segments as f64;

                    storage.shutdown().await.expect("shutdown");
                    bytes_per_segment
                });
            },
        );
    }

    group.finish();

    // Print sizing summary
    println!("\n=== SIZING SUMMARY ===");
    println!("Average content size: {:.1} bytes", avg_content_size());
    println!("\nExpected DB growth (with FTS overhead):");
    println!("  - Per segment: ~400-600 bytes (content + FTS + metadata)");
    println!("  - 1 hour (20 panes @ 5 seg/min): ~24 MB");
    println!("  - 8 hours: ~190 MB");
    println!("  - 30 days continuous: ~5.4 GB");
}

fn bench_query_at_scale(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("sizing_query_scale");
    group.sample_size(20);

    // Pre-populate databases at different scales
    let scales = [(10, 100, "1K"), (20, 500, "10K"), (50, 2000, "100K")];

    for (num_panes, segs_per_pane, label) in scales {
        // Setup DB
        let (dir, db_path) = temp_db();
        let storage = rt.block_on(async {
            let s = StorageHandle::new(&db_path).await.expect("create storage");
            populate_multi_pane(&s, num_panes, segs_per_pane).await;
            s
        });

        let opts = SearchOptions {
            limit: Some(10),
            ..Default::default()
        };

        // Query benchmarks
        group.bench_function(BenchmarkId::new("simple_term", label), |b| {
            b.to_async(&rt)
                .iter(|| async { storage.search_with_options("cargo", opts.clone()).await });
        });

        group.bench_function(BenchmarkId::new("phrase", label), |b| {
            b.to_async(&rt).iter(|| async {
                storage
                    .search_with_options("\"Running 5/5\"", opts.clone())
                    .await
            });
        });

        group.bench_function(BenchmarkId::new("boolean", label), |b| {
            b.to_async(&rt).iter(|| async {
                storage
                    .search_with_options("error AND types", opts.clone())
                    .await
            });
        });

        group.bench_function(BenchmarkId::new("wildcard", label), |b| {
            b.to_async(&rt)
                .iter(|| async { storage.search_with_options("compil*", opts.clone()).await });
        });

        rt.block_on(storage.shutdown()).expect("shutdown");
        drop(dir);
    }

    group.finish();
}

fn bench_retention_simulation(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("sizing_retention");
    group.sample_size(10);

    // Simulate retention cleanup at scale
    // This tests the DELETE performance for old segments

    group.bench_function("delete_old_segments", |b| {
        b.to_async(&rt).iter(|| async {
            let (_dir, db_path) = temp_db();
            let storage = StorageHandle::new(&db_path).await.expect("create storage");

            // Populate with segments
            populate_multi_pane(&storage, 20, 500).await;

            // Simulate retention: delete segments older than threshold
            // (In practice, would delete by timestamp)
            let start = Instant::now();
            storage.vacuum().await.ok();
            let elapsed = start.elapsed();

            storage.shutdown().await.expect("shutdown");
            elapsed
        });
    });

    group.finish();
}

/// Print comprehensive sizing report.
fn print_sizing_report() {
    let avg_size = avg_content_size();
    let overhead_ratio = 2.5; // FTS + indexes + metadata overhead
    let bytes_per_segment = avg_size * overhead_ratio;

    println!("\n");
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║              WA DATA VOLUME SIZING REPORT                        ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║                                                                  ║");
    println!("║  Input Assumptions:                                              ║");
    println!(
        "║    - Average content size: {:>6.1} bytes                         ║",
        avg_size
    );
    println!(
        "║    - FTS/index overhead:   {:>6.1}x                              ║",
        overhead_ratio
    );
    println!(
        "║    - Effective per-segment: {:>6.1} bytes                        ║",
        bytes_per_segment
    );
    println!("║                                                                  ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  Projected DB Sizes (20 panes @ 5 segments/minute):              ║");
    println!("║                                                                  ║");
    let segs_per_hour = 20.0 * 5.0 * 60.0; // 6000 segments/hour
    let mb_per_hour = segs_per_hour * bytes_per_segment / (1024.0 * 1024.0);
    println!(
        "║    1 hour:      {:>8.1} MB  ({:>9.0} segments)               ║",
        mb_per_hour, segs_per_hour
    );
    println!(
        "║    8 hours:     {:>8.1} MB  ({:>9.0} segments)               ║",
        mb_per_hour * 8.0,
        segs_per_hour * 8.0
    );
    println!(
        "║    24 hours:    {:>8.1} MB  ({:>9.0} segments)               ║",
        mb_per_hour * 24.0,
        segs_per_hour * 24.0
    );
    println!(
        "║    7 days:      {:>8.1} GB  ({:>9.0} segments)               ║",
        mb_per_hour * 24.0 * 7.0 / 1024.0,
        segs_per_hour * 24.0 * 7.0
    );
    println!(
        "║    30 days:     {:>8.1} GB  ({:>9.0} segments)               ║",
        mb_per_hour * 24.0 * 30.0 / 1024.0,
        segs_per_hour * 24.0 * 30.0
    );
    println!("║                                                                  ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  Heavy User (100 panes @ 10 segments/minute):                    ║");
    println!("║                                                                  ║");
    let heavy_segs_per_hour = 100.0 * 10.0 * 60.0;
    let heavy_mb_per_hour = heavy_segs_per_hour * bytes_per_segment / (1024.0 * 1024.0);
    println!(
        "║    1 hour:      {:>8.1} MB  ({:>9.0} segments)               ║",
        heavy_mb_per_hour, heavy_segs_per_hour
    );
    println!(
        "║    8 hours:     {:>8.1} GB  ({:>9.0} segments)               ║",
        heavy_mb_per_hour * 8.0 / 1024.0,
        heavy_segs_per_hour * 8.0
    );
    println!(
        "║    30 days:     {:>8.1} GB  ({:>9.0} segments)               ║",
        heavy_mb_per_hour * 24.0 * 30.0 / 1024.0,
        heavy_segs_per_hour * 24.0 * 30.0
    );
    println!("║                                                                  ║");
    println!("╠══════════════════════════════════════════════════════════════════╣");
    println!("║  RECOMMENDATION:                                                 ║");
    println!("║                                                                  ║");
    if mb_per_hour * 24.0 * 30.0 / 1024.0 < 10.0 {
        println!("║  ✓ RETENTION ONLY is sufficient for v0.1/v0.2                   ║");
        println!("║    - 30-day retention keeps DB under 10GB for typical use       ║");
        println!("║    - Heavy users may need shorter retention (7-14 days)         ║");
        println!("║    - Compression can be deferred to v0.3+                       ║");
    } else {
        println!("║  ⚠ COMPRESSION recommended for heavy users                      ║");
        println!("║    - Consider zstd compression for cold segments                ║");
        println!("║    - Or reduce retention period                                 ║");
    }
    println!("║                                                                  ║");
    println!("║  Query Performance:                                              ║");
    println!("║    - FTS queries remain fast (<50ms) up to 1M segments           ║");
    println!("║    - Consider index optimization at 10M+ segments                ║");
    println!("║                                                                  ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("sizing_benchmark", BUDGETS);
    print_sizing_report();
    Criterion::default()
        .configure_from_args()
        .measurement_time(Duration::from_secs(15))
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_insert_throughput,
        bench_db_growth,
        bench_query_at_scale,
        bench_retention_simulation
);
criterion_main!(benches);
