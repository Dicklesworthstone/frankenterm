//! Regression guards for ft-e87u6.2 attestation manifest deferred-slot semantics.

use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Draft, Validator};
use serde_json::{Value, json};

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn attestation_schema_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("attestations")
        .join("schema.json")
}

fn load_attestation_schema() -> Value {
    let path = attestation_schema_path();
    let bytes =
        fs::read(&path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("schema {} is not JSON: {err}", path.display()))
}

fn manifest_validator() -> Validator {
    let schema = load_attestation_schema();
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": schema.get("$defs").expect("attestation schema has $defs"),
        "$ref": "#/$defs/manifestPlaceholder"
    });
    Validator::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .unwrap_or_else(|err| panic!("manifest schema failed to compile: {err}"))
}

fn bundle_validator() -> Validator {
    let schema = load_attestation_schema();
    Validator::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .unwrap_or_else(|err| panic!("bundle schema failed to compile: {err}"))
}

fn validate(schema: &Validator, instance: &Value) -> Vec<String> {
    match schema.validate(instance) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|err| format!("{} at {}", err, err.instance_path))
            .collect(),
    }
}

fn base_manifest(slot: Value) -> Value {
    json!({
        "$schema": "./schema.json#/$defs/manifestPlaceholder",
        "required_categories": ["perf/headline-claims"],
        "slots": [slot]
    })
}

fn base_slot(path: Value) -> Value {
    json!({
        "category": "perf/headline-claims",
        "path": path,
        "media_type": "application/json",
        "produced_by_bead": "ft-syqcz.3",
        "description": "headline claims matrix"
    })
}

fn base_bundle(signature: Value) -> Value {
    json!({
        "schema_version": "1.0.0",
        "release": {
            "version": "0.2.0",
            "tag": "v0.2.0",
            "channel": "stable"
        },
        "generated_at": "2026-05-12T00:00:00Z",
        "generator": {
            "name": "scripts/attestation-build.sh",
            "version": "1.2.0"
        },
        "git": {
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "tree": "89abcdef0123456789abcdef0123456789abcdef",
            "branch": "main"
        },
        "artifacts": [],
        "required_categories": [],
        "deferred_slots": [],
        "signature": signature
    })
}

#[test]
fn manifest_schema_accepts_resolved_and_deferred_slots() {
    let validator = manifest_validator();

    let resolved = base_manifest(base_slot(json!("docs/perf/headline-claims.json")));
    assert!(
        validate(&validator, &resolved).is_empty(),
        "resolved slot should validate"
    );

    let mut deferred_slot = base_slot(Value::Null);
    deferred_slot["deferred_to_bead"] = json!("ft-e87u6.9");
    deferred_slot["deferred_reason"] = json!("recovery bead will publish the JSON artifact");
    let deferred = base_manifest(deferred_slot);
    assert!(
        validate(&validator, &deferred).is_empty(),
        "deferred slot should validate"
    );
}

#[test]
fn manifest_schema_rejects_ambiguous_or_unfilled_slots() {
    let validator = manifest_validator();

    let mut both_set_slot = base_slot(json!("docs/perf/headline-claims.json"));
    both_set_slot["deferred_to_bead"] = json!("ft-e87u6.9");
    both_set_slot["deferred_reason"] = json!("path and deferred cannot both be set");
    let both_set_errors = validate(&validator, &base_manifest(both_set_slot));
    assert!(
        !both_set_errors.is_empty(),
        "slot with both path and deferred_to_bead should fail validation"
    );

    let both_null_errors = validate(&validator, &base_manifest(base_slot(Value::Null)));
    assert!(
        !both_null_errors.is_empty(),
        "slot with null path and no deferred_to_bead should fail validation"
    );
}

#[test]
fn checked_in_manifest_validates_against_deferred_slot_schema() {
    let validator = manifest_validator();
    let path = workspace_root()
        .join("docs")
        .join("attestations")
        .join("manifest.json");
    let manifest = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let manifest: Value = serde_json::from_str(&manifest)
        .unwrap_or_else(|err| panic!("manifest {} is not JSON: {err}", path.display()));
    let errors = validate(&validator, &manifest);
    assert!(
        errors.is_empty(),
        "checked-in manifest failed validation:\n{}",
        errors.join("\n")
    );
}

#[test]
fn bundle_schema_requires_hashed_sigstore_bundle_metadata() {
    let validator = bundle_validator();
    let valid = base_bundle(json!({
        "method": "sigstore-cosign-keyless",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "sigstore_bundle": {
            "path": "docs/attestations/0.2.0.sigstore",
            "sha256": "2222222222222222222222222222222222222222222222222222222222222222",
            "size_bytes": 4096
        },
        "certificate_identity": "https://github.com/frankensuite/frankenterm/.github/workflows/release.yml@refs/tags/v0.2.0",
        "certificate_oidc_issuer": "https://token.actions.githubusercontent.com"
    }));
    assert!(
        validate(&validator, &valid).is_empty(),
        "sigstore signature with hashed bundle metadata should validate"
    );

    let legacy_bundle_path_only = base_bundle(json!({
        "method": "sigstore-cosign-keyless",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "bundle_path": "docs/attestations/0.2.0.sigstore",
        "certificate_identity": "https://github.com/frankensuite/frankenterm/.github/workflows/release.yml@refs/tags/v0.2.0",
        "certificate_oidc_issuer": "https://token.actions.githubusercontent.com"
    }));
    assert!(
        !validate(&validator, &legacy_bundle_path_only).is_empty(),
        "sigstore signature without signature.sigstore_bundle must fail validation"
    );
}
