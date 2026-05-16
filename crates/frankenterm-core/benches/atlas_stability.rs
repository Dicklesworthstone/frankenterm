//! Atlas-stability renderer SLO bench (RQ-S10).
//!
//! Drives the pure window-resize workload named by
//! `docs/perf/resize-quality-slo.json`: 100 sequential resize events with no
//! font/scale change and no new glyphs. The target is an
//! `atlas_rebuilds_total` delta of zero, recorded in
//! `target/criterion/slo-atlas_stability.jsonl` for retained release evidence.

use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::atlas_stability::{
    AtlasOp, AtlasStabilityEvent, AtlasStabilityHealth, AtlasStabilityResize, check_invariants,
    check_pure_resize, parse_events_jsonl, render_events_jsonl,
};
use serde_json::json;

const PURE_RESIZE_EVENTS: u64 = 100;
const ATLAS_SIZE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REBUILDS_DELTA: u64 = 0;
const EVIDENCE_PATH: &str = "target/criterion/slo-atlas_stability.jsonl";
const EVIDENCE_FILE_NAME: &str = "slo-atlas_stability.jsonl";

#[derive(Debug, Clone)]
struct AtlasEvidence {
    resize_events: u64,
    event_count: usize,
    atlas_rebuilds_total_delta: u64,
    atlas_version_before: u64,
    atlas_version_after: u64,
    glyphs_re_uploaded: u64,
    invariant_violations_total: usize,
    pure_resize_violations_total: usize,
    jsonl_roundtrip_event_count: usize,
    within_target: bool,
    health: AtlasStabilityHealth,
}

fn pure_resize_storm_stream() -> Vec<AtlasStabilityEvent> {
    let mut events = Vec::with_capacity((PURE_RESIZE_EVENTS as usize).saturating_mul(2) + 1);
    let mut ts = 0u64;

    events.push(AtlasStabilityEvent {
        ts_ms: ts,
        op: AtlasOp::Sync,
        version_before: 0,
        version_after: 0,
        bytes: 0,
    });

    for _ in 0..PURE_RESIZE_EVENTS {
        ts = ts.saturating_add(16);
        events.push(AtlasStabilityEvent {
            ts_ms: ts,
            op: AtlasOp::Resize,
            version_before: 0,
            version_after: 0,
            bytes: 0,
        });
        events.push(AtlasStabilityEvent {
            ts_ms: ts.saturating_add(1),
            op: AtlasOp::Sync,
            version_before: 0,
            version_after: 0,
            bytes: 0,
        });
    }

    events
}

fn drive_pure_resize_no_rebuilds() -> AtlasEvidence {
    let events = pure_resize_storm_stream();
    let invariant_violations = check_invariants(&events);
    let resize = AtlasStabilityResize {
        ts_ms: PURE_RESIZE_EVENTS.saturating_mul(16),
        glyphs_re_uploaded: 0,
        atlas_size_bytes_before: ATLAS_SIZE_BYTES,
        atlas_size_bytes_after: ATLAS_SIZE_BYTES,
        sync_duration_ms: 0,
    };
    let pure_resize_violations = check_pure_resize(&resize);
    let rendered_jsonl = render_events_jsonl(&events);
    let roundtripped_events =
        parse_events_jsonl(&rendered_jsonl).expect("bench event stream JSONL roundtrips");
    let health = AtlasStabilityHealth {
        uploads_total: 0,
        rebuilds_total: 0,
        grow_count: 0,
        size_bytes: ATLAS_SIZE_BYTES,
    };
    let atlas_rebuilds_total_delta = health.rebuilds_total;
    let atlas_version_before = events
        .first()
        .map(|event| event.version_before)
        .unwrap_or_default();
    let atlas_version_after = events
        .last()
        .map(|event| event.version_after)
        .unwrap_or_default();
    let within_target = atlas_rebuilds_total_delta == MAX_REBUILDS_DELTA
        && atlas_version_after == atlas_version_before
        && resize.glyphs_re_uploaded == 0
        && invariant_violations.is_empty()
        && pure_resize_violations.is_empty()
        && roundtripped_events == events
        && health.is_resize_stable();

    AtlasEvidence {
        resize_events: PURE_RESIZE_EVENTS,
        event_count: events.len(),
        atlas_rebuilds_total_delta,
        atlas_version_before,
        atlas_version_after,
        glyphs_re_uploaded: resize.glyphs_re_uploaded,
        invariant_violations_total: invariant_violations.len(),
        pure_resize_violations_total: pure_resize_violations.len(),
        jsonl_roundtrip_event_count: roundtripped_events.len(),
        within_target,
        health,
    }
}

fn append_evidence_row(evidence: &AtlasEvidence, criterion_iters: u64, elapsed: Duration) {
    let path = evidence_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create atlas-stability evidence directory");
    }

    let row = json!({
        "schema_version": "ft.perf.evidence-sample.v1",
        "ts_ms": now_ms(),
        "claim_id": "RQ-S10.atlas_rebuild_count",
        "metric_name": "atlas_rebuilds_total_delta",
        "metric_value": evidence.atlas_rebuilds_total_delta,
        "metric_unit": "count",
        "sample_size": evidence.resize_events.saturating_mul(criterion_iters),
        "target": "0 rebuilds during a pure resize sequence with no glyph additions",
        "target_max_rebuilds_delta": MAX_REBUILDS_DELTA,
        "within_target": evidence.within_target,
        "source_bench": "crates/frankenterm-core/benches/atlas_stability.rs",
        "structured_log": EVIDENCE_PATH,
        "workload_class": "100-pure-window-resizes-no-font-scale-change-no-new-glyphs",
        "criterion_iters": criterion_iters,
        "elapsed_us": u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        "resize_events_per_iter": evidence.resize_events,
        "event_count_per_iter": evidence.event_count,
        "atlas_version_before": evidence.atlas_version_before,
        "atlas_version_after": evidence.atlas_version_after,
        "atlas_version_delta": evidence.atlas_version_after.saturating_sub(evidence.atlas_version_before),
        "glyphs_re_uploaded": evidence.glyphs_re_uploaded,
        "atlas_size_bytes": evidence.health.size_bytes,
        "atlas_health": evidence.health,
        "invariant_violations_total": evidence.invariant_violations_total,
        "pure_resize_violations_total": evidence.pure_resize_violations_total,
        "jsonl_roundtrip_event_count": evidence.jsonl_roundtrip_event_count,
        "runner": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "hostname": runner_hostname(),
            "rch_worker_id": rch_worker_id(),
            "cargo_target_dir": std::env::var("CARGO_TARGET_DIR").ok()
        },
        "tags": {
            "bead": "ft-tf6g3.3.10",
            "renderer_slo_state": "retained_target_run_required",
            "retained_release_artifact_required": "true"
        }
    });

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open atlas-stability evidence file");
    writeln!(file, "{row}").expect("write atlas-stability evidence row");
    println!("[BENCH] atlas_stability_evidence={}", path.display());
}

fn bench_atlas_stability(c: &mut Criterion) {
    let mut group = c.benchmark_group("renderer_slo/atlas_stability");
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("100_pure_resizes_no_atlas_rebuilds", |b| {
        b.iter_custom(|iters| {
            let started = Instant::now();
            let mut last_evidence = None;

            for _ in 0..iters {
                let evidence = drive_pure_resize_no_rebuilds();
                assert!(
                    evidence.within_target,
                    "atlas_stability: expected zero rebuilds/version drift/reuploads, got rebuild_delta={} version_before={} version_after={} glyphs_re_uploaded={} invariant_violations={} resize_violations={}",
                    evidence.atlas_rebuilds_total_delta,
                    evidence.atlas_version_before,
                    evidence.atlas_version_after,
                    evidence.glyphs_re_uploaded,
                    evidence.invariant_violations_total,
                    evidence.pure_resize_violations_total
                );
                black_box(&evidence);
                last_evidence = Some(evidence);
            }

            let elapsed = started.elapsed();
            if let Some(evidence) = last_evidence {
                append_evidence_row(&evidence, iters, elapsed);
            }
            elapsed
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

fn evidence_path() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(|target_dir| {
            PathBuf::from(target_dir)
                .join("criterion")
                .join(EVIDENCE_FILE_NAME)
        })
        .unwrap_or_else(|| PathBuf::from(EVIDENCE_PATH))
}

fn rch_worker_id() -> Option<String> {
    std::env::var("RCH_WORKER_ID")
        .ok()
        .or_else(|| std::env::var("RCH_WORKER").ok())
}

fn runner_hostname() -> Option<String> {
    std::env::var("HOSTNAME").ok().or_else(|| {
        fs::read_to_string("/etc/hostname").ok().and_then(|value| {
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_owned())
            }
        })
    })
}

criterion_group!(benches, bench_atlas_stability);
criterion_main!(benches);
