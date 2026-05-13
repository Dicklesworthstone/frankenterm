//! Input-to-photon renderer SLO Criterion substrate.
//!
//! This bench intentionally writes a structured evidence row before Criterion
//! starts sampling. Criterion output proves timing regressions; the JSONL row
//! tells the release-attestation consumer whether the SLO was measured or
//! degraded because GPU/photon instrumentation was unavailable.

use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_gui::headless_render::render_headless;
use frankenterm_gui::renderer_slo::headless::{
    known_key_headless_input, trace_from_headless_frame,
};
use frankenterm_gui::renderer_slo::{
    INPUT_TO_PHOTON_CLAIM_ID, InputToPhotonState, summarize_input_to_photon_traces,
    unavailable_evidence,
};
use serde_json::json;

fn bench_known_key_input_to_headless_frame(c: &mut Criterion) {
    let input = known_key_headless_input();
    if render_headless(&input).is_err() {
        c.bench_function("input_to_photon/headless_unavailable_noop", |b| {
            b.iter(|| black_box(()));
        });
        return;
    }

    c.bench_function("input_to_photon/known_key_headless_frame", |b| {
        b.iter(|| {
            let frame = render_headless(black_box(&input))
                .expect("headless renderer must be available for measured Criterion run");
            black_box(frame.rgba.len());
        });
    });
}

fn bench_config() -> Criterion {
    emit_evidence_row();
    Criterion::default().configure_from_args()
}

fn emit_evidence_row() {
    let platform = std::env::consts::OS.to_string();
    let input = known_key_headless_input();
    let evidence = match render_headless(&input) {
        Ok(frame) => {
            let marker_started = Instant::now();
            let _ = trace_from_headless_frame(0, "a", platform.clone(), &frame, 0);
            let marker_overhead_us = u64::try_from(marker_started.elapsed().as_micros())
                .unwrap_or(u64::MAX)
                .max(1);
            let trace =
                trace_from_headless_frame(0, "a", platform.clone(), &frame, marker_overhead_us);
            summarize_input_to_photon_traces(platform.clone(), &[trace])
        }
        Err(error) if error.is_gpu_init_failed() => unavailable_evidence(
            platform.clone(),
            InputToPhotonState::PhotonDetectionUnavailable,
            error.to_string(),
        ),
        Err(error) => unavailable_evidence(
            platform.clone(),
            InputToPhotonState::InstrumentationUnavailable,
            error.to_string(),
        ),
    };

    let row = json!({
        "schema_version": "ft.perf.evidence-sample.v1",
        "ts_ms": now_ms(),
        "claim_id": INPUT_TO_PHOTON_CLAIM_ID,
        "metric_value": evidence.p95_us.map(|value| value as f64 / 1_000.0).unwrap_or(0.0),
        "metric_unit": "ms",
        "sample_size": evidence.sample_count.max(1),
        "commit_sha": option_env!("VERGEN_GIT_SHA"),
        "hardware_fingerprint": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "runner_sku": std::env::var("RUNNER_OS").unwrap_or_else(|_| std::env::consts::OS.to_string()),
        "workload_class": "known-key-headless-render",
        "tags": {
            "frankenterm_version": env!("CARGO_PKG_VERSION"),
            "renderer_slo_state": state_tag(evidence.state),
            "within_target": evidence.within_target.map(|value| value.to_string()).unwrap_or_else(|| "unknown".to_string())
        }
    });

    let evidence_path = evidence_path(&platform);
    if let Some(parent) = evidence_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&evidence_path)
    {
        let _ = writeln!(file, "{row}");
    }
    println!(
        "[BENCH] input_to_photon_evidence={}",
        evidence_path.display()
    );
}

fn evidence_path(platform: &str) -> PathBuf {
    let suffix = match platform {
        "macos" => "macos",
        "linux" => "wayland",
        other => other,
    };
    PathBuf::from(format!(
        "target/criterion/slo-input_to_photon_{suffix}.jsonl"
    ))
}

fn state_tag(state: InputToPhotonState) -> &'static str {
    match state {
        InputToPhotonState::Measured => "measured",
        InputToPhotonState::InstrumentationUnavailable => "instrumentation_unavailable",
        InputToPhotonState::PhotonDetectionUnavailable => "photon_detection_unavailable",
        InputToPhotonState::InstrumentationOverheadExceeded => "instrumentation_overhead_exceeded",
        InputToPhotonState::InvalidTrace => "invalid_trace",
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_known_key_input_to_headless_frame
);
criterion_main!(benches);
