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
fn raw_br_update_notes_replacement_check_warns_on_section_loss() {
    let repo = repo_root();
    let temp = tempfile::tempdir().expect("temp dir");
    let existing = temp.path().join("existing.md");
    let candidate = temp.path().join("candidate.md");

    fs::write(
        &existing,
        "## 2026-05-12 - Round 1\n\n### Test companion\nprior\n\n### Operator surface\nprior\n\n### Degradation behavior\nprior\n\n### Proof category\nprior\n\n## 2026-05-12 - Round 2\n\nlater\n",
    )
    .expect("write existing notes");
    fs::write(&candidate, "## 2026-05-12 - Round 2\n\nlater\n").expect("write candidate notes");

    let output = Command::new("bash")
        .arg(repo.join("scripts/safe-br-update-notes.sh"))
        .arg("--check-raw-replacement")
        .arg(&existing)
        .arg(&candidate)
        .current_dir(&repo)
        .output()
        .expect("run raw replacement check");

    assert!(
        !output.status.success(),
        "raw replacement check should fail when sections are lost"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("raw br update --notes would replace"),
        "missing replacement warning:\n{stderr}"
    );
    assert!(
        stderr.contains("drops required section: Test companion"),
        "missing required-section warning:\n{stderr}"
    );
}
