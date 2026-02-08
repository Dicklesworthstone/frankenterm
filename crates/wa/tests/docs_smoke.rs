//! Docs smoke tests (wa-nu4.3.9.9)
//!
//! Validates that quickstart commands referenced in docs remain executable.
//! Runs in a temp environment to avoid touching real user configs.
//!
//! Artifact capture: each test emits structured artifacts via eprintln
//! for CI debugging. On failure, artifacts include stdout/stderr and
//! environment info.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

/// Build a wa command configured to run in a temp workspace.
///
/// Sets WA_WORKSPACE to a temp dir so commands don't touch real state.
#[allow(deprecated)]
fn wa_cmd() -> Command {
    let mut cmd = Command::cargo_bin("wa").expect("wa binary should be built");
    // Use temp workspace to avoid touching real state
    let tmp = std::env::temp_dir().join(format!("wa_smoke_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).ok();
    cmd.env("WA_WORKSPACE", tmp.to_string_lossy().to_string());
    // Prevent any real WezTerm interaction
    cmd.env("WA_WEZTERM_CLI", "/nonexistent/wezterm");
    cmd
}

/// Emit an artifact for CI debugging.
fn emit_artifact(label: &str, content: &str) {
    eprintln!("[ARTIFACT][docs-smoke] {label}:\n{content}");
}

/// Emit environment info artifact.
fn emit_env_artifact() {
    let info = format!(
        "os={}\narch={}\nrustc={}\npid={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        option_env!("RUSTC_VERSION").unwrap_or("unknown"),
        std::process::id(),
    );
    emit_artifact("env", &info);
}

fn artifact_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wa_smoke_artifacts_{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn save_artifact(name: &str, content: &str) {
    let dir = artifact_dir();
    let path = dir.join(name);
    std::fs::write(&path, content).ok();
}

// =============================================================================
// Quickstart command smoke tests
// =============================================================================

#[test]
fn smoke_wa_help() {
    emit_env_artifact();

    let output = wa_cmd()
        .arg("--help")
        .output()
        .expect("wa --help should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    save_artifact("help_stdout.txt", &stdout);
    save_artifact("help_stderr.txt", &stderr);
    emit_artifact("wa_help_stdout", &stdout);

    assert!(
        output.status.success(),
        "wa --help should exit 0, got: {}",
        output.status
    );
    assert!(
        stdout.contains("Usage") || stdout.contains("usage"),
        "wa --help should contain usage info"
    );
    assert!(
        stdout.contains("wa") || stdout.contains("WezTerm"),
        "wa --help should mention wa"
    );
}

#[test]
fn smoke_wa_version() {
    let output = wa_cmd()
        .arg("--version")
        .output()
        .expect("wa --version should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    save_artifact("version_stdout.txt", &stdout);
    emit_artifact("wa_version_stdout", &stdout);

    assert!(output.status.success(), "wa --version should exit 0");
    assert!(
        stdout.contains("wa") || stdout.contains("0."),
        "wa --version should contain version info"
    );
}

#[test]
fn smoke_wa_version_full() {
    // `wa version --full` shows detailed build metadata
    let output = wa_cmd()
        .args(["version", "--full"])
        .output()
        .expect("wa version --full should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("version_full_stdout.txt", &stdout);
    emit_artifact("wa_version_full", &stdout);

    // Should succeed or at least not panic
    assert!(
        output.status.success() || !stderr.contains("panicked"),
        "wa version --full should not panic"
    );
}

#[test]
fn smoke_wa_doctor_json() {
    let output = wa_cmd()
        .args(["doctor", "--json"])
        .output()
        .expect("wa doctor --json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("doctor_json_stdout.txt", &stdout);
    save_artifact("doctor_json_stderr.txt", &stderr);
    emit_artifact("wa_doctor_json", &stdout);

    // Doctor may report warnings (no WezTerm running) but should not panic.
    // In JSON mode, it should produce parseable JSON regardless of pass/fail.
    assert!(
        !stderr.contains("panicked"),
        "wa doctor --json should not panic"
    );

    // If it succeeded, stdout should be valid JSON
    if output.status.success() {
        assert!(
            serde_json::from_str::<serde_json::Value>(&stdout).is_ok(),
            "wa doctor --json should produce valid JSON when successful"
        );
    }
}

#[test]
fn smoke_wa_doctor_plain() {
    let output = wa_cmd()
        .arg("doctor")
        .output()
        .expect("wa doctor should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("doctor_plain_stdout.txt", &stdout);
    save_artifact("doctor_plain_stderr.txt", &stderr);
    emit_artifact("wa_doctor_plain", &stdout);

    // Doctor should not panic; it may exit non-zero if WezTerm is missing
    assert!(!stderr.contains("panicked"), "wa doctor should not panic");
}

#[test]
fn smoke_wa_setup_dry_run() {
    let output = wa_cmd()
        .args(["setup", "--dry-run"])
        .output()
        .expect("wa setup --dry-run should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("setup_dry_run_stdout.txt", &stdout);
    save_artifact("setup_dry_run_stderr.txt", &stderr);
    emit_artifact("wa_setup_dry_run", &stdout);

    // Dry run should not panic and should not modify any files
    assert!(
        !stderr.contains("panicked"),
        "wa setup --dry-run should not panic"
    );
}

#[test]
fn smoke_wa_robot_quick_start() {
    let output = wa_cmd()
        .args(["robot", "quick-start"])
        .output()
        .expect("wa robot quick-start should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("robot_quickstart_stdout.txt", &stdout);
    save_artifact("robot_quickstart_stderr.txt", &stderr);
    emit_artifact("wa_robot_quickstart", &stdout);

    assert!(
        output.status.success(),
        "wa robot quick-start should exit 0, stderr: {stderr}"
    );

    // Quick-start should output structured data (JSON for robot mode)
    let parsed = serde_json::from_str::<serde_json::Value>(&stdout);
    assert!(
        parsed.is_ok(),
        "wa robot quick-start should output valid JSON"
    );
}

#[test]
fn smoke_wa_robot_default() {
    // `wa robot` with no subcommand defaults to quick-start
    let output = wa_cmd()
        .arg("robot")
        .output()
        .expect("wa robot should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    save_artifact("robot_default_stdout.txt", &stdout);

    assert!(output.status.success(), "wa robot (default) should exit 0");
}

#[test]
fn smoke_wa_export_help() {
    // Export help should always work without a DB
    let output = wa_cmd()
        .args(["export", "--help"])
        .output()
        .expect("wa export --help should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    save_artifact("export_help_stdout.txt", &stdout);

    assert!(output.status.success(), "wa export --help should exit 0");
    assert!(
        stdout.contains("segments") || stdout.contains("Export"),
        "wa export --help should list export kinds"
    );
}

#[test]
fn smoke_wa_robot_health() {
    let output = wa_cmd()
        .args(["robot", "health"])
        .output()
        .expect("wa robot health should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    save_artifact("robot_health_stdout.txt", &stdout);
    save_artifact("robot_health_stderr.txt", &stderr);
    emit_artifact("wa_robot_health", &stdout);

    assert!(
        output.status.success(),
        "wa robot health should exit 0, stderr: {stderr}"
    );

    // Should produce valid JSON with version field
    let parsed = serde_json::from_str::<serde_json::Value>(&stdout);
    assert!(parsed.is_ok(), "wa robot health should output valid JSON");
    let val = parsed.unwrap();
    // Robot response wraps data
    assert!(
        val["data"]["version"].is_string() || val["version"].is_string(),
        "wa robot health should include version"
    );
}

#[test]
fn smoke_robot_playbook_commands_emit_json_envelopes() {
    let commands: [(&str, &[&str]); 4] = [
        ("robot_state", &["robot", "--format", "json", "state"]),
        (
            "robot_search",
            &[
                "robot",
                "--format",
                "json",
                "search",
                "playbook-smoke",
                "--limit",
                "1",
            ],
        ),
        (
            "robot_events",
            &["robot", "--format", "json", "events", "--limit", "1"],
        ),
        (
            "robot_workflow_list",
            &["robot", "--format", "json", "workflow", "list"],
        ),
    ];

    for (label, args) in commands {
        let output = wa_cmd()
            .args(args)
            .output()
            .expect("playbook command should execute");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        save_artifact(&format!("{label}_stdout.txt"), &stdout);
        save_artifact(&format!("{label}_stderr.txt"), &stderr);

        assert!(
            !stderr.contains("panicked"),
            "{label} should not panic, stderr: {stderr}"
        );

        let parsed = serde_json::from_str::<serde_json::Value>(&stdout).unwrap_or_else(|e| {
            panic!("{label} should emit valid JSON envelope, parse error: {e}, stdout: {stdout}")
        });
        assert!(
            parsed
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .is_some(),
            "{label} JSON should include boolean 'ok' field: {parsed}"
        );
    }
}

// =============================================================================
// Predicate-based tests (using assert_cmd sugar)
// =============================================================================

#[test]
fn smoke_help_contains_subcommands() {
    wa_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("setup"))
        .stdout(predicate::str::contains("export"))
        .stdout(predicate::str::contains("accounts"));
}

#[test]
fn smoke_wa_accounts_help() {
    let output = wa_cmd()
        .args(["accounts", "--help"])
        .output()
        .expect("wa accounts --help should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    save_artifact("accounts_help_stdout.txt", &stdout);

    assert!(output.status.success(), "wa accounts --help should exit 0");
    assert!(
        stdout.contains("accounts") || stdout.contains("Accounts"),
        "wa accounts --help should mention accounts"
    );
    assert!(
        stdout.contains("refresh") || stdout.contains("Refresh"),
        "wa accounts --help should mention refresh subcommand"
    );
}

#[test]
fn smoke_unknown_subcommand_fails() {
    wa_cmd().arg("nonexistent-command-xyz").assert().failure();
}

// =============================================================================
// Summary artifact generation
// =============================================================================

#[test]
fn smoke_generate_summary() {
    // This test runs last (alphabetically) and generates a summary artifact
    let commands = vec![
        ("wa --help", vec!["--help"]),
        ("wa --version", vec!["--version"]),
        ("wa doctor --json", vec!["doctor", "--json"]),
        ("wa robot quick-start", vec!["robot", "quick-start"]),
        ("wa export --help", vec!["export", "--help"]),
    ];

    let mut results = Vec::new();
    for (name, args) in &commands {
        let start = std::time::Instant::now();
        let output = wa_cmd()
            .args(args)
            .output()
            .expect("command should execute");
        let duration_ms = start.elapsed().as_millis();
        let passed = output.status.success();
        results.push(serde_json::json!({
            "command": name,
            "passed": passed,
            "exit_code": output.status.code(),
            "duration_ms": duration_ms,
            "stdout_len": output.stdout.len(),
            "stderr_len": output.stderr.len(),
        }));
    }

    let summary = serde_json::json!({
        "test": "docs_smoke",
        "total": results.len(),
        "passed": results.iter().filter(|r| r["passed"] == true).count(),
        "results": results,
    });

    let summary_str = serde_json::to_string_pretty(&summary).unwrap();
    save_artifact("summary.json", &summary_str);
    emit_artifact("summary", &summary_str);
}
