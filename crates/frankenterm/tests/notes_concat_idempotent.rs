use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under repo/crates/frankenterm")
        .to_path_buf()
}

#[cfg(unix)]
fn install_fake_br(bin_dir: &Path) {
    let path = bin_dir.join("br");
    fs::write(
        &path,
        r#"#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  show)
    cat "$FAKE_BR_SHOW_JSON"
    ;;
  update)
    printf '%s' "${4:-}" > "$FAKE_BR_UPDATED_NOTES"
    ;;
  *)
    exit 12
    ;;
esac
"#,
    )
    .expect("write fake br");
    let mut perms = fs::metadata(&path).expect("fake br metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod fake br");
}

#[cfg(unix)]
#[test]
fn notes_concat_is_idempotent_for_existing_section() {
    let repo = repo_root();
    let temp = tempfile::tempdir().expect("temp dir");
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    install_fake_br(&bin_dir);

    let existing_notes = "## 2026-05-12 - New section\n\nnew body\n";
    let show_json = temp.path().join("show.json");
    let updated_notes = temp.path().join("updated-notes.md");
    fs::write(
        &show_json,
        serde_json::json!([{
            "id": "ft-fixture.1",
            "notes": existing_notes
        }])
        .to_string(),
    )
    .expect("write show JSON");

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("bash")
        .arg(repo.join("scripts/safe-br-update-notes.sh"))
        .arg("ft-fixture.1")
        .arg("New section")
        .arg("new body")
        .env("PATH", path)
        .env("FAKE_BR_SHOW_JSON", &show_json)
        .env("FAKE_BR_UPDATED_NOTES", &updated_notes)
        .env("SAFE_BR_UPDATE_NOTES_DATE", "2026-05-12")
        .current_dir(&repo)
        .output()
        .expect("run safe notes helper");

    assert!(
        output.status.success(),
        "helper failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("unchanged"),
        "expected unchanged no-op, stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !updated_notes.exists(),
        "idempotent run should not call br update"
    );
}

#[cfg(not(unix))]
#[test]
fn notes_concat_is_idempotent_for_existing_section() {
    // The helper is a Bash script and is exercised on Unix CI lanes.
}
