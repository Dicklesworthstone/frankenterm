//! Regression guard for ft-e87u6.5 attestation manifest completeness.
//!
//! Every checked-in manifest slot must either point at an existing repo
//! artifact or defer to a live bead with enough graph context for a future
//! agent to find the producer.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use proptest::prelude::*;
use serde_json::{Value, json};

const MANIFEST_REL_PATH: &str = "docs/attestations/manifest.json";
const ISSUES_REL_PATH: &str = ".beads/issues.jsonl";
const ROOT_BEAD_ID: &str = "ft-e87u6";

#[derive(Debug, Clone)]
struct BeadRecord {
    status: String,
    edges: Vec<BeadEdge>,
}

#[derive(Debug, Clone)]
struct BeadEdge {
    issue_id: String,
    depends_on_id: String,
    edge_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManifestError {
    JsonParse(String),
    MissingSlotsArray,
    MissingCategory {
        slot_index: usize,
    },
    AmbiguousSlot {
        slot_index: usize,
        category: String,
    },
    MissingArtifact {
        slot_index: usize,
        category: String,
        path: String,
    },
    UnsafePath {
        slot_index: usize,
        category: String,
        path: String,
    },
    UnfilledSlot {
        slot_index: usize,
        category: String,
    },
    MissingDeferredReason {
        slot_index: usize,
        category: String,
        bead_id: String,
    },
    DeferredBeadMissing {
        slot_index: usize,
        category: String,
        bead_id: String,
    },
    DeferredBeadInactive {
        slot_index: usize,
        category: String,
        bead_id: String,
        status: String,
    },
    DeferredBeadOrphan {
        slot_index: usize,
        category: String,
        bead_id: String,
    },
    DuplicateSlotFingerprint {
        slot_index: usize,
        category: String,
        fingerprint: String,
    },
    RequiredCategoryMissing {
        category: String,
    },
}

impl ManifestError {
    fn code(&self) -> &'static str {
        match self {
            ManifestError::JsonParse(_) => "json_parse",
            ManifestError::MissingSlotsArray => "slots_array_missing",
            ManifestError::MissingCategory { .. } => "category_missing",
            ManifestError::AmbiguousSlot { .. } => "path_and_deferred_both_set",
            ManifestError::MissingArtifact { .. } => "path_resolves_missing",
            ManifestError::UnsafePath { .. } => "unsafe_repo_relative_path",
            ManifestError::UnfilledSlot { .. } => "unfilled_slot",
            ManifestError::MissingDeferredReason { .. } => "deferred_reason_missing",
            ManifestError::DeferredBeadMissing { .. } => "deferred_bead_missing",
            ManifestError::DeferredBeadInactive { .. } => "deferred_bead_not_active",
            ManifestError::DeferredBeadOrphan { .. } => "orphan_deferred_bead",
            ManifestError::DuplicateSlotFingerprint { .. } => "duplicate_slot_fingerprint",
            ManifestError::RequiredCategoryMissing { .. } => "required_category_missing",
        }
    }

    fn detail(&self) -> String {
        match self {
            ManifestError::JsonParse(err) => format!("manifest is not JSON: {err}"),
            ManifestError::MissingSlotsArray => "manifest.slots must be an array".to_string(),
            ManifestError::MissingCategory { slot_index } => {
                format!("slot[{slot_index}] is missing category")
            }
            ManifestError::AmbiguousSlot {
                slot_index,
                category,
            } => {
                format!(
                    "slot[{slot_index}] {category}: path and deferred_to_bead cannot both be set"
                )
            }
            ManifestError::MissingArtifact {
                slot_index,
                category,
                path,
            } => {
                format!("slot[{slot_index}] {category}: path={path:?} but file missing on disk")
            }
            ManifestError::UnsafePath {
                slot_index,
                category,
                path,
            } => {
                format!(
                    "slot[{slot_index}] {category}: path={path:?} is not a safe repo-relative path"
                )
            }
            ManifestError::UnfilledSlot {
                slot_index,
                category,
            } => {
                format!(
                    "slot[{slot_index}] {category}: both path and deferred_to_bead are null - this is the exact ft-e87u6 NO_BEAD gap"
                )
            }
            ManifestError::MissingDeferredReason {
                slot_index,
                category,
                bead_id,
            } => {
                format!(
                    "slot[{slot_index}] {category}: deferred_to_bead={bead_id:?} is missing deferred_reason"
                )
            }
            ManifestError::DeferredBeadMissing {
                slot_index,
                category,
                bead_id,
            } => {
                format!(
                    "slot[{slot_index}] {category}: deferred_to_bead={bead_id:?} was not found in .beads/issues.jsonl"
                )
            }
            ManifestError::DeferredBeadInactive {
                slot_index,
                category,
                bead_id,
                status,
            } => {
                format!(
                    "slot[{slot_index}] {category}: deferred_to_bead={bead_id:?} but that bead is not active (status={status:?})"
                )
            }
            ManifestError::DeferredBeadOrphan {
                slot_index,
                category,
                bead_id,
            } => {
                format!(
                    "slot[{slot_index}] {category}: deferred_to_bead={bead_id:?} has no parent-child/blocks edge back to ft-e87u6"
                )
            }
            ManifestError::DuplicateSlotFingerprint {
                slot_index,
                category,
                fingerprint,
            } => {
                format!("slot[{slot_index}] {category}: duplicate slot fingerprint {fingerprint:?}")
            }
            ManifestError::RequiredCategoryMissing { category } => {
                format!("required_categories entry {category:?} has no matching slot")
            }
        }
    }
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn read_workspace_file(rel_path: &str) -> String {
    let path = workspace_root().join(rel_path);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

fn read_live_issues_if_manifest_defers(manifest_text: &str) -> String {
    let Ok(manifest) = serde_json::from_str::<Value>(manifest_text) else {
        return String::new();
    };
    let manifest_needs_beads = manifest
        .get("slots")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|slot| {
            slot.get("deferred_to_bead")
                .and_then(Value::as_str)
                .is_some()
        });
    if manifest_needs_beads {
        read_workspace_file(ISSUES_REL_PATH)
    } else {
        String::new()
    }
}

fn parse_beads_issues_jsonl(jsonl: &str) -> HashMap<String, BeadRecord> {
    let mut records: HashMap<String, BeadRecord> = HashMap::new();

    for line in jsonl.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(issue) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = issue.get("id").and_then(Value::as_str) else {
            continue;
        };
        let status = issue
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut edges = Vec::new();
        for dep in issue
            .get("dependencies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let issue_id = dep
                .get("issue_id")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string();
            let depends_on_id = dep
                .get("depends_on_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let edge_type = dep
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !issue_id.is_empty() && !depends_on_id.is_empty() {
                edges.push(BeadEdge {
                    issue_id,
                    depends_on_id,
                    edge_type,
                });
            }
        }

        records.insert(id.to_string(), BeadRecord { status, edges });
    }

    records
}

fn is_live_deferred_status(status: &str) -> bool {
    matches!(status, "open" | "in_progress" | "blocked")
}

fn is_safe_repo_relative_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return false;
    }
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn deferred_bead_has_root_edge(bead_id: &str, record: &BeadRecord) -> bool {
    let root_child_prefix = format!("{ROOT_BEAD_ID}.");
    record.edges.iter().any(|edge| {
        matches!(edge.edge_type.as_str(), "parent-child" | "blocks")
            && ((edge.issue_id == bead_id
                && (edge.depends_on_id == ROOT_BEAD_ID
                    || edge.depends_on_id.starts_with(&root_child_prefix)))
                || (edge.issue_id == ROOT_BEAD_ID && edge.depends_on_id == bead_id))
    })
}

fn slot_fingerprint(slot: &Value, category: &str) -> String {
    let path = slot.get("path").and_then(Value::as_str).unwrap_or("");
    let deferred = slot
        .get("deferred_to_bead")
        .and_then(Value::as_str)
        .unwrap_or("");
    format!("{category}\u{0}{path}\u{0}{deferred}")
}

fn validate_manifest_text(
    manifest_text: &str,
    issues_text: &str,
    repo_root: &Path,
) -> Result<(), Vec<ManifestError>> {
    let manifest: Value = match serde_json::from_str(manifest_text) {
        Ok(manifest) => manifest,
        Err(err) => return Err(vec![ManifestError::JsonParse(err.to_string())]),
    };
    let Some(slots) = manifest.get("slots").and_then(Value::as_array) else {
        return Err(vec![ManifestError::MissingSlotsArray]);
    };
    let beads = parse_beads_issues_jsonl(issues_text);
    let mut errors = Vec::new();
    let mut seen_slot_fingerprints = HashSet::new();
    let mut seen_categories = HashSet::new();

    for (slot_index, slot) in slots.iter().enumerate() {
        let Some(category) = slot.get("category").and_then(Value::as_str) else {
            errors.push(ManifestError::MissingCategory { slot_index });
            continue;
        };
        seen_categories.insert(category.to_string());
        let path = slot.get("path").and_then(Value::as_str);
        let deferred = slot.get("deferred_to_bead").and_then(Value::as_str);
        let fingerprint = slot_fingerprint(slot, category);
        if !seen_slot_fingerprints.insert(fingerprint.clone()) {
            errors.push(ManifestError::DuplicateSlotFingerprint {
                slot_index,
                category: category.to_string(),
                fingerprint,
            });
        }

        match (path, deferred) {
            (Some(_), Some(_)) => errors.push(ManifestError::AmbiguousSlot {
                slot_index,
                category: category.to_string(),
            }),
            (Some(path), None) => {
                if !is_safe_repo_relative_path(path) {
                    errors.push(ManifestError::UnsafePath {
                        slot_index,
                        category: category.to_string(),
                        path: path.to_string(),
                    });
                } else if !repo_root.join(path).is_file() {
                    errors.push(ManifestError::MissingArtifact {
                        slot_index,
                        category: category.to_string(),
                        path: path.to_string(),
                    });
                }
            }
            (None, Some(bead_id)) => {
                let reason = slot
                    .get("deferred_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if reason.trim().is_empty() {
                    errors.push(ManifestError::MissingDeferredReason {
                        slot_index,
                        category: category.to_string(),
                        bead_id: bead_id.to_string(),
                    });
                }

                let Some(record) = beads.get(bead_id) else {
                    errors.push(ManifestError::DeferredBeadMissing {
                        slot_index,
                        category: category.to_string(),
                        bead_id: bead_id.to_string(),
                    });
                    continue;
                };

                if !is_live_deferred_status(&record.status) {
                    errors.push(ManifestError::DeferredBeadInactive {
                        slot_index,
                        category: category.to_string(),
                        bead_id: bead_id.to_string(),
                        status: record.status.clone(),
                    });
                }
                if !deferred_bead_has_root_edge(bead_id, record) {
                    errors.push(ManifestError::DeferredBeadOrphan {
                        slot_index,
                        category: category.to_string(),
                        bead_id: bead_id.to_string(),
                    });
                }
            }
            (None, None) => errors.push(ManifestError::UnfilledSlot {
                slot_index,
                category: category.to_string(),
            }),
        }
    }

    for required in manifest
        .get("required_categories")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !seen_categories.contains(required) {
            errors.push(ManifestError::RequiredCategoryMissing {
                category: required.to_string(),
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn valid_manifest_slot() -> Value {
    json!({
        "category": "perf/headline-claims",
        "path": "docs/perf/headline-claims.json",
        "media_type": "application/json",
        "produced_by_bead": "ft-syqcz.3",
        "description": "headline claims matrix"
    })
}

fn manifest_with_slot(slot: Value) -> String {
    json!({
        "$schema": "./schema.json#/$defs/manifestPlaceholder",
        "required_categories": ["perf/headline-claims"],
        "slots": [slot]
    })
    .to_string()
}

fn synthetic_issues(status: &str, edge_type: &str) -> String {
    json!({
        "id": "ft-e87u6.99",
        "title": "synthetic deferred producer",
        "status": status,
        "dependencies": [
            {
                "issue_id": "ft-e87u6.99",
                "depends_on_id": ROOT_BEAD_ID,
                "type": edge_type,
                "created_at": "2026-05-13T00:00:00Z",
                "created_by": "test"
            }
        ]
    })
    .to_string()
}

fn assert_error_contains(errors: &[ManifestError], code: &str, text: &str) {
    assert!(
        errors
            .iter()
            .any(|err| err.code() == code && err.detail().contains(text)),
        "expected error code={code:?} containing {text:?}, got:\n{}",
        errors
            .iter()
            .map(ManifestError::detail)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn every_slot_is_resolved_or_explicitly_deferred() {
    let manifest_text = read_workspace_file(MANIFEST_REL_PATH);
    let issues_text = read_live_issues_if_manifest_defers(&manifest_text);
    if let Err(errors) = validate_manifest_text(&manifest_text, &issues_text, &workspace_root()) {
        panic!(
            "attestation manifest is incoherent:\n  - {}",
            errors
                .iter()
                .map(ManifestError::detail)
                .collect::<Vec<_>>()
                .join("\n  - ")
        );
    }
}

#[test]
fn mutation_path_to_missing_file_is_rejected() {
    let mut slot = valid_manifest_slot();
    slot["path"] = json!("docs/attestations/__missing_ft_e87u6_5__.json");
    let errors = validate_manifest_text(&manifest_with_slot(slot), "", &workspace_root())
        .expect_err("missing path mutation must fail");
    assert_error_contains(&errors, "path_resolves_missing", "file missing");
}

#[test]
fn mutation_both_path_and_deferred_is_rejected() {
    let mut slot = valid_manifest_slot();
    slot["deferred_to_bead"] = json!("ft-e87u6.99");
    slot["deferred_reason"] = json!("synthetic ambiguity");
    let errors = validate_manifest_text(
        &manifest_with_slot(slot),
        &synthetic_issues("open", "parent-child"),
        &workspace_root(),
    )
    .expect_err("path plus deferred mutation must fail");
    assert_error_contains(&errors, "path_and_deferred_both_set", "cannot both be set");
}

#[test]
fn mutation_both_null_is_rejected() {
    let mut slot = valid_manifest_slot();
    slot.as_object_mut().expect("slot is object").remove("path");
    let errors = validate_manifest_text(&manifest_with_slot(slot), "", &workspace_root())
        .expect_err("both-null mutation must fail");
    assert_error_contains(&errors, "unfilled_slot", "exact ft-e87u6 NO_BEAD gap");
}

#[test]
fn mutation_required_category_without_slot_is_rejected() {
    let manifest = json!({
        "$schema": "./schema.json#/$defs/manifestPlaceholder",
        "required_categories": ["perf/headline-claims", "security/passive-watch"],
        "slots": [valid_manifest_slot()]
    })
    .to_string();
    let errors = validate_manifest_text(&manifest, "", &workspace_root())
        .expect_err("missing required category slot mutation must fail");
    assert_error_contains(
        &errors,
        "required_category_missing",
        "security/passive-watch",
    );
}

#[test]
fn mutation_deferred_to_closed_bead_is_rejected() {
    let mut slot = valid_manifest_slot();
    slot["path"] = Value::Null;
    slot["deferred_to_bead"] = json!("ft-e87u6.99");
    slot["deferred_reason"] = json!("synthetic closed bead");
    let errors = validate_manifest_text(
        &manifest_with_slot(slot),
        &synthetic_issues("closed", "parent-child"),
        &workspace_root(),
    )
    .expect_err("closed deferred bead mutation must fail");
    assert_error_contains(&errors, "deferred_bead_not_active", "not active");
}

#[test]
fn blocked_deferred_bead_counts_as_live() {
    let mut slot = valid_manifest_slot();
    slot["path"] = Value::Null;
    slot["deferred_to_bead"] = json!("ft-e87u6.99");
    slot["deferred_reason"] = json!("synthetic blocked producer is still active");
    validate_manifest_text(
        &manifest_with_slot(slot),
        &synthetic_issues("blocked", "parent-child"),
        &workspace_root(),
    )
    .expect("blocked deferred bead should count as active");
}

#[test]
fn deferred_beads_have_proper_graph_edges() {
    let mut slot = valid_manifest_slot();
    slot["path"] = Value::Null;
    slot["deferred_to_bead"] = json!("ft-e87u6.99");
    slot["deferred_reason"] = json!("synthetic producer with graph edge");
    validate_manifest_text(
        &manifest_with_slot(slot),
        &synthetic_issues("open", "blocks"),
        &workspace_root(),
    )
    .expect("deferred bead with ft-e87u6 blocks edge should validate");
}

#[test]
fn deferred_bead_without_graph_edge_is_rejected() {
    let mut slot = valid_manifest_slot();
    slot["path"] = Value::Null;
    slot["deferred_to_bead"] = json!("ft-unrelated.1");
    slot["deferred_reason"] = json!("synthetic orphan producer");
    let issues = json!({
        "id": "ft-unrelated.1",
        "title": "unrelated producer",
        "status": "open",
        "dependencies": [
            {
                "issue_id": "ft-unrelated.1",
                "depends_on_id": "ft-other",
                "type": "related"
            }
        ]
    })
    .to_string();
    let errors = validate_manifest_text(&manifest_with_slot(slot), &issues, &workspace_root())
        .expect_err("orphan deferred bead must fail");
    assert_error_contains(
        &errors,
        "orphan_deferred_bead",
        "no parent-child/blocks edge",
    );
}

#[test]
fn deferred_e87u6_bead_without_graph_edge_is_rejected() {
    let mut slot = valid_manifest_slot();
    slot["path"] = Value::Null;
    slot["deferred_to_bead"] = json!("ft-e87u6.99");
    slot["deferred_reason"] = json!("synthetic same-epic producer without graph edge");
    let issues = json!({
        "id": "ft-e87u6.99",
        "title": "same-epic producer without edge",
        "status": "open",
        "dependencies": []
    })
    .to_string();
    let errors = validate_manifest_text(&manifest_with_slot(slot), &issues, &workspace_root())
        .expect_err("same-epic deferred bead without graph edge must fail");
    assert_error_contains(
        &errors,
        "orphan_deferred_bead",
        "no parent-child/blocks edge",
    );
}

#[test]
fn deferred_bead_with_unrelated_issue_id_edge_is_rejected() {
    let mut slot = valid_manifest_slot();
    slot["path"] = Value::Null;
    slot["deferred_to_bead"] = json!("ft-e87u6.99");
    slot["deferred_reason"] = json!("synthetic producer with unrelated edge owner");
    let issues = json!({
        "id": "ft-e87u6.99",
        "title": "same-epic producer with unrelated edge owner",
        "status": "open",
        "dependencies": [
            {
                "issue_id": "ft-other.1",
                "depends_on_id": "ft-e87u6.1",
                "type": "blocks"
            }
        ]
    })
    .to_string();
    let errors = validate_manifest_text(&manifest_with_slot(slot), &issues, &workspace_root())
        .expect_err("deferred bead edge owned by another issue must fail");
    assert_error_contains(
        &errors,
        "orphan_deferred_bead",
        "no parent-child/blocks edge",
    );
}

#[derive(Debug, Clone)]
enum ManifestMutation {
    ValidResolved,
    ValidDeferred { status: &'static str },
    MissingPath,
    BothNull,
    BothSet,
    ClosedDeferred,
    MissingDeferredReason,
    OrphanDeferred,
    SameEpicOrphanDeferred,
    WrongIssueEdgeDeferred,
    DuplicateSlot,
    MissingRequiredCategory,
}

impl ManifestMutation {
    fn is_valid_shape(&self) -> bool {
        matches!(
            self,
            ManifestMutation::ValidResolved
                | ManifestMutation::ValidDeferred {
                    status: "open" | "in_progress" | "blocked"
                }
        )
    }
}

fn mutation_strategy() -> impl Strategy<Value = ManifestMutation> {
    prop_oneof![
        Just(ManifestMutation::ValidResolved),
        Just(ManifestMutation::ValidDeferred { status: "open" }),
        Just(ManifestMutation::ValidDeferred {
            status: "in_progress"
        }),
        Just(ManifestMutation::ValidDeferred { status: "blocked" }),
        Just(ManifestMutation::MissingPath),
        Just(ManifestMutation::BothNull),
        Just(ManifestMutation::BothSet),
        Just(ManifestMutation::ClosedDeferred),
        Just(ManifestMutation::MissingDeferredReason),
        Just(ManifestMutation::OrphanDeferred),
        Just(ManifestMutation::SameEpicOrphanDeferred),
        Just(ManifestMutation::WrongIssueEdgeDeferred),
        Just(ManifestMutation::DuplicateSlot),
        Just(ManifestMutation::MissingRequiredCategory),
    ]
}

fn apply_mutation(mutation: &ManifestMutation) -> (String, String) {
    let mut slot = valid_manifest_slot();
    let mut issues = synthetic_issues("open", "parent-child");
    match mutation {
        ManifestMutation::ValidResolved => {}
        ManifestMutation::ValidDeferred { status } => {
            slot["path"] = Value::Null;
            slot["deferred_to_bead"] = json!("ft-e87u6.99");
            slot["deferred_reason"] = json!("valid synthetic deferral");
            issues = synthetic_issues(status, "parent-child");
        }
        ManifestMutation::MissingPath => {
            slot["path"] = json!("docs/attestations/__missing_ft_e87u6_5__.json");
        }
        ManifestMutation::BothNull => {
            slot.as_object_mut().expect("slot is object").remove("path");
        }
        ManifestMutation::BothSet => {
            slot["deferred_to_bead"] = json!("ft-e87u6.99");
            slot["deferred_reason"] = json!("ambiguous synthetic deferral");
        }
        ManifestMutation::ClosedDeferred => {
            slot["path"] = Value::Null;
            slot["deferred_to_bead"] = json!("ft-e87u6.99");
            slot["deferred_reason"] = json!("closed synthetic producer");
            issues = synthetic_issues("closed", "parent-child");
        }
        ManifestMutation::MissingDeferredReason => {
            slot["path"] = Value::Null;
            slot["deferred_to_bead"] = json!("ft-e87u6.99");
        }
        ManifestMutation::OrphanDeferred => {
            slot["path"] = Value::Null;
            slot["deferred_to_bead"] = json!("ft-unrelated.1");
            slot["deferred_reason"] = json!("orphan synthetic producer");
            issues = json!({
                "id": "ft-unrelated.1",
                "status": "open",
                "dependencies": []
            })
            .to_string();
        }
        ManifestMutation::SameEpicOrphanDeferred => {
            slot["path"] = Value::Null;
            slot["deferred_to_bead"] = json!("ft-e87u6.99");
            slot["deferred_reason"] = json!("same-epic orphan synthetic producer");
            issues = json!({
                "id": "ft-e87u6.99",
                "status": "open",
                "dependencies": []
            })
            .to_string();
        }
        ManifestMutation::WrongIssueEdgeDeferred => {
            slot["path"] = Value::Null;
            slot["deferred_to_bead"] = json!("ft-e87u6.99");
            slot["deferred_reason"] = json!("wrong issue edge synthetic producer");
            issues = json!({
                "id": "ft-e87u6.99",
                "status": "open",
                "dependencies": [
                    {
                        "issue_id": "ft-other.1",
                        "depends_on_id": "ft-e87u6.1",
                        "type": "blocks"
                    }
                ]
            })
            .to_string();
        }
        ManifestMutation::DuplicateSlot => {
            return (
                json!({
                    "$schema": "./schema.json#/$defs/manifestPlaceholder",
                    "required_categories": ["perf/headline-claims"],
                    "slots": [slot.clone(), slot]
                })
                .to_string(),
                issues,
            );
        }
        ManifestMutation::MissingRequiredCategory => {
            return (
                json!({
                    "$schema": "./schema.json#/$defs/manifestPlaceholder",
                    "required_categories": ["perf/headline-claims", "security/passive-watch"],
                    "slots": [slot]
                })
                .to_string(),
                issues,
            );
        }
    }
    (manifest_with_slot(slot), issues)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        .. ProptestConfig::default()
    })]

    #[test]
    fn manifest_invariants_hold_under_random_perturbation(mutation in mutation_strategy()) {
        let (manifest, issues) = apply_mutation(&mutation);
        let result = validate_manifest_text(&manifest, &issues, &workspace_root());
        prop_assert_eq!(
            result.is_ok(),
            mutation.is_valid_shape(),
            "mutation {:?} produced result {:?}",
            mutation,
            result
        );
    }
}
