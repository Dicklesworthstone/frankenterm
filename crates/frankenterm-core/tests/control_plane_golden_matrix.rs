//! Deterministic control-plane golden matrix for Robot and MCP envelopes.
//!
//! The fixture is intentionally a matrix rather than another one-off payload:
//! it ties control-plane family, transport, output format, scenario, expected
//! code, golden artifact, and proof command together so drift is caught where
//! operators actually consume the API.

#![allow(deprecated)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::robot_envelope::canonicalize_json;
use frankenterm_core::robot_family_contract::{
    checkpoint_family_contract, context_family_contract, fleet_family_contract,
    profile_family_contract, work_family_contract,
};
use jsonschema::{Draft, JSONSchema as Validator};
use serde::Deserialize;
use serde_json::Value;

const MATRIX_JSON: &str = include_str!("golden_robot_envelope/control_plane_golden_matrix.json");
const TARGET_DIR: &str = "CARGO_TARGET_DIR=/tmp/ft-bsfb9-5";

#[derive(Debug, Deserialize)]
struct GoldenMatrix {
    schema_version: u32,
    generated_by: String,
    proof_target: String,
    nondeterministic_fields: Vec<String>,
    entries: Vec<MatrixEntry>,
}

#[derive(Debug, Deserialize)]
struct MatrixEntry {
    id: String,
    surface: String,
    family: String,
    action: String,
    transport: String,
    format: String,
    scenario: String,
    status: String,
    #[serde(default)]
    expected_code: Option<String>,
    #[serde(default)]
    fixture: Option<String>,
    proof_command: String,
    #[serde(default)]
    envelope: Option<Value>,
    notes: String,
}

fn load_matrix() -> GoldenMatrix {
    serde_json::from_str(MATRIX_JSON).expect("control-plane golden matrix must be valid JSON")
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden_robot_envelope")
}

fn schema_path(name: &str) -> PathBuf {
    workspace_root().join("docs").join("json-schema").join(name)
}

fn load_schema(name: &str) -> Validator {
    let path = schema_path(name);
    let bytes = fs::read(&path)
        .unwrap_or_else(|err| panic!("failed to read schema {}: {err}", path.display()));
    let schema_json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("schema {} is not JSON: {err}", path.display()));
    Validator::options()
        .with_draft(Draft::Draft202012)
        .compile(&schema_json)
        .unwrap_or_else(|err| panic!("schema {} failed to compile: {err}", path.display()))
}

fn validation_errors(schema: &Validator, envelope: &Value) -> Vec<String> {
    match schema.validate(envelope) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|err| format!("{} at {}", err, err.instance_path))
            .collect(),
    }
}

fn envelope_from_fixture<'a>(fixture: &'a Value, entry: &MatrixEntry) -> (&'a Value, &'static str) {
    if fixture.get("ok").is_some() {
        return (fixture, "fixture root");
    }
    if let Some(envelope) = fixture.get("success_envelope") {
        return (envelope, "fixture success_envelope");
    }
    panic!(
        "{} fixture must expose either an envelope root or success_envelope",
        entry.id
    );
}

fn schema_for_entry<'a>(
    entry: &MatrixEntry,
    robot_schema: &'a Validator,
    mcp_schema: &'a Validator,
) -> &'a Validator {
    match entry.transport.as_str() {
        "robot" => robot_schema,
        "mcp" => mcp_schema,
        other => panic!(
            "{} references an envelope for unsupported transport {other}",
            entry.id
        ),
    }
}

fn assert_envelope_matches_entry(
    entry: &MatrixEntry,
    envelope: &Value,
    schema: &Validator,
    source: &str,
) {
    let mut canonical = envelope.clone();
    canonicalize_json(&mut canonical, None);
    assert_eq!(
        &canonical, envelope,
        "{} {source} must already be canonical and scrubbed",
        entry.id
    );

    let failures = validation_errors(schema, envelope);
    assert!(
        failures.is_empty(),
        "{} {source} failed schema validation:\n{}",
        entry.id,
        failures.join("\n")
    );

    match entry.status.as_str() {
        "ok" => assert_eq!(
            envelope.get("ok").and_then(Value::as_bool),
            Some(true),
            "{} {source} must be a success envelope",
            entry.id
        ),
        "error" => assert_eq!(
            envelope.get("ok").and_then(Value::as_bool),
            Some(false),
            "{} {source} must be an error envelope",
            entry.id
        ),
        "contract" => panic!("{} contract entries must not carry envelopes", entry.id),
        status => panic!("{} has unknown status {status}", entry.id),
    }

    if let Some(expected_code) = entry.expected_code.as_deref() {
        assert_eq!(
            envelope.get("error_code").and_then(Value::as_str),
            Some(expected_code),
            "{} {source} does not carry expected_code",
            entry.id
        );
    }
}

#[test]
fn matrix_metadata_is_current_and_rch_backed() {
    let matrix = load_matrix();
    assert_eq!(matrix.schema_version, 1);
    assert_eq!(
        matrix.generated_by,
        "ft-bsfb9.5-control-plane-golden-matrix"
    );
    assert!(
        matrix.proof_target.starts_with("rch exec -- env "),
        "top-level proof target must use rch: {}",
        matrix.proof_target
    );
    assert!(
        matrix.proof_target.contains(TARGET_DIR),
        "top-level proof target must preserve the bead target dir: {}",
        matrix.proof_target
    );
    assert!(
        !matrix.entries.is_empty(),
        "control-plane matrix must contain entries"
    );
}

#[test]
fn matrix_entries_are_unique_and_have_remote_proofs() {
    let matrix = load_matrix();
    let mut ids = BTreeSet::new();
    let mut keys = BTreeSet::new();

    for entry in &matrix.entries {
        assert!(ids.insert(entry.id.as_str()), "duplicate id {}", entry.id);
        assert!(
            keys.insert((
                entry.surface.as_str(),
                entry.transport.as_str(),
                entry.format.as_str(),
                entry.scenario.as_str(),
            )),
            "duplicate matrix key for {}",
            entry.id
        );
        assert!(
            entry.proof_command.starts_with("rch exec -- env "),
            "{} proof must use rch: {}",
            entry.id,
            entry.proof_command
        );
        assert!(
            entry.proof_command.contains(TARGET_DIR),
            "{} proof must preserve the bead target dir: {}",
            entry.id,
            entry.proof_command
        );
        assert!(
            entry.proof_command.contains(" cargo test "),
            "{} proof must be a concrete cargo test lane: {}",
            entry.id,
            entry.proof_command
        );
        assert!(!entry.notes.trim().is_empty(), "{} needs notes", entry.id);
        match entry.status.as_str() {
            "ok" | "contract" => assert!(
                entry.expected_code.is_none(),
                "{} cannot carry an error code when status={}",
                entry.id,
                entry.status
            ),
            "error" => assert!(
                entry.expected_code.is_some(),
                "{} must carry expected_code for an error scenario",
                entry.id
            ),
            status => panic!("{} has unknown status {status}", entry.id),
        }
    }
}

#[test]
fn matrix_covers_required_control_plane_surfaces_and_scenarios() {
    let matrix = load_matrix();

    let families: BTreeSet<&str> = matrix
        .entries
        .iter()
        .map(|entry| entry.family.as_str())
        .collect();
    for required in [
        "state",
        "get_text",
        "search",
        "events",
        "rules",
        "send",
        "profile",
        "checkpoint",
        "context",
        "work",
        "fleet",
    ] {
        assert!(
            families.contains(required),
            "matrix is missing required family {required}"
        );
    }

    let scenarios: BTreeSet<&str> = matrix
        .entries
        .iter()
        .map(|entry| entry.scenario.as_str())
        .collect();
    for required in [
        "healthy",
        "degraded",
        "blocked",
        "policy_required",
        "capability_unavailable",
        "unsupported",
    ] {
        assert!(
            scenarios.contains(required),
            "matrix is missing required scenario {required}"
        );
    }

    let transports: BTreeSet<&str> = matrix
        .entries
        .iter()
        .map(|entry| entry.transport.as_str())
        .collect();
    assert!(transports.contains("mcp"), "matrix must cover MCP");
    assert!(transports.contains("robot"), "matrix must cover Robot mode");

    let formats: BTreeSet<&str> = matrix
        .entries
        .iter()
        .map(|entry| entry.format.as_str())
        .collect();
    assert!(formats.contains("json"), "matrix must cover JSON");
    assert!(formats.contains("toon"), "matrix must cover TOON");
}

#[test]
fn matrix_covers_representative_contract_cases() {
    let matrix = load_matrix();

    assert!(
        matrix.entries.iter().any(|entry| entry.family == "send"
            && entry.action == "send_dry_run"
            && entry.transport == "mcp"
            && entry.status == "ok"),
        "matrix must pin an MCP send dry-run success envelope"
    );
    assert!(
        matrix.entries.iter().any(|entry| entry.family == "send"
            && entry.action == "send_dry_run"
            && entry.transport == "robot"
            && entry.status == "ok"),
        "matrix must pin a Robot CLI send dry-run success envelope"
    );
    assert!(
        matrix.entries.iter().any(|entry| entry.family == "send"
            && entry.scenario == "blocked"
            && entry.expected_code.as_deref() == Some("robot.policy_denied")),
        "matrix must pin policy-denied send as robot.policy_denied"
    );
    assert!(
        matrix
            .entries
            .iter()
            .any(|entry| entry.scenario == "policy_required"
                && entry.expected_code.as_deref() == Some("robot.require_approval")),
        "matrix must pin require-approval separately from hard denial"
    );
    assert!(
        matrix
            .entries
            .iter()
            .any(|entry| entry.scenario == "capability_unavailable"
                && entry.format == "toon"
                && entry.expected_code.as_deref() == Some("robot.fleet.capability_unavailable")),
        "matrix must pin capability-unavailable TOON parity"
    );
}

#[test]
fn matrix_scrubs_known_nondeterministic_fields() {
    let matrix = load_matrix();
    let fields: BTreeSet<&str> = matrix
        .nondeterministic_fields
        .iter()
        .map(String::as_str)
        .collect();

    for required in [
        "elapsed_ms",
        "now",
        "timestamp",
        "captured_at",
        "queried_at_ms",
        "path",
        "pid",
        "uuid",
        "worker_id",
        "duration_ms",
    ] {
        assert!(
            fields.contains(required),
            "matrix scrub list is missing {required}"
        );
    }
}

#[test]
fn matrix_referenced_golden_fixtures_exist() {
    let matrix = load_matrix();
    let dir = fixtures_dir();

    for entry in &matrix.entries {
        let Some(fixture) = &entry.fixture else {
            continue;
        };
        let path = dir.join(fixture);
        assert!(
            path.exists(),
            "{} references missing fixture {}",
            entry.id,
            path.display()
        );
    }
}

#[test]
fn matrix_embedded_envelopes_validate_against_transport_schema() {
    let matrix = load_matrix();
    let robot_schema = load_schema("wa-robot-envelope.json");
    let mcp_schema = load_schema("wa-mcp-envelope.json");

    let mut validated = 0usize;
    for entry in &matrix.entries {
        let Some(envelope) = &entry.envelope else {
            continue;
        };
        let schema = schema_for_entry(entry, &robot_schema, &mcp_schema);
        assert_envelope_matches_entry(entry, envelope, schema, "embedded envelope");
        validated += 1;
    }

    assert!(
        validated >= 5,
        "expected at least five embedded envelopes, got {validated}"
    );
}

#[test]
fn matrix_fixture_envelopes_validate_against_transport_schema() {
    let matrix = load_matrix();
    let dir = fixtures_dir();
    let robot_schema = load_schema("wa-robot-envelope.json");
    let mcp_schema = load_schema("wa-mcp-envelope.json");

    let mut validated = 0usize;
    for entry in &matrix.entries {
        let Some(fixture) = &entry.fixture else {
            continue;
        };
        let path = dir.join(fixture);
        let bytes = fs::read(&path)
            .unwrap_or_else(|err| panic!("failed to read fixture {}: {err}", path.display()));
        let fixture_json: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|err| panic!("fixture {} is not JSON: {err}", path.display()));
        let (envelope, source) = envelope_from_fixture(&fixture_json, entry);
        let schema = schema_for_entry(entry, &robot_schema, &mcp_schema);
        assert_envelope_matches_entry(entry, envelope, schema, source);
        validated += 1;
    }

    assert!(
        validated >= 4,
        "expected at least four fixture-backed envelopes, got {validated}"
    );
}

#[test]
fn matrix_tracks_all_schema_driven_ntm_family_actions() {
    let matrix = load_matrix();
    let contracts = [
        profile_family_contract(),
        checkpoint_family_contract(),
        work_family_contract(),
        fleet_family_contract(),
        context_family_contract(),
    ];

    let expected: BTreeSet<(String, String)> = contracts
        .iter()
        .flat_map(|contract| {
            contract
                .actions
                .iter()
                .map(|action| (contract.family_name.clone(), action.action.clone()))
        })
        .collect();

    let actual: BTreeSet<(String, String)> = matrix
        .entries
        .iter()
        .filter(|entry| entry.scenario == "contract_shape")
        .map(|entry| (entry.family.clone(), entry.action.clone()))
        .collect();

    assert_eq!(
        actual, expected,
        "control-plane matrix must track every schema-driven NTM family action"
    );
}
