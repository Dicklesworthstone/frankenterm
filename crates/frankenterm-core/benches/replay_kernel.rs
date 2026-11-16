//! Criterion benchmark for replay kernel throughput.
//!
//! Bead: ft-og6q6.7.3

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::recorder_replay::{ReplayConfig, ReplayScheduler};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "replay_kernel/instant_mode_20000_events",
        budget: ">= 100K events/sec in instant mode (20K-event batches)",
    },
    bench_common::BenchBudget {
        name: "replay_kernel/artifact_read_stream_250000_events",
        budget: ">= 500K events/sec streaming artifact read rate",
    },
];

const REPLAY_BATCH_EVENTS: usize = 20_000;
const ARTIFACT_STREAM_EVENTS: usize = 250_000;

fn make_ingress_event(seq: u64, pane_id: u64, ts_ms: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("evt-{pane_id}-{seq}-{ts_ms}"),
        pane_id,
        session_id: Some("bench-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: ts_ms,
        recorded_at_ms: ts_ms,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: "bench-input".to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn build_replay_events(count: usize) -> Vec<RecorderEvent> {
    let mut events = Vec::with_capacity(count);
    for i in 0..count {
        let seq = u64::try_from(i).unwrap_or_default();
        let pane_id = u64::try_from(i % 8).unwrap_or_default();
        let ts_ms = 1_700_000_000_000_u64.saturating_add(seq);
        events.push(make_ingress_event(seq, pane_id, ts_ms));
    }
    events
}

fn build_artifact_stream(count: usize) -> String {
    let mut out = String::with_capacity(count.saturating_mul(64));
    for i in 0..count {
        let event_id = format!("evt-{i}");
        let ts = 1_700_000_000_000_u64.saturating_add(u64::try_from(i).unwrap_or_default());
        out.push_str(&format!(
            "{{\"event_id\":\"{event_id}\",\"pane_id\":1,\"occurred_at_ms\":{ts},\"text\":\"ok\"}}\n"
        ));
    }
    out
}

fn bench_instant_mode_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("replay_kernel");
    group.throughput(Throughput::Elements(
        u64::try_from(REPLAY_BATCH_EVENTS).unwrap_or_default(),
    ));

    let events = build_replay_events(REPLAY_BATCH_EVENTS);

    group.bench_function("instant_mode_20000_events", |b| {
        b.iter_batched(
            || {
                ReplayScheduler::new(events.clone(), ReplayConfig::instant())
                    .expect("valid replay scheduler")
            },
            |mut scheduler| {
                let steps = scheduler.run_to_completion();
                black_box(steps.len());
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_artifact_read_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("replay_kernel");
    group.throughput(Throughput::Elements(
        u64::try_from(ARTIFACT_STREAM_EVENTS).unwrap_or_default(),
    ));

    let artifact_stream = build_artifact_stream(ARTIFACT_STREAM_EVENTS);

    group.bench_function("artifact_read_stream_250000_events", |b| {
        b.iter(|| {
            let count = artifact_stream.lines().count();
            black_box(count);
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("replay_kernel", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group! {
    name = benches;
    config = bench_config();
    targets = bench_instant_mode_throughput, bench_artifact_read_stream
}
criterion_main!(benches);
