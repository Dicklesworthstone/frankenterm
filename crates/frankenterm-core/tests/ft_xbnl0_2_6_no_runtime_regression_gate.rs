//! ft-xbnl0.2.6 — Repo-level invariant: no direct tokio/smol/async-io
//! reintroduction outside the explicitly quarantined surface.
//!
//! This test invokes the gate validator at
//! `scripts/check_no_runtime_regression.sh` against the canonical policy at
//! `docs/ft-xbnl0-2-6-no-runtime-regression-gate.json`. If the gate fails,
//! the failing report is included in the panic message so the regression is
//! diagnosable without re-running the script by hand.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    let core_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    core_manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("expected frankenterm-core to live under <workspace>/crates/")
}

fn skip_if_python_missing() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|out| !out.status.success())
        .unwrap_or(true)
}

fn run_gate(self_test: bool) -> (bool, String, String, PathBuf) {
    let root = workspace_root();
    let script = root.join("scripts/check_no_runtime_regression.sh");
    let policy = root.join("docs/ft-xbnl0-2-6-no-runtime-regression-gate.json");
    let out_dir = root.join("target/ft-xbnl0-2-6-gate");
    std::fs::create_dir_all(&out_dir).expect("create gate output dir");
    let out = out_dir.join(if self_test {
        "self-test-validation.json"
    } else {
        "validation.json"
    });

    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .arg("--policy-path")
        .arg(&policy)
        .arg("--output")
        .arg(&out);
    if self_test {
        cmd.arg("--self-test");
    }
    cmd.current_dir(&root);

    let output = cmd.output().expect("invoke gate validator");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), stdout, stderr, out)
}

#[test]
fn no_runtime_regression_gate_passes_against_canonical_policy() {
    if skip_if_python_missing() {
        eprintln!("python3 unavailable; skipping ft-xbnl0.2.6 gate test");
        return;
    }

    let (success, stdout, stderr, report_path) = run_gate(false);
    if !success {
        let report = std::fs::read_to_string(&report_path).unwrap_or_default();
        panic!(
            "ft-xbnl0.2.6 no-runtime regression gate FAILED.\n\
             Report: {}\n\
             stdout: {stdout}\n\
             stderr: {stderr}\n\
             ---report contents---\n{report}",
            report_path.display()
        );
    }

    let report =
        std::fs::read_to_string(&report_path).expect("gate must write a report on success");
    assert!(
        report.contains("\"status\": \"passed\""),
        "expected passed report; got: {report}"
    );
}

#[test]
fn no_runtime_regression_gate_self_tests_pass() {
    if skip_if_python_missing() {
        eprintln!("python3 unavailable; skipping ft-xbnl0.2.6 gate self-test");
        return;
    }

    let (success, stdout, stderr, report_path) = run_gate(true);
    assert!(
        success,
        "self-test invocation must succeed.\nstdout: {stdout}\nstderr: {stderr}\nreport: {}",
        report_path.display()
    );
}

#[test]
fn gate_policy_file_is_well_formed_and_self_consistent() {
    let policy_path = workspace_root().join("docs/ft-xbnl0-2-6-no-runtime-regression-gate.json");
    let raw = std::fs::read_to_string(&policy_path).expect("policy file must exist");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("policy must be valid JSON");

    assert_eq!(
        parsed["contract_id"], "ft.xbnl0.2.6.no_runtime_regression_gate.v1",
        "policy contract_id must match the validator's expected contract"
    );
    assert_eq!(
        parsed["bead_id"], "ft-xbnl0.2.6",
        "policy must declare its owning bead"
    );

    // Each manifest_dependency_allowlist entry must declare owner+reason+removal_bead.
    let allowlist = parsed["manifest_dependency_allowlist"]
        .as_object()
        .expect("manifest_dependency_allowlist must be an object");
    for (dep, paths) in allowlist {
        let paths = paths
            .as_object()
            .unwrap_or_else(|| panic!("manifest allowlist for {dep} must be an object of paths"));
        for (path, meta) in paths {
            for field in ["owner", "reason", "removal_bead"] {
                assert!(
                    meta.get(field).and_then(|v| v.as_str()).is_some(),
                    "manifest allowlist entry {dep}:{path} missing field {field}"
                );
            }
        }
    }

    // Each source_quarantine entry must declare owner+reason+removal_bead+allowed_tokens.
    let src_quarantine = parsed["source_quarantine"]
        .as_object()
        .expect("source_quarantine must be an object");
    for (path, meta) in src_quarantine {
        for field in ["owner", "reason", "removal_bead"] {
            assert!(
                meta.get(field).and_then(|v| v.as_str()).is_some(),
                "source_quarantine entry {path} missing field {field}"
            );
        }
        let tokens = meta["allowed_tokens"]
            .as_array()
            .unwrap_or_else(|| panic!("source_quarantine entry {path} missing allowed_tokens"));
        assert!(
            !tokens.is_empty(),
            "source_quarantine entry {path} allowed_tokens must be non-empty"
        );
    }

    // runtime_compat.rs must be quarantined per the bead's design notes.
    assert!(
        src_quarantine.contains_key("crates/frankenterm-core/src/runtime_compat.rs"),
        "ft-xbnl0.2.6 design notes name runtime_compat.rs as the only intentional Tokio quarantine root; it must appear in source_quarantine"
    );
}
