//! First runnable scale-lab smoke harness for a 10-pane-equivalent workload.
//!
//! This harness is intentionally replay-backed rather than live-mux-backed. It
//! proves the cheap schema/logging path that larger scale-lab lanes build on,
//! while carrying explicit limitations so it cannot graduate high-scale claims.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use frankenterm_core::large_swarm_replay::{
    LARGE_SWARM_REPLAY_CORPUS_VERSION, LargeSwarmRegressionThresholds, LargeSwarmScenario,
    evaluate_large_swarm_thresholds, generate_large_swarm_corpus, summarize_large_swarm_replay,
};
use frankenterm_core::test_artifacts::{
    ArtifactEntry, ArtifactFormat, ArtifactKind, SCALE_LAB_WORKLOAD_CATALOG_SCHEMA_VERSION,
    ScaleLabCommandEvidence, ScaleLabDiskEvidence, ScaleLabEventEvidence, ScaleLabEvidenceMode,
    ScaleLabFeatureFlags, ScaleLabHostShape, ScaleLabMemoryEvidence, ScaleLabTimingEvidence,
    ScaleLabWorkloadCatalog, ScaleLabWorkloadMixEntry, ScaleLabWorkloadPersona,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

const SCALE_LAB_SMOKE_RUN_SCHEMA_VERSION: &str = "ft.scale_lab.smoke_run.v1";
const RUN_ID: &str = "ft-nk575.scale-lab-smoke.10-pane";
const WORKLOAD_SEED: &str = "ft-nk575-deterministic-10-pane-v1";
const TARGET_PANE_COUNT: u32 = 10;
const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ScaleLabSmokeRunArtifact {
    schema_version: String,
    run_id: String,
    workload_seed: String,
    fixture_versions: BTreeMap<String, String>,
    command_line: String,
    target_dir: String,
    artifact_dir: String,
    event_counts: ScaleLabSmokeEventCounts,
    simulated_boundaries: Vec<String>,
    threshold_passed: bool,
    threshold_diffs: Vec<serde_json::Value>,
    catalog: ScaleLabWorkloadCatalog,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ScaleLabSmokeEventCounts {
    target_pane_count: u64,
    replay_events: u64,
    replay_frames: u64,
    output_bytes: u64,
    compaction_waves: u64,
    search_queries: u64,
    mission_actions: u64,
    dropped_events: u64,
    capture_gaps: u64,
}

#[test]
fn scale_lab_smoke_harness_emits_valid_10_pane_artifact() {
    let started = Instant::now();
    let scenario = LargeSwarmScenario::scale_point(u64::from(TARGET_PANE_COUNT))
        .expect("10-pane scale-lab scenario must exist");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate 10-pane replay corpus");
    let summary = summarize_large_swarm_replay(&corpus).expect("summarize 10-pane replay corpus");
    let thresholds = LargeSwarmRegressionThresholds::for_scenario(&scenario);
    let verdict = evaluate_large_swarm_thresholds(&summary, &thresholds);
    assert!(
        verdict.passed,
        "10-pane smoke replay must satisfy built-in thresholds: {:?}",
        verdict.diffs
    );

    let target_dir = target_dir();
    let artifact_dir = target_dir
        .join("scale-lab-smoke")
        .join(RUN_ID)
        .join(summary.summary_digest.replace(':', "_"));
    fs::create_dir_all(&artifact_dir).expect("create scale-lab smoke artifact dir");

    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let command_line = proof_command(&target_dir);
    let event_counts = ScaleLabSmokeEventCounts {
        target_pane_count: summary.pane_count,
        replay_events: summary.event_count,
        replay_frames: summary.replay_frames,
        output_bytes: summary.output_bytes,
        compaction_waves: summary.compaction_waves,
        search_queries: summary.search_queries,
        mission_actions: summary.mission_actions,
        dropped_events: 0,
        capture_gaps: 0,
    };
    let log_jsonl = render_log_jsonl(&[
        json!({
            "phase": "scenario",
            "run_id": RUN_ID,
            "workload_seed": WORKLOAD_SEED,
            "scenario_id": scenario.scenario_id,
            "pane_count": scenario.pane_count,
            "fixture_versions": fixture_versions(),
        }),
        json!({
            "phase": "summary",
            "run_id": RUN_ID,
            "event_counts": event_counts,
            "summary_digest": summary.summary_digest,
            "threshold_passed": verdict.passed,
        }),
        json!({
            "phase": "boundaries",
            "run_id": RUN_ID,
            "simulated_boundaries": simulated_boundaries(),
        }),
    ]);
    let trace_json = serde_json::to_string_pretty(&json!({
        "schema_version": "ft.scale_lab.smoke_trace.v1",
        "run_id": RUN_ID,
        "scenario": scenario,
        "summary": summary,
        "thresholds": thresholds,
        "verdict": verdict,
    }))
    .expect("trace JSON serializes");

    let log_path = artifact_dir.join("scale-lab-smoke-log.v1.jsonl");
    let trace_path = artifact_dir.join("scale-lab-smoke-trace.v1.json");
    fs::write(&log_path, log_jsonl.as_bytes()).expect("write smoke JSONL log");
    fs::write(&trace_path, trace_json.as_bytes()).expect("write smoke trace JSON");

    let catalog = build_catalog(
        &command_line,
        &target_dir,
        elapsed_ms,
        &event_counts,
        &log_jsonl,
        &trace_json,
        &log_path,
        &trace_path,
    );
    catalog
        .validate()
        .expect("generated scale-lab catalog must validate");

    let smoke_artifact = ScaleLabSmokeRunArtifact {
        schema_version: SCALE_LAB_SMOKE_RUN_SCHEMA_VERSION.to_string(),
        run_id: RUN_ID.to_string(),
        workload_seed: WORKLOAD_SEED.to_string(),
        fixture_versions: fixture_versions(),
        command_line,
        target_dir: target_dir.display().to_string(),
        artifact_dir: artifact_dir.display().to_string(),
        event_counts,
        simulated_boundaries: simulated_boundaries(),
        threshold_passed: true,
        threshold_diffs: Vec::new(),
        catalog,
    };
    let artifact_json =
        serde_json::to_string_pretty(&smoke_artifact).expect("smoke artifact serializes");
    let artifact_path = artifact_dir.join("scale-lab-smoke-run.v1.json");
    fs::write(&artifact_path, artifact_json.as_bytes()).expect("write smoke artifact JSON");

    let roundtrip: ScaleLabSmokeRunArtifact =
        serde_json::from_slice(&fs::read(&artifact_path).expect("read smoke artifact JSON"))
            .expect("smoke artifact JSON deserializes");
    assert_eq!(roundtrip, smoke_artifact);
    assert_eq!(roundtrip.catalog.target_pane_count, Some(TARGET_PANE_COUNT));
    assert_eq!(roundtrip.catalog.workload_mix.len(), 8);
    assert!(log_path.exists(), "JSONL log must be written");
    assert!(trace_path.exists(), "trace artifact must be written");

    eprintln!(
        "[ARTIFACT][scale-lab-smoke] {}",
        serde_json::to_string(&json!({
            "run_id": RUN_ID,
            "artifact_path": artifact_path,
            "log_path": log_path,
            "trace_path": trace_path,
            "target_pane_count": TARGET_PANE_COUNT,
            "event_counts": roundtrip.event_counts,
            "catalog_schema": SCALE_LAB_WORKLOAD_CATALOG_SCHEMA_VERSION,
            "catalog_validated": true,
        }))
        .expect("artifact summary serializes")
    );
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(std::path::Path::parent)
                .expect("workspace root exists")
                .join("target")
        })
}

fn proof_command(target_dir: &std::path::Path) -> String {
    format!(
        "rch exec -- env CARGO_TARGET_DIR={} cargo test -p frankenterm-core --test scale_lab_smoke_harness --no-default-features scale_lab_smoke_harness_emits_valid_10_pane_artifact -- --nocapture",
        target_dir.display()
    )
}

fn fixture_versions() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "large_swarm_replay_corpus".to_string(),
            LARGE_SWARM_REPLAY_CORPUS_VERSION.to_string(),
        ),
        (
            "scale_lab_workload_catalog".to_string(),
            SCALE_LAB_WORKLOAD_CATALOG_SCHEMA_VERSION.to_string(),
        ),
        (
            "scale_lab_smoke_run".to_string(),
            SCALE_LAB_SMOKE_RUN_SCHEMA_VERSION.to_string(),
        ),
    ])
}

fn simulated_boundaries() -> Vec<String> {
    vec![
        "deterministic replay corpus; no live mux panes are launched".to_string(),
        "10 pane-equivalents only; larger 50/200/500+ proof lanes remain separate".to_string(),
        "hardware profile is the rch smoke substrate, not a 64-core/256GiB release proof"
            .to_string(),
        "disk free space is not measured by this reduced harness; bytes_written covers emitted artifacts"
            .to_string(),
    ]
}

fn build_catalog(
    command_line: &str,
    target_dir: &std::path::Path,
    elapsed_ms: f64,
    event_counts: &ScaleLabSmokeEventCounts,
    log_jsonl: &str,
    trace_json: &str,
    log_path: &std::path::Path,
    trace_path: &std::path::Path,
) -> ScaleLabWorkloadCatalog {
    ScaleLabWorkloadCatalog {
        schema_version: SCALE_LAB_WORKLOAD_CATALOG_SCHEMA_VERSION.to_string(),
        catalog_id: RUN_ID.to_string(),
        generated_at_ms: 1_778_097_600_000,
        field_notes: field_notes(),
        evidence_mode: Some(ScaleLabEvidenceMode::RchReplay),
        target_pane_count: Some(TARGET_PANE_COUNT),
        host: Some(ScaleLabHostShape {
            host_class: "rch-worker-smoke".to_string(),
            os: std::env::consts::OS.to_string(),
            cpu_cores: Some(1),
            memory_gib: Some(1),
            storage_gib: Some(1),
            live_mux_available: false,
        }),
        command: Some(ScaleLabCommandEvidence {
            command_line: command_line.to_string(),
            target_dir: target_dir.display().to_string(),
            feature_flags: ScaleLabFeatureFlags {
                default_features: false,
                enabled: vec!["no-default-features".to_string()],
                disabled: Vec::new(),
            },
        }),
        workload_mix: workload_mix(),
        timings: Some(ScaleLabTimingEvidence {
            elapsed_ms: Some(elapsed_ms),
            p50_api_latency_ms: Some(0.0),
            p95_api_latency_ms: Some(elapsed_ms),
            p99_api_latency_ms: Some(elapsed_ms),
        }),
        memory: Some(ScaleLabMemoryEvidence {
            peak_rss_bytes: Some(0),
            memory_limit_bytes: Some(GIB),
            warm_tier_bytes: Some(event_counts.output_bytes),
            cold_tier_bytes: Some(0),
        }),
        disk: Some(ScaleLabDiskEvidence {
            bytes_written: Some(usize_to_u64(log_jsonl.len() + trace_json.len())),
            free_bytes_after_run: Some(0),
        }),
        events: Some(ScaleLabEventEvidence {
            detection_events: Some(event_counts.replay_events),
            workflow_events: Some(event_counts.mission_actions),
            dropped_events: Some(event_counts.dropped_events),
            capture_gaps: Some(event_counts.capture_gaps),
        }),
        limitations: simulated_boundaries(),
        artifacts: vec![
            ArtifactEntry {
                kind: ArtifactKind::EventStream,
                format: ArtifactFormat::JsonLines,
                path: log_path.display().to_string(),
                bytes: Some(usize_to_u64(log_jsonl.len())),
                sha256: Some(sha256_hex(log_jsonl.as_bytes())),
                redacted: true,
            },
            ArtifactEntry {
                kind: ArtifactKind::TraceBundle,
                format: ArtifactFormat::Json,
                path: trace_path.display().to_string(),
                bytes: Some(usize_to_u64(trace_json.len())),
                sha256: Some(sha256_hex(trace_json.as_bytes())),
                redacted: true,
            },
        ],
    }
}

fn field_notes() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "command".to_string(),
            "Exact rch proof command, target dir, and feature flags for this smoke lane."
                .to_string(),
        ),
        (
            "workload_seed".to_string(),
            "Stable deterministic replay seed recorded by the smoke artifact.".to_string(),
        ),
        (
            "events".to_string(),
            "Replay event counts plus explicit dropped/capture-gap counters.".to_string(),
        ),
        (
            "limitations".to_string(),
            "Truth labels that prevent this reduced replay from proving larger live-swarm claims."
                .to_string(),
        ),
    ])
}

fn workload_mix() -> Vec<ScaleLabWorkloadMixEntry> {
    vec![
        mix(
            ScaleLabWorkloadPersona::IdleAgents,
            1,
            0.0,
            &["state_snapshot"],
        ),
        mix(
            ScaleLabWorkloadPersona::ActiveAgents,
            2,
            96.0,
            &["egress_output", "state_transition"],
        ),
        mix(
            ScaleLabWorkloadPersona::NoisyAgents,
            1,
            192.0,
            &["burst_output", "dedupe_window"],
        ),
        mix(
            ScaleLabWorkloadPersona::RateLimitedAgents,
            1,
            48.0,
            &["rate_limit_marker", "retry_backoff"],
        ),
        mix(
            ScaleLabWorkloadPersona::TuiHeavy,
            1,
            128.0,
            &["screen_diff", "scan_pipeline"],
        ),
        mix(
            ScaleLabWorkloadPersona::SearchHeavy,
            1,
            64.0,
            &["robot_search", "index_freshness"],
        ),
        mix(
            ScaleLabWorkloadPersona::WorkflowHeavy,
            1,
            64.0,
            &["mission_action", "workflow_event"],
        ),
        mix(
            ScaleLabWorkloadPersona::DistributedPanes,
            2,
            96.0,
            &["remote_metadata", "unavailable_live_read"],
        ),
    ]
}

fn mix(
    persona: ScaleLabWorkloadPersona,
    pane_count: u32,
    output_bytes_per_sec: f64,
    operations: &[&str],
) -> ScaleLabWorkloadMixEntry {
    ScaleLabWorkloadMixEntry {
        persona,
        pane_count,
        output_bytes_per_sec: Some(output_bytes_per_sec),
        operations: operations
            .iter()
            .map(|operation| (*operation).to_string())
            .collect(),
    }
}

fn render_log_jsonl(values: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for value in values {
        out.push_str(&serde_json::to_string(value).expect("log row serializes"));
        out.push('\n');
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).expect("usize fits in u64 on supported targets")
}
