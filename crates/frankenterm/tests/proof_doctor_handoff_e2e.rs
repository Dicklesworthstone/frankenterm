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
        "proof-doctor handoff E2E wrapper failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
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
