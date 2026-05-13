//! Regression guard for the ft-e87u6.6 attestation closing template.

use std::fs;
use std::path::{Path, PathBuf};

const TEMPLATE_REL_PATH: &str = "docs/release/attestation-bead-closing-template.md";
const CHECKLIST_REL_PATH: &str = "docs/release/attestation-checklist.md";

const REQUIRED_TEMPLATE_LINES: &[&str] = &[
    "Manifest slot category: `<category>`",
    "Artifact path: `<path>` (sha256 `<hash>`)",
    "Build smoke: `bash scripts/attestation-build.sh --version 0.0.0-dev --channel dev --sign unsigned` exit `<code>`",
    "Strict-deferred build: `bash scripts/attestation-build.sh ... --strict-deferred` exit `<code>`",
    "Verify round-trip: `bash scripts/attestation-verify.sh <bundle>` exit `<code>`",
    "Hedge alignment: `cargo test -p frankenterm-core --test readme_hedge_alignment` exit `<code>`",
    "Manifest completeness: `cargo test -p frankenterm-core --test attestation_manifest_completeness` exit `<code>`",
    "RCH artifact bundle: `<path>`",
];

const REQUIRED_FIELD_NAMES: &[&str] = &[
    "Manifest slot category: ",
    "Artifact path: ",
    "Build smoke:",
    "Strict-deferred build:",
    "Verify round-trip:",
    "Hedge alignment:",
    "Manifest completeness:",
    "RCH artifact bundle:",
];

const REQUIRED_BEAD_REFS: &[&str] = &["ft-187kv", "ft-e87u6.4", "ft-e87u6.5"];

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

#[test]
fn attestation_closing_template_contains_required_placeholder_lines() {
    let template = read_workspace_file(TEMPLATE_REL_PATH);

    for required in REQUIRED_TEMPLATE_LINES {
        assert!(
            template.contains(required),
            "attestation closing template is missing required placeholder line: {required}"
        );
    }
}

#[test]
fn attestation_closing_template_field_names_stay_grep_stable() {
    let template = read_workspace_file(TEMPLATE_REL_PATH);

    for required in REQUIRED_FIELD_NAMES {
        assert!(
            template.contains(required),
            "attestation closing template renamed required field name: {required}"
        );
    }
}

#[test]
fn attestation_closing_template_mentions_required_beads() {
    let template = read_workspace_file(TEMPLATE_REL_PATH);

    for bead_id in REQUIRED_BEAD_REFS {
        assert!(
            template.contains(bead_id),
            "attestation closing template must reference sibling bead {bead_id}"
        );
    }
}

#[test]
fn attestation_checklist_points_producing_beads_at_the_template_and_test() {
    let checklist = read_workspace_file(CHECKLIST_REL_PATH);

    for required in [
        "Producing-bead closing convention",
        TEMPLATE_REL_PATH,
        "ft-e87u6.5",
        "attestation_manifest_completeness",
    ] {
        assert!(
            checklist.contains(required),
            "{CHECKLIST_REL_PATH} is missing required closing-convention breadcrumb: {required}"
        );
    }
}
