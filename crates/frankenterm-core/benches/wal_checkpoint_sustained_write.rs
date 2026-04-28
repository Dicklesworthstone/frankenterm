//! Criterion coverage for SQLite WAL checkpoint cost under sustained event writes.
//!
//! The workload writes 10,000 pattern events with logical 100/sec timestamps,
//! then measures the writer-thread WAL checkpoint pause and event-write tail
//! latency. It models sustained capture/storage pressure without sleeping for
//! the full 100-second wall-clock interval.

use std::hint::black_box;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::cx::{Cx, for_request};
use frankenterm_core::runtime_async::{CompatRuntime, Runtime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, StorageHandle, StoredEvent};
use serde::Serialize;
use serde_json::json;
use tempfile::TempDir;

mod bench_common;

const PANE_COUNT: u64 = 10;
const EVENT_COUNT: usize = 10_000;
const EVENTS_PER_SECOND: usize = 100;
const EVENT_INTERVAL_MS: i64 = 1_000 / EVENTS_PER_SECOND as i64;
const BASE_TS_MS: i64 = 1_777_300_000_000;

const BUDGETS: &[bench_common::BenchBudget] = &[bench_common::BenchBudget {
    name: "wal_checkpoint_sustained_write/write_10k_events_checkpoint",
    budget: "10k logical 100/sec event writes report p99 write latency and WAL checkpoint pause",
}];

struct Workload {
    _dir: TempDir,
    db_path: String,
    storage: StorageHandle,
}

#[derive(Debug, Serialize)]
struct WorkloadSummary {
    events_written: usize,
    steady_state_p99_write_us: u128,
    max_write_us: u128,
    checkpoint_pause_us: u128,
    wal_pages: i64,
}

impl WorkloadSummary {
    fn checkpoint_vs_p99_ratio(&self) -> f64 {
        if self.steady_state_p99_write_us == 0 {
            return 0.0;
        }
        self.checkpoint_pause_us as f64 / self.steady_state_p99_write_us as f64
    }
}

#[derive(Serialize)]
struct BaselineSummary {
    bead: &'static str,
    workload: &'static str,
    logical_event_rate_per_second: usize,
    pane_count: u64,
    event_count: usize,
    summary: WorkloadSummary,
    checkpoint_vs_p99_ratio: f64,
}

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("build benchmark runtime")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn pane_record(pane_id: u64) -> PaneRecord {
    let now = now_ms();
    PaneRecord {
        pane_id,
        pane_uuid: Some(format!("wal-checkpoint-pane-{pane_id}")),
        domain: "local".to_string(),
        window_id: Some(1),
        tab_id: Some(1),
        title: Some(format!("wal checkpoint pane {pane_id}")),
        cwd: Some("/tmp/frankenterm-wal-bench".to_string()),
        tty_name: None,
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn stored_event(index: usize) -> StoredEvent {
    let pane_id = (index as u64 % PANE_COUNT) + 1;
    StoredEvent {
        id: 0,
        pane_id,
        rule_id: if index % 16 == 0 {
            "codex.warning"
        } else {
            "codex.output"
        }
        .to_string(),
        agent_type: "codex".to_string(),
        event_type: "pane_delta".to_string(),
        severity: if index % 16 == 0 { "warning" } else { "info" }.to_string(),
        confidence: 0.94,
        extracted: Some(json!({
            "event_index": index,
            "pane_id": pane_id,
            "logical_rate_per_second": EVENTS_PER_SECOND,
            "workload": "wal_checkpoint_sustained_write"
        })),
        matched_text: Some(format!(
            "pane={pane_id} event={index} sustained WAL checkpoint benchmark capture payload"
        )),
        segment_id: None,
        detected_at: BASE_TS_MS + (index as i64 * EVENT_INTERVAL_MS),
        dedupe_key: Some(format!("wal-checkpoint-{pane_id}-{index}")),
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

fn percentile_us(values: &mut [u128], percentile: usize) -> u128 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = ((values.len() - 1) * percentile) / 100;
    values[index]
}

fn setup_workload(rt: &Runtime, cx: &Cx) -> Workload {
    let dir = tempfile::tempdir().expect("create WAL benchmark temp dir");
    let db_path = dir
        .path()
        .join("wal-checkpoint.db")
        .to_string_lossy()
        .to_string();
    let storage = rt.block_on(async {
        let storage = StorageHandle::new_with_cx(cx, &db_path)
            .await
            .expect("create benchmark storage");
        for pane_id in 1..=PANE_COUNT {
            storage
                .upsert_pane_with_cx(cx, pane_record(pane_id))
                .await
                .expect("upsert benchmark pane");
        }
        storage
    });

    Workload {
        _dir: dir,
        db_path,
        storage,
    }
}

fn run_workload(rt: &Runtime, cx: &Cx, workload: Workload) -> WorkloadSummary {
    rt.block_on(async {
        let mut write_latencies_us = Vec::with_capacity(EVENT_COUNT);

        for index in 0..EVENT_COUNT {
            let start = Instant::now();
            workload
                .storage
                .record_event_with_cx(cx, stored_event(index))
                .await
                .expect("record benchmark event");
            write_latencies_us.push(start.elapsed().as_micros());
        }

        let checkpoint_start = Instant::now();
        let checkpoint = workload
            .storage
            .checkpoint_with_cx(cx)
            .await
            .expect("checkpoint benchmark storage");
        let checkpoint_pause_us = checkpoint_start.elapsed().as_micros();

        workload
            .storage
            .shutdown_with_cx(cx)
            .await
            .expect("shutdown benchmark storage");

        // Keep the DB path observable to prevent the temp workload from being
        // optimized into a pure StorageHandle exercise in future edits.
        black_box(&workload.db_path);

        let max_write_us = write_latencies_us.iter().copied().max().unwrap_or_default();
        let warmup_skip = EVENTS_PER_SECOND.min(write_latencies_us.len());
        let steady_state_p99_write_us = percentile_us(&mut write_latencies_us[warmup_skip..], 99);

        WorkloadSummary {
            events_written: EVENT_COUNT,
            steady_state_p99_write_us,
            max_write_us,
            checkpoint_pause_us,
            wal_pages: checkpoint.wal_pages,
        }
    })
}

fn bench_wal_checkpoint_sustained_write(c: &mut Criterion) {
    let rt = runtime();
    let cx = for_request();
    let mut group = c.benchmark_group("wal_checkpoint_sustained_write");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(100));
    group.measurement_time(Duration::from_secs(1));
    group.throughput(Throughput::Elements(EVENT_COUNT as u64));

    group.bench_function("write_10k_events_checkpoint", |b| {
        b.iter_batched(
            || setup_workload(&rt, &cx),
            |workload| {
                let summary = run_workload(&rt, &cx, workload);
                assert_eq!(summary.events_written, EVENT_COUNT);
                black_box(summary.steady_state_p99_write_us);
                black_box(summary.max_write_us);
                black_box(summary.checkpoint_pause_us);
                black_box(summary.wal_pages);
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("wal-checkpoint-ft-ctt7k", BUDGETS);
    emit_baseline_summary();
    Criterion::default().configure_from_args()
}

fn emit_baseline_summary() {
    let rt = runtime();
    let cx = for_request();
    let workload = setup_workload(&rt, &cx);
    let summary = run_workload(&rt, &cx, workload);
    let baseline = BaselineSummary {
        bead: "ft-ctt7k",
        workload: "write_10k_events_checkpoint",
        logical_event_rate_per_second: EVENTS_PER_SECOND,
        pane_count: PANE_COUNT,
        event_count: EVENT_COUNT,
        checkpoint_vs_p99_ratio: summary.checkpoint_vs_p99_ratio(),
        summary,
    };

    let path = Path::new("target/criterion/wal-checkpoint-ft-ctt7k-summary.json");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::File::create(path) {
        let _ = serde_json::to_writer_pretty(&mut file, &baseline);
        let _ = file.write_all(b"\n");
    }
    println!("[BENCH] summary={}", path.display());
}

criterion_group! {
    name = benches;
    config = bench_config();
    targets = bench_wal_checkpoint_sustained_write
}
criterion_main!(benches);
