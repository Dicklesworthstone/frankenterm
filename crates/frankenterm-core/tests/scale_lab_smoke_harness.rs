//! First runnable scale-lab smoke harness for a 10-pane-equivalent workload.
//!
//! This harness is intentionally replay-backed rather than live-mux-backed. It
//! proves the cheap schema/logging path that larger scale-lab lanes build on,
//! while carrying explicit limitations so it cannot graduate high-scale claims.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use frankenterm_core::hardware_profile::ProbeValue;
use frankenterm_core::large_swarm_replay::{
    LARGE_SWARM_PROOF_GAUNTLET_VERSION, LARGE_SWARM_REPLAY_CORPUS_VERSION,
    LargeSwarmProofEvidenceMode, LargeSwarmProofGauntletConfig, LargeSwarmProofGauntletManifest,
    LargeSwarmProofGauntletStatus, LargeSwarmProofRunContext, LargeSwarmProofScaleRequest,
    LargeSwarmRegressionThresholds, LargeSwarmReleaseClaimStatus, LargeSwarmScenario,
    build_large_swarm_proof_gauntlet_manifest, evaluate_large_swarm_thresholds,
    generate_large_swarm_corpus, large_swarm_release_claim_status, summarize_large_swarm_replay,
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
const SCALE_LAB_STAGED_PROOF_SCHEMA_VERSION: &str = "ft.scale_lab.staged_proof.v1";
const RUN_ID: &str = "ft-nk575.scale-lab-smoke.10-pane";
const STAGED_RUN_ID: &str = "ft-5kt3d.scale-lab-staged.50-200-1000-pane";
const WORKLOAD_SEED: &str = "ft-nk575-deterministic-10-pane-v1";
const STAGED_WORKLOAD_SEED: &str = "ft-5kt3d-deterministic-staged-v1";
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ScaleLabStagedProofArtifact {
    schema_version: String,
    run_id: String,
    workload_seed: String,
    command_line: String,
    target_dir: String,
    artifact_dir: String,
    manifest_schema_version: String,
    manifest_status: LargeSwarmProofGauntletStatus,
    release_claim_status: LargeSwarmReleaseClaimStatus,
    manifest: LargeSwarmProofGauntletManifest,
    stage_records: Vec<ScaleLabStageTruthRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ScaleLabStageTruthRecord {
    stage_id: String,
    stage_class: String,
    pane_count: u64,
    evidence_mode: LargeSwarmProofEvidenceMode,
    truth_status: LargeSwarmProofGauntletStatus,
    release_claim_status: LargeSwarmReleaseClaimStatus,
    requested_logical_cores: u64,
    requested_memory_bytes: u64,
    timeout_secs: u64,
    memory_limit_bytes: u64,
    disk_available_bytes: ProbeValue<u64>,
    queue_depth_upper_bound_events_per_pane: u64,
    event_drop_count: u64,
    capture_gap_count: u64,
    threshold_passed: bool,
    live_mux_required: bool,
    live_mux_available: bool,
    hardware_truth_labels: Vec<String>,
    degraded_reasons: Vec<String>,
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
    let command_line = proof_command(
        &target_dir,
        "scale_lab_smoke_harness_emits_valid_10_pane_artifact",
    );
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

#[test]
fn scale_lab_staged_lanes_mark_replay_unproven_without_live_hardware() {
    let target_dir = target_dir();
    let workspace_root = workspace_root();
    let command_line = proof_command(
        &target_dir,
        "scale_lab_staged_lanes_mark_replay_unproven_without_live_hardware",
    );
    let manifest = build_large_swarm_proof_gauntlet_manifest(
        &workspace_root,
        staged_scale_lab_config(STAGED_RUN_ID),
    )
    .expect("build staged scale-lab proof manifest");
    let release_claim_status = large_swarm_release_claim_status(Some(&manifest));

    assert_eq!(
        manifest.status,
        LargeSwarmProofGauntletStatus::SkippedNotProven,
        "synthetic staged lanes must not graduate hardware proof"
    );
    assert_eq!(
        release_claim_status,
        LargeSwarmReleaseClaimStatus::LocalSmoke
    );
    assert_eq!(manifest.scale_artifacts.len(), 3);
    assert!(
        manifest
            .scale_artifacts
            .iter()
            .all(|artifact| artifact.verdict.passed),
        "staged replay thresholds should pass before truth labels mark the run unproven"
    );
    assert!(
        manifest.skip_reasons.iter().any(|reason| reason
            .contains("synthetic smoke replay cannot prove the 64-core/256GB release claim")),
        "synthetic replay must carry an explicit release-proof blocker"
    );

    let stage_records = staged_truth_records(&manifest, release_claim_status);
    assert_eq!(stage_records.len(), 3);
    assert!(
        stage_records.iter().any(|record| record.pane_count >= 500),
        "future 500+ lane must be represented by the built-in 1000-pane scenario"
    );
    for record in &stage_records {
        assert_eq!(
            record.truth_status,
            LargeSwarmProofGauntletStatus::SkippedNotProven
        );
        assert_eq!(
            record.release_claim_status,
            LargeSwarmReleaseClaimStatus::LocalSmoke
        );
        assert!(record.threshold_passed);
        assert!(record.timeout_secs > 0);
        assert!(record.memory_limit_bytes > 0);
        assert_eq!(record.event_drop_count, 0);
        assert_eq!(record.capture_gap_count, 0);
        assert!(record.live_mux_required);
        assert!(!record.live_mux_available);
        assert!(
            record
                .hardware_truth_labels
                .iter()
                .any(|label| label == "evidence_mode=synthetic_smoke"),
            "stage must label synthetic evidence mode"
        );
        assert!(
            record
                .degraded_reasons
                .iter()
                .any(|reason| reason.contains("live mux")),
            "stage must explain the missing live-mux substrate"
        );
    }

    let artifact_dir = target_dir
        .join("scale-lab-smoke")
        .join(STAGED_RUN_ID)
        .join(manifest.summary_digest.replace(':', "_"));
    fs::create_dir_all(&artifact_dir).expect("create staged scale-lab artifact dir");
    let artifact = ScaleLabStagedProofArtifact {
        schema_version: SCALE_LAB_STAGED_PROOF_SCHEMA_VERSION.to_string(),
        run_id: STAGED_RUN_ID.to_string(),
        workload_seed: STAGED_WORKLOAD_SEED.to_string(),
        command_line,
        target_dir: target_dir.display().to_string(),
        artifact_dir: artifact_dir.display().to_string(),
        manifest_schema_version: LARGE_SWARM_PROOF_GAUNTLET_VERSION.to_string(),
        manifest_status: manifest.status,
        release_claim_status,
        manifest,
        stage_records,
    };
    let artifact_json =
        serde_json::to_string_pretty(&artifact).expect("staged artifact serializes");
    let artifact_path = artifact_dir.join("scale-lab-staged-proof.v1.json");
    fs::write(&artifact_path, artifact_json.as_bytes()).expect("write staged proof artifact JSON");
    let roundtrip: ScaleLabStagedProofArtifact =
        serde_json::from_slice(&fs::read(&artifact_path).expect("read staged artifact JSON"))
            .expect("staged artifact JSON deserializes");
    assert_eq!(roundtrip, artifact);

    eprintln!(
        "[ARTIFACT][scale-lab-staged] {}",
        serde_json::to_string(&json!({
            "run_id": STAGED_RUN_ID,
            "artifact_path": artifact_path,
            "manifest_status": roundtrip.manifest_status,
            "release_claim_status": roundtrip.release_claim_status,
            "stage_pane_counts": roundtrip
                .stage_records
                .iter()
                .map(|record| record.pane_count)
                .collect::<Vec<_>>(),
            "truth_labels": roundtrip
                .stage_records
                .iter()
                .flat_map(|record| record.hardware_truth_labels.iter().cloned())
                .collect::<Vec<_>>(),
        }))
        .expect("staged artifact summary serializes")
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

fn proof_command(target_dir: &std::path::Path, test_name: &str) -> String {
    format!(
        "rch exec -- env CARGO_TARGET_DIR={} cargo test -p frankenterm-core --test scale_lab_smoke_harness --no-default-features {test_name} -- --nocapture",
        target_dir.display(),
    )
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn staged_scale_lab_config(run_id: &str) -> LargeSwarmProofGauntletConfig {
    LargeSwarmProofGauntletConfig {
        run_context: LargeSwarmProofRunContext {
            run_id: run_id.to_string(),
            evidence_mode: LargeSwarmProofEvidenceMode::SyntheticSmoke,
            build_profile: "scale-lab-staged-replay".to_string(),
            kernel: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            ft_config_digest: "test-harness:unrecorded".to_string(),
            runtime_feature_flags: vec!["no-default-features".to_string()],
            agent_count: 1_000,
            input_rate_events_per_sec: 10_000,
            capture_rate_events_per_sec: 10_000,
            search_query_mix: BTreeMap::from([
                ("exact_tail_lookup".to_string(), 40),
                ("lexical_search".to_string(), 35),
                ("hybrid_search".to_string(), 25),
            ]),
        },
        scale_requests: vec![
            staged_scale_request(8, 8 * GIB, 50),
            staged_scale_request(16, 32 * GIB, 200),
            staged_scale_request(64, 256 * GIB, 1_000),
        ],
    }
}

fn staged_scale_request(
    requested_logical_cores: u64,
    requested_memory_bytes: u64,
    pane_count: u64,
) -> LargeSwarmProofScaleRequest {
    LargeSwarmProofScaleRequest {
        requested_logical_cores,
        requested_memory_bytes,
        scenario: LargeSwarmScenario::scale_point(pane_count)
            .expect("built-in staged scale point exists"),
    }
}

fn staged_truth_records(
    manifest: &LargeSwarmProofGauntletManifest,
    release_claim_status: LargeSwarmReleaseClaimStatus,
) -> Vec<ScaleLabStageTruthRecord> {
    manifest
        .scale_artifacts
        .iter()
        .map(|artifact| {
            let request = &artifact.request;
            ScaleLabStageTruthRecord {
                stage_id: format!("{}-pane", request.scenario.pane_count),
                stage_class: stage_class(request.scenario.pane_count).to_string(),
                pane_count: request.scenario.pane_count,
                evidence_mode: manifest.run_context.evidence_mode,
                truth_status: manifest.status,
                release_claim_status,
                requested_logical_cores: request.requested_logical_cores,
                requested_memory_bytes: request.requested_memory_bytes,
                timeout_secs: timeout_secs_for_stage(request.scenario.pane_count),
                memory_limit_bytes: request.requested_memory_bytes,
                disk_available_bytes: manifest.hardware_profile.storage.available_bytes.clone(),
                queue_depth_upper_bound_events_per_pane: artifact.summary.max_events_per_pane,
                event_drop_count: 0,
                capture_gap_count: 0,
                threshold_passed: artifact.verdict.passed,
                live_mux_required: true,
                live_mux_available: false,
                hardware_truth_labels: hardware_truth_labels(
                    manifest,
                    request.scenario.pane_count,
                    release_claim_status,
                ),
                degraded_reasons: degraded_reasons(manifest, request.scenario.pane_count),
            }
        })
        .collect()
}

fn stage_class(pane_count: u64) -> &'static str {
    match pane_count {
        50 => "rch-replay-50-pane",
        200 => "rch-replay-200-pane",
        count if count >= 500 => "future-500-plus-live-mux-required",
        _ => "unstaged",
    }
}

fn timeout_secs_for_stage(pane_count: u64) -> u64 {
    match pane_count {
        50 => 300,
        200 => 900,
        count if count >= 500 => 1_800,
        _ => 120,
    }
}

fn hardware_truth_labels(
    manifest: &LargeSwarmProofGauntletManifest,
    pane_count: u64,
    release_claim_status: LargeSwarmReleaseClaimStatus,
) -> Vec<String> {
    vec![
        format!(
            "evidence_mode={}",
            match manifest.run_context.evidence_mode {
                LargeSwarmProofEvidenceMode::SyntheticSmoke => "synthetic_smoke",
                LargeSwarmProofEvidenceMode::RealHardwareRun => "real_hardware_run",
            }
        ),
        format!("manifest_status={:?}", manifest.status),
        format!("release_claim_status={}", release_claim_status.as_str()),
        format!("pane_count={pane_count}"),
        format!(
            "hardware_predicate_status={:?}",
            manifest.hardware_profile.proof_predicates.proof_status
        ),
        "live_mux_available=false".to_string(),
    ]
}

fn degraded_reasons(manifest: &LargeSwarmProofGauntletManifest, pane_count: u64) -> Vec<String> {
    let mut reasons = manifest.skip_reasons.clone();
    reasons.push("live mux substrate was not launched by this replay harness".to_string());
    if pane_count >= 500 {
        reasons.push("500+ pane lane is replay-only until an operator stages live hardware".into());
    }
    reasons.sort();
    reasons.dedup();
    reasons
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
