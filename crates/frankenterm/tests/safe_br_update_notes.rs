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
fn install_fake_br(bin_dir: &Path) -> PathBuf {
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
    [[ "${3:-}" == "--notes" ]] || exit 11
    printf '%s' "${4:-}" > "$FAKE_BR_UPDATED_NOTES"
    printf '{"ok":true}\n'
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
    path
}

#[cfg(unix)]
#[test]
fn safe_br_update_notes_preserves_existing_notes() {
    let repo = repo_root();
    let temp = tempfile::tempdir().expect("temp dir");
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    install_fake_br(&bin_dir);

    let show_json = temp.path().join("show.json");
    let updated_notes = temp.path().join("updated-notes.md");
    fs::write(
        &show_json,
        serde_json::json!([{
            "id": "ft-fixture.1",
            "notes": "## 2026-05-11 - Prior section\n\nprior body\n"
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

    let updated = fs::read_to_string(updated_notes).expect("updated notes written");
    assert!(updated.contains("## 2026-05-11 - Prior section"));
    assert!(updated.contains("prior body"));
    assert!(updated.contains("## 2026-05-12 - New section"));
    assert!(updated.contains("new body"));
    assert!(
        updated.find("Prior section") < updated.find("New section"),
        "prior notes should remain before appended notes:\n{updated}"
    );
}

#[cfg(not(unix))]
#[test]
fn safe_br_update_notes_preserves_existing_notes() {
    // The helper is a Bash script and is exercised on Unix CI lanes.
}
