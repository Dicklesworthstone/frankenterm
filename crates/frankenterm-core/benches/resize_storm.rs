//! Resize-storm renderer SLO bench (RQ-S1).
//!
//! Simulates the 200-pane, continuous 5s resize gesture named in
//! `docs/perf/resize-quality-slo.json` and emits
//! `target/criterion/slo-resize_fps.jsonl` so release attestation can retain a
//! content-addressed artifact. Criterion records iteration timing; the
//! assertion-bearing harness records per-frame samples and fails if p99 frame
//! processing time exceeds the 60 FPS budget.

use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::dirty_line_telemetry::{
    DirtyEventSource, DirtyMark, DirtyMarkClassification,
};
use serde_json::json;

const PANE_COUNT: u32 = 200;
const MIN_ROWS: u32 = 24;
const MAX_ROWS: u32 = 60;
const MIN_COLS: u32 = 80;
const MAX_COLS: u32 = 200;
const FRAMES_PER_GESTURE: u32 = 300;
const WARMUP_FRAMES: u32 = 12;
const P99_FRAME_BUDGET_US: u64 = 16_600;
const EVIDENCE_PATH: &str = "target/criterion/slo-resize_fps.jsonl";

fn resize_mark(frame: u32, pane_id: u64) -> (DirtyMark, u32) {
    let phase = frame % FRAMES_PER_GESTURE;
    let distance_from_midpoint = phase.abs_diff(FRAMES_PER_GESTURE / 2);
    let scale_num = (FRAMES_PER_GESTURE / 2).saturating_sub(distance_from_midpoint);
    let scale_den = (FRAMES_PER_GESTURE / 2).max(1);
    let rows = MIN_ROWS + ((MAX_ROWS - MIN_ROWS) * scale_num / scale_den);
    let cols = MIN_COLS + ((MAX_COLS - MIN_COLS) * scale_num / scale_den);

    (
        DirtyMark {
            pane_id,
            source: DirtyEventSource::Resize,
            start_row: 0,
            end_row: rows,
        },
        cols,
    )
}

#[inline(never)]
fn drive_resize_frame(frame: u32) {
    for pane_id in 0..u64::from(PANE_COUNT) {
        let (mark, cols) = resize_mark(frame, pane_id);
        let classification = mark.classify(mark.end_row);
        debug_assert_eq!(classification, DirtyMarkClassification::WholeScreen);

        for row in mark.start_row..mark.end_row {
            black_box((pane_id, row, cols));
        }
        black_box(classification);
    }
}

fn percentile_us(samples: &mut [u64], percentile: u8) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let percentile = u64::from(percentile.min(100));
    let sample_len = u64::try_from(samples.len()).unwrap_or(u64::MAX);
    let target = sample_len.saturating_mul(percentile).div_ceil(100).max(1);
    let index = usize::try_from(target - 1).unwrap_or(samples.len() - 1);
    samples[index.min(samples.len() - 1)]
}

fn append_evidence_row(p50_us: u64, p95_us: u64, p99_us: u64, sample_count: usize) {
    let path = PathBuf::from(EVIDENCE_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create resize FPS evidence directory");
    }

    let estimated_p99_fps_milli = if p99_us == 0 {
        0
    } else {
        1_000_000_000u64 / p99_us
    };

    let row = json!({
        "schema_version": "ft.perf.evidence-sample.v1",
        "ts_ms": now_ms(),
        "claim_id": "RQ-S1.resize_fps",
        "metric_name": "p99_frame_time",
        "metric_value": p99_us,
        "metric_unit": "us",
        "sample_size": sample_count,
        "target": "p99_frame_time <= 16.6ms",
        "target_p99_frame_time_us": P99_FRAME_BUDGET_US,
        "estimated_p99_fps_milli": estimated_p99_fps_milli,
        "p50_frame_time_us": p50_us,
        "p95_frame_time_us": p95_us,
        "p99_frame_time_us": p99_us,
        "within_target": p99_us <= P99_FRAME_BUDGET_US,
        "source_bench": "crates/frankenterm-core/benches/resize_storm.rs",
        "structured_log": EVIDENCE_PATH,
        "workload_class": "200-pane-continuous-5s-resize",
        "panes": PANE_COUNT,
        "frames_per_gesture": FRAMES_PER_GESTURE,
        "warmup_frames": WARMUP_FRAMES,
        "runner": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "hostname": std::env::var("HOSTNAME").ok(),
            "rch_worker_id": std::env::var("RCH_WORKER_ID").ok(),
            "cargo_target_dir": std::env::var("CARGO_TARGET_DIR").ok()
        },
        "tags": {
            "bead": "ft-tf6g3.3.7",
            "renderer_slo_state": "measured_or_degraded_by_assertion",
            "retained_release_artifact_required": "true"
        }
    });

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open resize FPS evidence file");
    writeln!(file, "{row}").expect("write resize FPS evidence row");
    println!("[BENCH] resize_fps_evidence={}", path.display());
}

fn bench_resize_storm(c: &mut Criterion) {
    let mut group = c.benchmark_group("renderer_slo/resize_fps");
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("200_panes_continuous_5s_resize", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let iter_capacity = usize::try_from(iters)
                .unwrap_or(usize::MAX)
                .saturating_mul(usize::try_from(FRAMES_PER_GESTURE - WARMUP_FRAMES).unwrap_or(0));
            let mut samples = Vec::with_capacity(iter_capacity);

            for _outer in 0..iters {
                let outer_start = Instant::now();
                for frame in 0..FRAMES_PER_GESTURE {
                    let frame_start = Instant::now();
                    drive_resize_frame(frame);
                    let elapsed_us =
                        u64::try_from(frame_start.elapsed().as_micros()).unwrap_or(u64::MAX);
                    if frame >= WARMUP_FRAMES {
                        samples.push(elapsed_us);
                    }
                }
                total += outer_start.elapsed();
            }

            let mut p50_samples = samples.clone();
            let mut p95_samples = samples.clone();
            let mut p99_samples = samples;
            let p50 = percentile_us(&mut p50_samples, 50);
            let p95 = percentile_us(&mut p95_samples, 95);
            let p99 = percentile_us(&mut p99_samples, 99);

            append_evidence_row(p50, p95, p99, p99_samples.len());
            assert!(
                p99 <= P99_FRAME_BUDGET_US,
                "resize_storm: p99 frame time exceeded {} us target. p50={} us p95={} us p99={} us samples={}",
                P99_FRAME_BUDGET_US,
                p50,
                p95,
                p99,
                p99_samples.len()
            );

            total
        });
    });

    group.finish();
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

criterion_group!(benches, bench_resize_storm);
criterion_main!(benches);
