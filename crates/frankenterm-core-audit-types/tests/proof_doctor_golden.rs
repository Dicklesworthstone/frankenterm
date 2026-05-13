use frankenterm_core_audit_types::proof_doctor::{
    ProofDoctorDirtyPath, ProofDoctorEvidence, ProofDoctorOwner, ProofDoctorPhase,
    ProofDoctorPreflightInput, ProofDoctorScaleLabArtifactEvidence, ProofDoctorToolVersionState,
    classify_proof_doctor,
};
use frankenterm_core_audit_types::proof_handoff::build_proof_handoff;
use frankenterm_core_audit_types::proof_lane::{
    ArtifactRetrievalStatus, ProofAttemptRecord, ProofBackend, ProofFindingSeverity,
    ProofRedactionStatus, ProofScope, validate_proof_record,
};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

const GENERATED_AT: &str = "2026-05-13T00:00:00Z";
const REPO_PATH: &str = "/Users/jemanuel/projects/frankenterm";
const TARGET_DIR: &str = "/tmp/ft-782hw-6-proof-doctor-golden-target";
const WORKER_ID: &str = "vmi-proof-golden";

struct GoldenCase {
    name: &'static str,
    input: ProofDoctorPreflightInput,
    redaction_status: ProofRedactionStatus,
}

#[test]
fn proof_doctor_handoff_and_ledger_goldens_match() {
    let actual: Vec<Value> = golden_cases()
        .into_iter()
        .map(|case| canonical_case_projection(&case))
        .collect();

    assert_matches_golden("expected.json", &json!(actual));
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("proof_doctor_golden")
}

fn assert_matches_golden(file_name: &str, actual: &Value) {
    let path = fixtures_dir().join(file_name);
    let actual_text = format!(
        "{}\n",
        serde_json::to_string_pretty(actual).expect("serialize golden projection")
    );

    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        fs::create_dir_all(path.parent().expect("fixture has parent"))
            .expect("create proof-doctor golden fixture dir");
        fs::write(&path, actual_text).expect("write proof-doctor golden fixture");
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing proof-doctor golden fixture {}: {err}\n\
             Regenerate only after reviewing the diff with:\n  \
             UPDATE_GOLDENS=1 cargo test -p frankenterm-core-audit-types \
             --test proof_doctor_golden -- --nocapture",
            path.display()
        )
    });
    assert_eq!(
        expected,
        actual_text,
        "proof-doctor golden drift in {}.\n\
         Review the structured diff before accepting it; then regenerate with:\n  \
         UPDATE_GOLDENS=1 cargo test -p frankenterm-core-audit-types \
         --test proof_doctor_golden -- --nocapture\n\nactual:\n{actual_text}",
        path.display()
    );
}

fn canonical_case_projection(case: &GoldenCase) -> Value {
    let verdict = classify_proof_doctor(&case.input);
    let handoff = build_proof_handoff(&verdict);
    assert_handoff_shape(&handoff.beads_comment);
    if let Some(mail) = &handoff.agent_mail {
        assert_agent_mail_shape(&mail.body_md);
    }
    let record = ProofAttemptRecord::from_proof_doctor_verdict(&verdict, case.redaction_status);
    let findings = validate_proof_record(&record);
    let error_count = findings
        .iter()
        .filter(|finding| finding.severity == ProofFindingSeverity::Error)
        .count();
    let warning_count = findings
        .iter()
        .filter(|finding| finding.severity == ProofFindingSeverity::Warning)
        .count();
    let finding_reason_codes = findings
        .iter()
        .map(|finding| finding.reason_code.as_str())
        .collect::<Vec<_>>();

    json!({
        "case": case.name,
        "handoff": {
            "schema_version": handoff.schema_version,
            "status": handoff.status,
            "phase": handoff.phase,
            "reason_code": handoff.reason_code,
            "owner": handoff.owner,
            "safe_to_close": handoff.safe_to_close,
            "beads_comment_prefix": handoff
                .beads_comment
                .split(" Verdict ")
                .next()
                .expect("handoff has status prefix"),
            "agent_mail": handoff.agent_mail.map(|mail| {
                json!({
                    "to": mail.to,
                    "subject": mail.subject,
                    "importance": mail.importance,
                })
            }),
        },
        "proof_record": canonical_record_projection(&record),
        "validation": {
            "error_count": error_count,
            "warning_count": warning_count,
            "reason_codes": finding_reason_codes,
        },
    })
}

fn canonical_record_projection(record: &ProofAttemptRecord) -> Value {
    json!({
        "schema_version": record.schema_version,
        "proof_id": scrub_text(&record.proof_id),
        "bead_id": record.bead_id,
        "parent_bead_id": record.parent_bead_id,
        "attempted_at_utc": scrub_text(&record.attempted_at_utc),
        "finished_at_utc": record.finished_at_utc.as_deref().map(scrub_text),
        "agent_name": record.agent_name,
        "cwd": scrub_text(&record.cwd),
        "command": record.command.iter().map(|arg| scrub_text(arg)).collect::<Vec<_>>(),
        "declared_target_dir": record.declared_target_dir.as_deref().map(scrub_text),
        "state": record.state,
        "reason_code": record.reason_code,
        "summary": scrub_text(&record.summary),
        "report_bucket": record.report_bucket(),
        "safe_to_close_source_bead": record.safe_to_close_source_bead(),
        "allows_high_scale_claim": record.allows_high_scale_claim(),
        "proof_scope": record.proof_scope,
        "required_backend": record.required_backend,
        "observed_backend": record.observed_backend,
        "rch_version": record.rch_version,
        "rch_config_fingerprint": record.rch_config_fingerprint,
        "selected_worker": record.selected_worker.as_deref().map(scrub_text),
        "worker_probe_artifact": record.worker_probe_artifact.as_deref().map(scrub_text),
        "sync_duration_ms": record.sync_duration_ms,
        "remote_command_duration_ms": record.remote_command_duration_ms,
        "wrapper_exit_code": record.wrapper_exit_code,
        "remote_exit_code": record.remote_exit_code,
        "remote_cargo_reached": record.remote_cargo_reached,
        "rustc_reached": record.rustc_reached,
        "test_binary_started": record.test_binary_started,
        "local_cargo_detected": record.local_cargo_detected,
        "artifact_retrieval_status": record.artifact_retrieval_status,
        "artifact_paths": record.artifact_paths.iter().map(|path| scrub_text(path)).collect::<Vec<_>>(),
        "hardware_predicate": record.hardware_predicate,
        "redaction_status": record.redaction_status,
        "claims_allowed": record.claims_allowed,
        "next_action": scrub_text(&record.next_action),
        "proof_doctor": record.proof_doctor.as_ref().map(|snapshot| {
            json!({
                "verdict_id": scrub_text(&snapshot.verdict_id),
                "status": snapshot.status,
                "phase": snapshot.phase,
                "reason_code": snapshot.reason_code,
                "blocker_kind": snapshot.blocker_kind,
                "tool_version_state": snapshot.tool_version_state,
                "remote_cargo_reached": snapshot.remote_cargo_reached,
                "affected_paths": snapshot.affected_paths.iter().map(|path| scrub_text(path)).collect::<Vec<_>>(),
                "operator_summary": scrub_text(&snapshot.operator_summary),
                "next_action": scrub_text(&snapshot.next_action),
            })
        }),
    })
}

fn assert_handoff_shape(comment: &str) {
    for required in [
        "Proof-doctor handoff for ",
        " Verdict ",
        "; phase ",
        "; reason ",
        "; remote Cargo ",
        "; RCH tool state ",
        "; owner ",
        "Command: `",
        "Affected paths: ",
        "Summary: ",
        "Next action: ",
    ] {
        assert!(
            comment.contains(required),
            "handoff comment missing required fragment {required:?}: {comment}"
        );
    }
}

fn assert_agent_mail_shape(body: &str) {
    for required in [
        "- Verdict:",
        "- Status:",
        "- Reason:",
        "- Remote Cargo:",
        "- RCH tool state:",
        "- Command:",
        "Summary:",
        "Next action:",
    ] {
        assert!(
            body.contains(required),
            "agent-mail handoff missing required fragment {required:?}: {body}"
        );
    }
}

fn scrub_text(value: &str) -> String {
    value
        .replace(GENERATED_AT, "<timestamp>")
        .replace(REPO_PATH, "<repo>")
        .replace(TARGET_DIR, "<target-dir>")
        .replace(WORKER_ID, "<worker>")
}

fn golden_cases() -> Vec<GoldenCase> {
    vec![
        GoldenCase {
            name: "remote_pass",
            input: remote_pass_input(),
            redaction_status: ProofRedactionStatus::NoneNeeded,
        },
        GoldenCase {
            name: "source_compile_failure",
            input: source_compile_failure_input(),
            redaction_status: ProofRedactionStatus::Redacted,
        },
        GoldenCase {
            name: "test_assertion_failure",
            input: test_assertion_failure_input(),
            redaction_status: ProofRedactionStatus::Redacted,
        },
        GoldenCase {
            name: "pre_cargo_infra_blocker",
            input: pre_cargo_infra_blocker_input(),
            redaction_status: ProofRedactionStatus::Unknown,
        },
        GoldenCase {
            name: "post_cargo_infra_blocker",
            input: post_cargo_infra_blocker_input(),
            redaction_status: ProofRedactionStatus::Unknown,
        },
        GoldenCase {
            name: "local_fallback_invalid",
            input: local_fallback_invalid_input(),
            redaction_status: ProofRedactionStatus::Unknown,
        },
        GoldenCase {
            name: "dirty_tree_blocker",
            input: dirty_tree_blocker_input(),
            redaction_status: ProofRedactionStatus::Unknown,
        },
        GoldenCase {
            name: "skipped_not_proven",
            input: skipped_not_proven_input(),
            redaction_status: ProofRedactionStatus::Redacted,
        },
    ]
}

fn base_input() -> ProofDoctorPreflightInput {
    ProofDoctorPreflightInput {
        bead_id: Some("ft-782hw.6".to_string()),
        parent_bead_id: Some("ft-782hw".to_string()),
        agent_name: "Codex".to_string(),
        repo_path: REPO_PATH.to_string(),
        git_head: "485e743b9".to_string(),
        branch: "main".to_string(),
        generated_at_utc: GENERATED_AT.to_string(),
        intended_command: direct_rch_command("proof_doctor_golden"),
        intended_target_dir: Some(TARGET_DIR.to_string()),
        intended_scope: ProofScope::CargoTest,
        required_backend: ProofBackend::Rch,
        phase: ProofDoctorPhase::TerminalClassified,
        proof_path_prefixes: vec!["crates/frankenterm-core-audit-types/src".to_string()],
        evidence: ProofDoctorEvidence {
            tool_version_state: ProofDoctorToolVersionState::InstalledCurrent,
            ..ProofDoctorEvidence::default()
        },
    }
}

fn direct_rch_command(filter: &str) -> Vec<String> {
    vec![
        "rch".to_string(),
        "exec".to_string(),
        "--".to_string(),
        "env".to_string(),
        format!("CARGO_TARGET_DIR={TARGET_DIR}"),
        "cargo".to_string(),
        "test".to_string(),
        "-p".to_string(),
        "frankenterm-core-audit-types".to_string(),
        filter.to_string(),
        "--".to_string(),
        "--nocapture".to_string(),
    ]
}

fn remote_pass_input() -> ProofDoctorPreflightInput {
    let mut input = base_input();
    input.evidence = complete_remote_evidence("remote_pass");
    input
}

fn source_compile_failure_input() -> ProofDoctorPreflightInput {
    let mut input = base_input();
    input.evidence = complete_remote_evidence("source_compile_failure");
    input.evidence.test_binary_started = false;
    input.evidence.remote_exit_code = Some(101);
    input.evidence.diagnostic_paths =
        vec!["crates/frankenterm-core-audit-types/src/proof_lane.rs".to_string()];
    input.evidence.diagnostic_summary =
        Some("missing field initializer in proof lane fixture".to_string());
    input
}

fn test_assertion_failure_input() -> ProofDoctorPreflightInput {
    let mut input = base_input();
    input.evidence = complete_remote_evidence("test_assertion_failure");
    input.evidence.remote_exit_code = Some(101);
    input.evidence.diagnostic_paths =
        vec!["crates/frankenterm-core-audit-types/src/proof_handoff.rs".to_string()];
    input.evidence.diagnostic_summary =
        Some("assertion failed in proof handoff fixture".to_string());
    input
}

fn pre_cargo_infra_blocker_input() -> ProofDoctorPreflightInput {
    let mut input = base_input();
    input.phase = ProofDoctorPhase::LaunchObserved;
    input.evidence.selected_worker = Some(WORKER_ID.to_string());
    input.evidence.sync_duration_ms = Some(1250);
    input.evidence.wrapper_exit_code = Some(127);
    input.evidence.artifact_retrieval_status = ArtifactRetrievalStatus::Partial;
    input.evidence.artifact_paths =
        vec!["tests/fixtures/proof_doctor_golden/pre-cargo-rch.log".to_string()];
    input
}

fn post_cargo_infra_blocker_input() -> ProofDoctorPreflightInput {
    let mut input = base_input();
    input.phase = ProofDoctorPhase::RemoteCargoObserved;
    input.evidence.selected_worker = Some(WORKER_ID.to_string());
    input.evidence.remote_cargo_reached = true;
    input.evidence.rustc_reached = true;
    input.evidence.wrapper_exit_code = Some(124);
    input.evidence.rch_failure_reason_code = Some("dep-info-loss-after-cargo-started".to_string());
    input.evidence.rch_failure_reason_detail =
        Some("dep-info sidecar disappeared after Cargo started".to_string());
    input.evidence.artifact_retrieval_status = ArtifactRetrievalStatus::Partial;
    input.evidence.artifact_paths =
        vec!["tests/fixtures/proof_doctor_golden/post-cargo-rch.log".to_string()];
    input
}

fn local_fallback_invalid_input() -> ProofDoctorPreflightInput {
    let mut input = base_input();
    input.phase = ProofDoctorPhase::Preflight;
    input.intended_command = vec![
        "cargo".to_string(),
        "test".to_string(),
        "-p".to_string(),
        "frankenterm-core-audit-types".to_string(),
        "proof_doctor_golden".to_string(),
    ];
    input.intended_target_dir = None;
    input.evidence.local_cargo_detected = true;
    input.evidence.artifact_retrieval_status = ArtifactRetrievalStatus::NotApplicable;
    input
}

fn dirty_tree_blocker_input() -> ProofDoctorPreflightInput {
    let mut input = base_input();
    input.phase = ProofDoctorPhase::Preflight;
    input.evidence.dirty_paths.push(ProofDoctorDirtyPath {
        path: "crates/frankenterm-core-audit-types/src/proof_lane.rs".to_string(),
        status: "M".to_string(),
        affects_proof: true,
        owner: Some(ProofDoctorOwner::Bead {
            bead_id: "ft-proof-owner".to_string(),
            assignee: Some("SageRobin".to_string()),
        }),
    });
    input
}

fn skipped_not_proven_input() -> ProofDoctorPreflightInput {
    let mut input = base_input();
    input.intended_scope = ProofScope::HighScale;
    input.evidence = complete_remote_evidence("skipped_not_proven");
    input.evidence.high_scale_predicate_met = Some(false);
    input.evidence.scale_lab_artifact = Some(ProofDoctorScaleLabArtifactEvidence {
        required: true,
        artifact_path: Some("tests/fixtures/proof_doctor_golden/scale-lab.json".to_string()),
        release_claim_status: Some("reduced_remote".to_string()),
        manifest_status: Some("proven".to_string()),
        evidence_mode: Some("real_hardware".to_string()),
        live_mux_available: Some(true),
        pane_scales: vec![50, 200, 500],
        max_requested_logical_cores: Some(32),
        max_requested_memory_bytes: Some(128 * 1024 * 1024 * 1024),
        ..ProofDoctorScaleLabArtifactEvidence::default()
    });
    input
}

fn complete_remote_evidence(case_name: &str) -> ProofDoctorEvidence {
    ProofDoctorEvidence {
        tool_version_state: ProofDoctorToolVersionState::InstalledCurrent,
        selected_worker: Some(WORKER_ID.to_string()),
        sync_duration_ms: Some(1250),
        remote_command_duration_ms: Some(2400),
        wrapper_exit_code: Some(0),
        remote_exit_code: Some(0),
        remote_cargo_reached: true,
        rustc_reached: true,
        test_binary_started: true,
        artifact_retrieval_status: ArtifactRetrievalStatus::Complete,
        artifact_paths: vec![format!(
            "tests/fixtures/proof_doctor_golden/{case_name}.summary.json"
        )],
        ..ProofDoctorEvidence::default()
    }
}
