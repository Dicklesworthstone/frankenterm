use serde_json::Value;
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
fn proof_doctor_handoff_shell_wrapper_is_fail_closed() {
    let repo = repo_root();
    let ft_binary = assert_cmd::cargo::cargo_bin("ft");
    let script = repo.join("tests/e2e/test_ft_782hw_4_proof_doctor_handoff.sh");

    let output = Command::new("bash")
        .arg(&script)
        .env("FT_BINARY", &ft_binary)
        .current_dir(&repo)
        .output()
        .expect("run proof-doctor handoff E2E wrapper");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "proof-doctor handoff E2E wrapper failed\nstdout:\n{stdout}\nstderr:\n{stderr}\n{}",
        latest_run_diagnostics(&repo)
    );

    let summary: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("parse E2E summary JSON: {err}\nstdout:\n{stdout}"));
    assert_eq!(summary["outcome"], "passed");
    assert_eq!(summary["counts"]["failed"], 0);
    assert_eq!(summary["counts"]["proof_records"], 8);
    assert!(
        summary["structured_log"]
            .as_str()
            .is_some_and(|path| path.ends_with("/structured.log")),
        "summary must point at retained structured log:\n{summary}"
    );
    assert!(
        summary["commands"]
            .as_str()
            .is_some_and(|path| path.ends_with("/commands.txt")),
        "summary must point at retained command transcript:\n{summary}"
    );
}

/// The wrapper reports a step failure by writing its summary and structured log
/// to the artifact directory and exiting 1 with no output at all, so a failure
/// on a remote proof worker arrives as an empty panic message. Read the newest
/// run back so the assertion says which step failed and why (ft-yykm1: three
/// hours of a release lane were spent rediscovering this by hand).
fn latest_run_diagnostics(repo: &Path) -> String {
    let runs = repo.join("tests/e2e/artifacts/goal-line/ft-782hw.4/proof_doctor_handoff");
    let Some(newest) = std::fs::read_dir(&runs).ok().and_then(|entries| {
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .max_by(|left, right| left.file_name().cmp(&right.file_name()))
    }) else {
        return format!("no wrapper run directory under {}", runs.display());
    };

    let summary = std::fs::read_to_string(newest.join("summary.json"))
        .unwrap_or_else(|error| format!("<unreadable summary.json: {error}>"));
    let structured = std::fs::read_to_string(newest.join("structured.log"))
        .unwrap_or_else(|error| format!("<unreadable structured.log: {error}>"));
    let failures: Vec<&str> = structured
        .lines()
        .filter(|line| line.contains("\"status\":\"failed\""))
        .collect();

    format!(
        "wrapper run: {}\nsummary.json:\n{summary}\nfailed steps:\n{}",
        newest.display(),
        if failures.is_empty() {
            "<none recorded>".to_string()
        } else {
            failures.join("\n")
        }
    )
}
