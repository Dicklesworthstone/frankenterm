//! Regression-guard test for Python ↔ Rust cx-propagation
//! allow-list lockstep.
//!
//! **Bead:** [BR-RC-RUNTIME-SEMANTICS.G14.1.cont.drift] / `ft-jbsbx`.
//! **Script:** `scripts/check_cx_propagation_lockstep.py`.
//! **Sources:**
//! - `scripts/check_runtime_proof_coverage.py` (Python audit, ft-3kv6e).
//! - `lints/cx_propagation/src/allow_list.rs` (Rust analyzer, ft-t9a6q.1 +
//!   ft-l8bmk full-list port).
//!
//! Shells out to the lockstep script and asserts exit 0. Any
//! divergence between the two allow-lists fails this test before
//! CI even sees the PR — exactly the gate the bead asks for.
//!
//! The test is `cfg(unix)` because Windows shells out differently;
//! the script itself is portable Python and CI on Windows can run
//! it directly via the workflow YAML rather than this test.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn python_and_rust_allow_lists_are_in_lockstep() {
    let script = repo_root().join("scripts/check_cx_propagation_lockstep.py");
    assert!(
        script.is_file(),
        "lockstep script missing at {}",
        script.display()
    );
    let output = Command::new("python3")
        .arg(&script)
        .output()
        .expect("python3 must be available to run the lockstep guard");
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "cx-propagation allow-lists drifted between Python audit and Rust analyzer.\n\
             Stdout:\n{stdout}\nStderr:\n{stderr}\n\
             Run `scripts/check_cx_propagation_lockstep.py --print` for the diff."
        );
    }
}

#[test]
fn lockstep_script_emits_well_formed_json() {
    // Smoke test: --json mode produces valid JSON with the
    // documented top-level keys.
    let script = repo_root().join("scripts/check_cx_propagation_lockstep.py");
    let output = Command::new("python3")
        .arg(&script)
        .arg("--json")
        .output()
        .expect("python3 must be available");
    assert!(
        output.status.success(),
        "--json mode failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Documented top-level keys.
    assert!(stdout.contains("\"in_sync\""));
    assert!(stdout.contains("\"exempt_files\""));
    assert!(stdout.contains("\"wrapper_exemptions\""));
    assert!(stdout.contains("\"shared_count\""));
}
