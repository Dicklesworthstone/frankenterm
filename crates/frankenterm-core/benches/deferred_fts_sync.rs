//! Deferred FTS sync throughput benchmark.
//!
//! Compares the legacy per-segment FTS catch-up loop against the default-off
//! `FT_MOONSHOT_FTS_INSERT_SELECT_BATCH` set-based `INSERT ... SELECT` path.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::runtime_async::{CompatRuntime, Runtime, RuntimeBuilder};
use frankenterm_core::storage::{
    FtsSyncConfig, PaneRecord, StorageConfig, StorageHandle,
    set_fts_insert_select_batch_override_for_bench,
};
use std::hint::black_box;
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;

mod bench_common;

const SEGMENT_COUNT: usize = 4_096;
const SEGMENT_COUNT_U64: u64 = 4_096;
const BATCH_SIZE: usize = 512;
const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "deferred_fts_sync/per_segment_insert",
        budget: "baseline oracle: deferred sync with one FTS insert per segment",
    },
    bench_common::BenchBudget {
        name: "deferred_fts_sync/insert_select_batch",
        budget: "expected faster than per_segment_insert by reducing per-row Rust/SQLite round trips",
    },
];

#[derive(Clone, Copy)]
enum SyncMode {
    PerSegmentInsert,
    InsertSelectBatch,
}

impl SyncMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::PerSegmentInsert => "per_segment_insert",
            Self::InsertSelectBatch => "insert_select_batch",
        }
    }

    fn insert_select_enabled(self) -> bool {
        matches!(self, Self::InsertSelectBatch)
    }
}

struct Workload {
    storage: StorageHandle,
    _dir: TempDir,
}

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn test_pane(pane_id: u64) -> PaneRecord {
    let now = now_ms();
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: Some(1),
        tab_id: Some(1),
        title: Some("deferred FTS sync bench".to_string()),
        cwd: Some("/tmp/frankenterm-deferred-fts-sync".to_string()),
        tty_name: None,
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn temp_db(mode: SyncMode, iteration: u64) -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir
        .path()
        .join(format!("{}_{}.db", mode.as_str(), iteration))
        .to_string_lossy()
        .to_string();
    (dir, path)
}

fn segment_content(i: usize) -> String {
    match i % 8 {
        0 => format!("$ cargo check -p frankenterm-core\nsegment={i}\nstatus=green\n"),
        1 => format!("search token needle{i} pane=1 deferred fts catchup line\n"),
        2 => format!("warning: backpressure window opened for batch {i}\nretry_after=42ms\n"),
        3 => format!("event=agent_prompt_ready trace_id=trace-{i:06}\n"),
        4 => format!("stdout: compiling storage shard {i} with fts payload\n"),
        5 => format!("stderr: transient database busy recovered at seq {i}\n"),
        6 => format!("json {{\"pane\":1,\"seq\":{i},\"kind\":\"capture\"}}\n"),
        _ => format!("plain terminal scrollback line {i} with repeated searchable words\n"),
    }
}

fn sync_config() -> FtsSyncConfig {
    FtsSyncConfig {
        batch_size: BATCH_SIZE,
        max_batch_bytes: MAX_BATCH_BYTES,
        commit_progress: true,
    }
}

fn storage_config() -> StorageConfig {
    StorageConfig {
        defer_fts_triggers: true,
        ..StorageConfig::default()
    }
}

async fn prepare_workload(mode: SyncMode, iteration: u64) -> Workload {
    let (dir, db_path) = temp_db(mode, iteration);
    let storage = StorageHandle::with_config(&db_path, storage_config())
        .await
        .expect("create deferred FTS storage");
    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert bench pane");

    for i in 0..SEGMENT_COUNT {
        storage
            .append_segment(1, &segment_content(i), None)
            .await
            .expect("append deferred FTS segment");
    }

    Workload { storage, _dir: dir }
}

fn bench_sync_mode(c: &mut Criterion, mode: SyncMode) {
    let rt = runtime();
    let mut group = c.benchmark_group("deferred_fts_sync");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(SEGMENT_COUNT_U64));

    group.bench_with_input(
        BenchmarkId::new(mode.as_str(), SEGMENT_COUNT),
        &mode,
        |b, &bench_mode| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for iteration in 0..iters {
                    let workload = rt.block_on(prepare_workload(bench_mode, iteration));
                    set_fts_insert_select_batch_override_for_bench(Some(
                        bench_mode.insert_select_enabled(),
                    ));
                    let started = Instant::now();
                    let result = rt
                        .block_on(workload.storage.sync_fts(sync_config()))
                        .expect("sync deferred FTS");
                    total += started.elapsed();
                    set_fts_insert_select_batch_override_for_bench(None);
                    assert_eq!(
                        result.segments_indexed, SEGMENT_COUNT_U64,
                        "deferred FTS sync should index the seeded workload"
                    );
                    black_box(result);
                    rt.block_on(workload.storage.shutdown())
                        .expect("shutdown storage");
                }
                total
            });
        },
    );

    group.finish();
}

fn bench_deferred_fts_sync(c: &mut Criterion) {
    bench_sync_mode(c, SyncMode::PerSegmentInsert);
    bench_sync_mode(c, SyncMode::InsertSelectBatch);
    bench_common::emit_bench_artifacts("deferred_fts_sync", BUDGETS);
}

criterion_group!(benches, bench_deferred_fts_sync);
criterion_main!(benches);
