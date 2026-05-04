use frankenterm_core::hardware_profile::{
    CgroupProfile, CpuProfile, FileDescriptorProfile, HardwareProfileReport, HardwareProofStatus,
    HighScaleProofPredicates, MemoryProfile, NumaProfile, ProbeValue, StorageProfile,
};
use frankenterm_core::large_swarm_replay::{
    LARGE_SWARM_PROOF_GAUNTLET_VERSION, LARGE_SWARM_RELEASE_EVIDENCE_SCOREBOARD_VERSION,
    LARGE_SWARM_REPLAY_CORPUS_VERSION, LargeSwarmProofEvidenceMode, LargeSwarmProofGauntletConfig,
    LargeSwarmProofGauntletManifest, LargeSwarmProofGauntletStatus, LargeSwarmRegressionThresholds,
    LargeSwarmReleaseClaimStatus, LargeSwarmReplayCorpus, LargeSwarmScenario,
    build_large_swarm_proof_gauntlet_manifest_from_hardware,
    build_large_swarm_release_evidence_scoreboard, evaluate_large_swarm_thresholds,
    generate_large_swarm_corpus, large_swarm_release_claim_status,
    render_large_swarm_release_evidence_markdown, summarize_large_swarm_replay,
    summarize_required_scale_points, validate_large_swarm_release_evidence_scoreboard,
};
use frankenterm_core::recording::RECORDER_EVENT_SCHEMA_VERSION_V1;

#[test]
fn required_scale_points_cover_large_swarm_contract() {
    let pane_counts: Vec<u64> = LargeSwarmScenario::required_scale_points()
        .iter()
        .map(|scenario| scenario.pane_count)
        .collect();
    assert_eq!(pane_counts, vec![10, 50, 200, 1_000]);
}

#[test]
fn corpus_schema_roundtrips_json() {
    let scenario = LargeSwarmScenario::scale_point(10).expect("10-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");

    assert_eq!(corpus.version, LARGE_SWARM_REPLAY_CORPUS_VERSION);
    assert!(
        corpus
            .events
            .iter()
            .all(|event| event.schema_version == RECORDER_EVENT_SCHEMA_VERSION_V1)
    );

    let json = serde_json::to_string(&corpus).expect("serialize corpus");
    let roundtrip: LargeSwarmReplayCorpus =
        serde_json::from_str(&json).expect("deserialize corpus");
    assert_eq!(roundtrip, corpus);
}

#[test]
fn canonical_order_is_stable_after_reversal() {
    let scenario = LargeSwarmScenario::scale_point(50).expect("50-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
    let expected_ids: Vec<String> = corpus
        .events
        .iter()
        .map(|event| event.event_id.clone())
        .collect();

    let mut reversed = corpus.clone();
    reversed.events.reverse();
    reversed.canonicalize();
    let actual_ids: Vec<String> = reversed
        .events
        .iter()
        .map(|event| event.event_id.clone())
        .collect();

    assert_eq!(actual_ids, expected_ids);
}

#[test]
fn replay_summary_is_deterministic_for_1000_panes() {
    let scenario = LargeSwarmScenario::scale_point(1_000).expect("1000-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");

    let first = summarize_large_swarm_replay(&corpus).expect("first summary");
    let second = summarize_large_swarm_replay(&corpus).expect("second summary");

    assert_eq!(first, second);
    assert_eq!(first.pane_count, 1_000);
    assert_eq!(first.compaction_waves, 3_000);
    assert_eq!(first.search_queries, 50);
    assert_eq!(first.mission_actions, 30);
    assert_eq!(first.event_count, first.replay_frames);
    assert_eq!(first.collectors.latency_arrivals, first.event_count);
    assert_eq!(first.collectors.latency_completions, first.event_count);
    assert_eq!(first.collectors.admission_arrivals, 80);
    assert_eq!(first.collectors.admission_completions, 80);
    assert_eq!(first.collectors.memory_panes_registered, 1_000);
    assert_eq!(first.collectors.memory_samples, 1);
    assert_eq!(first.collectors.storage_events_appended, first.event_count);
    assert_eq!(first.collectors.storage_batches, 1);
    assert_eq!(first.collectors.storage_flushes, 1);
    assert!(first.summary_digest.starts_with("fnv1a64:"));
}

#[test]
fn all_required_scale_points_pass_default_thresholds() {
    for scenario in LargeSwarmScenario::required_scale_points() {
        let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
        let summary = summarize_large_swarm_replay(&corpus).expect("summary");
        let thresholds = LargeSwarmRegressionThresholds::for_scenario(&scenario);
        let verdict = evaluate_large_swarm_thresholds(&summary, &thresholds);

        assert!(
            verdict.passed,
            "scenario {} should pass thresholds: {:?}",
            scenario.scenario_id, verdict.diffs
        );
    }
}

#[test]
fn threshold_verdict_reports_explicit_diffs() {
    let scenario = LargeSwarmScenario::scale_point(10).expect("10-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
    let summary = summarize_large_swarm_replay(&corpus).expect("summary");
    let thresholds = LargeSwarmRegressionThresholds {
        max_event_count: summary.event_count - 1,
        max_duration_ms: summary.duration_ms,
        max_output_bytes: summary.output_bytes,
        max_events_per_pane: summary.max_events_per_pane,
    };

    let verdict = evaluate_large_swarm_thresholds(&summary, &thresholds);

    assert!(!verdict.passed);
    assert_eq!(verdict.diffs.len(), 1);
    assert_eq!(verdict.diffs[0].field, "event_count");
    assert!(verdict.diffs[0].message.contains("event_count"));
}

#[test]
fn summarize_required_scale_points_returns_deterministic_digest_set() {
    let first = summarize_required_scale_points().expect("first summaries");
    let second = summarize_required_scale_points().expect("second summaries");

    assert_eq!(first, second);
    assert_eq!(first.len(), 4);
    assert!(
        first
            .iter()
            .all(|summary| summary.summary_digest.starts_with("fnv1a64:"))
    );
}

#[test]
fn proof_gauntlet_high_scale_release_requests_required_core_and_memory_points() {
    let config = LargeSwarmProofGauntletConfig::high_scale_release("contract");

    let requested_cores: Vec<u64> = config
        .scale_requests
        .iter()
        .map(|request| request.requested_logical_cores)
        .collect();
    let requested_memory: Vec<u64> = config
        .scale_requests
        .iter()
        .map(|request| request.requested_memory_bytes)
        .collect();
    let requested_panes: Vec<u64> = config
        .scale_requests
        .iter()
        .map(|request| request.scenario.pane_count)
        .collect();

    assert_eq!(
        config.run_context.evidence_mode,
        LargeSwarmProofEvidenceMode::RealHardwareRun
    );
    assert_eq!(requested_cores, vec![1, 8, 16, 32, 64]);
    assert_eq!(
        requested_memory,
        vec![gib(1), gib(8), gib(32), gib(128), gib(256)]
    );
    assert_eq!(requested_panes, vec![10, 50, 200, 1_000, 1_000]);
}

#[test]
fn proof_gauntlet_synthetic_smoke_manifest_is_machine_readable_but_not_proof() {
    let config = LargeSwarmProofGauntletConfig::synthetic_smoke("local-smoke");
    let manifest =
        build_large_swarm_proof_gauntlet_manifest_from_hardware(high_scale_hardware(), config)
            .expect("build smoke manifest");

    assert_eq!(manifest.version, LARGE_SWARM_PROOF_GAUNTLET_VERSION);
    assert_eq!(
        manifest.status,
        LargeSwarmProofGauntletStatus::SkippedNotProven
    );
    assert_eq!(manifest.scale_artifacts.len(), 1);
    assert!(manifest.failure_reasons.is_empty());
    assert!(
        manifest
            .skip_reasons
            .iter()
            .any(|reason| reason.contains("synthetic smoke replay"))
    );
    assert!(
        manifest
            .skip_reasons
            .iter()
            .any(|reason| reason.contains("64-core / 256 GiB release point"))
    );
    assert!(manifest.summary_digest.starts_with("fnv1a64:"));

    let json = serde_json::to_string(&manifest).expect("serialize proof manifest");
    let roundtrip: LargeSwarmProofGauntletManifest =
        serde_json::from_str(&json).expect("deserialize proof manifest");
    assert_eq!(roundtrip, manifest);
}

#[test]
fn proof_gauntlet_fails_closed_when_hardware_predicates_are_missing() {
    let manifest = build_large_swarm_proof_gauntlet_manifest_from_hardware(
        insufficient_hardware(),
        LargeSwarmProofGauntletConfig::synthetic_smoke("insufficient")
            .with_evidence_mode(LargeSwarmProofEvidenceMode::RealHardwareRun),
    )
    .expect("build insufficient-hardware manifest");

    assert_eq!(
        manifest.status,
        LargeSwarmProofGauntletStatus::SkippedNotProven
    );
    assert!(manifest.failure_reasons.is_empty());
    assert!(
        manifest
            .skip_reasons
            .iter()
            .any(|reason| reason.contains("hardware predicates not met"))
    );
    assert!(
        manifest
            .scale_artifacts
            .iter()
            .all(|artifact| artifact.verdict.passed)
    );
}

#[test]
fn proof_gauntlet_status_is_proven_only_for_real_mode_and_passing_thresholds() {
    let manifest = build_large_swarm_proof_gauntlet_manifest_from_hardware(
        high_scale_hardware(),
        LargeSwarmProofGauntletConfig::high_scale_release("real-hardware-release"),
    )
    .expect("build real-mode manifest");

    assert_eq!(manifest.status, LargeSwarmProofGauntletStatus::Proven);
    assert!(manifest.skip_reasons.is_empty());
    assert!(manifest.failure_reasons.is_empty());
    assert_eq!(manifest.scale_artifacts.len(), 5);
}

#[test]
fn release_evidence_scoreboard_tracks_truth_status_transitions() {
    assert_eq!(
        large_swarm_release_claim_status(None),
        LargeSwarmReleaseClaimStatus::Planned
    );

    let smoke_manifest = build_large_swarm_proof_gauntlet_manifest_from_hardware(
        high_scale_hardware(),
        LargeSwarmProofGauntletConfig::synthetic_smoke("local-smoke"),
    )
    .expect("build smoke manifest");
    assert_eq!(
        large_swarm_release_claim_status(Some(&smoke_manifest)),
        LargeSwarmReleaseClaimStatus::LocalSmoke
    );

    let replay_only_manifest = build_large_swarm_proof_gauntlet_manifest_from_hardware(
        insufficient_hardware(),
        LargeSwarmProofGauntletConfig::high_scale_release("replay-only"),
    )
    .expect("build replay-only manifest");
    assert_eq!(
        large_swarm_release_claim_status(Some(&replay_only_manifest)),
        LargeSwarmReleaseClaimStatus::ReplayProven
    );

    let proven_manifest = build_large_swarm_proof_gauntlet_manifest_from_hardware(
        high_scale_hardware(),
        LargeSwarmProofGauntletConfig::high_scale_release("real-hardware-release"),
    )
    .expect("build proven manifest");
    assert_eq!(
        large_swarm_release_claim_status(Some(&proven_manifest)),
        LargeSwarmReleaseClaimStatus::RealHardwareProven
    );

    let mut simulated_manifest = smoke_manifest.clone();
    simulated_manifest.run_context.evidence_mode = LargeSwarmProofEvidenceMode::RealHardwareRun;
    simulated_manifest
        .failure_reasons
        .push("forced failure".into());
    simulated_manifest.scale_artifacts[0].verdict.passed = false;
    assert_eq!(
        large_swarm_release_claim_status(Some(&simulated_manifest)),
        LargeSwarmReleaseClaimStatus::Simulated
    );
}

#[test]
fn release_evidence_scoreboard_links_manifest_and_replay_artifacts() {
    let manifest = build_large_swarm_proof_gauntlet_manifest_from_hardware(
        high_scale_hardware(),
        LargeSwarmProofGauntletConfig::high_scale_release("release-evidence"),
    )
    .expect("build proven manifest");
    let scoreboard = build_large_swarm_release_evidence_scoreboard(Some(&manifest));

    assert_eq!(
        scoreboard.version,
        LARGE_SWARM_RELEASE_EVIDENCE_SCOREBOARD_VERSION
    );
    assert!(!scoreboard.release_blocked);
    assert!(scoreboard.summary_digest.starts_with("fnv1a64:"));
    assert_eq!(scoreboard.claims.len(), 1);
    assert_eq!(
        scoreboard.claims[0].status,
        LargeSwarmReleaseClaimStatus::RealHardwareProven
    );
    assert_eq!(scoreboard.claims[0].artifact_refs.len(), 6);
    assert!(
        scoreboard.claims[0]
            .artifact_refs
            .iter()
            .any(|artifact| artifact.digest == manifest.summary_digest)
    );
    for scale_artifact in &manifest.scale_artifacts {
        assert!(
            scoreboard.claims[0]
                .artifact_refs
                .iter()
                .any(|artifact| artifact.digest == scale_artifact.summary.summary_digest),
            "scoreboard should link replay summary {}",
            scale_artifact.summary.scenario_id
        );
    }

    validate_large_swarm_release_evidence_scoreboard(&scoreboard)
        .expect("proven scoreboard should validate");

    let json = serde_json::to_string(&scoreboard).expect("serialize scoreboard");
    let roundtrip: frankenterm_core::large_swarm_replay::LargeSwarmReleaseEvidenceScoreboard =
        serde_json::from_str(&json).expect("deserialize scoreboard");
    assert_eq!(roundtrip, scoreboard);
}

#[test]
fn release_evidence_scoreboard_renders_unsupported_claims_as_not_proven() {
    let manifest = build_large_swarm_proof_gauntlet_manifest_from_hardware(
        high_scale_hardware(),
        LargeSwarmProofGauntletConfig::synthetic_smoke("local-smoke"),
    )
    .expect("build smoke manifest");
    let scoreboard = build_large_swarm_release_evidence_scoreboard(Some(&manifest));
    let rendered = render_large_swarm_release_evidence_markdown(&scoreboard);

    assert!(scoreboard.release_blocked);
    assert!(rendered.contains("local-smoke"));
    assert!(rendered.contains("SKIPPED_NOT_PROVEN"));
    assert!(rendered.contains("local smoke evidence is parser/schema coverage"));
    assert!(!rendered.contains("| real-hardware-proven | real-hardware-proven |"));
}

#[test]
fn release_evidence_gate_rejects_proven_claim_without_manifest_artifacts() {
    let manifest = build_large_swarm_proof_gauntlet_manifest_from_hardware(
        high_scale_hardware(),
        LargeSwarmProofGauntletConfig::high_scale_release("release-evidence"),
    )
    .expect("build proven manifest");
    let mut scoreboard = build_large_swarm_release_evidence_scoreboard(Some(&manifest));
    scoreboard.claims[0].artifact_refs.clear();
    scoreboard.summary_digest = "fnv1a64:tampered".into();

    let error = validate_large_swarm_release_evidence_scoreboard(&scoreboard)
        .expect_err("proven claim without artifacts must fail the release gate");
    assert!(
        error
            .to_string()
            .contains("missing a proof-gauntlet manifest artifact"),
        "{error}"
    );
}

// ── br-ft-o60ul cc1 property-test slice ────────────────────────────

/// br-ft-o60ul: deterministic-summary contract MUST hold across
/// EVERY required scale point, not just the 1000-pane case the
/// existing `replay_summary_is_deterministic_for_1000_panes` test
/// pins. Property-style sweep: for each of {10, 50, 200, 1000}
/// panes, generate the corpus twice + summarize twice + assert the
/// two summaries are byte-identical (including the fnv1a64 digest).
/// Pre-fix the determinism guarantee was only pinned at the largest
/// scale; this test catches a regression at any scale point that
/// would have shipped silently.
#[test]
fn replay_summary_is_deterministic_at_every_scale_point_ft_o60ul() {
    for scenario in LargeSwarmScenario::required_scale_points() {
        let corpus_a =
            generate_large_swarm_corpus(&scenario).expect("first corpus generation must succeed");
        let corpus_b =
            generate_large_swarm_corpus(&scenario).expect("second corpus generation must succeed");
        assert_eq!(
            corpus_a, corpus_b,
            "ft-o60ul: corpus generation must be byte-identical across calls \
             (scale={}, scenario_id={})",
            scenario.pane_count, scenario.scenario_id
        );

        let summary_a =
            summarize_large_swarm_replay(&corpus_a).expect("first summary must succeed");
        let summary_b =
            summarize_large_swarm_replay(&corpus_b).expect("second summary must succeed");
        assert_eq!(
            summary_a, summary_b,
            "ft-o60ul: replay summary must be byte-identical across calls \
             (scale={}, scenario_id={})",
            scenario.pane_count, scenario.scenario_id
        );

        // The digest is the load-bearing diff anchor — pin its shape
        // (non-empty + fnv1a64: prefix) at every scale.
        assert!(
            summary_a.summary_digest.starts_with("fnv1a64:"),
            "ft-o60ul: summary_digest must use the fnv1a64 prefix at every scale \
             (scale={}, got={:?})",
            scenario.pane_count,
            summary_a.summary_digest
        );
        assert!(
            summary_a.summary_digest.len() > "fnv1a64:".len(),
            "ft-o60ul: summary_digest must have a non-empty hash portion \
             (scale={}, got={:?})",
            scenario.pane_count,
            summary_a.summary_digest
        );
    }
}

/// br-ft-o60ul: corpus serde roundtrip must hold at EVERY required
/// scale point. The existing `corpus_schema_roundtrips_json` test
/// pins this for the 10-pane case only; this property-style sweep
/// extends the contract to the larger scales where serde
/// performance + size characteristics may surface schema-shape
/// regressions that the small case wouldn't catch.
#[test]
fn corpus_schema_roundtrips_json_at_every_scale_point_ft_o60ul() {
    for scenario in LargeSwarmScenario::required_scale_points() {
        let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
        let json =
            serde_json::to_string(&corpus).expect("ft-o60ul: corpus serializes at every scale");
        let roundtrip: LargeSwarmReplayCorpus =
            serde_json::from_str(&json).expect("ft-o60ul: corpus deserializes at every scale");
        assert_eq!(
            roundtrip, corpus,
            "ft-o60ul: serde roundtrip must be lossless at scale={} (scenario={})",
            scenario.pane_count, scenario.scenario_id
        );
        // Schema-version pin: every event must declare the expected
        // V1 recorder schema regardless of scale.
        assert!(
            corpus
                .events
                .iter()
                .all(|event| event.schema_version == RECORDER_EVENT_SCHEMA_VERSION_V1),
            "ft-o60ul: all events at scale={} must use V1 schema",
            scenario.pane_count
        );
    }
}

/// br-ft-o60ul: every threshold violation must produce a diff with
/// a non-empty field name AND a non-empty message that cites the
/// field. Pins the "useful diffs when changed" acceptance criterion
/// via a universal-quantifier check across all 4 threshold fields.
/// Pre-fix a generic / empty diff message would slip through; this
/// test forces every field violation to carry actionable context
/// for operator triage.
#[test]
fn threshold_diffs_are_actionable_for_every_field_ft_o60ul() {
    let scenario = LargeSwarmScenario::scale_point(10).expect("10-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
    let summary = summarize_large_swarm_replay(&corpus).expect("summary");

    // Force ALL four thresholds below the actual value so every
    // field violates and produces a diff.
    let thresholds = LargeSwarmRegressionThresholds {
        max_event_count: summary.event_count.saturating_sub(1),
        max_duration_ms: summary.duration_ms.saturating_sub(1),
        max_output_bytes: summary.output_bytes.saturating_sub(1),
        max_events_per_pane: summary.max_events_per_pane.saturating_sub(1),
    };

    let verdict = evaluate_large_swarm_thresholds(&summary, &thresholds);

    assert!(
        !verdict.passed,
        "ft-o60ul: forcing all thresholds below actual must fail the verdict"
    );
    // Each diff must name its field + carry a non-empty message
    // citing the field name. Operators reading a regression report
    // rely on these for actionable triage.
    for diff in &verdict.diffs {
        assert!(
            !diff.field.is_empty(),
            "ft-o60ul: threshold diff must name its field (got empty)"
        );
        assert!(
            !diff.message.is_empty(),
            "ft-o60ul: threshold diff must carry a non-empty message (field={})",
            diff.field
        );
        assert!(
            diff.message.contains(&diff.field),
            "ft-o60ul: threshold diff message must cite the field name for triage \
             (field={}, message={})",
            diff.field,
            diff.message
        );
    }
}

fn high_scale_hardware() -> HardwareProfileReport {
    hardware_profile(64, gib(256), HardwareProofStatus::ProvenPredicateMet)
}

fn insufficient_hardware() -> HardwareProfileReport {
    hardware_profile(8, gib(32), HardwareProofStatus::SkippedNotProven)
}

fn hardware_profile(
    logical_cores: usize,
    memory_bytes: u64,
    proof_status: HardwareProofStatus,
) -> HardwareProfileReport {
    let proof_met = proof_status == HardwareProofStatus::ProvenPredicateMet;
    HardwareProfileReport {
        schema_version: 1,
        platform: "test".into(),
        cpu: CpuProfile {
            logical_cores: ProbeValue::known(logical_cores),
            physical_cores: ProbeValue::known(logical_cores / 2),
            topology_source: "test".into(),
        },
        memory: MemoryProfile {
            total_bytes: ProbeValue::known(memory_bytes),
            available_bytes: ProbeValue::known(memory_bytes / 2),
            source: "test".into(),
        },
        numa: NumaProfile {
            nodes: ProbeValue::known(vec![0, 1]),
            source: "test".into(),
        },
        page_size_bytes: ProbeValue::known(4096),
        file_descriptors: FileDescriptorProfile {
            nofile_soft: ProbeValue::known(65_536),
            nofile_hard: ProbeValue::known(65_536),
            current_open_fds: ProbeValue::known(128),
        },
        storage: StorageProfile {
            path: "/tmp/frankenterm-test".into(),
            total_bytes: ProbeValue::known(gib(1024)),
            available_bytes: ProbeValue::known(gib(512)),
            filesystem: ProbeValue::known("testfs".into()),
        },
        cgroup: CgroupProfile {
            memory_max_bytes: ProbeValue::unsupported("test"),
            cpu_quota: ProbeValue::unsupported("test"),
        },
        proof_predicates: HighScaleProofPredicates {
            required_logical_cores: 64,
            required_memory_bytes: gib(256),
            logical_cores_ok: proof_met,
            memory_ok: proof_met,
            proof_status,
            reason: if proof_met {
                "hardware predicates met: >= 64 logical cores and >= 256.0 GiB memory".into()
            } else {
                "hardware predicates not met or unverifiable: need >= 64 logical cores and >= 256.0 GiB memory".into()
            },
        },
        recommendations: Vec::new(),
    }
}

fn gib(value: u64) -> u64 {
    value * 1024 * 1024 * 1024
}
