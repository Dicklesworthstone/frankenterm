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

#[test]
fn reality_check_structure_rejects_silent_close_canary() {
    let repo = repo_root();
    let temp = tempfile::tempdir().expect("temp dir");
    let issues = temp.path().join("issues.jsonl");
    let rows = [
        serde_json::json!({
            "id": "ft-fixture",
            "title": "fixture epic",
            "status": "open",
            "description": "proof_category: process"
        }),
        serde_json::json!({
            "id": "ft-fixture.1",
            "title": "silently closed malformed child",
            "status": "closed",
            "created_at": "2026-05-12T19:00:00Z",
            "description": "Closed without the reality-check template.",
            "comments": []
        }),
    ]
    .into_iter()
    .map(|value| serde_json::to_string(&value).expect("serialize fixture row"))
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    fs::write(&issues, rows).expect("write fixture JSONL");

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
        !output.status.success(),
        "validator accepted silent close canary:\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("parse JSON output");
    assert_eq!(payload["ok"], false);
    let violations = payload["violations"].as_array().expect("violations array");
    assert!(
        violations
            .iter()
            .any(|item| item["kind"] == "missing_proof_category")
    );
    assert!(
        violations
            .iter()
            .any(|item| item["kind"] == "missing_closeout_evidence_comment")
    );
}
