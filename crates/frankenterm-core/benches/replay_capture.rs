//! Criterion benchmark for replay capture adapter overhead.
//!
//! Bead: ft-og6q6.7.3

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::ingest::{CapturedSegment, CapturedSegmentKind};
use frankenterm_core::replay_capture::{CaptureAdapter, CaptureConfig, NoopCaptureSink};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[bench_common::BenchBudget {
    name: "replay_capture/capture_overhead_per_event",
    budget: "< 1ms per capture event overhead",
}];

fn sample_segment() -> CapturedSegment {
    CapturedSegment {
        pane_id: 42,
        seq: 7,
        content: "cargo test -p frankenterm-core --lib replay_capture".to_string(),
        kind: CapturedSegmentKind::Delta,
        captured_at: 1_710_000_000_000,
    }
}

fn bench_capture_overhead_per_event(c: &mut Criterion) {
    let mut group = c.benchmark_group("replay_capture");
    group.throughput(Throughput::Elements(1));

    let adapter = CaptureAdapter::new(Arc::new(NoopCaptureSink), CaptureConfig::default());
    let segment = sample_segment();

    group.bench_function("capture_overhead_per_event", |b| {
        b.iter(|| adapter.capture_egress(black_box(&segment)));
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("replay_capture", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group! {
    name = benches;
    config = bench_config();
    targets = bench_capture_overhead_per_event
}
criterion_main!(benches);
