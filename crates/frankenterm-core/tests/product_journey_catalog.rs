//! Contract, schema, reference, and mutation tests for the product-journey catalog.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use frankenterm_core::product_journey_catalog::{
    ActorMode, CatalogClaimState, CatalogLineageValidationCode, CatalogLineageValidationReport,
    CatalogValidationCode, ContradictionStatus, EvidenceState, FleetPoint, FreshnessState,
    JourneyIdentityRequirementV2, JourneyLifecyclePhaseV2, JourneyLifecycleRoleV2,
    JourneyLifecycleV2, JourneyMutationClassV2, JourneyPhaseOutcomeV2,
    JourneyPhasePreconditionV2, JourneyVariant,
    MAX_PRODUCT_JOURNEY_CATALOG_BYTES, MAX_PRODUCT_JOURNEY_LINEAGE_BYTES, ProducerCoverage,
    ProductJourneyCatalog, ProductJourneyCatalogV2, ProductJourneyDecodeCode,
    ProductJourneyLineageDecodeError, ProductJourneyLineageManifest, REQUIRED_COVERAGE_CELL_COUNT,
    REQUIRED_FIELD_JOURNEY_COUNT, REQUIRED_LIFECYCLE_PHASE_COUNT_V2, ReleaseRequirement,
    ReviewAuthorityKind, ReviewDisposition, RunVerdict, SupportDeclaration, TargetAvailability,
    TargetMode, Topology, Transport, verify_product_journey_lineage,
};
use jsonschema::{Draft, Validator};
use proptest::prelude::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CATALOG_RELATIVE_PATH: &str = "docs/design/product-journey-catalog.v1.json";
const CATALOG_V2_RELATIVE_PATH: &str = "docs/design/product-journey-catalog.v2.json";
const LINEAGE_RELATIVE_PATH: &str = "docs/design/product-journey-lineage.v1.json";
const SCHEMA_RELATIVE_PATH: &str = "docs/json-schema/ft-product-journey-catalog.json";
const SCHEMA_V2_RELATIVE_PATH: &str = "docs/json-schema/ft-product-journey-catalog-v2.json";
const LINEAGE_SCHEMA_RELATIVE_PATH: &str = "docs/json-schema/ft-product-journey-lineage.json";
const ISSUES_RELATIVE_PATH: &str = ".beads/issues.jsonl";

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root should resolve")
}

fn read_json(path: &Path) -> Value {
    let bytes =
        fs::read(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("failed to parse JSON {}: {error}", path.display()))
}

fn load_catalog_value(root: &Path) -> Value {
    read_json(&root.join(CATALOG_RELATIVE_PATH))
}

fn load_catalog(root: &Path) -> ProductJourneyCatalog {
    let path = root.join(CATALOG_RELATIVE_PATH);
    let bytes = fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    assert!(
        bytes.len() <= MAX_PRODUCT_JOURNEY_CATALOG_BYTES,
        "checked-in catalog exceeds its public bounded-decoder limit"
    );
    ProductJourneyCatalog::decode_json_bounded(&bytes).unwrap_or_else(|error| {
        panic!(
            "catalog {} failed bounded typed decode: {error}",
            path.display()
        )
    })
}

fn load_catalog_v2(root: &Path) -> ProductJourneyCatalogV2 {
    let path = root.join(CATALOG_V2_RELATIVE_PATH);
    let bytes = fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    ProductJourneyCatalogV2::decode_json_bounded(&bytes).unwrap_or_else(|error| {
        panic!(
            "catalog {} failed bounded typed decode: {error}",
            path.display()
        )
    })
}

fn load_lineage(root: &Path) -> ProductJourneyLineageManifest {
    let path = root.join(LINEAGE_RELATIVE_PATH);
    let bytes = fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    assert!(
        bytes.len() <= MAX_PRODUCT_JOURNEY_LINEAGE_BYTES,
        "checked-in lineage exceeds its public bounded-decoder limit"
    );
    ProductJourneyLineageManifest::decode_json_bounded(&bytes).unwrap_or_else(|error| {
        panic!(
            "lineage {} failed bounded typed decode: {error}",
            path.display()
        )
    })
}

fn lineage_snapshots(
    root: &Path,
    manifest: &ProductJourneyLineageManifest,
) -> BTreeMap<String, Vec<u8>> {
    manifest
        .records
        .iter()
        .filter_map(|record| record.snapshot.as_ref())
        .map(|snapshot| {
            let (relative_path, fragment) = snapshot
                .snapshot_ref
                .split_once('#')
                .expect("snapshot reference must contain its detached SHA-256 fragment");
            assert_eq!(
                fragment,
                format!("sha256={}", snapshot.raw_sha256),
                "snapshot reference must bind its declared raw SHA-256"
            );
            let relative_path = Path::new(relative_path);
            assert!(
                relative_path.is_relative()
                    && relative_path
                        .components()
                        .all(|component| matches!(component, Component::Normal(_))),
                "snapshot resolver refuses absolute paths or non-normal components: {}",
                relative_path.display()
            );
            assert_eq!(
                relative_path.parent(),
                Some(Path::new("docs/design/product-journey-catalog.snapshots")),
                "snapshot resolver is closed to the retained catalog directory"
            );
            let expected_file_name = format!("{}.json", snapshot.catalog_revision);
            assert_eq!(
                relative_path.file_name().and_then(|name| name.to_str()),
                Some(expected_file_name.as_str()),
                "snapshot filename must equal its catalog revision"
            );
            let path = root.join(relative_path);
            let resolved_path = path.canonicalize().unwrap_or_else(|error| {
                panic!(
                    "failed to resolve retained snapshot {}: {error}",
                    path.display()
                )
            });
            assert!(
                resolved_path.starts_with(root),
                "snapshot resolver refuses a path escaping the repository: {}",
                resolved_path.display()
            );
            let bytes = fs::read(&resolved_path).unwrap_or_else(|error| {
                panic!(
                    "failed to read retained snapshot {}: {error}",
                    resolved_path.display()
                )
            });
            (snapshot.snapshot_ref.clone(), bytes)
        })
        .collect()
}

fn assert_lineage_has_code(
    report: &CatalogLineageValidationReport,
    code: CatalogLineageValidationCode,
) {
    assert!(
        report.contains_code(code),
        "expected lineage code {} ({code:?}), got:\n{}",
        code.as_str(),
        report
            .errors
            .iter()
            .map(|error| format!("{} {}: {}", error.code.as_str(), error.path, error.detail))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[derive(Debug, Clone, Copy)]
enum DuplicateFieldOrder {
    BeforeOriginal,
    AfterOriginal,
}

fn duplicate_scalar_field(
    raw: &[u8],
    key: &str,
    occurrence: usize,
    duplicate_json: &str,
    order: DuplicateFieldOrder,
) -> Vec<u8> {
    let text = std::str::from_utf8(raw).expect("catalog JSON is UTF-8");
    let marker = format!("\"{key}\"");
    let field_start = text
        .match_indices(&marker)
        .filter(|(start, _)| text[*start + marker.len()..].trim_start().starts_with(':'))
        .nth(occurrence)
        .map(|(start, _)| start)
        .unwrap_or_else(|| panic!("catalog contains field occurrence {occurrence} of `{key}`"));
    let after_key = field_start + marker.len();
    let colon = after_key
        + text[after_key..]
            .find(':')
            .unwrap_or_else(|| panic!("catalog field `{key}` has a colon"));
    let value_start = colon
        + 1
        + raw[colon + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
    let value_end = if raw.get(value_start) == Some(&b'"') {
        let mut escaped = false;
        let mut end = None;
        for (offset, byte) in raw[value_start + 1..].iter().copied().enumerate() {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                end = Some(value_start + offset + 2);
                break;
            }
        }
        end.unwrap_or_else(|| panic!("catalog string field `{key}` is terminated"))
    } else {
        value_start
            + raw[value_start..]
                .iter()
                .position(|byte| byte.is_ascii_whitespace() || matches!(*byte, b',' | b'}' | b']'))
                .unwrap_or(raw.len() - value_start)
    };
    let (insertion_at, insertion) = match order {
        DuplicateFieldOrder::BeforeOriginal => {
            (field_start, format!("\"{key}\":{duplicate_json},"))
        }
        DuplicateFieldOrder::AfterOriginal => (value_end, format!(",\"{key}\":{duplicate_json}")),
    };
    let mut mutated = Vec::with_capacity(raw.len() + insertion.len());
    mutated.extend_from_slice(&raw[..insertion_at]);
    mutated.extend_from_slice(insertion.as_bytes());
    mutated.extend_from_slice(&raw[insertion_at..]);
    mutated
}

fn load_schema_validator(root: &Path) -> Validator {
    let path = root.join(SCHEMA_RELATIVE_PATH);
    let schema = read_json(&path);
    Validator::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|error| panic!("schema {} failed to compile: {error}", path.display()))
}

fn load_schema_v2_validator(root: &Path) -> Validator {
    let path = root.join(SCHEMA_V2_RELATIVE_PATH);
    let schema = read_json(&path);
    Validator::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|error| panic!("schema {} failed to compile: {error}", path.display()))
}

fn load_lineage_schema_validator(root: &Path) -> Validator {
    let path = root.join(LINEAGE_SCHEMA_RELATIVE_PATH);
    let schema = read_json(&path);
    Validator::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|error| panic!("schema {} failed to compile: {error}", path.display()))
}

fn schema_errors(validator: &Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|error| format!("{error} at {}", error.instance_path()))
        .collect()
}

fn issue_ids(root: &Path) -> HashSet<String> {
    let path = root.join(ISSUES_RELATIVE_PATH);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    raw.lines()
        .enumerate()
        .filter_map(|(line_index, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let value = serde_json::from_str::<Value>(trimmed).unwrap_or_else(|error| {
                panic!(
                    "invalid JSON on line {} of {}: {error}",
                    line_index + 1,
                    path.display()
                )
            });
            Some(
                value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        panic!(
                            "missing string id on line {} of {}",
                            line_index + 1,
                            path.display()
                        )
                    })
                    .to_string(),
            )
        })
        .collect()
}

fn is_bead_reference(reference: &str) -> bool {
    reference.starts_with("ft-") || reference.starts_with("wa-")
}

fn catalog_bead_refs(catalog: &ProductJourneyCatalog) -> BTreeSet<String> {
    let mut refs = BTreeSet::from([catalog.source_bead_id.clone()]);
    for gate in &catalog.gates {
        refs.extend(gate.producer_bead_ids.iter().cloned());
    }
    for journey in &catalog.journey_definitions {
        refs.insert(journey.field_bead_id.clone());
    }
    for variant in &catalog.variants {
        refs.extend(
            variant
                .exact_producer_bindings
                .iter()
                .map(|binding| binding.producer_bead_id.clone()),
        );
        refs.extend(variant.partial_producer_bead_ids.iter().cloned());
        refs.extend(
            variant
                .target_qualifications
                .iter()
                .flat_map(|qualification| qualification.blocker_refs.iter())
                .filter(|reference| is_bead_reference(reference))
                .cloned(),
        );
        match &variant.support {
            SupportDeclaration::Supported { .. } => {}
            SupportDeclaration::Conditional {
                tracking_bead_ids, ..
            }
            | SupportDeclaration::Unavailable {
                tracking_bead_ids, ..
            } => refs.extend(tracking_bead_ids.iter().cloned()),
        }
    }
    for contradiction in &catalog.contradictions {
        refs.extend(contradiction.tracking_bead_ids.iter().cloned());
    }
    refs
}

fn catalog_repository_refs(catalog: &ProductJourneyCatalog) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    for persona in &catalog.personas {
        refs.extend(persona.source_refs.iter().cloned());
    }
    for fleet_point in &catalog.fleet_points {
        refs.extend(fleet_point.source_refs.iter().cloned());
    }
    for topology in &catalog.topologies {
        refs.extend(topology.source_refs.iter().cloned());
    }
    for target_class in &catalog.target_classes {
        refs.extend(target_class.source_refs.iter().cloned());
    }
    for actor_mode in &catalog.actor_modes {
        refs.extend(actor_mode.source_refs.iter().cloned());
    }
    for gate in &catalog.gates {
        refs.extend(gate.evidence_refs.iter().cloned());
        refs.extend(gate.source_refs.iter().cloned());
    }
    for journey in &catalog.journey_definitions {
        refs.extend(journey.source_refs.iter().cloned());
    }
    for variant in &catalog.variants {
        refs.extend(variant.source_refs.iter().cloned());
        for binding in &variant.exact_producer_bindings {
            refs.extend(binding.source_refs.iter().cloned());
        }
        for qualification in &variant.target_qualifications {
            refs.extend(qualification.route_identity_ref.iter().cloned());
            refs.extend(qualification.candidate_identity_ref.iter().cloned());
            refs.extend(qualification.evidence_refs.iter().cloned());
            refs.extend(
                qualification
                    .blocker_refs
                    .iter()
                    .filter(|reference| !is_bead_reference(reference))
                    .cloned(),
            );
        }
        if let SupportDeclaration::Supported {
            promotion_receipt_ref,
            ..
        } = &variant.support
        {
            refs.insert(promotion_receipt_ref.clone());
        }
    }
    for mapping in &catalog.legacy_mappings {
        refs.extend(mapping.source_refs.iter().cloned());
    }
    refs.extend(
        catalog
            .readme_mappings
            .iter()
            .map(|mapping| mapping.readme_ref.clone()),
    );
    for contradiction in &catalog.contradictions {
        refs.extend(contradiction.source_refs.iter().cloned());
        refs.extend(contradiction.resolution_refs.iter().cloned());
    }
    for review in &catalog.review_history {
        refs.extend(review.authority_receipt_ref.iter().cloned());
        refs.extend(review.source_refs.iter().cloned());
    }
    for change in &catalog.change_history {
        refs.extend(change.source_refs.iter().cloned());
    }
    refs
}

fn reference_path(reference: &str) -> &str {
    reference
        .split_once('#')
        .map_or(reference, |(path, _fragment)| path)
}

fn github_heading_slug(heading: &str) -> String {
    heading
        .trim()
        .trim_end_matches('#')
        .trim_end()
        .chars()
        .filter_map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                Some(character.to_ascii_lowercase())
            } else if character.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn markdown_section<'a>(document: &'a str, fragment: &str) -> Option<&'a str> {
    let mut headings = Vec::new();
    let mut offset = 0;
    let mut fence_marker = None;
    for line in document.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let fence_candidate = trimmed.trim_start();
        let marker = if fence_candidate.starts_with("```") {
            Some('`')
        } else if fence_candidate.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        if let Some(marker) = marker {
            match fence_marker {
                Some(open) if open == marker => fence_marker = None,
                None => fence_marker = Some(marker),
                Some(_) => {}
            }
            offset += line.len();
            continue;
        }
        if fence_marker.is_some() {
            offset += line.len();
            continue;
        }
        let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
        if level != 0
            && level <= 6
            && trimmed
                .as_bytes()
                .get(level)
                .is_some_and(u8::is_ascii_whitespace)
        {
            let title = &trimmed[level..];
            headings.push((offset, level, github_heading_slug(title)));
        }
        offset += line.len();
    }

    let matches = headings
        .iter()
        .enumerate()
        .filter(|(_, (_, _, slug))| slug == fragment)
        .collect::<Vec<_>>();
    let [(heading_index, (start, level, _))] = matches.as_slice() else {
        return None;
    };
    let end = headings
        .iter()
        .skip(*heading_index + 1)
        .find_map(|(offset, candidate_level, _)| (candidate_level <= level).then_some(*offset))
        .unwrap_or(document.len());
    Some(&document[*start..end])
}

fn unresolved_repository_refs(
    root: &Path,
    references: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut unresolved = Vec::new();
    for reference in references {
        let (relative_path, fragment) = reference
            .split_once('#')
            .map_or((reference.as_str(), None), |(path, fragment)| {
                (path, Some(fragment))
            });
        let path = root.join(relative_path);
        if !path.exists() {
            unresolved.push(reference);
            continue;
        }
        let Some(fragment) = fragment else {
            continue;
        };
        let is_markdown = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"));
        if !is_markdown {
            unresolved.push(reference);
            continue;
        }
        let document = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        if markdown_section(&document, fragment).is_none() {
            unresolved.push(reference);
        }
    }
    unresolved
}

fn assert_detached_sha256(root: &Path, reference: &str, expected: &str) {
    let path = root.join(reference_path(reference));
    let bytes = fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let actual = hex::encode(Sha256::digest(bytes));
    assert_eq!(
        actual,
        expected,
        "detached receipt bytes drifted for {}",
        path.display()
    );
}

fn assert_has_code(catalog: &ProductJourneyCatalog, code: CatalogValidationCode) {
    let report = catalog.validate();
    assert!(
        report.contains_code(code),
        "expected validation code {} ({code:?}), got:\n{}",
        code.as_str(),
        report
            .errors
            .iter()
            .map(|error| format!("{} {}: {}", error.code.as_str(), error.path, error.detail))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn assert_v2_has_code(catalog: &ProductJourneyCatalogV2, code: CatalogValidationCode) {
    let report = catalog.validate();
    assert!(
        report.contains_code(code),
        "expected v2 validation code {} ({code:?}), got:\n{}",
        code.as_str(),
        report
            .errors
            .iter()
            .map(|error| format!("{} {}: {}", error.code.as_str(), error.path, error.detail))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn first_variant(catalog: &mut ProductJourneyCatalog) -> &mut JourneyVariant {
    catalog
        .variants
        .first_mut()
        .expect("checked-in catalog must contain variants")
}

fn lifecycle_phase_v2_mut(
    lifecycle: &mut JourneyLifecycleV2,
    index: usize,
) -> &mut JourneyLifecyclePhaseV2 {
    match index {
        0 => &mut lifecycle.identity_preflight,
        1 => &mut lifecycle.clean_setup,
        2 => &mut lifecycle.steady_work,
        3 => &mut lifecycle.failure_overload,
        4 => &mut lifecycle.recovery_convergence,
        5 => &mut lifecycle.teardown_outcome,
        _ => panic!("v2 lifecycle phase index {index} is out of range"),
    }
}

#[test]
fn checked_in_catalog_matches_draft_2020_12_schema() {
    let root = repository_root();
    let catalog = load_catalog_value(&root);
    let validator = load_schema_validator(&root);
    let errors = schema_errors(&validator, &catalog);
    assert!(
        errors.is_empty(),
        "checked-in product-journey catalog failed JSON Schema:\n{}",
        errors.join("\n")
    );
}

#[test]
fn checked_in_catalog_passes_typed_semantic_validation() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    let report = catalog.validate();
    assert!(
        report.valid,
        "checked-in product-journey catalog failed semantic validation:\n{}",
        report
            .errors
            .iter()
            .map(|error| format!("{} {}: {}", error.code.as_str(), error.path, error.detail))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(
        catalog.variants.len(),
        REQUIRED_COVERAGE_CELL_COUNT,
        "catalog must materialize every exact persona/fleet/topology cell"
    );
}

#[test]
fn checked_in_catalog_round_trips_without_shape_loss() {
    let root = repository_root();
    let original = load_catalog_value(&root);
    let typed = load_catalog(&root);
    let encoded =
        serde_json::to_value(&typed).expect("public catalog DTO should serialize back to JSON");
    assert_eq!(
        encoded, original,
        "serde DTO shape drifted from the checked-in public JSON contract"
    );
    let first = serde_json::to_vec(&typed).expect("product catalog should encode");
    let second = serde_json::to_vec(&typed).expect("product catalog should encode again");
    assert_eq!(first, second, "typed encoding must be deterministic");
    assert_eq!(
        ProductJourneyCatalog::decode_json_bounded(&first)
            .expect("deterministic encoding should decode"),
        typed
    );
}

#[test]
fn bounded_decoder_stably_classifies_malformed_trailing_and_oversized_documents() {
    for malformed in [b"".as_slice(), b"{".as_slice(), b"null".as_slice()] {
        let error = ProductJourneyCatalog::decode_json_bounded(malformed)
            .expect_err("malformed document must be rejected");
        assert_eq!(error.code(), ProductJourneyDecodeCode::InvalidJson);
        assert!(
            error
                .to_string()
                .starts_with(ProductJourneyDecodeCode::InvalidJson.as_str()),
            "invalid-JSON diagnostic must retain its stable code"
        );
    }

    let root = repository_root();
    let mut unknown = load_catalog_value(&root);
    unknown["silent_support_promotion"] = json!(true);
    assert_eq!(
        ProductJourneyCatalog::decode_json_bounded(
            &serde_json::to_vec(&unknown).expect("unknown-field mutation should encode")
        )
        .expect_err("unknown root field must be rejected")
        .code(),
        ProductJourneyDecodeCode::InvalidJson
    );

    let mut trailing = fs::read(root.join(CATALOG_RELATIVE_PATH))
        .expect("checked-in product catalog should be readable");
    trailing.extend_from_slice(b"\n{}");
    let trailing_error = ProductJourneyCatalog::decode_json_bounded(&trailing)
        .expect_err("trailing second value must be rejected");
    assert_eq!(
        trailing_error.code(),
        ProductJourneyDecodeCode::TrailingData
    );
    assert!(
        trailing_error
            .to_string()
            .starts_with(ProductJourneyDecodeCode::TrailingData.as_str()),
        "trailing-data diagnostic must retain its stable code"
    );

    let oversized = vec![b' '; MAX_PRODUCT_JOURNEY_CATALOG_BYTES + 1];
    let oversized_error = ProductJourneyCatalog::decode_json_bounded(&oversized)
        .expect_err("oversized document must be rejected before parsing");
    assert_eq!(
        oversized_error.code(),
        ProductJourneyDecodeCode::PayloadTooLarge
    );
    assert!(
        oversized_error
            .to_string()
            .starts_with(ProductJourneyDecodeCode::PayloadTooLarge.as_str()),
        "payload-too-large diagnostic must retain its stable code"
    );
}

#[test]
fn bounded_decoder_rejects_duplicate_fields_at_every_authority_boundary() {
    let raw = fs::read(repository_root().join(CATALOG_RELATIVE_PATH))
        .expect("checked-in product catalog should be readable");
    let cases = [
        ("contract_id", 0, "\"ft.product_journey_catalog.invalid\""),
        ("schema_version", 0, "2"),
        ("title", 0, "\"duplicate nested persona title\""),
        ("pane_count", 0, "999"),
        ("claim_id", 0, "\"claim.duplicate\""),
        ("state", 0, "\"conditional\""),
        ("reason", 0, "\"duplicate tagged payload reason\""),
    ];
    for (key, occurrence, duplicate_json) in cases {
        for order in [
            DuplicateFieldOrder::BeforeOriginal,
            DuplicateFieldOrder::AfterOriginal,
        ] {
            let mutated = duplicate_scalar_field(&raw, key, occurrence, duplicate_json, order);
            let error = ProductJourneyCatalog::decode_json_bounded(&mutated)
                .expect_err("duplicate field must fail closed");
            assert_eq!(
                error.code(),
                ProductJourneyDecodeCode::InvalidJson,
                "duplicate `{key}` occurrence {occurrence} in {order:?} order was misclassified"
            );
        }
    }
}

#[test]
fn bounded_decoder_rejects_duplicate_claim_id_for_every_variant_and_key_order() {
    let raw = fs::read(repository_root().join(CATALOG_RELATIVE_PATH))
        .expect("checked-in product catalog should be readable");
    for index in 0..REQUIRED_COVERAGE_CELL_COUNT {
        for order in [
            DuplicateFieldOrder::BeforeOriginal,
            DuplicateFieldOrder::AfterOriginal,
        ] {
            let mutated = duplicate_scalar_field(
                &raw,
                "claim_id",
                index,
                "\"claim.deterministic_canary\"",
                order,
            );
            let error = ProductJourneyCatalog::decode_json_bounded(&mutated)
                .expect_err("every duplicate claim_id must fail closed");
            assert_eq!(
                error.code(),
                ProductJourneyDecodeCode::InvalidJson,
                "variant {index} duplicate in {order:?} order was misclassified"
            );
        }
    }
}

#[test]
fn checked_in_lineage_verifies_exact_retained_snapshots_offline() {
    let root = repository_root();
    let manifest = load_lineage(&root);
    let lineage_value = read_json(&root.join(LINEAGE_RELATIVE_PATH));
    let schema = load_lineage_schema_validator(&root);
    let schema_failures = schema_errors(&schema, &lineage_value);
    assert!(
        schema_failures.is_empty(),
        "checked-in lineage failed its JSON Schema:\n{}",
        schema_failures.join("\n")
    );
    let snapshots = lineage_snapshots(&root, &manifest);
    let report = verify_product_journey_lineage(&manifest, &snapshots);
    assert!(
        report.valid,
        "checked-in lineage failed offline verification:\n{}",
        report
            .errors
            .iter()
            .map(|error| format!("{} {}: {}", error.code.as_str(), error.path, error.detail))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let unretained = &manifest.records[0];
    assert!(unretained.snapshot.is_none());
    assert!(unretained.canonical_record_sha256.is_none());
    assert!(unretained.signature_ed25519_hex.is_none());

    let current_snapshot = manifest
        .records
        .last()
        .and_then(|record| record.snapshot.as_ref())
        .expect("current lineage record retains a snapshot");
    let current_snapshot_path = current_snapshot
        .snapshot_ref
        .split_once('#')
        .map_or(current_snapshot.snapshot_ref.as_str(), |(path, _)| path);
    assert_eq!(
        fs::read(root.join(CATALOG_RELATIVE_PATH)).expect("current catalog is readable"),
        fs::read(root.join(current_snapshot_path)).expect("current retained snapshot is readable"),
        "the mutable catalog doorway must equal its signed retained current snapshot"
    );
}

#[test]
fn lineage_bounded_decoder_rejects_duplicates_unknown_trailing_and_oversized_input() {
    let root = repository_root();
    let raw = fs::read(root.join(LINEAGE_RELATIVE_PATH)).expect("lineage fixture is readable");

    for order in [
        DuplicateFieldOrder::BeforeOriginal,
        DuplicateFieldOrder::AfterOriginal,
    ] {
        let duplicate = duplicate_scalar_field(&raw, "schema_version", 0, "1", order);
        assert!(matches!(
            ProductJourneyLineageManifest::decode_json_bounded(&duplicate),
            Err(ProductJourneyLineageDecodeError::InvalidJson { .. })
        ));
    }

    let mut unknown = serde_json::from_slice::<Value>(&raw).expect("lineage fixture is JSON");
    unknown["ambient_head"] = json!("must-not-be-consulted");
    assert!(matches!(
        ProductJourneyLineageManifest::decode_json_bounded(
            &serde_json::to_vec(&unknown).expect("unknown-field mutation encodes")
        ),
        Err(ProductJourneyLineageDecodeError::InvalidJson { .. })
    ));

    let mut trailing = raw;
    trailing.extend_from_slice(b"\n{}\n");
    assert!(matches!(
        ProductJourneyLineageManifest::decode_json_bounded(&trailing),
        Err(ProductJourneyLineageDecodeError::TrailingData { .. })
    ));

    let oversized = vec![b' '; MAX_PRODUCT_JOURNEY_LINEAGE_BYTES + 1];
    assert!(matches!(
        ProductJourneyLineageManifest::decode_json_bounded(&oversized),
        Err(ProductJourneyLineageDecodeError::PayloadTooLarge { .. })
    ));
}

#[test]
fn lineage_rejects_modified_reordered_missing_and_copied_history() {
    let root = repository_root();
    let manifest = load_lineage(&root);
    let snapshots = lineage_snapshots(&root, &manifest);

    let mut modified_snapshots = snapshots.clone();
    let genesis_ref = manifest.records[1]
        .snapshot
        .as_ref()
        .expect("genesis snapshot exists")
        .snapshot_ref
        .clone();
    modified_snapshots
        .get_mut(&genesis_ref)
        .expect("genesis bytes are supplied")
        .push(b'\n');
    assert_lineage_has_code(
        &verify_product_journey_lineage(&manifest, &modified_snapshots),
        CatalogLineageValidationCode::SnapshotDigestMismatch,
    );

    let mut reordered = manifest.clone();
    reordered.records.swap(1, 2);
    assert_lineage_has_code(
        &verify_product_journey_lineage(&reordered, &snapshots),
        CatalogLineageValidationCode::InvalidHistoryOrder,
    );

    let mut missing = manifest.clone();
    missing.records.remove(1);
    assert_lineage_has_code(
        &verify_product_journey_lineage(&missing, &snapshots),
        CatalogLineageValidationCode::InvalidGenesis,
    );

    let mut truncated_tail = manifest.clone();
    truncated_tail.records.pop();
    truncated_tail.current_catalog_revision = "2026-07-27.2".to_string();
    assert_lineage_has_code(
        &verify_product_journey_lineage(&truncated_tail, &snapshots),
        CatalogLineageValidationCode::InvalidHistoryOrder,
    );

    let mut invented_tail = manifest.clone();
    invented_tail.records.push(
        invented_tail
            .records
            .last()
            .expect("lineage has a current record")
            .clone(),
    );
    assert_lineage_has_code(
        &verify_product_journey_lineage(&invented_tail, &snapshots),
        CatalogLineageValidationCode::InvalidHistoryOrder,
    );

    let mut impossible_date = manifest.clone();
    impossible_date.records[0].catalog_revision = "2026-02-31.1".to_string();
    assert_lineage_has_code(
        &verify_product_journey_lineage(&impossible_date, &snapshots),
        CatalogLineageValidationCode::InvalidHistoryOrder,
    );

    let mut copied = manifest.clone();
    let copied_genesis_snapshot = copied.records[1].snapshot.clone();
    copied.records[2].snapshot = copied_genesis_snapshot;
    assert_lineage_has_code(
        &verify_product_journey_lineage(&copied, &snapshots),
        CatalogLineageValidationCode::InvalidSnapshotIdentity,
    );

    let mut missing_snapshot = snapshots;
    missing_snapshot.remove(&genesis_ref);
    assert_lineage_has_code(
        &verify_product_journey_lineage(&manifest, &missing_snapshot),
        CatalogLineageValidationCode::MissingSnapshot,
    );

    let mut oversized_snapshot = lineage_snapshots(&root, &manifest);
    oversized_snapshot.insert(
        genesis_ref,
        vec![b' '; MAX_PRODUCT_JOURNEY_CATALOG_BYTES + 1],
    );
    assert_lineage_has_code(
        &verify_product_journey_lineage(&manifest, &oversized_snapshot),
        CatalogLineageValidationCode::SnapshotCatalogMismatch,
    );
}

#[test]
fn lineage_rejects_wrong_git_identity_unsigned_or_invented_predecessors() {
    let root = repository_root();
    let manifest = load_lineage(&root);
    let snapshots = lineage_snapshots(&root, &manifest);

    let mut wrong_commit = manifest.clone();
    wrong_commit.records[1]
        .snapshot
        .as_mut()
        .expect("genesis snapshot exists")
        .git_commit = "0".repeat(40);
    assert_lineage_has_code(
        &verify_product_journey_lineage(&wrong_commit, &snapshots),
        CatalogLineageValidationCode::InvalidGenesis,
    );

    let mut wrong_parent = manifest.clone();
    wrong_parent.records[1]
        .snapshot
        .as_mut()
        .expect("genesis snapshot exists")
        .git_parent = "0".repeat(40);
    assert_lineage_has_code(
        &verify_product_journey_lineage(&wrong_parent, &snapshots),
        CatalogLineageValidationCode::InvalidGenesis,
    );

    let mut wrong_successor_commit = manifest.clone();
    wrong_successor_commit.records[2]
        .snapshot
        .as_mut()
        .expect("successor snapshot exists")
        .git_commit = "0".repeat(40);
    assert_lineage_has_code(
        &verify_product_journey_lineage(&wrong_successor_commit, &snapshots),
        CatalogLineageValidationCode::InvalidSignature,
    );

    let mut wrong_successor_parent = manifest.clone();
    wrong_successor_parent.records[2]
        .snapshot
        .as_mut()
        .expect("successor snapshot exists")
        .git_parent = "0".repeat(40);
    assert_lineage_has_code(
        &verify_product_journey_lineage(&wrong_successor_parent, &snapshots),
        CatalogLineageValidationCode::InvalidSignature,
    );

    let mut unsigned = manifest.clone();
    unsigned.records[2].signature_ed25519_hex = None;
    assert_lineage_has_code(
        &verify_product_journey_lineage(&unsigned, &snapshots),
        CatalogLineageValidationCode::MissingSignature,
    );

    let mut invented_draft = manifest.clone();
    let invented_genesis_snapshot = invented_draft.records[1].snapshot.clone();
    invented_draft.records[0].snapshot = invented_genesis_snapshot;
    assert_lineage_has_code(
        &verify_product_journey_lineage(&invented_draft, &snapshots),
        CatalogLineageValidationCode::InventedUnretainedHistory,
    );

    let mut invented_predecessor = manifest.clone();
    invented_predecessor.records[2].predecessor_snapshot = None;
    assert_lineage_has_code(
        &verify_product_journey_lineage(&invented_predecessor, &snapshots),
        CatalogLineageValidationCode::PredecessorMismatch,
    );
}

#[test]
fn lineage_rejects_digest_signature_and_delegation_tampering() {
    let root = repository_root();
    let manifest = load_lineage(&root);
    let snapshots = lineage_snapshots(&root, &manifest);

    let mut wrong_digest = manifest.clone();
    wrong_digest.records[2].canonical_record_sha256 = Some("0".repeat(64));
    assert_lineage_has_code(
        &verify_product_journey_lineage(&wrong_digest, &snapshots),
        CatalogLineageValidationCode::CanonicalDigestMismatch,
    );

    let mut wrong_signature = manifest.clone();
    wrong_signature.records[2].signature_ed25519_hex = Some("0".repeat(128));
    assert_lineage_has_code(
        &verify_product_journey_lineage(&wrong_signature, &snapshots),
        CatalogLineageValidationCode::InvalidSignature,
    );

    let mut missing_delegation = manifest;
    missing_delegation.records[2].delegation = None;
    assert_lineage_has_code(
        &verify_product_journey_lineage(&missing_delegation, &snapshots),
        CatalogLineageValidationCode::InvalidDelegation,
    );

    let mut unknown_delegated_key = load_lineage(&root);
    unknown_delegated_key.records[1]
        .delegation
        .as_mut()
        .expect("genesis delegation exists")
        .authorized_successor_keys[0]
        .key_id = "ft.product-journey-lineage.untrusted".to_string();
    let unknown_key_report = verify_product_journey_lineage(&unknown_delegated_key, &snapshots);
    assert_lineage_has_code(
        &unknown_key_report,
        CatalogLineageValidationCode::UnknownSigner,
    );
    assert_lineage_has_code(
        &unknown_key_report,
        CatalogLineageValidationCode::InvalidDelegation,
    );

    let mut changed_domain = load_lineage(&root);
    changed_domain.digest_domain.push_str(".ambient");
    assert_lineage_has_code(
        &verify_product_journey_lineage(&changed_domain, &snapshots),
        CatalogLineageValidationCode::UnknownCanonicalDomain,
    );
}

#[test]
fn every_catalog_bead_reference_exists_in_the_export() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    let known = issue_ids(&root);
    let missing = catalog_bead_refs(&catalog)
        .into_iter()
        .filter(|bead_id| !known.contains(bead_id))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "catalog references Beads absent from {ISSUES_RELATIVE_PATH}: {}",
        missing.join(", ")
    );
}

#[test]
fn every_catalog_repository_reference_resolves() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    let missing = unresolved_repository_refs(&root, catalog_repository_refs(&catalog));
    assert!(
        missing.is_empty(),
        "catalog contains missing paths, non-Markdown fragments, or absent/ambiguous Markdown headings: {}",
        missing.join(", ")
    );

    let broken = unresolved_repository_refs(
        &root,
        ["README.md#definitely-not-a-real-heading".to_string()],
    );
    assert_eq!(
        broken,
        ["README.md#definitely-not-a-real-heading"],
        "generic repository-reference validation must not strip and ignore fragments"
    );
}

#[test]
fn schema_rejects_unknown_fields_versions_and_ambiguous_support_shapes() {
    let root = repository_root();
    let validator = load_schema_validator(&root);

    let mut unknown_field = load_catalog_value(&root);
    unknown_field["silent_support_promotion"] = json!(true);
    assert!(
        !schema_errors(&validator, &unknown_field).is_empty(),
        "additional top-level fields must fail closed"
    );

    let mut unknown_version = load_catalog_value(&root);
    unknown_version["schema_version"] = json!(2);
    assert!(
        !schema_errors(&validator, &unknown_version).is_empty(),
        "unknown schema versions must fail closed"
    );

    let mut ambiguous_support = load_catalog_value(&root);
    let support = &mut ambiguous_support["variants"][0]["support"];
    support["state"] = json!("unavailable");
    support["reason"] = json!("synthetic unavailable mutation");
    support["fallback"] = json!("use a qualified target");
    support["tracking_bead_ids"] = json!(["ft-interactive-swarm-product-convergence-7xqz4.11.15"]);
    assert!(
        !schema_errors(&validator, &ambiguous_support).is_empty(),
        "unavailable support must reject the conditional-only constraints field"
    );

    let mut resolved_contradiction = load_catalog_value(&root);
    resolved_contradiction["contradictions"][0]["status"] = json!("resolved");
    resolved_contradiction["contradictions"][0]["resolution_refs"] = json!([CATALOG_RELATIVE_PATH]);
    assert!(
        !schema_errors(&validator, &resolved_contradiction).is_empty(),
        "schema v1 must reject every resolved contradiction until a signed receipt version exists"
    );

    let mut noncanonical_timestamp = load_catalog_value(&root);
    noncanonical_timestamp["review_history"][0]["reviewed_at_utc"] =
        json!("2026-07-27T23:04:36+00:00");
    assert!(
        !schema_errors(&validator, &noncanonical_timestamp).is_empty(),
        "schema timestamps must use canonical UTC Z form"
    );

    let mut impossible_calendar_date = load_catalog_value(&root);
    impossible_calendar_date["review_history"][1]["reviewed_at_utc"] =
        json!("2026-02-30T12:00:00Z");
    assert!(
        !schema_errors(&validator, &impossible_calendar_date).is_empty(),
        "the designated draft-2020-12 validator must assert date-time semantics, not just regex shape"
    );

    let mut optional_release_scope = load_catalog_value(&root);
    optional_release_scope["gates"][0]["release_requirement"] = json!("optional");
    assert!(
        !schema_errors(&validator, &optional_release_scope).is_empty(),
        "schema v1 is all-required and must reject reserved future scope values"
    );

    let catalog_sha256 = hex::encode(Sha256::digest(
        fs::read(root.join(CATALOG_RELATIVE_PATH))
            .expect("catalog bytes should be readable for forged-receipt mutation"),
    ));
    let mut forged_human_authority = load_catalog_value(&root);
    let mut forged_review = forged_human_authority["review_history"][1].clone();
    forged_review["review_id"] = json!("ft.product-journey-review.forged-human");
    forged_review["reviewer"] = json!("Untrusted claimed product owner");
    forged_review["authority_kind"] = json!("human_product_owner");
    forged_review["disposition"] = json!("approved");
    forged_review["scope"] = json!(["catalog_contract"]);
    forged_review["reviewed_commit"] = json!("0".repeat(40));
    forged_review["authority_receipt_ref"] = json!(CATALOG_RELATIVE_PATH);
    forged_review["authority_receipt_sha256"] = json!(catalog_sha256);
    forged_human_authority["review_history"]
        .as_array_mut()
        .expect("review history is an array")
        .push(forged_review);
    assert!(
        !schema_errors(&validator, &forged_human_authority).is_empty(),
        "schema v1 must reject even a fully shaped content-bound claimed human approval without a trusted signature verifier"
    );

    let mut forged_human_information = load_catalog_value(&root);
    forged_human_information["review_history"][1]["authority_kind"] = json!("human_product_owner");
    assert!(
        !schema_errors(&validator, &forged_human_information).is_empty(),
        "schema v1 must reject claimed human authority even with an informational disposition"
    );

    let mut automated_with_commit = load_catalog_value(&root);
    automated_with_commit["review_history"][1]["reviewed_commit"] = json!("0".repeat(40));
    assert!(
        !schema_errors(&validator, &automated_with_commit).is_empty(),
        "automated informational metadata cannot claim a reviewed commit"
    );

    let mut unbound_retained_evidence = load_catalog_value(&root);
    let qualification = &mut unbound_retained_evidence["variants"][0]["target_qualifications"][0];
    qualification["availability"] = json!("available");
    qualification["evidence_state"] = json!("fixture_only");
    qualification["run_verdict"] = json!("fail");
    qualification["freshness_state"] = json!("stale");
    qualification["evidence_refs"] = json!([CATALOG_RELATIVE_PATH]);
    qualification["route_identity_ref"] = Value::Null;
    qualification["candidate_identity_ref"] = Value::Null;
    assert!(
        !schema_errors(&validator, &unbound_retained_evidence).is_empty(),
        "retained, executed, or stale evidence must bind both candidate and route identity"
    );

    let mut promoted_combined_m5 = load_catalog_value(&root);
    let qualification = &mut promoted_combined_m5["variants"][0]["target_qualifications"][2];
    qualification["availability"] = json!("available");
    qualification["evidence_state"] = json!("fixture_only");
    qualification["run_verdict"] = json!("fail");
    qualification["freshness_state"] = json!("stale");
    qualification["route_identity_ref"] = json!(CATALOG_RELATIVE_PATH);
    qualification["candidate_identity_ref"] = json!(CATALOG_RELATIVE_PATH);
    qualification["evidence_refs"] = json!([CATALOG_RELATIVE_PATH]);
    assert!(
        !schema_errors(&validator, &promoted_combined_m5).is_empty(),
        "transitional combined M5 Pro/Max lanes must remain unqualifiable in schema v1"
    );

    for (field, forbidden_value) in [
        ("evidence_state", "proven"),
        ("run_verdict", "pass"),
        ("freshness_state", "current"),
    ] {
        let mut forged_positive_authority = load_catalog_value(&root);
        let qualification =
            &mut forged_positive_authority["variants"][0]["target_qualifications"][0];
        qualification["availability"] = json!("available");
        qualification["evidence_state"] = json!("fixture_only");
        qualification["run_verdict"] = json!("fail");
        qualification["freshness_state"] = json!("stale");
        qualification["route_identity_ref"] = json!(CATALOG_RELATIVE_PATH);
        qualification["candidate_identity_ref"] = json!(CATALOG_RELATIVE_PATH);
        qualification["evidence_refs"] = json!([CATALOG_RELATIVE_PATH]);
        qualification[field] = json!(forbidden_value);
        assert!(
            !schema_errors(&validator, &forged_positive_authority).is_empty(),
            "schema v1 must reject unsupported positive authority `{field}={forbidden_value}`"
        );
    }

    let mut missing_required_setup_slot = load_catalog_value(&root);
    missing_required_setup_slot["journey_definitions"][0]["setup"]
        .as_array_mut()
        .expect("setup is an array")
        .remove(0);
    assert!(
        !schema_errors(&validator, &missing_required_setup_slot).is_empty(),
        "schema v1 structurally requires two nonempty setup slots but cannot type their semantic roles"
    );
}

#[test]
fn semantic_validator_detects_duplicate_ids_claims_and_composite_keys() {
    let root = repository_root();
    let mut catalog = load_catalog(&root);
    let duplicate = catalog
        .variants
        .first()
        .cloned()
        .expect("catalog contains variants");
    catalog.variants.push(duplicate);
    let report = catalog.validate();
    for code in [
        CatalogValidationCode::DuplicateId,
        CatalogValidationCode::DuplicateClaimId,
        CatalogValidationCode::DuplicateCompositeKey,
    ] {
        assert!(
            report.contains_code(code),
            "duplicate mutation should report {} ({code:?})",
            code.as_str()
        );
    }
}

#[test]
fn semantic_validator_detects_missing_coverage_and_field_journey_binding() {
    let root = repository_root();

    let mut missing_cell = load_catalog(&root);
    missing_cell.variants.pop();
    assert_has_code(
        &missing_cell,
        CatalogValidationCode::MissingRequiredCoverage,
    );

    let mut missing_field_journey = load_catalog(&root);
    missing_field_journey.journey_definitions.pop();
    assert_has_code(
        &missing_field_journey,
        CatalogValidationCode::IncompleteFieldJourneyBinding,
    );
}

#[test]
fn semantic_validator_detects_dangling_and_malformed_references() {
    let root = repository_root();

    let mut dangling = load_catalog(&root);
    first_variant(&mut dangling)
        .journey_ids
        .push("journey.does_not_exist".to_string());
    assert_has_code(&dangling, CatalogValidationCode::DanglingReference);

    let mut traversal = load_catalog(&root);
    first_variant(&mut traversal)
        .source_refs
        .push("../outside.json".to_string());
    assert_has_code(&traversal, CatalogValidationCode::MalformedReference);

    let mut missing_gate = load_catalog(&root);
    first_variant(&mut missing_gate).gate_ids.clear();
    assert_has_code(&missing_gate, CatalogValidationCode::EmptyRequiredField);
}

#[test]
fn producer_coverage_requires_exact_direct_partial_and_gap_shapes() {
    let root = repository_root();
    let catalog = load_catalog(&root);

    let direct = catalog
        .variants
        .iter()
        .find(|variant| variant.producer_coverage == ProducerCoverage::Direct)
        .expect("checked-in catalog must retain at least one exact direct binding");
    assert!(!direct.exact_producer_bindings.is_empty());
    assert!(direct.partial_producer_bead_ids.is_empty());

    let partial = catalog
        .variants
        .iter()
        .find(|variant| variant.producer_coverage == ProducerCoverage::Partial)
        .expect("checked-in catalog must expose at least one partial producer lane");
    assert!(partial.exact_producer_bindings.is_empty());
    assert!(!partial.partial_producer_bead_ids.is_empty());

    let gap = catalog
        .variants
        .iter()
        .find(|variant| variant.producer_coverage == ProducerCoverage::Gap)
        .expect("checked-in catalog must retain required producer gaps fail-closed");
    assert!(gap.exact_producer_bindings.is_empty());
    assert!(gap.partial_producer_bead_ids.is_empty());

    let mut missing_exact_binding = catalog.clone();
    let direct = missing_exact_binding
        .variants
        .iter_mut()
        .find(|variant| variant.producer_coverage == ProducerCoverage::Direct)
        .expect("direct fixture exists");
    direct.exact_producer_bindings.clear();
    assert_has_code(
        &missing_exact_binding,
        CatalogValidationCode::InvalidProducerCoverage,
    );

    let mut shrunk_target_set = catalog.clone();
    let direct = shrunk_target_set
        .variants
        .iter_mut()
        .find(|variant| variant.producer_coverage == ProducerCoverage::Direct)
        .expect("direct fixture exists");
    direct.exact_producer_bindings[0]
        .controller_target_class_ids
        .pop();
    assert_has_code(
        &shrunk_target_set,
        CatalogValidationCode::InvalidProducerCoverage,
    );

    let mut substituted_exact_producer = catalog.clone();
    let direct = substituted_exact_producer
        .variants
        .iter_mut()
        .find(|variant| {
            variant.producer_coverage == ProducerCoverage::Direct
                && variant.coverage.fleet_point == FleetPoint::Q002
                && variant.coverage.topology == Topology::LocalOnly
        })
        .expect("local q002 direct fixture exists");
    direct.exact_producer_bindings[0].producer_bead_id =
        "ft-interactive-swarm-product-convergence-7xqz4.11.1".to_string();
    assert_has_code(
        &substituted_exact_producer,
        CatalogValidationCode::InvalidProducerCoverage,
    );

    let mut removed_partial_producer = catalog.clone();
    let partial = removed_partial_producer
        .variants
        .iter_mut()
        .find(|variant| variant.producer_coverage == ProducerCoverage::Partial)
        .expect("partial fixture exists");
    partial.partial_producer_bead_ids.pop();
    assert_has_code(
        &removed_partial_producer,
        CatalogValidationCode::InvalidProducerCoverage,
    );

    let mut substituted_partial_producer = catalog;
    let partial = substituted_partial_producer
        .variants
        .iter_mut()
        .find(|variant| {
            variant.producer_coverage == ProducerCoverage::Partial
                && !variant
                    .partial_producer_bead_ids
                    .iter()
                    .any(|bead_id| bead_id.ends_with(".11.12"))
        })
        .expect("partial fixture without accessibility producer exists");
    partial.partial_producer_bead_ids[0] =
        "ft-interactive-swarm-product-convergence-7xqz4.11.12".to_string();
    assert_has_code(
        &substituted_partial_producer,
        CatalogValidationCode::InvalidProducerCoverage,
    );
}

#[test]
fn target_qualifications_are_exact_per_pair_and_fail_closed() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    for variant in &catalog.variants {
        assert_eq!(
            variant.target_qualifications.len(),
            3,
            "{} must carry three exact target pairs",
            variant.variant_id
        );
        let actual = variant
            .target_qualifications
            .iter()
            .map(|qualification| {
                (
                    qualification.controller_target_class_id.as_str(),
                    qualification.session_host_target_class_id.as_str(),
                )
            })
            .collect::<BTreeSet<_>>();
        let expected = match variant.coverage.topology {
            Topology::LocalOnly => BTreeSet::from([
                ("mac16_11_m4_pro", "mac16_11_m4_pro"),
                ("m5_native", "m5_native"),
                ("m5_pro_max_native", "m5_pro_max_native"),
            ]),
            Topology::MacLanRemote => BTreeSet::from([
                ("mac16_11_m4_pro", "trj_5995wx"),
                ("m5_native", "trj_5995wx"),
                ("m5_pro_max_native", "trj_5995wx"),
            ]),
        };
        assert_eq!(
            actual, expected,
            "{} target pairs drifted",
            variant.variant_id
        );
        assert!(
            variant
                .target_qualifications
                .iter()
                .all(|qualification| qualification.transport == variant.transport),
            "{} qualification transport drifted",
            variant.variant_id
        );
    }

    let mut cross_pair = catalog.clone();
    first_variant(&mut cross_pair).target_qualifications[0].session_host_target_class_id =
        "trj_5995wx".to_string();
    assert_has_code(
        &cross_pair,
        CatalogValidationCode::InvalidTargetQualification,
    );

    let mut drifted_qualification_id = catalog.clone();
    first_variant(&mut drifted_qualification_id).target_qualifications[0]
        .qualification_id
        .push_str(".drift");
    assert_has_code(
        &drifted_qualification_id,
        CatalogValidationCode::InvalidTargetQualification,
    );

    let mut unavailable_pass = catalog;
    let qualification = &mut first_variant(&mut unavailable_pass).target_qualifications[1];
    qualification.availability = TargetAvailability::Unavailable;
    qualification.evidence_state = EvidenceState::Proven;
    qualification.run_verdict = RunVerdict::Pass;
    qualification.freshness_state = FreshnessState::Current;
    qualification.evidence_refs = vec![CATALOG_RELATIVE_PATH.to_string()];
    qualification.blocker_refs.clear();
    assert_has_code(
        &unavailable_pass,
        CatalogValidationCode::InvalidTargetQualification,
    );

    for clear_route in [true, false] {
        let mut unbound_evidence = load_catalog(&root);
        let qualification = &mut first_variant(&mut unbound_evidence).target_qualifications[0];
        qualification.availability = TargetAvailability::Available;
        qualification.evidence_state = EvidenceState::Proven;
        qualification.run_verdict = RunVerdict::Pass;
        qualification.freshness_state = FreshnessState::Current;
        qualification.evidence_refs = vec![CATALOG_RELATIVE_PATH.to_string()];
        qualification.route_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
        qualification.candidate_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
        if clear_route {
            qualification.route_identity_ref = None;
        } else {
            qualification.candidate_identity_ref = None;
        }
        assert_has_code(
            &unbound_evidence,
            CatalogValidationCode::InvalidTargetQualification,
        );
    }

    let mut promoted_combined_m5 = load_catalog(&root);
    let qualification = &mut first_variant(&mut promoted_combined_m5).target_qualifications[2];
    qualification.availability = TargetAvailability::Available;
    qualification.evidence_state = EvidenceState::Proven;
    qualification.run_verdict = RunVerdict::Pass;
    qualification.freshness_state = FreshnessState::Current;
    qualification.route_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    qualification.candidate_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    qualification.evidence_refs = vec![CATALOG_RELATIVE_PATH.to_string()];
    assert_has_code(
        &promoted_combined_m5,
        CatalogValidationCode::InvalidTargetQualification,
    );
}

#[test]
fn schema_v1_rejects_each_positive_evidence_authority_axis_independently() {
    let root = repository_root();

    let mut forged_proven = load_catalog(&root);
    let qualification = &mut first_variant(&mut forged_proven).target_qualifications[0];
    qualification.availability = TargetAvailability::Available;
    qualification.evidence_state = EvidenceState::Proven;
    qualification.run_verdict = RunVerdict::Fail;
    qualification.freshness_state = FreshnessState::Stale;
    qualification.route_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    qualification.candidate_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    qualification.evidence_refs = vec![CATALOG_RELATIVE_PATH.to_string()];
    assert_has_code(
        &forged_proven,
        CatalogValidationCode::UnsupportedEvidenceAuthority,
    );

    let mut forged_pass = load_catalog(&root);
    let qualification = &mut first_variant(&mut forged_pass).target_qualifications[0];
    qualification.availability = TargetAvailability::Available;
    qualification.evidence_state = EvidenceState::FixtureOnly;
    qualification.run_verdict = RunVerdict::Pass;
    qualification.freshness_state = FreshnessState::Stale;
    qualification.route_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    qualification.candidate_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    qualification.evidence_refs = vec![CATALOG_RELATIVE_PATH.to_string()];
    assert_has_code(
        &forged_pass,
        CatalogValidationCode::UnsupportedEvidenceAuthority,
    );

    let mut forged_current = load_catalog(&root);
    let qualification = &mut first_variant(&mut forged_current).target_qualifications[0];
    qualification.availability = TargetAvailability::Available;
    qualification.evidence_state = EvidenceState::FixtureOnly;
    qualification.run_verdict = RunVerdict::Fail;
    qualification.freshness_state = FreshnessState::Current;
    qualification.route_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    qualification.candidate_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    qualification.evidence_refs = vec![CATALOG_RELATIVE_PATH.to_string()];
    assert_has_code(
        &forged_current,
        CatalogValidationCode::UnsupportedEvidenceAuthority,
    );
}

#[test]
fn contract_only_v1_cannot_mint_supported_or_evidence_bound_claims() {
    let root = repository_root();
    let mut catalog = load_catalog(&root);
    assert_eq!(
        catalog.catalog_claim_state,
        CatalogClaimState::ContractOnly,
        "fixture assumes initial catalog is contract-only"
    );
    let variant = catalog
        .variants
        .iter_mut()
        .find(|variant| variant.producer_coverage == ProducerCoverage::Direct)
        .expect("supported mutation starts from an exact producer binding");
    variant.support = SupportDeclaration::Supported {
        promotion_receipt_ref: CATALOG_RELATIVE_PATH.to_string(),
        promotion_receipt_sha256: "0".repeat(64),
    };
    for qualification in &mut variant.target_qualifications {
        qualification.availability = TargetAvailability::Available;
        qualification.evidence_state = EvidenceState::Proven;
        qualification.run_verdict = RunVerdict::Pass;
        qualification.freshness_state = FreshnessState::Current;
        qualification.route_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
        qualification.candidate_identity_ref = Some(CATALOG_RELATIVE_PATH.to_string());
        qualification.evidence_refs = vec![CATALOG_RELATIVE_PATH.to_string()];
        qualification.blocker_refs.clear();
    }
    assert_has_code(&catalog, CatalogValidationCode::ContractOnlySupportedClaim);
    assert_has_code(&catalog, CatalogValidationCode::UnsupportedClaimAuthority);
    assert_has_code(
        &catalog,
        CatalogValidationCode::UnsupportedEvidenceAuthority,
    );

    catalog.catalog_claim_state = CatalogClaimState::EvidenceBound;
    assert_has_code(&catalog, CatalogValidationCode::UnsupportedClaimAuthority);
}

#[test]
fn conditional_and_unavailable_payloads_fail_closed_without_explanations() {
    let root = repository_root();

    let mut empty_constraints = load_catalog(&root);
    let variant = first_variant(&mut empty_constraints);
    variant.support = SupportDeclaration::Conditional {
        reason: "synthetic conditional mutation".to_string(),
        constraints: Vec::new(),
        fallback: "retain the current safe path".to_string(),
        tracking_bead_ids: vec!["ft-interactive-swarm-product-convergence-7xqz4.11.15".to_string()],
    };
    assert_has_code(
        &empty_constraints,
        CatalogValidationCode::EmptyRequiredField,
    );

    let mut empty_unavailable_reason = load_catalog(&root);
    first_variant(&mut empty_unavailable_reason).support = SupportDeclaration::Unavailable {
        reason: String::new(),
        fallback: "use an explicitly qualified lane".to_string(),
        tracking_bead_ids: vec!["ft-interactive-swarm-product-convergence-7xqz4.11.15".to_string()],
    };
    assert_has_code(
        &empty_unavailable_reason,
        CatalogValidationCode::EmptyRequiredField,
    );
}

#[test]
fn deliberate_unavailable_variant_is_a_valid_non_claim() {
    let root = repository_root();
    let mut catalog = load_catalog(&root);
    first_variant(&mut catalog).support = SupportDeclaration::Unavailable {
        reason: "this product cell is deliberately unavailable".to_string(),
        fallback: "use a separately qualified conditional cell".to_string(),
        tracking_bead_ids: vec!["ft-interactive-swarm-product-convergence-7xqz4.11.15".to_string()],
    };
    let report = catalog.validate();
    assert!(
        report.valid,
        "an explicit unavailable product declaration must remain a valid non-claim: {:?}",
        report.errors
    );
}

#[test]
fn checked_in_negative_target_evidence_is_retained_per_lane() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    for variant in &catalog.variants {
        for qualification in &variant.target_qualifications {
            let controller = qualification.controller_target_class_id.as_str();
            if matches!(controller, "m5_native" | "m5_pro_max_native") {
                assert_eq!(
                    qualification.availability,
                    TargetAvailability::Unknown,
                    "{} must not extrapolate M4 evidence to {controller}",
                    qualification.qualification_id
                );
                assert_eq!(qualification.evidence_state, EvidenceState::Missing);
                assert_eq!(qualification.run_verdict, RunVerdict::NotRun);
                assert_eq!(qualification.freshness_state, FreshnessState::Unknown);
                assert!(qualification.route_identity_ref.is_none());
                assert!(qualification.candidate_identity_ref.is_none());
                assert!(qualification.evidence_refs.is_empty());
                assert!(qualification.blocker_refs.is_empty());
            } else if controller == "mac16_11_m4_pro"
                && variant.coverage.fleet_point == FleetPoint::Q200
            {
                assert_eq!(
                    qualification.availability,
                    TargetAvailability::Available,
                    "{} has target inventory but no exact q200 proof",
                    qualification.qualification_id
                );
                assert_eq!(
                    qualification.evidence_state,
                    EvidenceState::Missing,
                    "{} must not extrapolate the Linux high-core resource artifact",
                    qualification.qualification_id
                );
                assert_eq!(qualification.run_verdict, RunVerdict::NotRun);
                assert_eq!(qualification.freshness_state, FreshnessState::Unknown);
                assert!(qualification.route_identity_ref.is_none());
                assert!(qualification.candidate_identity_ref.is_none());
                assert!(qualification.evidence_refs.is_empty());
                assert!(qualification.blocker_refs.is_empty());
            }
        }
    }
}

#[test]
fn readme_claim_text_and_fingerprints_match_exact_bytes() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    let readme = fs::read_to_string(root.join("README.md")).expect("README.md should be readable");
    for mapping in &catalog.readme_mappings {
        assert_eq!(
            readme.match_indices(&mapping.claim_text).count(),
            1,
            "{} exact claim text must occur exactly once in README.md",
            mapping.mapping_id
        );
        let (_, fragment) = mapping
            .readme_ref
            .split_once('#')
            .expect("canonical README mappings carry a section fragment");
        let section = markdown_section(&readme, fragment).unwrap_or_else(|| {
            panic!(
                "{} section fragment `{fragment}` is absent or ambiguous",
                mapping.mapping_id
            )
        });
        assert!(
            section.contains(&mapping.claim_text),
            "{} claim text exists globally but not under {}",
            mapping.mapping_id,
            mapping.readme_ref
        );
        let actual = hex::encode(Sha256::digest(mapping.claim_text.as_bytes()));
        assert_eq!(
            mapping.claim_sha256, actual,
            "{} README claim fingerprint drifted",
            mapping.mapping_id
        );
    }
}

#[test]
fn target_mode_serde_uses_the_contractual_threadripper_spelling() {
    let encoded = serde_json::to_value(TargetMode::ThreadripperPro5995wxNative)
        .expect("target mode should serialize");
    assert_eq!(encoded, json!("threadripper_pro_5995wx_native"));
    let decoded: TargetMode =
        serde_json::from_value(encoded).expect("contractual target spelling should deserialize");
    assert_eq!(decoded, TargetMode::ThreadripperPro5995wxNative);
}

#[test]
fn automated_review_cannot_approve_or_mint_authority() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    let current = catalog
        .review_history
        .iter()
        .find(|review| review.reviewed_catalog_revision == catalog.catalog_revision)
        .expect("checked-in current catalog revision retains an informational review");
    assert_eq!(
        current.authority_kind,
        ReviewAuthorityKind::AutomatedInformational
    );
    assert_eq!(current.disposition, ReviewDisposition::Informational);
    assert!(current.reviewed_commit.is_none());
    assert!(current.authority_receipt_ref.is_none());
    assert!(current.authority_receipt_sha256.is_none());

    let mut automated_approval = catalog.clone();
    let review = automated_approval
        .review_history
        .first_mut()
        .expect("review fixture exists");
    review.disposition = ReviewDisposition::Approved;
    review.reviewed_commit = Some("0".repeat(40));
    review.authority_receipt_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    review.authority_receipt_sha256 = Some("0".repeat(64));
    assert_has_code(
        &automated_approval,
        CatalogValidationCode::InvalidReviewAuthority,
    );

    let mut automated_with_commit = catalog.clone();
    automated_with_commit.review_history[1].reviewed_commit = Some("0".repeat(40));
    assert_has_code(
        &automated_with_commit,
        CatalogValidationCode::InvalidReviewAuthority,
    );

    let mut forged_human_information = catalog.clone();
    forged_human_information.review_history[1].authority_kind =
        ReviewAuthorityKind::HumanProductOwner;
    assert_has_code(
        &forged_human_information,
        CatalogValidationCode::InvalidReviewAuthority,
    );

    let mut unsigned_human_approval = catalog.clone();
    let review = unsigned_human_approval
        .review_history
        .first_mut()
        .expect("review fixture exists");
    review.authority_kind = ReviewAuthorityKind::HumanProductOwner;
    review.disposition = ReviewDisposition::Approved;
    assert_has_code(
        &unsigned_human_approval,
        CatalogValidationCode::InvalidReviewAuthority,
    );

    let mut forged_human_approval = catalog;
    let mut forged = forged_human_approval.review_history[1].clone();
    forged.review_id = "ft.product-journey-review.forged-human".to_string();
    forged.reviewer = "Untrusted claimed product owner".to_string();
    forged.authority_kind = ReviewAuthorityKind::HumanProductOwner;
    forged.disposition = ReviewDisposition::Approved;
    forged.scope = vec!["catalog_contract".to_string()];
    forged.reviewed_commit = Some("0".repeat(40));
    forged.authority_receipt_ref = Some(CATALOG_RELATIVE_PATH.to_string());
    forged.authority_receipt_sha256 = Some(hex::encode(Sha256::digest(
        fs::read(root.join(CATALOG_RELATIVE_PATH))
            .expect("catalog bytes should be readable for forged approval"),
    )));
    forged_human_approval.review_history.push(forged);
    assert_has_code(
        &forged_human_approval,
        CatalogValidationCode::InvalidReviewAuthority,
    );
}

#[test]
fn every_declared_authority_or_promotion_receipt_is_content_bound() {
    let root = repository_root();
    let catalog = load_catalog(&root);

    for review in &catalog.review_history {
        match (
            review.authority_receipt_ref.as_deref(),
            review.authority_receipt_sha256.as_deref(),
        ) {
            (Some(reference), Some(expected)) => {
                assert_detached_sha256(&root, reference, expected);
            }
            (None, None) => {}
            _ => panic!(
                "{} has a receipt reference/hash shape that is not content-bindable",
                review.review_id
            ),
        }
    }

    for variant in &catalog.variants {
        if let SupportDeclaration::Supported {
            promotion_receipt_ref,
            promotion_receipt_sha256,
        } = &variant.support
        {
            assert_detached_sha256(&root, promotion_receipt_ref, promotion_receipt_sha256);
        }
    }
}

#[test]
fn canonical_ids_actor_transport_posture_and_release_scope_cannot_drift() {
    let root = repository_root();

    let mut variant_id = load_catalog(&root);
    first_variant(&mut variant_id).variant_id.push_str(".drift");
    assert_has_code(&variant_id, CatalogValidationCode::InvalidDefinition);

    let mut claim_id = load_catalog(&root);
    first_variant(&mut claim_id).claim_id.push_str(".drift");
    assert_has_code(&claim_id, CatalogValidationCode::InvalidDefinition);

    let mut actor = load_catalog(&root);
    first_variant(&mut actor).actor_mode = ActorMode::AutomationUnattended;
    assert_has_code(&actor, CatalogValidationCode::InvalidDefinition);

    let mut transport = load_catalog(&root);
    first_variant(&mut transport).transport = Transport::RemoteMux;
    assert_has_code(&transport, CatalogValidationCode::InvalidDefinition);

    let mut posture = load_catalog(&root);
    posture.topologies[0].target_posture.controller_modes.pop();
    assert_has_code(&posture, CatalogValidationCode::InvalidDefinition);

    let mut field_binding = load_catalog(&root);
    let first = field_binding.journey_definitions[0].field_bead_id.clone();
    field_binding.journey_definitions[0].field_bead_id =
        field_binding.journey_definitions[1].field_bead_id.clone();
    field_binding.journey_definitions[1].field_bead_id = first;
    assert_has_code(
        &field_binding,
        CatalogValidationCode::IncompleteFieldJourneyBinding,
    );

    let release_mutations: [fn(&mut ProductJourneyCatalog); 3] = [
        |catalog: &mut ProductJourneyCatalog| {
            catalog.gates[0].release_requirement = ReleaseRequirement::Optional;
        },
        |catalog: &mut ProductJourneyCatalog| {
            catalog.journey_definitions[0].release_requirement = ReleaseRequirement::Excluded;
        },
        |catalog: &mut ProductJourneyCatalog| {
            catalog.variants[0].release_requirement = ReleaseRequirement::Optional;
        },
    ];
    for mutate in release_mutations {
        let mut catalog = load_catalog(&root);
        mutate(&mut catalog);
        assert_has_code(&catalog, CatalogValidationCode::InvalidDefinition);
    }
}

#[test]
fn canonical_journey_gate_and_mapping_assignments_cannot_be_swapped() {
    let root = repository_root();

    let mut journey_personas = load_catalog(&root);
    let journey = journey_personas
        .journey_definitions
        .iter_mut()
        .find(|journey| journey.personas.len() > 1)
        .expect("multi-persona journey fixture exists");
    journey.personas.pop();
    assert_has_code(&journey_personas, CatalogValidationCode::InvalidDefinition);

    let mut journey_gate = load_catalog(&root);
    journey_gate.journey_definitions[0].gate_ids.pop();
    assert_has_code(&journey_gate, CatalogValidationCode::InvalidDefinition);

    let mut variant_journey = load_catalog(&root);
    let variant = first_variant(&mut variant_journey);
    variant.journey_ids.pop();
    variant
        .journey_ids
        .push("journey.component_crash_recovery".to_string());
    assert_has_code(&variant_journey, CatalogValidationCode::InvalidDefinition);

    let mut variant_gate = load_catalog(&root);
    first_variant(&mut variant_gate).gate_ids.pop();
    assert_has_code(&variant_gate, CatalogValidationCode::InvalidDefinition);

    let mut gate_producer = load_catalog(&root);
    gate_producer.gates[0].producer_bead_ids[0] =
        "ft-interactive-swarm-product-convergence-7xqz4.11.1".to_string();
    assert_has_code(&gate_producer, CatalogValidationCode::InvalidDefinition);

    let mut legacy_journeys = load_catalog(&root);
    let replacement = legacy_journeys.legacy_mappings[1].journey_ids.clone();
    legacy_journeys.legacy_mappings[0].journey_ids = replacement;
    assert_has_code(&legacy_journeys, CatalogValidationCode::InvalidDefinition);

    let mut legacy_source = load_catalog(&root);
    legacy_source.legacy_mappings[0].source_refs = vec!["docs/demo-scenarios.md".to_string()];
    assert_has_code(&legacy_source, CatalogValidationCode::InvalidDefinition);

    let mut readme_journeys = load_catalog(&root);
    let replacement = readme_journeys.readme_mappings[1].journey_ids.clone();
    readme_journeys.readme_mappings[0].journey_ids = replacement;
    assert_has_code(&readme_journeys, CatalogValidationCode::InvalidDefinition);

    let mut readme_ref = load_catalog(&root);
    readme_ref.readme_mappings[0].readme_ref = "README.md#tldr".to_string();
    assert_has_code(&readme_ref, CatalogValidationCode::InvalidDefinition);
}

#[test]
fn declared_review_and_change_metadata_are_revision_bound() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    assert_eq!(catalog.catalog_revision, "2026-07-27.3");
    assert_eq!(
        catalog
            .change_history
            .iter()
            .map(|record| record.catalog_revision.as_str())
            .collect::<Vec<_>>(),
        ["2026-07-27.1", "2026-07-27.2", "2026-07-27.3"]
    );
    assert_eq!(
        catalog
            .review_history
            .iter()
            .map(|record| record.reviewed_catalog_revision.as_str())
            .collect::<Vec<_>>(),
        ["2026-07-27.1", "2026-07-27.2", "2026-07-27.3"],
        "each declared revision must retain its own informational review metadata"
    );

    let mut rewritten_initial_review = catalog.clone();
    rewritten_initial_review.review_history[0].reviewed_catalog_revision =
        "2026-07-27.2".to_string();
    assert_has_code(
        &rewritten_initial_review,
        CatalogValidationCode::InvalidReviewAuthority,
    );

    let mut removed_initial_change = catalog.clone();
    removed_initial_change.change_history.remove(0);
    assert_has_code(
        &removed_initial_change,
        CatalogValidationCode::InvalidDefinition,
    );

    let mut ambient_unreviewed_revision = catalog.clone();
    ambient_unreviewed_revision.catalog_revision = "2026-07-27.4".to_string();
    assert_has_code(
        &ambient_unreviewed_revision,
        CatalogValidationCode::InvalidReviewAuthority,
    );
    assert_has_code(
        &ambient_unreviewed_revision,
        CatalogValidationCode::InvalidDefinition,
    );
}

#[test]
fn timestamps_and_initial_review_provenance_are_canonical() {
    let root = repository_root();

    let mut invalid_review_date = load_catalog(&root);
    invalid_review_date.review_history[0].reviewed_at_utc = "2026-02-30T12:00:00Z".to_string();
    assert_has_code(
        &invalid_review_date,
        CatalogValidationCode::InvalidReviewAuthority,
    );

    let mut noncanonical_change_zone = load_catalog(&root);
    noncanonical_change_zone.change_history[0].changed_at_utc =
        "2026-07-27T23:04:36+00:00".to_string();
    assert_has_code(
        &noncanonical_change_zone,
        CatalogValidationCode::InvalidDefinition,
    );

    let mut narrowed_review_sources = load_catalog(&root);
    narrowed_review_sources.review_history[0].source_refs.pop();
    assert_has_code(
        &narrowed_review_sources,
        CatalogValidationCode::InvalidReviewAuthority,
    );

    let mut rewritten_initial_reviewer = load_catalog(&root);
    rewritten_initial_reviewer.review_history[0].reviewer =
        "Claimed human product owner".to_string();
    assert_has_code(
        &rewritten_initial_reviewer,
        CatalogValidationCode::InvalidReviewAuthority,
    );

    let mut missing_required_setup_slot = load_catalog(&root);
    missing_required_setup_slot.journey_definitions[0]
        .setup
        .remove(0);
    assert_has_code(
        &missing_required_setup_slot,
        CatalogValidationCode::EmptyLifecyclePhase,
    );
}

#[test]
fn contradiction_ledger_is_closed_domain_and_resolution_evidence_bound() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    let ids = catalog
        .contradictions
        .iter()
        .map(|contradiction| contradiction.contradiction_id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        ids,
        BTreeSet::from([
            "contradiction.clean_first_hour_gap",
            "contradiction.legacy_rch_local_fallback",
            "contradiction.performance_q001_product_q002",
            "contradiction.persona_namespace_collision",
            "contradiction.readme_200_plus_scope",
            "contradiction.readme_capacity_overlap",
            "contradiction.readme_lossless_capture",
            "contradiction.remote_path_conflation",
        ])
    );

    let mut false_resolution = catalog.clone();
    let contradiction = false_resolution
        .contradictions
        .first_mut()
        .expect("contradiction fixture exists");
    contradiction.status = ContradictionStatus::Resolved;
    contradiction.resolution_refs = vec![CATALOG_RELATIVE_PATH.to_string()];
    assert_has_code(
        &false_resolution,
        CatalogValidationCode::InvalidContradiction,
    );

    let mut narrowed_clean_first_hour = catalog.clone();
    let contradiction = narrowed_clean_first_hour
        .contradictions
        .iter_mut()
        .find(|record| record.contradiction_id == "contradiction.clean_first_hour_gap")
        .expect("clean-first-hour contradiction fixture exists");
    contradiction.affected_claim_ids.pop();
    assert_has_code(
        &narrowed_clean_first_hour,
        CatalogValidationCode::InvalidContradiction,
    );

    let mut ambiguous_scope = catalog;
    let claim_id = ambiguous_scope.variants[0].claim_id.clone();
    let contradiction = ambiguous_scope
        .contradictions
        .first_mut()
        .expect("contradiction fixture exists");
    contradiction.blocks_all_claims = true;
    contradiction.affected_claim_ids = vec![claim_id];
    assert_has_code(
        &ambiguous_scope,
        CatalogValidationCode::InvalidContradiction,
    );
}

#[test]
fn checked_in_v2_catalog_is_typed_complete_and_exactly_migrates_v1_inventory() {
    let root = repository_root();
    let value = read_json(&root.join(CATALOG_V2_RELATIVE_PATH));
    let schema = load_schema_v2_validator(&root);
    let schema_errors = schema_errors(&schema, &value);
    assert!(
        schema_errors.is_empty(),
        "checked-in v2 catalog failed JSON Schema:\n{}",
        schema_errors.join("\n")
    );

    let v1 = load_catalog(&root);
    let v2 = load_catalog_v2(&root);
    let report = v2.validate();
    assert!(
        report.valid,
        "checked-in v2 catalog failed semantic validation:\n{}",
        report
            .errors
            .iter()
            .map(|error| format!("{} {}: {}", error.code.as_str(), error.path, error.detail))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(v2.phase_contracts.len(), REQUIRED_LIFECYCLE_PHASE_COUNT_V2);
    assert_eq!(v2.journey_producers.len(), REQUIRED_FIELD_JOURNEY_COUNT);
    assert_eq!(v2.closure_consumers.len(), REQUIRED_COVERAGE_CELL_COUNT);

    for producer in &v2.journey_producers {
        let source = v1
            .journey_definitions
            .iter()
            .find(|definition| definition.journey_id == producer.journey_id)
            .unwrap_or_else(|| panic!("v1 journey `{}` must exist", producer.journey_id));
        assert_eq!(producer.field_bead_id, source.field_bead_id);
        assert_eq!(
            producer.lifecycle.identity_preflight.steps.as_slice(),
            &source.setup[..1],
            "identity preflight must explicitly retain only v1 setup[0]"
        );
        assert_eq!(
            producer.lifecycle.clean_setup.steps.as_slice(),
            &source.setup[1..],
            "clean setup must explicitly retain only the later v1 setup steps"
        );
        assert_eq!(producer.lifecycle.steady_work.steps, source.steady_work);
        assert_eq!(
            producer.lifecycle.failure_overload.steps,
            source.failure_overload
        );
        assert_eq!(
            producer.lifecycle.recovery_convergence.steps,
            source.recovery
        );
        assert_eq!(producer.lifecycle.teardown_outcome.steps, source.teardown);
    }

    for consumer in &v2.closure_consumers {
        let source = v1
            .variants
            .iter()
            .find(|variant| variant.claim_id == consumer.claim_id)
            .unwrap_or_else(|| panic!("v1 claim `{}` must exist", consumer.claim_id));
        assert_eq!(consumer.coverage, source.coverage);
        assert_eq!(consumer.journey_ids, source.journey_ids);
    }
}

#[test]
fn v1_and_v2_decoders_have_no_implicit_compatibility_path() {
    let root = repository_root();
    let v1_bytes = fs::read(root.join(CATALOG_RELATIVE_PATH)).expect("read v1 catalog");
    let v2_bytes = fs::read(root.join(CATALOG_V2_RELATIVE_PATH)).expect("read v2 catalog");

    let v1_as_v2 = ProductJourneyCatalogV2::decode_json_bounded(&v1_bytes)
        .expect_err("v1 positional data must not auto-upgrade to v2");
    assert_eq!(v1_as_v2.code(), ProductJourneyDecodeCode::InvalidJson);

    let v2_as_v1 = ProductJourneyCatalog::decode_json_bounded(&v2_bytes)
        .expect_err("v2 typed data must not be accepted by the v1 decoder");
    assert_eq!(v2_as_v1.code(), ProductJourneyDecodeCode::InvalidJson);
}

#[test]
fn v2_schema_rejects_omitted_and_legacy_positional_lifecycle_fields() {
    let root = repository_root();
    let schema = load_schema_v2_validator(&root);
    let mut omitted = read_json(&root.join(CATALOG_V2_RELATIVE_PATH));
    omitted["journey_producers"][0]["lifecycle"]
        .as_object_mut()
        .expect("lifecycle object")
        .remove("identity_preflight");
    assert!(!schema.is_valid(&omitted));
    let omitted_bytes = serde_json::to_vec(&omitted).expect("serialize omitted-field mutation");
    assert_eq!(
        ProductJourneyCatalogV2::decode_json_bounded(&omitted_bytes)
            .expect_err("missing explicit phase must fail typed decode")
            .code(),
        ProductJourneyDecodeCode::InvalidJson
    );

    let mut legacy = read_json(&root.join(CATALOG_V2_RELATIVE_PATH));
    legacy["journey_producers"][0]["lifecycle"]["setup"] =
        json!(["positional preflight", "positional setup"]);
    assert!(!schema.is_valid(&legacy));
    let legacy_bytes = serde_json::to_vec(&legacy).expect("serialize legacy-field mutation");
    assert_eq!(
        ProductJourneyCatalogV2::decode_json_bounded(&legacy_bytes)
            .expect_err("legacy setup array must not be accepted")
            .code(),
        ProductJourneyDecodeCode::InvalidJson
    );
}

#[test]
fn v2_semantics_reject_swapped_empty_duplicated_and_collapsed_phases() {
    let root = repository_root();

    let mut swapped = load_catalog_v2(&root);
    swapped.journey_producers[0]
        .lifecycle
        .identity_preflight
        .contract_role = JourneyLifecycleRoleV2::CleanSetup;
    assert_v2_has_code(&swapped, CatalogValidationCode::SwappedLifecyclePhase);

    let mut empty = load_catalog_v2(&root);
    empty.journey_producers[0]
        .lifecycle
        .steady_work
        .steps
        .clear();
    assert_v2_has_code(&empty, CatalogValidationCode::EmptyRequiredField);

    let mut duplicated = load_catalog_v2(&root);
    let duplicated_steps = duplicated.journey_producers[0]
        .lifecycle
        .identity_preflight
        .steps
        .clone();
    duplicated.journey_producers[0].lifecycle.clean_setup.steps = duplicated_steps;
    assert_v2_has_code(&duplicated, CatalogValidationCode::DuplicateLifecyclePhase);

    let mut collapsed = load_catalog_v2(&root);
    let repeated = "one collapsed lifecycle action".to_string();
    for phase_index in 0..REQUIRED_LIFECYCLE_PHASE_COUNT_V2 {
        lifecycle_phase_v2_mut(&mut collapsed.journey_producers[0].lifecycle, phase_index).steps =
            vec![repeated.clone()];
    }
    assert_v2_has_code(&collapsed, CatalogValidationCode::DuplicateLifecyclePhase);
}

#[test]
fn v2_semantics_reject_post_mutation_preflight_recovery_without_failure_and_no_outcome_teardown() {
    let root = repository_root();

    let mut duplicate_requirement = load_catalog_v2(&root);
    duplicate_requirement.phase_contracts[0]
        .required_identities
        .push(JourneyIdentityRequirementV2::RendererDisplay);
    assert_v2_has_code(
        &duplicate_requirement,
        CatalogValidationCode::InvalidLifecycleContract,
    );

    let mut post_mutation_preflight = load_catalog_v2(&root);
    let preflight = post_mutation_preflight
        .phase_contracts
        .iter_mut()
        .find(|contract| contract.role == JourneyLifecycleRoleV2::IdentityPreflight)
        .expect("identity preflight contract");
    preflight
        .allowed_mutations
        .push(JourneyMutationClassV2::CleanSetup);
    assert_v2_has_code(
        &post_mutation_preflight,
        CatalogValidationCode::PostMutationPreflight,
    );

    let mut recovery_without_failure = load_catalog_v2(&root);
    let recovery = recovery_without_failure
        .phase_contracts
        .iter_mut()
        .find(|contract| contract.role == JourneyLifecycleRoleV2::RecoveryConvergence)
        .expect("recovery contract");
    recovery.required_preconditions = vec![JourneyPhasePreconditionV2::CleanSetupComplete];
    assert_v2_has_code(
        &recovery_without_failure,
        CatalogValidationCode::RecoveryWithoutFailure,
    );

    let mut teardown_without_outcome = load_catalog_v2(&root);
    let teardown = teardown_without_outcome
        .phase_contracts
        .iter_mut()
        .find(|contract| contract.role == JourneyLifecycleRoleV2::TeardownOutcome)
        .expect("teardown contract");
    teardown.required_outcomes = vec![JourneyPhaseOutcomeV2::ResourcesReleased];
    assert_v2_has_code(
        &teardown_without_outcome,
        CatalogValidationCode::TeardownWithoutOutcome,
    );
}

#[test]
fn v2_semantics_reject_incomplete_producer_and_consumer_migrations() {
    let root = repository_root();

    let mut missing_producer = load_catalog_v2(&root);
    missing_producer.journey_producers.pop();
    assert_v2_has_code(
        &missing_producer,
        CatalogValidationCode::InvalidLifecycleMigration,
    );

    let mut missing_consumer = load_catalog_v2(&root);
    missing_consumer.closure_consumers.pop();
    assert_v2_has_code(
        &missing_consumer,
        CatalogValidationCode::MissingRequiredCoverage,
    );

    let mut dangling_consumer = load_catalog_v2(&root);
    dangling_consumer.closure_consumers[0].journey_ids[0] = "journey.not_migrated".to_string();
    assert_v2_has_code(&dangling_consumer, CatalogValidationCode::DanglingReference);

    let mut wrong_but_valid_mapping = load_catalog_v2(&root);
    let replacement = wrong_but_valid_mapping
        .journey_producers
        .iter()
        .map(|producer| &producer.journey_id)
        .find(|journey_id| {
            !wrong_but_valid_mapping.closure_consumers[0]
                .journey_ids
                .contains(journey_id)
        })
        .expect("the first closure cell does not consume all fourteen journeys")
        .clone();
    wrong_but_valid_mapping.closure_consumers[0].journey_ids[0] = replacement;
    assert_v2_has_code(
        &wrong_but_valid_mapping,
        CatalogValidationCode::InvalidLifecycleMigration,
    );
}

proptest! {
    #[test]
    fn removing_any_materialized_variant_breaks_exact_coverage(
        index in 0usize..REQUIRED_COVERAGE_CELL_COUNT
    ) {
        let root = repository_root();
        let mut catalog = load_catalog(&root);
        prop_assume!(catalog.variants.len() == REQUIRED_COVERAGE_CELL_COUNT);
        catalog.variants.remove(index);
        let report = catalog.validate();
        prop_assert!(
            report.contains_code(CatalogValidationCode::MissingRequiredCoverage),
            "removing variant {index} did not break exact coverage"
        );
    }

    #[test]
    fn duplicating_any_materialized_variant_breaks_uniqueness(
        index in 0usize..REQUIRED_COVERAGE_CELL_COUNT
    ) {
        let root = repository_root();
        let mut catalog = load_catalog(&root);
        prop_assume!(catalog.variants.len() == REQUIRED_COVERAGE_CELL_COUNT);
        let duplicate = catalog.variants[index].clone();
        catalog.variants.push(duplicate);
        let report = catalog.validate();
        prop_assert!(
            report.contains_code(CatalogValidationCode::DuplicateCompositeKey)
                && report.contains_code(CatalogValidationCode::DuplicateClaimId),
            "duplicating variant {index} did not break key and claim uniqueness"
        );
    }

    #[test]
    fn assigning_any_wrong_v2_role_is_rejected_as_a_swapped_phase(
        producer_index in 0usize..REQUIRED_FIELD_JOURNEY_COUNT,
        phase_index in 0usize..REQUIRED_LIFECYCLE_PHASE_COUNT_V2,
        role_offset in 1usize..REQUIRED_LIFECYCLE_PHASE_COUNT_V2,
    ) {
        let root = repository_root();
        let mut catalog = load_catalog_v2(&root);
        prop_assume!(catalog.journey_producers.len() == REQUIRED_FIELD_JOURNEY_COUNT);
        let wrong_role = JourneyLifecycleRoleV2::ALL
            [(phase_index + role_offset) % REQUIRED_LIFECYCLE_PHASE_COUNT_V2];
        lifecycle_phase_v2_mut(
            &mut catalog.journey_producers[producer_index].lifecycle,
            phase_index,
        )
        .contract_role = wrong_role;
        let report = catalog.validate();
        prop_assert!(
            report.contains_code(CatalogValidationCode::SwappedLifecyclePhase),
            "producer {producer_index} phase {phase_index} accepted wrong role {wrong_role:?}"
        );
    }
}
