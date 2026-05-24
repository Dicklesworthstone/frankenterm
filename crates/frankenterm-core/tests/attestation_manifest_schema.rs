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

fn retraction_validator() -> Validator {
    let schema = load_attestation_schema();
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": schema.get("$defs").expect("attestation schema has $defs"),
        "$ref": "#/$defs/attestationRetraction"
    });
    Validator::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .unwrap_or_else(|err| panic!("retraction schema failed to compile: {err}"))
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
        "taxonomy_coverage": base_taxonomy_coverage(),
        "confidence_summary": base_confidence_summary(),
        "signature": signature
    })
}

fn base_retraction(signature: Value) -> Value {
    json!({
        "schema_version": "1.0.0",
        "retracted_at": "2026-05-12T00:00:00Z",
        "retracted_by_release": "0.2.1",
        "retraction_rationale": "synthetic retraction signature regression",
        "affected_slot": "perf/headline-claims",
        "original_bundle_sha256": "4444444444444444444444444444444444444444444444444444444444444444",
        "original_claim_value": {
            "status": "claimed"
        },
        "corrected_claim_value": null,
        "retraction_signature": signature
    })
}

fn base_taxonomy_coverage() -> Value {
    json!({
        "schema_version": "1.0.0",
        "taxonomy_path": "docs/proof-taxonomy.json",
        "category_counts": [
            {
                "id": 5,
                "slug": "quantitative-attestation",
                "name": "Quantitative Attestation",
                "bridge_plan_core": true,
                "artifact_count": 1,
                "deferred_slot_count": 0,
                "below_threshold": false
            }
        ],
        "below_threshold_count": 0,
        "uncategorized_artifact_count": 0,
        "delta_from_prior_release": {
            "status": "no_prior_bundle",
            "category_deltas": []
        }
    })
}

fn base_confidence_record() -> Value {
    json!({
        "proof_id": "release-bundle.quantitative-attestation.best-confidence",
        "proof_category": 5,
        "claim": "Best available confidence for Quantitative Attestation in this release bundle.",
        "confidence_type": "frequentist",
        "confidence_value": {
            "status": "not_quantified",
            "reason": "Source artifact is attested by hash but does not yet publish a canonical numeric confidence record."
        },
        "sample_size_or_state_count": {
            "kind": "artifact_count",
            "value": 1,
            "unit": "delivered_artifacts"
        },
        "time_budget_consumed": {
            "seconds": 0,
            "budget_seconds": null,
            "status": "not_reported"
        },
        "methodology_url": "docs/proof-taxonomy.json#quantitative-attestation",
        "source_artifact_hash": "3333333333333333333333333333333333333333333333333333333333333333",
        "source_artifact_path": "docs/attestations/schema.json"
    })
}

fn base_confidence_summary() -> Value {
    let record = base_confidence_record();
    json!({
        "schema_version": "1.0.0",
        "schema_path": "docs/proofs/confidence-format-schema.json",
        "records": [record.clone()],
        "best_confidence_by_category": [record]
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
fn manifest_schema_rejects_parent_directory_paths() {
    let validator = manifest_validator();

    let mut traversal_slot = base_slot(json!("../perf/headline-claims.json"));
    traversal_slot["description"] = json!("synthetic path traversal");
    let errors = validate(&validator, &base_manifest(traversal_slot));
    assert!(
        !errors.is_empty(),
        "manifest slot paths must reject parent-directory traversal"
    );
}

#[test]
fn manifest_schema_rejects_blank_deferred_reasons() {
    let validator = manifest_validator();

    let mut deferred_slot = base_slot(Value::Null);
    deferred_slot["deferred_to_bead"] = json!("ft-e87u6.9");
    deferred_slot["deferred_reason"] = json!(" \t\n");
    let errors = validate(&validator, &base_manifest(deferred_slot));
    assert!(
        !errors.is_empty(),
        "manifest slot deferrals must carry a non-blank deferred_reason"
    );
}

#[test]
fn manifest_schema_rejects_malformed_bead_ids() {
    let validator = manifest_validator();

    let mut bad_producer = base_slot(json!("docs/perf/headline-claims.json"));
    bad_producer["produced_by_bead"] = json!("ft-.");
    assert!(
        !validate(&validator, &base_manifest(bad_producer)).is_empty(),
        "manifest slot produced_by_bead must reject malformed dotted IDs"
    );

    let mut bad_deferred = base_slot(Value::Null);
    bad_deferred["deferred_to_bead"] = json!("ft-e87u6.");
    bad_deferred["deferred_reason"] = json!("synthetic malformed bead ID");
    assert!(
        !validate(&validator, &base_manifest(bad_deferred)).is_empty(),
        "manifest slot deferred_to_bead must reject trailing-dot IDs"
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
fn checked_in_dev_bundle_validates_against_schema() {
    let validator = bundle_validator();
    let path = workspace_root()
        .join("docs")
        .join("attestations")
        .join("0.0.0-dev.json");
    let bundle = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let bundle: Value = serde_json::from_str(&bundle)
        .unwrap_or_else(|err| panic!("bundle {} is not JSON: {err}", path.display()));
    let errors = validate(&validator, &bundle);
    assert!(
        errors.is_empty(),
        "checked-in dev bundle failed validation:\n{}",
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

    let legacy_bundle_path_with_hashes = base_bundle(json!({
        "method": "sigstore-cosign-keyless",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "sigstore_bundle": {
            "path": "docs/attestations/0.2.0.sigstore",
            "sha256": "2222222222222222222222222222222222222222222222222222222222222222",
            "size_bytes": 4096
        },
        "bundle_path": "docs/attestations/0.2.0.sigstore",
        "certificate_identity": "https://github.com/frankensuite/frankenterm/.github/workflows/release.yml@refs/tags/v0.2.0",
        "certificate_oidc_issuer": "https://token.actions.githubusercontent.com"
    }));
    assert!(
        !validate(&validator, &legacy_bundle_path_with_hashes).is_empty(),
        "sigstore signature must reject legacy signature.bundle_path even when hashed metadata is present"
    );
}

#[test]
fn bundle_schema_rejects_unsafe_signature_paths() {
    let validator = bundle_validator();

    let unsafe_sigstore_path = base_bundle(json!({
        "method": "sigstore-cosign-keyless",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "sigstore_bundle": {
            "path": "../0.2.0.sigstore",
            "sha256": "2222222222222222222222222222222222222222222222222222222222222222",
            "size_bytes": 4096
        },
        "certificate_identity": "https://github.com/frankensuite/frankenterm/.github/workflows/release.yml@refs/tags/v0.2.0",
        "certificate_oidc_issuer": "https://token.actions.githubusercontent.com"
    }));
    assert!(
        !validate(&validator, &unsafe_sigstore_path).is_empty(),
        "sigstore bundle path must reject parent-directory traversal"
    );

    let unsafe_ed25519_path = base_bundle(json!({
        "method": "ed25519",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "signature_path": "../0.2.0.sig",
        "public_key": "3333333333333333333333333333333333333333333333333333333333333333"
    }));
    assert!(
        !validate(&validator, &unsafe_ed25519_path).is_empty(),
        "ed25519 signature_path must reject parent-directory traversal"
    );
}

#[test]
fn bundle_schema_rejects_unsafe_reference_paths() {
    let validator = bundle_validator();
    let valid = base_bundle(json!({
        "method": "unsigned",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "reason": "dev bundle tracked by ft-e87u6.2"
    }));

    let mut unsafe_taxonomy_path = valid.clone();
    unsafe_taxonomy_path["taxonomy_coverage"]["taxonomy_path"] = json!("../proof-taxonomy.json");
    assert!(
        !validate(&validator, &unsafe_taxonomy_path).is_empty(),
        "taxonomy_path must reject parent-directory traversal"
    );

    let mut unsafe_source_path = valid;
    unsafe_source_path["confidence_summary"]["records"][0]["source_artifact_path"] =
        json!("../schema.json");
    assert!(
        !validate(&validator, &unsafe_source_path).is_empty(),
        "confidence source_artifact_path must reject parent-directory traversal"
    );

    let mut unsafe_retraction_path = base_bundle(json!({
        "method": "unsigned",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "reason": "dev bundle tracked by ft-e87u6.2"
    }));
    unsafe_retraction_path["retractions"] = json!([
        {
            "original_bundle_sha256": "4444444444444444444444444444444444444444444444444444444444444444",
            "affected_slot": "perf/headline-claims",
            "retracted_at": "2026-05-12T00:00:00Z",
            "retracted_by_release": "0.2.1",
            "retraction_rationale": "synthetic unsafe path regression",
            "retraction_path": "../retractions/unsafe.json",
            "retraction_sha256": "5555555555555555555555555555555555555555555555555555555555555555",
            "size_bytes": 1,
            "corrected_claim_value": null
        }
    ]);
    assert!(
        !validate(&validator, &unsafe_retraction_path).is_empty(),
        "retraction_path must reject parent-directory traversal"
    );
}

#[test]
fn bundle_schema_rejects_blank_retraction_summary_metadata() {
    let validator = bundle_validator();
    for (surface, bundle) in [
        (
            "retracted_by_release",
            {
                let mut bundle = base_bundle(json!({
                    "method": "unsigned",
                    "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
                    "reason": "dev bundle tracked by ft-e87u6.2"
                }));
                bundle["retractions"] = json!([
                    {
                        "original_bundle_sha256": "4444444444444444444444444444444444444444444444444444444444444444",
                        "affected_slot": "perf/headline-claims",
                        "retracted_at": "2026-05-12T00:00:00Z",
                        "retracted_by_release": " \t",
                        "retraction_rationale": "synthetic blank release regression",
                        "retraction_path": "docs/attestations/retractions/synthetic.json",
                        "retraction_sha256": "5555555555555555555555555555555555555555555555555555555555555555",
                        "size_bytes": 1,
                        "corrected_claim_value": null
                    }
                ]);
                bundle
            },
        ),
        (
            "retraction_rationale",
            {
                let mut bundle = base_bundle(json!({
                    "method": "unsigned",
                    "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
                    "reason": "dev bundle tracked by ft-e87u6.2"
                }));
                bundle["retractions"] = json!([
                    {
                        "original_bundle_sha256": "4444444444444444444444444444444444444444444444444444444444444444",
                        "affected_slot": "perf/headline-claims",
                        "retracted_at": "2026-05-12T00:00:00Z",
                        "retracted_by_release": "0.2.1",
                        "retraction_rationale": "\n",
                        "retraction_path": "docs/attestations/retractions/synthetic.json",
                        "retraction_sha256": "5555555555555555555555555555555555555555555555555555555555555555",
                        "size_bytes": 1,
                        "corrected_claim_value": null
                    }
                ]);
                bundle
            },
        ),
    ] {
        assert!(
            !validate(&validator, &bundle).is_empty(),
            "bundle retraction summary must reject blank {surface}"
        );
    }
}

#[test]
fn bundle_schema_requires_canonical_confidence_summary() {
    let validator = bundle_validator();
    let valid = base_bundle(json!({
        "method": "unsigned",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "reason": "dev bundle tracked by ft-e87u6.2"
    }));
    assert!(
        validate(&validator, &valid).is_empty(),
        "bundle with canonical confidence summary should validate"
    );

    let mut missing_summary = valid.clone();
    missing_summary
        .as_object_mut()
        .expect("base bundle is an object")
        .remove("confidence_summary");
    assert!(
        !validate(&validator, &missing_summary).is_empty(),
        "bundle without confidence_summary should fail validation"
    );

    let mut bad_hash = valid;
    bad_hash["confidence_summary"]["best_confidence_by_category"][0]["source_artifact_hash"] =
        json!("not-a-sha256");
    assert!(
        !validate(&validator, &bad_hash).is_empty(),
        "confidence record without a SHA-256 source hash should fail validation"
    );
}

#[test]
fn bundle_schema_rejects_blank_confidence_summary_text() {
    let validator = bundle_validator();
    let valid = base_bundle(json!({
        "method": "unsigned",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "reason": "dev bundle tracked by ft-e87u6.2"
    }));

    for (surface, mut bundle) in [
        ("claim", valid.clone()),
        ("confidence_value.reason", valid.clone()),
        ("sample_size_or_state_count.unit", valid.clone()),
        ("methodology_url", valid),
    ] {
        match surface {
            "claim" => {
                bundle["confidence_summary"]["records"][0]["claim"] = json!(" \t");
            }
            "confidence_value.reason" => {
                bundle["confidence_summary"]["records"][0]["confidence_value"]["reason"] =
                    json!("\n");
            }
            "sample_size_or_state_count.unit" => {
                bundle["confidence_summary"]["records"][0]["sample_size_or_state_count"]["unit"] =
                    json!(" \t");
            }
            "methodology_url" => {
                bundle["confidence_summary"]["records"][0]["methodology_url"] = json!("\n");
            }
            _ => unreachable!("test case is exhaustive"),
        }
        assert!(
            !validate(&validator, &bundle).is_empty(),
            "bundle confidence summary must reject blank {surface}"
        );
    }
}

#[test]
fn bundle_schema_rejects_unsigned_reason_without_tracking_bead() {
    let validator = bundle_validator();
    let bundle = base_bundle(json!({
        "method": "unsigned",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "reason": "dev bundle"
    }));
    assert!(
        !validate(&validator, &bundle).is_empty(),
        "unsigned bundle reason must include a tracking bead ID"
    );
}

#[test]
fn attestation_retraction_schema_rejects_unsigned_signatures() {
    let validator = retraction_validator();
    let signed = base_retraction(json!({
        "method": "ed25519",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "signature_path": "docs/attestations/retractions/0.2.1.sig",
        "public_key": "3333333333333333333333333333333333333333333333333333333333333333"
    }));
    assert!(
        validate(&validator, &signed).is_empty(),
        "signed retraction should validate"
    );

    let unsigned = base_retraction(json!({
        "method": "unsigned",
        "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
        "reason": "synthetic retraction signature gap tracked by ft-e87u6.2"
    }));
    assert!(
        !validate(&validator, &unsigned).is_empty(),
        "retraction_signature must reject unsigned signatures"
    );
}

#[test]
fn attestation_retraction_schema_rejects_blank_metadata() {
    let validator = retraction_validator();
    for (surface, mut retraction) in [
        (
            "retracted_by_release",
            base_retraction(json!({
                "method": "ed25519",
                "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
                "signature_path": "docs/attestations/retractions/0.2.1.sig",
                "public_key": "3333333333333333333333333333333333333333333333333333333333333333"
            })),
        ),
        (
            "retraction_rationale",
            base_retraction(json!({
                "method": "ed25519",
                "canonical_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
                "signature_path": "docs/attestations/retractions/0.2.1.sig",
                "public_key": "3333333333333333333333333333333333333333333333333333333333333333"
            })),
        ),
    ] {
        retraction[surface] = json!(" \t\n");
        assert!(
            !validate(&validator, &retraction).is_empty(),
            "standalone attestation retraction must reject blank {surface}"
        );
    }
}
