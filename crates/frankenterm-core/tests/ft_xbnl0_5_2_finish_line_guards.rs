//! ft-xbnl0.5.2 — Repo-level invariant: every finish-line guard listed in
//! the manifest must pass and must be reachable through a single
//! composition entry point.
//!
//! This test shells out to `scripts/check_finish_line_guards.sh` with the
//! cargo-test guard explicitly skipped (we're already INSIDE cargo test —
//! re-invoking it here would recurse). Failure output includes the full
//! composition report so the regression is diagnosable from the test
//! log without re-running the script manually.

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

fn skip_if_prereqs_missing() -> Option<&'static str> {
    for tool in ["python3", "jq", "bash"] {
        if Command::new(tool)
            .arg("--version")
            .output()
            .map(|out| !out.status.success())
            .unwrap_or(true)
        {
            return Some(tool);
        }
    }
    None
}

#[test]
fn finish_line_guard_composition_passes() {
    if let Some(missing) = skip_if_prereqs_missing() {
        eprintln!("{missing} unavailable; skipping ft-xbnl0.5.2 composition test");
        return;
    }

    let root = workspace_root();
    let script = root.join("scripts/check_finish_line_guards.sh");
    let manifest = root.join("docs/ft-xbnl0-5-2-finish-line-guards.json");
    let out_dir = root.join("target/ft-xbnl0-5-2-guards");
    std::fs::create_dir_all(&out_dir).expect("create out dir");
    let out = out_dir.join("validation.json");

    assert!(
        script.exists(),
        "composition script missing: {}",
        script.display()
    );
    assert!(
        manifest.exists(),
        "composition manifest missing: {}",
        manifest.display()
    );

    let output = Command::new("bash")
        .arg(&script)
        .arg("--manifest")
        .arg(&manifest)
        .arg("--output")
        .arg(&out)
        .env("FT_XBNL0_5_2_SKIP_CARGO_TEST", "1")
        .current_dir(&root)
        .output()
        .expect("invoke composition");

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let report = std::fs::read_to_string(&out).unwrap_or_default();
        panic!(
            "ft-xbnl0.5.2 finish-line guard composition FAILED.\n\
             Report: {}\n\
             stdout: {stdout}\n\
             stderr: {stderr}\n\
             ---report contents---\n{report}",
            out.display()
        );
    }

    let report = std::fs::read_to_string(&out).expect("composition must write a report");
    let parsed: serde_json::Value =
        serde_json::from_str(&report).expect("composition report must be valid JSON");
    assert_eq!(
        parsed["status"], "passed",
        "expected overall status=passed; got: {report}"
    );
    assert_eq!(
        parsed["bead_id"], "ft-xbnl0.5.2",
        "composition must declare its owning bead"
    );
}

#[test]
fn manifest_is_well_formed_and_lists_expected_guards() {
    let manifest_path = workspace_root().join("docs/ft-xbnl0-5-2-finish-line-guards.json");
    let raw = std::fs::read_to_string(&manifest_path).expect("manifest must exist");
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).expect("manifest must be valid JSON");

    assert_eq!(
        parsed["contract_id"], "ft.xbnl0.5.2.finish_line_guards.v1",
        "manifest contract_id must match composition script expectation"
    );

    let guards = parsed["guards"]
        .as_array()
        .expect("guards must be an array");
    let guard_ids: Vec<String> = guards
        .iter()
        .map(|g| g["guard_id"].as_str().unwrap_or("").to_string())
        .collect();

    // The four permanent finish-line invariants must each appear.
    for required in [
        "no_runtime_regression",
        "asupersync_cutover_runtime_guards",
        "fake_sdk_capability_contract",
        "finish_line_verification_contract_shape",
    ] {
        assert!(
            guard_ids.iter().any(|gid| gid == required),
            "manifest must list guard {required}; found {guard_ids:?}"
        );
    }

    // Every guard entry must declare upstream_bead, invariant, and
    // failure_signatures so contributors can understand the regression.
    for guard in guards {
        let gid = guard["guard_id"].as_str().unwrap_or("<no guard_id>");
        for field in ["guard_id", "invariant", "upstream_bead"] {
            assert!(
                guard.get(field).and_then(|v| v.as_str()).is_some(),
                "guard {gid} missing required field {field}"
            );
        }
    }
}

#[test]
fn ci_binding_points_to_expected_workflow() {
    let manifest_path = workspace_root().join("docs/ft-xbnl0-5-2-finish-line-guards.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let workflow_rel = parsed["ci_binding"]["workflow_path"]
        .as_str()
        .expect("ci_binding.workflow_path required");
    let workflow_abs = workspace_root().join(workflow_rel);
    assert!(
        workflow_abs.exists(),
        "ci_binding.workflow_path points to missing file: {}",
        workflow_abs.display()
    );

    // The workflow must name the composition script so CI actually runs it.
    let workflow = std::fs::read_to_string(&workflow_abs).expect("workflow readable");
    assert!(
        workflow.contains("check_finish_line_guards.sh"),
        "CI workflow must invoke the composition script"
    );
    assert!(
        workflow.contains("ft_xbnl0_5_2_finish_line_guards"),
        "CI workflow must exercise the ft-xbnl0.5.2 integration test surface"
    );
}

#[test]
fn local_contributor_path_points_to_real_guard_commands() {
    let manifest_path = workspace_root().join("docs/ft-xbnl0-5-2-finish-line-guards.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let commands = parsed["local_contributor_path"]["commands"]
        .as_array()
        .expect("local_contributor_path.commands must be an array");
    let command_strings: Vec<&str> = commands
        .iter()
        .map(|value| value.as_str().expect("command entries must be strings"))
        .collect();

    assert!(
        command_strings
            .iter()
            .any(|command| command.contains("bash scripts/check_finish_line_guards.sh")),
        "local contributor path must include the composition script entrypoint"
    );
    assert!(
        command_strings.iter().any(|command| {
            command.contains("cargo test -p frankenterm-core --test ft_xbnl0_5_2_finish_line_guards")
        }),
        "local contributor path must point at the real ft_xbnl0_5_2_finish_line_guards integration test"
    );
}
