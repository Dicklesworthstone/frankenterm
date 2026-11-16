//! Criterion coverage for the recorder write hot path.
//!
//! The workload models one second of 100 detection events spread across
//! 10 actively recorded panes.  It exercises `RecordingManager::record_event`
//! plus recorder flush behavior without changing production recorder code.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::cx::{Cx, for_request};
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::recording::{RecordingManager, RecordingOptions};
use frankenterm_core::runtime_async::{CompatRuntime, Runtime, RuntimeBuilder};
use serde_json::json;
use tempfile::TempDir;

mod bench_common;

const PANE_COUNT: u64 = 10;
const EVENTS_PER_SECOND: usize = 100;
const BASE_TS_MS: i64 = 1_777_200_000_000;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "recorder_hot_path/event_flush_each_100eps_10panes",
        budget: "record_event with flush_threshold=1 for 100 events/sec across 10 panes",
    },
    bench_common::BenchBudget {
        name: "recorder_hot_path/event_buffered_100eps_10panes",
        budget: "record_event with default 64-frame buffering for 100 events/sec across 10 panes",
    },
];

struct RecorderWorkload {
    _dir: TempDir,
    manager: RecordingManager,
    detections: Vec<Detection>,
    events: Vec<(u64, usize, i64)>,
}

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("build benchmark runtime")
}

fn detection(index: usize) -> Detection {
    let pane_id = (index as u64 % PANE_COUNT) + 1;
    Detection {
        rule_id: if index % 4 == 0 {
            "codex.progress"
        } else {
            "codex.output"
        }
        .to_string(),
        agent_type: AgentType::Codex,
        event_type: "pane_delta".to_string(),
        severity: if index % 17 == 0 {
            Severity::Warning
        } else {
            Severity::Info
        },
        confidence: 0.92,
        extracted: json!({
            "pane_id": pane_id,
            "event_index": index,
            "rate_hz": EVENTS_PER_SECOND,
            "fleet": "10-pane-recorder-hot-path"
        }),
        matched_text: format!(
            "pane {pane_id} delta {index}: recorder frame payload with representative terminal text"
        ),
        span: (0, 64),
    }
}

fn setup_workload(rt: &Runtime, flush_threshold: usize) -> RecorderWorkload {
    let dir = tempfile::tempdir().expect("create recorder benchmark temp dir");
    let manager = RecordingManager::new(RecordingOptions {
        flush_threshold,
        redact_output: true,
        redact_events: true,
    });
    let cx = for_request();

    rt.block_on(async {
        for pane_id in 1..=PANE_COUNT {
            manager
                .start_recording_with_cx(
                    &cx,
                    pane_id,
                    &dir.path().join(format!("pane-{pane_id}.war")),
                    BASE_TS_MS,
                )
                .await
                .expect("start recorder");
        }
    });

    let detections = (0..EVENTS_PER_SECOND).map(detection).collect::<Vec<_>>();
    let events = (0..EVENTS_PER_SECOND)
        .map(|index| {
            (
                (index as u64 % PANE_COUNT) + 1,
                index,
                BASE_TS_MS + (index as i64 * 10),
            )
        })
        .collect::<Vec<_>>();

    RecorderWorkload {
        _dir: dir,
        manager,
        detections,
        events,
    }
}

fn run_workload(rt: &Runtime, cx: &Cx, workload: RecorderWorkload) -> u64 {
    rt.block_on(async {
        for &(pane_id, detection_index, captured_at_ms) in &workload.events {
            workload
                .manager
                .record_event_with_cx(
                    cx,
                    pane_id,
                    black_box(&workload.detections[detection_index]),
                    captured_at_ms,
                )
                .await
                .expect("record detection event");
        }

        let mut frames = 0u64;
        for pane_id in 1..=PANE_COUNT {
            if let Some(stats) = workload
                .manager
                .stop_recording_with_cx(cx, pane_id)
                .await
                .expect("stop recorder")
            {
                frames += stats.frames_written;
            }
        }
        frames
    })
}

fn bench_recorder_hot_path(c: &mut Criterion) {
    let rt = runtime();
    let cx = for_request();
    let mut group = c.benchmark_group("recorder_hot_path");
    group.sample_size(20);
    group.throughput(Throughput::Elements(EVENTS_PER_SECOND as u64));

    for &(name, flush_threshold) in &[
        ("event_flush_each_100eps_10panes", 1usize),
        ("event_buffered_100eps_10panes", 64usize),
    ] {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &flush_threshold,
            |b, &flush_threshold| {
                b.iter_batched(
                    || setup_workload(&rt, flush_threshold),
                    |workload| {
                        let frames = run_workload(&rt, &cx, workload);
                        assert_eq!(frames, EVENTS_PER_SECOND as u64);
                        black_box(frames);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("recorder_hot_path", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group! {
    name = benches;
    config = bench_config();
    targets = bench_recorder_hot_path
}
criterion_main!(benches);
