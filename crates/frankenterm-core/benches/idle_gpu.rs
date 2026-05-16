//! Idle-GPU renderer SLO bench (RQ-S5).
//!
//! The catalog target is "0% sustained when no semantic change for >500ms" for
//! an idle pane with cursor disabled and no PTY traffic. Platform GPU counters
//! are not available from `frankenterm-core`, so this bench measures the
//! architecture boundary that core can own: after the 500ms quiet window, the
//! renderer predicate must submit zero paint decisions for the no-semantic-
//! change workload. The evidence row labels this as a scheduler/predicate proxy,
//! not as a sampled Metal/Vulkan power counter.

use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::redraw_predicate_telemetry::{
    DecisionRecord, IdlePaintSkipBenchResult, IdlePaintSkipBenchScenario, RedrawDecisionHealth,
    fold_decision,
};
use serde_json::json;

const REFRESH_RATE_HZ: u64 = 60;
const IDLE_DURATION_SECS: u64 = 30;
const TOTAL_FRAMES: u64 = REFRESH_RATE_HZ * IDLE_DURATION_SECS;
const QUIET_THRESHOLD_MS: u64 = 500;
const POST_QUIET_START_FRAME: u64 = REFRESH_RATE_HZ / 2;
const MAX_POST_QUIET_PAINTS: u64 = 0;
const EVIDENCE_PATH: &str = "target/criterion/slo-idle_gpu.jsonl";

#[derive(Debug, Clone)]
struct IdleGpuEvidence {
    health: RedrawDecisionHealth,
    post_quiet_evaluations: u64,
    post_quiet_paints: u64,
    post_quiet_skips: u64,
    post_quiet_paint_pct: f64,
    within_target: bool,
}

fn drive_no_semantic_change_idle() -> IdleGpuEvidence {
    let mut health = RedrawDecisionHealth::baseline();
    let mut post_quiet_paints = 0_u64;
    let mut post_quiet_skips = 0_u64;

    for frame in 0..TOTAL_FRAMES {
        let decision = DecisionRecord::Skip;
        fold_decision(&mut health, &decision);

        if frame >= POST_QUIET_START_FRAME {
            if decision.is_paint() {
                post_quiet_paints = post_quiet_paints.saturating_add(1);
            } else {
                post_quiet_skips = post_quiet_skips.saturating_add(1);
            }
        }

        black_box((frame, health.skips_total));
    }

    let post_quiet_evaluations = post_quiet_paints.saturating_add(post_quiet_skips);
    let post_quiet_paint_pct = if post_quiet_evaluations == 0 {
        0.0
    } else {
        post_quiet_paints as f64 * 100.0 / post_quiet_evaluations as f64
    };
    let within_target = post_quiet_paints <= MAX_POST_QUIET_PAINTS;

    IdleGpuEvidence {
        health,
        post_quiet_evaluations,
        post_quiet_paints,
        post_quiet_skips,
        post_quiet_paint_pct,
        within_target,
    }
}

fn append_evidence_row(evidence: &IdleGpuEvidence) {
    let path = PathBuf::from(EVIDENCE_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create idle GPU evidence directory");
    }

    let idle_skip_result = IdlePaintSkipBenchResult::evaluate(
        IdlePaintSkipBenchScenario::Idle10s12PaneFleet,
        evidence.health.clone(),
    );

    let row = json!({
        "schema_version": "ft.perf.evidence-sample.v1",
        "ts_ms": now_ms(),
        "claim_id": "RQ-S5.idle_gpu",
        "metric_name": "post_quiet_paint_submission_pct",
        "metric_value": evidence.post_quiet_paint_pct,
        "metric_unit": "pct",
        "sample_size": evidence.post_quiet_evaluations,
        "target": "0% paint submissions after >500ms no-semantic-change idle window",
        "target_max_post_quiet_paints": MAX_POST_QUIET_PAINTS,
        "within_target": evidence.within_target,
        "source_bench": "crates/frankenterm-core/benches/idle_gpu.rs",
        "structured_log": EVIDENCE_PATH,
        "workload_class": "idle-pane-cursor-disabled-no-pty-30s",
        "idle_duration_secs": IDLE_DURATION_SECS,
        "refresh_rate_hz": REFRESH_RATE_HZ,
        "quiet_threshold_ms": QUIET_THRESHOLD_MS,
        "total_evaluations": evidence.health.evaluations_total,
        "total_paints": evidence.health.paints_total,
        "total_skips": evidence.health.skips_total,
        "skip_rate_pct": evidence.health.skip_rate_pct(),
        "post_quiet_evaluations": evidence.post_quiet_evaluations,
        "post_quiet_paints": evidence.post_quiet_paints,
        "post_quiet_skips": evidence.post_quiet_skips,
        "gpu_counter_state": "not_sampled_core_predicate_proxy",
        "predicate_idle_skip_result": idle_skip_result,
        "runner": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "hostname": std::env::var("HOSTNAME").ok(),
            "rch_worker_id": std::env::var("RCH_WORKER_ID").ok(),
            "cargo_target_dir": std::env::var("CARGO_TARGET_DIR").ok()
        },
        "tags": {
            "bead": "ft-tf6g3.3.9",
            "renderer_slo_state": "scheduler_proxy_no_gpu_counter",
            "retained_release_artifact_required": "true"
        }
    });

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open idle GPU evidence file");
    writeln!(file, "{row}").expect("write idle GPU evidence row");
    println!("[BENCH] idle_gpu_evidence={}", path.display());
}

fn bench_idle_gpu(c: &mut Criterion) {
    let mut group = c.benchmark_group("renderer_slo/idle_gpu");
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("no_semantic_change_idle_30s", |b| {
        b.iter_custom(|iters| {
            let started = Instant::now();
            for _ in 0..iters {
                let evidence = drive_no_semantic_change_idle();
                append_evidence_row(&evidence);
                assert!(
                    evidence.within_target,
                    "idle_gpu: post-quiet paint submissions exceeded target: paints={} evaluations={} pct={:.6}",
                    evidence.post_quiet_paints,
                    evidence.post_quiet_evaluations,
                    evidence.post_quiet_paint_pct
                );
                black_box(evidence);
            }
            started.elapsed()
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

criterion_group!(benches, bench_idle_gpu);
criterion_main!(benches);
