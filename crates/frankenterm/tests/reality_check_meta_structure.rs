use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under repo/crates/frankenterm")
        .to_path_buf()
}

fn write_fixture(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("issues.jsonl");
    fs::write(&path, body).expect("write fixture JSONL");
    path
}

fn valid_fixture() -> String {
    [
        serde_json::json!({
            "id": "ft-fixture",
            "title": "fixture epic",
            "status": "open",
            "description": "proof_category: process"
        }),
        serde_json::json!({
            "id": "ft-fixture.1",
            "title": "well-formed fixture child",
            "status": "closed",
            "created_at": "2026-05-12T19:00:00Z",
            "description": "Background: fixture.\n\nWhy this matters: fixture.\n\nAcceptance criteria: fixture.\n\nReferences: fixture.\n\n### Test companion\nfixture.\n\n### Operator surface\nfixture.\n\n### Degradation behavior\nfixture.\n\n### Proof category\n4 (conformance)\n\nproof_category: 4 (conformance)",
            "comments": [{
                "text": "G55 affected-bead audit: verified docs/example and command output."
            }]
        }),
    ]
    .into_iter()
    .map(|value| serde_json::to_string(&value).expect("serialize fixture row"))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

#[test]
fn reality_check_structure_accepts_well_formed_fixture() {
    let repo = repo_root();
    let temp = tempfile::tempdir().expect("temp dir");
    let issues = write_fixture(temp.path(), &valid_fixture());
    let output = Command::new("bash")
        .arg(repo.join("scripts/check-reality-check-bead-structure.sh"))
        .arg("--beads")
        .arg(&issues)
        .arg("--epic-id")
        .arg("ft-fixture")
        .arg("--strict-all")
        .arg("--json")
        .current_dir(&repo)
        .output()
        .expect("run structure validator");
    assert!(
        output.status.success(),
        "validator rejected valid fixture:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("parse JSON output");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["summary"]["error_count"], 0);
}
