//! JSON Schema conformance checks for the operating-envelope v1 contract.
//!
//! The fixtures are intentionally static contract examples. They do not execute
//! planner admission, Agent Mail, RCH, robot, or service mutation behavior.

use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Draft, Validator};
use serde_json::Value;

const SCHEMA_FILE: &str = "docs/json-schema/ft-operating-envelope.json";
const VALID_FIXTURES: &[&str] = &[
    "fixtures/operating-envelope/valid/agent-mail-unavailable.json",
    "fixtures/operating-envelope/valid/dirty-overlap.json",
    "fixtures/operating-envelope/valid/healthy.json",
    "fixtures/operating-envelope/valid/rch-no-worker.json",
    "fixtures/operating-envelope/valid/rch-topology-failure.json",
    "fixtures/operating-envelope/valid/target-hardware-skipped.json",
];
const INVALID_FIXTURES: &[&str] = &[
    "fixtures/operating-envelope/invalid/malformed-path.json",
    "fixtures/operating-envelope/invalid/missing-contract-id.json",
    "fixtures/operating-envelope/invalid/missing-field.json",
    "fixtures/operating-envelope/invalid/unknown-version.json",
];
const REQUIRED_INPUT_DOMAINS: &[&str] = &["capacity_resource", "rch", "beads", "agent_mail", "git"];
const REQUIRED_FORBIDDEN_ACTION_CLASSES: &[&str] = &[
    "agent_mail_repair",
    "build_cancellation",
    "local_cargo_proof",
    "raw_pane_content_capture",
    "raw_pane_content",
    "rch_daemon_restart",
    "service_restart",
    "pane_mutation",
    "service_mutation",
    "destructive_filesystem",
    "destructive_git",
    "worker_drain",
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn load_json(relative_path: &str) -> Value {
    let path = workspace_root().join(relative_path);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse {} as JSON: {err}", path.display()))
}

fn load_validator() -> Validator {
    let schema = load_json(SCHEMA_FILE);
    Validator::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .expect("operating-envelope schema compiles as Draft 2020-12")
}

fn assert_valid(label: &str, validator: &Validator, value: &Value) {
    if let Err(errors) = validator.validate(value) {
        let messages = errors
            .map(|error| format!("{}: {}", error.instance_path, error))
            .collect::<Vec<_>>()
            .join("\n");
        panic!("{label} did not match operating-envelope schema:\n{messages}");
    }
}

fn assert_invalid(label: &str, validator: &Validator, value: &Value) {
    assert!(
        validator.validate(value).is_err(),
        "{label} unexpectedly matched operating-envelope schema"
    );
}

fn string_array<'a>(value: &'a Value, label: &str) -> Vec<&'a str> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("{label} must be an array"))
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| panic!("{label} entries must be strings"))
        })
        .collect()
}

fn assert_fail_closed_defaults(label: &str, value: &Value) {
    assert_eq!(value["contract_id"], "ft.operating_envelope.v1", "{label}");
    assert_eq!(value["schema_version"], 1, "{label}");
    assert_eq!(value["raw_pane_content_stored"], false, "{label}");
    assert_eq!(value["side_effect_policy"]["dry_run_only"], true, "{label}");
    assert_eq!(
        value["side_effect_policy"]["raw_pane_content_allowed"], false,
        "{label}"
    );
    assert_eq!(
        value["side_effect_policy"]["pane_mutation_allowed"], false,
        "{label}"
    );
    assert_eq!(
        value["side_effect_policy"]["service_mutation_allowed"], false,
        "{label}"
    );
    assert_eq!(
        value["side_effect_policy"]["destructive_actions_allowed"], false,
        "{label}"
    );
    assert_eq!(
        value["side_effect_policy"]["local_cargo_proof_allowed"], false,
        "{label}"
    );
    assert_eq!(
        value["redaction_policy"]["raw_pane_content_allowed"], false,
        "{label}"
    );
    assert_eq!(
        value["redaction_policy"]["secret_material_allowed"], false,
        "{label}"
    );

    let input_domains = value["input_domains"]
        .as_object()
        .unwrap_or_else(|| panic!("{label} input_domains must be an object"));
    for domain in REQUIRED_INPUT_DOMAINS {
        let source = input_domains
            .get(*domain)
            .unwrap_or_else(|| panic!("{label} missing required input domain {domain}"));
        assert_eq!(
            source["source_kind"], *domain,
            "{label} source_kind does not match input domain {domain}"
        );
        assert_eq!(source["redacted"], true, "{label} source is not redacted");
        assert_eq!(
            source["raw_pane_content_stored"], false,
            "{label} source stored raw pane content"
        );
        assert!(
            !source["reason_codes"]
                .as_array()
                .unwrap_or_else(|| panic!("{label} source reason_codes must be an array"))
                .is_empty(),
            "{label} source must carry at least one reason code"
        );
        assert!(
            !source["evidence"]
                .as_array()
                .unwrap_or_else(|| panic!("{label} source evidence must be an array"))
                .is_empty(),
            "{label} source must carry at least one evidence item"
        );
    }

    for window in value["admission_windows"]
        .as_array()
        .unwrap_or_else(|| panic!("{label} admission_windows must be an array"))
    {
        let forbidden = string_array(&window["forbidden_action_classes"], label);
        for required in REQUIRED_FORBIDDEN_ACTION_CLASSES {
            assert!(
                forbidden.contains(required),
                "{label} admission window missing forbidden action class {required}"
            );
        }
    }
}

#[test]
fn operating_envelope_valid_fixtures_match_schema() {
    let validator = load_validator();
    for fixture in VALID_FIXTURES {
        let value = load_json(fixture);
        assert_valid(fixture, &validator, &value);
        assert_fail_closed_defaults(fixture, &value);
    }
}

#[test]
fn operating_envelope_invalid_fixtures_are_rejected() {
    let validator = load_validator();
    for fixture in INVALID_FIXTURES {
        let value = load_json(fixture);
        assert_invalid(fixture, &validator, &value);
    }
}

#[test]
fn operating_envelope_degraded_cases_pin_reason_codes() {
    let cases = VALID_FIXTURES
        .iter()
        .map(|fixture| (*fixture, load_json(fixture)))
        .collect::<Vec<_>>();

    let expected = [
        (
            "envelope-agent-mail-unavailable",
            "agent_mail.unavailable_after_retry",
        ),
        ("envelope-rch-no-worker", "rch.no_workers_passed_health"),
        (
            "envelope-rch-topology-failure",
            "rch.topology_preflight_failed",
        ),
        ("envelope-dirty-overlap", "dirty_overlap.present"),
        (
            "envelope-target-hardware-skipped",
            "target_hardware.skipped_not_proven",
        ),
        (
            "envelope-target-hardware-skipped",
            "capacity.target_class_unproven",
        ),
    ];

    for (envelope_id, reason_code) in expected {
        let (_, value) = cases
            .iter()
            .find(|(_, value)| value["envelope_id"] == envelope_id)
            .unwrap_or_else(|| panic!("missing fixture for envelope_id {envelope_id}"));
        let reason_present = value["input_domains"]
            .as_object()
            .expect("input_domains must be an object")
            .values()
            .any(|source| {
                source["reason_codes"]
                    .as_array()
                    .expect("reason_codes must be an array")
                    .iter()
                    .any(|entry| entry == reason_code)
            });
        assert!(
            reason_present,
            "{envelope_id} missing source reason code {reason_code}"
        );
    }
}
