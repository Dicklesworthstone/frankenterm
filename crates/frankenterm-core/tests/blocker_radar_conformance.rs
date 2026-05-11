//! Deterministic blocker-radar fixture and conformance matrix tests.
//!
//! The fixtures are intentionally semantic goldens: each case supplies scrubbed
//! collector input plus the expected v1 report states, action kinds, citations,
//! and artifact paths. Full reports are also validated against the public JSON
//! schema so field drift fails mechanically.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::blocker_radar::{
    BLOCKER_RADAR_CONTRACT_ID, BLOCKER_RADAR_SCHEMA_VERSION, BlockerRadarActionKind,
    BlockerRadarCollectorObservation, BlockerRadarEvidenceState, BlockerRadarFailureClass,
    BlockerRadarInput, BlockerRadarObservationStatus, BlockerRadarReport, BlockerRadarSourceKind,
    build_blocker_radar_report,
};
use jsonschema::{Draft, Validator};
use serde::Deserialize;
use serde_json::Value;

const MATRIX_JSON: &str = include_str!("fixtures/blocker_radar/conformance_cases.json");
const CLAIMABILITY_JSON: &str = include_str!("fixtures/blocker_radar/claimability_cases.json");
const TARGET_DIR: &str = "CARGO_TARGET_DIR=/tmp/ft-9ntud-4-blocker-radar-conformance";
const CLAIMABILITY_TARGET_DIR: &str = "CARGO_TARGET_DIR=/tmp/ft-htcwc-2-claimability-fixtures";

#[derive(Debug, Deserialize)]
struct ConformanceMatrix {
    schema_version: u16,
    generated_by: String,
    fixed_generated_at_ms: u64,
    proof_target: String,
    nondeterministic_fields: Vec<String>,
    requirements: Vec<Requirement>,
    cases: Vec<FixtureCase>,
}

#[derive(Debug, Deserialize)]
struct Requirement {
    id: String,
    level: String,
    evidence_state: BlockerRadarEvidenceState,
    covered_by: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FixtureCase {
    id: String,
    description: String,
    input: FixtureInput,
    expected: ExpectedReport,
}

#[derive(Debug, Deserialize)]
struct FixtureInput {
    generated_at_ms: u64,
    source: String,
    observations: Vec<FixtureObservation>,
    #[serde(default)]
    artifact_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FixtureObservation {
    source_id: String,
    source_kind: BlockerRadarSourceKind,
    status: BlockerRadarObservationStatus,
    command_or_api: String,
    summary: String,
    #[serde(default)]
    collected_at_ms: Option<u64>,
    #[serde(default)]
    freshness_ms: Option<u64>,
    #[serde(default)]
    reason_codes: Vec<String>,
    #[serde(default)]
    dependency_ids: Vec<String>,
    #[serde(default)]
    artifact_paths: Vec<String>,
    #[serde(default)]
    affected_paths: Vec<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    updated_at_ms: Option<u64>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    worker_id: Option<String>,
    #[serde(default)]
    artifact_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedReport {
    overall_state: BlockerRadarEvidenceState,
    source_states: Vec<BlockerRadarEvidenceState>,
    blocker_states: Vec<BlockerRadarEvidenceState>,
    external_queue_states: Vec<BlockerRadarEvidenceState>,
    active_agent_states: Vec<BlockerRadarEvidenceState>,
    dirty_paths: Vec<String>,
    unavailable_source_states: Vec<BlockerRadarEvidenceState>,
    unavailable_failure_classes: Vec<BlockerRadarFailureClass>,
    action_kinds: Vec<BlockerRadarActionKind>,
    required_reason_codes: Vec<String>,
    required_citation_ids: Vec<String>,
    artifact_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimabilityMatrix {
    schema_version: u16,
    generated_by: String,
    fixed_generated_at_ms: u64,
    proof_target: String,
    verdicts: Vec<ClaimabilityVerdictCoverage>,
    cases: Vec<ClaimabilityCase>,
}

#[derive(Debug, Deserialize)]
struct ClaimabilityVerdictCoverage {
    verdict: String,
    covered_by: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimabilityCase {
    id: String,
    description: String,
    source_commands: Vec<String>,
    input: ClaimabilityInput,
    expected: ExpectedClaimabilityVerdict,
}

#[derive(Debug, Deserialize)]
struct ClaimabilityInput {
    candidate_id: String,
    br_ready_ids: Vec<String>,
    br_show: ClaimabilityBrShow,
    bv_recommendation: ClaimabilityBvRecommendation,
    mail_state: String,
    dirty_paths: Vec<String>,
    external_state: String,
}

#[derive(Debug, Deserialize)]
struct ClaimabilityBrShow {
    status: String,
    assignee: Option<String>,
    dependencies: Vec<String>,
    fresh_comments: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimabilityBvRecommendation {
    status: String,
    reasons: Vec<String>,
    blocked_by: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedClaimabilityVerdict {
    final_verdict: String,
    supporting_verdicts: Vec<String>,
    reason_codes: Vec<String>,
    next_action: String,
    forbidden_actions: Vec<String>,
}

fn load_matrix() -> ConformanceMatrix {
    serde_json::from_str(MATRIX_JSON).expect("blocker-radar conformance matrix must be valid JSON")
}

fn load_claimability_matrix() -> ClaimabilityMatrix {
    serde_json::from_str(CLAIMABILITY_JSON).expect("claimability fixture matrix must be valid JSON")
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn schema_json() -> Value {
    let path = workspace_root()
        .join("docs")
        .join("json-schema")
        .join("ft-blocker-radar.json");
    let bytes = fs::read(&path)
        .unwrap_or_else(|err| panic!("failed to read schema {}: {err}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("schema {} is not JSON: {err}", path.display()))
}

fn load_schema() -> Validator {
    Validator::options()
        .with_draft(Draft::Draft202012)
        .build(&schema_json())
        .expect("ft-blocker-radar schema compiles as Draft 2020-12")
}

fn validation_errors(schema: &Validator, report: &Value) -> Vec<String> {
    match schema.validate(report) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|err| format!("{} at {}", err, err.instance_path))
            .collect(),
    }
}

fn build_report(case: &FixtureCase) -> BlockerRadarReport {
    let mut input = BlockerRadarInput::new(case.input.generated_at_ms, case.input.source.clone());
    for artifact_path in &case.input.artifact_paths {
        input = input.with_artifact_path(artifact_path.clone());
    }
    for observation in &case.input.observations {
        input = input.with_observation(observation.to_observation());
    }
    build_blocker_radar_report(&input)
}

impl FixtureObservation {
    fn to_observation(&self) -> BlockerRadarCollectorObservation {
        let mut observation = BlockerRadarCollectorObservation::new(
            self.source_id.clone(),
            self.source_kind,
            self.status,
            self.command_or_api.clone(),
            self.summary.clone(),
        );

        if let Some(collected_at_ms) = self.collected_at_ms {
            observation = observation.live(collected_at_ms, self.freshness_ms.unwrap_or(0));
        }
        for reason_code in &self.reason_codes {
            observation = observation.with_reason_code(reason_code.clone());
        }
        for dependency_id in &self.dependency_ids {
            observation = observation.with_dependency_id(dependency_id.clone());
        }
        for artifact_path in &self.artifact_paths {
            observation = observation.with_artifact_path(artifact_path.clone());
        }
        for affected_path in &self.affected_paths {
            observation = observation.with_affected_path(affected_path.clone());
        }
        if let Some(owner) = &self.owner {
            observation = observation.with_owner(owner.clone(), self.updated_at_ms);
        }
        if let Some(run_id) = &self.run_id {
            observation = observation.with_run_id(run_id.clone());
        }
        if let Some(url) = &self.url {
            observation = observation.with_url(url.clone());
        }
        if let Some(worker_id) = &self.worker_id {
            observation = observation.with_worker_id(worker_id.clone());
        }
        if let Some(artifact_name) = &self.artifact_name {
            observation = observation.with_artifact_name(artifact_name.clone());
        }

        observation
    }
}

fn source_states(report: &BlockerRadarReport) -> Vec<BlockerRadarEvidenceState> {
    report
        .sources
        .iter()
        .map(|source| source.evidence_state)
        .collect()
}

fn blocker_states(report: &BlockerRadarReport) -> Vec<BlockerRadarEvidenceState> {
    report
        .blockers
        .iter()
        .map(|blocker| blocker.evidence_state)
        .collect()
}

fn external_queue_states(report: &BlockerRadarReport) -> Vec<BlockerRadarEvidenceState> {
    report
        .external_queues
        .iter()
        .map(|queue| queue.evidence_state)
        .collect()
}

fn active_agent_states(report: &BlockerRadarReport) -> Vec<BlockerRadarEvidenceState> {
    report
        .active_agents
        .iter()
        .map(|agent| agent.evidence_state)
        .collect()
}

fn unavailable_source_states(report: &BlockerRadarReport) -> Vec<BlockerRadarEvidenceState> {
    report
        .unavailable_sources
        .iter()
        .map(|source| source.evidence_state)
        .collect()
}

fn unavailable_failure_classes(report: &BlockerRadarReport) -> Vec<BlockerRadarFailureClass> {
    report
        .unavailable_sources
        .iter()
        .map(|source| source.failure_class)
        .collect()
}

fn action_kinds(report: &BlockerRadarReport) -> Vec<BlockerRadarActionKind> {
    report
        .next_actions
        .iter()
        .map(|action| action.action_kind)
        .collect()
}

fn dirty_paths(report: &BlockerRadarReport) -> Vec<String> {
    report
        .dirty_overlap
        .iter()
        .map(|overlap| overlap.path.clone())
        .collect()
}

fn report_reason_codes(report: &BlockerRadarReport) -> BTreeSet<String> {
    report
        .sources
        .iter()
        .flat_map(|source| source.reason_codes.iter().cloned())
        .chain(
            report
                .next_actions
                .iter()
                .flat_map(|action| action.reason_codes.iter().cloned()),
        )
        .chain(
            report
                .unavailable_sources
                .iter()
                .flat_map(|source| source.reason_codes.iter().cloned()),
        )
        .collect()
}

fn report_citation_ids(report: &BlockerRadarReport) -> BTreeSet<String> {
    report
        .citations
        .iter()
        .map(|citation| citation.citation_id.clone())
        .collect()
}

fn all_report_states(report: &BlockerRadarReport) -> BTreeSet<BlockerRadarEvidenceState> {
    let mut states = BTreeSet::from([report.overall_state]);
    states.extend(source_states(report));
    states.extend(blocker_states(report));
    states.extend(external_queue_states(report));
    states.extend(active_agent_states(report));
    states.extend(unavailable_source_states(report));
    states
}

fn case_by_id<'a>(matrix: &'a ConformanceMatrix, id: &str) -> &'a FixtureCase {
    matrix
        .cases
        .iter()
        .find(|case| case.id == id)
        .unwrap_or_else(|| panic!("fixture case {id} exists"))
}

fn claimability_case_by_id<'a>(matrix: &'a ClaimabilityMatrix, id: &str) -> &'a ClaimabilityCase {
    matrix
        .cases
        .iter()
        .find(|case| case.id == id)
        .unwrap_or_else(|| panic!("claimability fixture case {id} exists"))
}

fn report_signature(report: &BlockerRadarReport) -> String {
    serde_json::json!({
        "overall_state": report.overall_state,
        "source_ids": report.sources.iter().map(|source| source.source_id.clone()).collect::<Vec<_>>(),
        "source_states": source_states(report),
        "blocker_states": blocker_states(report),
        "external_queue_states": external_queue_states(report),
        "active_agent_states": active_agent_states(report),
        "dirty_paths": dirty_paths(report),
        "unavailable_source_states": unavailable_source_states(report),
        "action_kinds": action_kinds(report),
        "reason_codes": report_reason_codes(report),
        "artifact_paths": report.artifact_paths,
    })
    .to_string()
}

fn assert_no_sensitive_text(label: &str, value: &Value) {
    let haystack = value.to_string().to_ascii_lowercase();
    for forbidden in [
        "begin private key",
        "sk-live",
        "sk-proj",
        "secret=",
        "password=",
        "raw pane transcript",
        "unredacted prompt",
    ] {
        assert!(
            !haystack.contains(forbidden),
            "{label} contains forbidden sensitive marker {forbidden}"
        );
    }
}

fn assert_read_only_source_command(case_id: &str, command: &str) {
    let normalized = command.to_ascii_lowercase();
    for forbidden in [
        "am service restart",
        "am service stop",
        "am doctor fix",
        "am doctor repair",
        "rch daemon restart",
        "git reset --hard",
        "git clean -fd",
        "rm -rf",
        "kill ",
    ] {
        assert!(
            !normalized.contains(forbidden),
            "{case_id} source command must not contain forbidden action {forbidden}: {command}"
        );
    }
    if normalized.contains("br update") {
        assert!(
            normalized.contains("--dry-run"),
            "{case_id} may only include br update as a dry-run source command: {command}"
        );
    }
}

fn assert_citations_are_complete(case_id: &str, report: &BlockerRadarReport) {
    let citation_ids = report_citation_ids(report);
    for source in &report.sources {
        let expected = format!("citation.{}", source.source_id);
        assert!(
            citation_ids.contains(&expected),
            "{case_id} source {} missing citation {}",
            source.source_id,
            expected
        );
    }
    for blocker in &report.blockers {
        for citation_id in &blocker.citation_ids {
            assert!(
                citation_ids.contains(citation_id),
                "{case_id} blocker {} references missing citation {}",
                blocker.blocker_id,
                citation_id
            );
        }
    }
    for action in &report.next_actions {
        for citation_id in &action.citation_ids {
            assert!(
                citation_ids.contains(citation_id),
                "{case_id} action {} references missing citation {}",
                action.action_id,
                citation_id
            );
        }
    }
}

fn assert_expected_report(case: &FixtureCase, report: &BlockerRadarReport) {
    let expected = &case.expected;
    assert_eq!(report.schema_version, BLOCKER_RADAR_SCHEMA_VERSION);
    assert_eq!(report.contract_id, BLOCKER_RADAR_CONTRACT_ID);
    assert_eq!(
        report.overall_state, expected.overall_state,
        "{} overall state drifted",
        case.id
    );
    assert_eq!(
        source_states(report),
        expected.source_states,
        "{} source states drifted",
        case.id
    );
    assert_eq!(
        blocker_states(report),
        expected.blocker_states,
        "{} blocker states drifted",
        case.id
    );
    assert_eq!(
        external_queue_states(report),
        expected.external_queue_states,
        "{} external queue states drifted",
        case.id
    );
    assert_eq!(
        active_agent_states(report),
        expected.active_agent_states,
        "{} active agent states drifted",
        case.id
    );
    assert_eq!(
        dirty_paths(report),
        expected.dirty_paths,
        "{} dirty overlap paths drifted",
        case.id
    );
    assert_eq!(
        unavailable_source_states(report),
        expected.unavailable_source_states,
        "{} unavailable source states drifted",
        case.id
    );
    assert_eq!(
        unavailable_failure_classes(report),
        expected.unavailable_failure_classes,
        "{} unavailable source failure classes drifted",
        case.id
    );
    assert_eq!(
        action_kinds(report),
        expected.action_kinds,
        "{} action kinds drifted",
        case.id
    );
    assert_eq!(
        report.artifact_paths, expected.artifact_paths,
        "{} artifact paths drifted",
        case.id
    );

    let reason_codes = report_reason_codes(report);
    for reason_code in &expected.required_reason_codes {
        assert!(
            reason_codes.contains(reason_code),
            "{} missing reason code {}",
            case.id,
            reason_code
        );
    }
    let citation_ids = report_citation_ids(report);
    for citation_id in &expected.required_citation_ids {
        assert!(
            citation_ids.contains(citation_id),
            "{} missing citation {}",
            case.id,
            citation_id
        );
    }

    assert!(
        report
            .next_actions
            .iter()
            .all(|action| !action.mutation_allowed),
        "{} must not emit mutating next actions",
        case.id
    );
    assert!(
        report.citations.iter().all(|citation| citation.redacted),
        "{} must keep all citations redacted",
        case.id
    );
    assert!(
        report.sources.iter().all(|source| source.redacted),
        "{} must keep all sources redacted",
        case.id
    );
    assert!(
        !report.raw_pane_content_stored,
        "{} must not store raw pane content",
        case.id
    );
    assert_citations_are_complete(&case.id, report);
}

#[test]
fn blocker_radar_conformance_matrix_metadata_is_remote_and_deterministic() {
    let matrix = load_matrix();
    assert_eq!(matrix.schema_version, 1);
    assert_eq!(matrix.generated_by, "ft-9ntud.4-blocker-radar-conformance");
    assert!(
        matrix.proof_target.starts_with("rch exec -- env "),
        "proof target must use rch: {}",
        matrix.proof_target
    );
    assert!(
        matrix.proof_target.contains(TARGET_DIR),
        "proof target must preserve isolated target dir: {}",
        matrix.proof_target
    );
    assert!(
        matrix.nondeterministic_fields.is_empty(),
        "blocker-radar fixtures must scrub nondeterminism"
    );
    assert!(
        !matrix.cases.is_empty(),
        "matrix must contain fixture cases"
    );

    let mut ids = BTreeSet::new();
    for case in &matrix.cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate case id {}",
            case.id
        );
        assert!(
            !case.description.trim().is_empty(),
            "{} needs a reviewer-facing description",
            case.id
        );
        assert_eq!(
            case.input.generated_at_ms, matrix.fixed_generated_at_ms,
            "{} must use the fixed generated_at_ms",
            case.id
        );
        assert!(
            case.input.source.starts_with("fixture.blocker_radar."),
            "{} must identify fixture provenance",
            case.id
        );
        for observation in &case.input.observations {
            if let Some(collected_at_ms) = observation.collected_at_ms {
                assert_eq!(
                    collected_at_ms, matrix.fixed_generated_at_ms,
                    "{} observation {} must use fixed collected_at_ms",
                    case.id, observation.source_id
                );
            }
            if let Some(updated_at_ms) = observation.updated_at_ms {
                assert!(
                    updated_at_ms <= matrix.fixed_generated_at_ms,
                    "{} observation {} must not use a future or current-wall timestamp",
                    case.id,
                    observation.source_id
                );
            }
        }
    }

    let fixture_value: Value = serde_json::from_str(MATRIX_JSON).expect("matrix parses as JSON");
    assert_no_sensitive_text("blocker-radar fixture matrix", &fixture_value);
}

#[test]
fn claimability_fixture_matrix_metadata_is_remote_and_deterministic() {
    let matrix = load_claimability_matrix();
    assert_eq!(matrix.schema_version, 1);
    assert_eq!(matrix.generated_by, "ft-htcwc.2-claimability-fixtures");
    assert!(
        matrix.proof_target.starts_with("rch exec -- env "),
        "claimability proof target must use rch: {}",
        matrix.proof_target
    );
    assert!(
        matrix.proof_target.contains(CLAIMABILITY_TARGET_DIR),
        "claimability proof target must preserve isolated target dir: {}",
        matrix.proof_target
    );
    assert_eq!(
        matrix.fixed_generated_at_ms, 1_770_000_000_001,
        "claimability fixtures should use the blocker-radar fixed timestamp"
    );
    assert!(
        !matrix.verdicts.is_empty(),
        "claimability matrix must define verdict coverage"
    );
    assert!(
        !matrix.cases.is_empty(),
        "claimability matrix must contain fixture cases"
    );

    let mut ids = BTreeSet::new();
    for case in &matrix.cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate claimability case id {}",
            case.id
        );
        assert!(
            !case.description.trim().is_empty(),
            "{} needs a reviewer-facing description",
            case.id
        );
        assert!(
            !case.input.candidate_id.trim().is_empty(),
            "{} must name the candidate or coordination snapshot",
            case.id
        );
        assert!(
            !case.source_commands.is_empty(),
            "{} must cite source commands",
            case.id
        );
        for command in &case.source_commands {
            assert_read_only_source_command(&case.id, command);
        }
        assert!(
            !case.input.br_show.status.trim().is_empty(),
            "{} must carry br_show status",
            case.id
        );
        assert!(
            !case.input.bv_recommendation.status.trim().is_empty(),
            "{} must carry bv recommendation status",
            case.id
        );
        assert!(
            !case.input.mail_state.trim().is_empty(),
            "{} must carry Agent Mail state",
            case.id
        );
        assert!(
            !case.input.external_state.trim().is_empty(),
            "{} must carry external substrate state",
            case.id
        );
        assert!(
            case.input
                .br_show
                .fresh_comments
                .iter()
                .all(|comment| !comment.trim().is_empty()),
            "{} fresh comments must not contain blank entries",
            case.id
        );
        assert!(
            case.input
                .bv_recommendation
                .reasons
                .iter()
                .all(|reason| !reason.trim().is_empty()),
            "{} BV reasons must not contain blank entries",
            case.id
        );
        assert!(
            case.input
                .bv_recommendation
                .blocked_by
                .iter()
                .all(|blocked_by| !blocked_by.trim().is_empty()),
            "{} blocked_by entries must not be blank",
            case.id
        );
        assert!(
            case.input
                .dirty_paths
                .iter()
                .all(|path| !path.trim().is_empty()),
            "{} dirty path entries must not be blank",
            case.id
        );
        assert!(
            !case.expected.reason_codes.is_empty(),
            "{} must carry reason codes",
            case.id
        );
        assert!(
            !case.expected.next_action.trim().is_empty(),
            "{} must name a next action",
            case.id
        );
        assert!(
            !case.expected.forbidden_actions.is_empty(),
            "{} must name forbidden actions",
            case.id
        );
    }

    let fixture_value: Value =
        serde_json::from_str(CLAIMABILITY_JSON).expect("claimability matrix parses as JSON");
    assert_no_sensitive_text("claimability fixture matrix", &fixture_value);
}

#[test]
fn claimability_fixture_matrix_covers_contract_verdicts_and_observed_mismatch() {
    let matrix = load_claimability_matrix();
    let case_ids = matrix
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<BTreeSet<_>>();
    let contract_verdicts = BTreeSet::from([
        "claimable",
        "no_ready",
        "dependency_blocked",
        "owner_blocked",
        "external_wait",
        "dirty_overlap",
        "mail_degraded",
        "tracker_inconsistent",
    ]);
    let coverage_verdicts = matrix
        .verdicts
        .iter()
        .map(|coverage| coverage.verdict.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        coverage_verdicts, contract_verdicts,
        "claimability fixture matrix must cover every ft-htcwc.1 verdict"
    );

    for coverage in &matrix.verdicts {
        assert!(
            !coverage.covered_by.is_empty(),
            "{} must name fixture coverage",
            coverage.verdict
        );
        for covered_by in &coverage.covered_by {
            assert!(
                case_ids.contains(covered_by.as_str()),
                "{} references missing claimability case {}",
                coverage.verdict,
                covered_by
            );
            let case = claimability_case_by_id(&matrix, covered_by);
            let mut verdicts = BTreeSet::from([case.expected.final_verdict.as_str()]);
            verdicts.extend(case.expected.supporting_verdicts.iter().map(String::as_str));
            assert!(
                verdicts.contains(coverage.verdict.as_str()),
                "{} says {} covers {}, but case verdicts were {:?}",
                coverage.verdict,
                covered_by,
                coverage.verdict,
                verdicts
            );
        }
    }

    let mismatch = claimability_case_by_id(&matrix, "bv_blocked_available_mismatch");
    assert_eq!(mismatch.input.candidate_id, "ft-e87u6.2");
    assert_eq!(mismatch.input.br_show.status, "blocked");
    assert_eq!(mismatch.input.br_show.assignee.as_deref(), Some("BluePike"));
    assert_eq!(mismatch.input.bv_recommendation.status, "blocked");
    assert!(
        mismatch
            .input
            .bv_recommendation
            .reasons
            .iter()
            .any(|reason| reason.contains("available for work")),
        "observed mismatch must preserve the misleading BV availability reason"
    );
    assert_eq!(mismatch.expected.final_verdict, "tracker_inconsistent");
    for verdict in ["owner_blocked", "external_wait", "mail_degraded"] {
        assert!(
            mismatch
                .expected
                .supporting_verdicts
                .iter()
                .any(|actual| actual == verdict),
            "mismatch case should retain supporting verdict {verdict}"
        );
    }
    for reason in [
        "bv.br_status_mismatch",
        "br.assignee_active",
        "github.current_head_queued",
        "agent_mail.degraded",
    ] {
        assert!(
            mismatch
                .expected
                .reason_codes
                .iter()
                .any(|actual| actual == reason),
            "mismatch case should retain reason code {reason}"
        );
    }

    let claimable = claimability_case_by_id(&matrix, "true_claimable");
    assert_eq!(claimable.expected.final_verdict, "claimable");
    assert!(
        claimable
            .input
            .br_ready_ids
            .iter()
            .any(|id| id == &claimable.input.candidate_id),
        "true claimable case must be present in br ready output"
    );
    assert_eq!(claimable.input.br_show.status, "open");
    assert!(claimable.input.br_show.assignee.is_none());
    assert!(claimable.input.br_show.dependencies.is_empty());
    assert!(claimable.input.dirty_paths.is_empty());
    assert_eq!(claimable.input.mail_state, "ok");
    assert_eq!(claimable.input.external_state, "none");
}

#[test]
fn blocker_radar_conformance_golden_cases_match_schema_and_expected_outputs() {
    let matrix = load_matrix();
    let schema = load_schema();

    for case in &matrix.cases {
        let report = build_report(case);
        assert_expected_report(case, &report);

        let report_json =
            serde_json::to_value(&report).expect("blocker-radar report serializes to JSON");
        let failures = validation_errors(&schema, &report_json);
        assert!(
            failures.is_empty(),
            "{} report failed ft-blocker-radar schema:\n{}",
            case.id,
            failures.join("\n")
        );
        assert_no_sensitive_text(&case.id, &report_json);
    }
}

#[test]
fn blocker_radar_conformance_matrix_covers_every_must_evidence_state() {
    let matrix = load_matrix();
    let case_ids = matrix
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<BTreeSet<_>>();
    let reports = matrix
        .cases
        .iter()
        .map(|case| (case.id.as_str(), build_report(case)))
        .collect::<BTreeMap<_, _>>();

    let schema_states = schema_json()
        .pointer("/$defs/evidence_state/enum")
        .and_then(Value::as_array)
        .expect("schema exposes evidence_state enum")
        .iter()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let requirement_states = matrix
        .requirements
        .iter()
        .filter(|requirement| requirement.level == "MUST")
        .map(|requirement| {
            serde_json::to_value(requirement.evidence_state)
                .expect("evidence state serializes")
                .as_str()
                .expect("evidence state serializes as string")
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        requirement_states, schema_states,
        "MUST conformance rows must cover the schema evidence_state enum"
    );

    for requirement in &matrix.requirements {
        assert_eq!(
            requirement.level, "MUST",
            "{} has unsupported level",
            requirement.id
        );
        assert!(
            !requirement.covered_by.is_empty(),
            "{} has no fixture coverage",
            requirement.id
        );
        for covered_by in &requirement.covered_by {
            assert!(
                case_ids.contains(covered_by.as_str()),
                "{} references missing case {}",
                requirement.id,
                covered_by
            );
            let states = all_report_states(&reports[covered_by.as_str()]);
            assert!(
                states.contains(&requirement.evidence_state),
                "{} claims {} covers {:?}, but report states were {:?}",
                requirement.id,
                covered_by,
                requirement.evidence_state,
                states
            );
        }
    }
}

#[test]
fn blocker_radar_conformance_fixtures_do_not_collapse_distinct_blockers() {
    let matrix = load_matrix();
    let signature = |id: &str| report_signature(&build_report(case_by_id(&matrix, id)));

    assert_ne!(
        signature("ci-queued"),
        signature("ci-zero-jobs"),
        "queued CI and zero-job CI must stay distinct"
    );
    assert_ne!(
        signature("rch-substrate-blocked"),
        signature("rch-local-fallback-refused"),
        "RCH substrate failure and local fallback refusal must stay distinct"
    );
    assert_ne!(
        signature("dirty-overlap"),
        signature("active-owner"),
        "dirty overlap must not collapse into active-owner waiting"
    );
    assert_ne!(
        signature("artifact-missing"),
        signature("ci-queued"),
        "missing artifacts must not collapse into queued CI"
    );
    assert_ne!(
        signature("mail-unavailable"),
        signature("mixed-degraded"),
        "Agent Mail unavailable must keep its dedicated fallback action"
    );
}
