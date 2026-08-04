//! Scripted e2e wrapper for Antigravity and legacy Gemini session-resume fixtures.
//!
//! The companion script creates retained isolated HOME/PATH fixtures. This test
//! validates them through the session_resume bridge so RCH can run the same e2e
//! scenarios via an ordinary `cargo test` target.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde_json::{Value, json};

use frankenterm_core::session_resume::{
    ANTIGRAVITY_MODEL, AgentProvider, SessionResumeConfig, SessionResumeError, SessionResumer,
    antigravity_native_resume_plan, provider_from_list_entry,
};

const BEAD_ID: &str = "ft-agy-provider-q8o4y-685af.3";

#[test]
fn scripted_antigravity_session_resume_harness_validates_all_scenarios() {
    let project_root = project_root();
    let (fixture_root, artifact_dir, log_jsonl) = fixture_root_or_prepare(&project_root);
    let manifest_path = fixture_root.join("manifest.json");
    let manifest: Value = read_json(&manifest_path);
    let scenarios = manifest
        .get("scenarios")
        .and_then(Value::as_array)
        .expect("root manifest scenarios array");

    log_jsonl_record(
        &log_jsonl,
        json!({
            "bead_id": BEAD_ID,
            "scenario_id": "harness",
            "step": "rust_validation_start",
            "command": "cargo test e2e_antigravity_session_resume_script",
            "cwd": project_root.display().to_string(),
            "temp_home": null,
            "provider": null,
            "session_id": null,
            "path": fixture_root.display().to_string(),
            "exit_code": null,
            "expected": "all required scenarios validate",
            "actual": format!("{} scenarios loaded", scenarios.len()),
            "duration_ms": 0,
            "status": "running",
        }),
    );

    let mut passed = 0usize;
    for scenario in scenarios {
        validate_scenario(scenario, &log_jsonl, &project_root);
        passed += 1;
    }
    log_optional_real_smoke(&log_jsonl, &project_root);

    let summary = json!({
        "schema_version": "ft.agy-session-resume.rust-summary.v1",
        "bead_id": BEAD_ID,
        "artifact_dir": artifact_dir.display().to_string(),
        "fixture_root": fixture_root.display().to_string(),
        "log_jsonl": log_jsonl.display().to_string(),
        "scenarios_passed": passed,
        "user_surface_status": manifest.get("user_surface_status").cloned().unwrap_or(Value::Null),
        "user_surface_note": manifest.get("user_surface_note").cloned().unwrap_or(Value::Null),
    });
    fs::write(
        artifact_dir.join("rust-validation-summary.json"),
        serde_json::to_string_pretty(&summary).expect("serialize rust validation summary") + "\n",
    )
    .expect("write rust validation summary");

    log_jsonl_record(
        &log_jsonl,
        json!({
            "bead_id": BEAD_ID,
            "scenario_id": "harness",
            "step": "rust_validation_complete",
            "command": "cargo test e2e_antigravity_session_resume_script",
            "cwd": project_root.display().to_string(),
            "temp_home": null,
            "provider": null,
            "session_id": null,
            "path": artifact_dir.display().to_string(),
            "exit_code": 0,
            "expected": "all required scenarios validate",
            "actual": format!("{passed} scenarios passed"),
            "duration_ms": 0,
            "status": "pass",
        }),
    );
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("project root from CARGO_MANIFEST_DIR")
        .to_path_buf()
}

fn fixture_root_or_prepare(project_root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    if let Some(root) = std::env::var_os("FT_AGY_E2E_FIXTURE_ROOT") {
        let fixture_root = PathBuf::from(root);
        let manifest: Value = read_json(&fixture_root.join("manifest.json"));
        let artifact_dir = path_field(&manifest, "artifact_dir");
        let log_jsonl = path_field(&manifest, "log_jsonl");
        return (fixture_root, artifact_dir, log_jsonl);
    }

    let artifact_dir = project_root
        .join("target")
        .join("e2e-logs")
        .join("antigravity-session-resume")
        .join(format!("cargo-wrapper-{}", std::process::id()));
    let fixture_root = artifact_dir.join("fixtures");
    let script = project_root.join("scripts/e2e_antigravity_session_resume.sh");
    let output = Command::new("bash")
        .arg(&script)
        .arg("--prepare-only")
        .arg("--artifact-dir")
        .arg(&artifact_dir)
        .arg("--fixture-root")
        .arg(&fixture_root)
        .output()
        .expect("run Antigravity e2e fixture-preparation script");
    assert!(
        output.status.success(),
        "fixture preparation failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: Value = read_json(&fixture_root.join("manifest.json"));
    let log_jsonl = path_field(&manifest, "log_jsonl");
    (fixture_root, artifact_dir, log_jsonl)
}

fn validate_scenario(scenario: &Value, log_jsonl: &Path, project_root: &Path) {
    let started = Instant::now();
    let scenario_id = str_field(scenario, "scenario_id");
    let home = path_field(scenario, "home");
    let casr_binary = str_field(scenario, "casr_binary");
    let path_env = str_field(scenario, "path_env");
    let agy_uuid = scenario.get("agy_uuid").and_then(Value::as_str);
    let legacy_session_id = scenario.get("legacy_session_id").and_then(Value::as_str);
    let resumer = SessionResumer::new(SessionResumeConfig {
        casr_binary: casr_binary.to_string(),
        working_dir: Some(project_root.to_path_buf()),
        timeout_secs: 5,
        dry_run: false,
    });
    let entries = resumer
        .discover_sessions_in_home(&home)
        .unwrap_or_else(|err| panic!("{scenario_id}: discover_sessions_in_home failed: {err}"));

    for entry in &entries {
        let provider = provider_from_list_entry(entry);
        log_jsonl_record(
            log_jsonl,
            json!({
                "bead_id": BEAD_ID,
                "scenario_id": scenario_id,
                "step": "discover_entry",
                "command": format!("{casr_binary} list --json + native agy scan"),
                "cwd": project_root.display().to_string(),
                "temp_home": home.display().to_string(),
                "provider": provider.slug(),
                "session_id": entry.session_id,
                "path": entry.path,
                "exit_code": 0,
                "expected": "provider/session/path follow scenario policy",
                "actual": entry,
                "duration_ms": duration_ms(started),
                "status": "pass",
            }),
        );
    }

    match scenario_id {
        "agy-only" => {
            assert_provider_count(&entries, AgentProvider::Antigravity, 1, scenario_id);
            assert_eq!(
                entries.len(),
                1,
                "{scenario_id}: expected exactly one entry"
            );
            let uuid = agy_uuid.expect("agy-only uuid");
            assert_agy_entry_and_resume(scenario, log_jsonl, project_root, &entries, uuid, false);
        }
        "legacy-gmi-only" => {
            assert_provider_count(&entries, AgentProvider::Gemini, 1, scenario_id);
            assert_provider_count(&entries, AgentProvider::Antigravity, 0, scenario_id);
            assert_eq!(
                entries.len(),
                1,
                "{scenario_id}: expected exactly one entry"
            );
            let legacy_id = legacy_session_id.expect("legacy id");
            let entry = entries
                .iter()
                .find(|entry| entry.session_id == legacy_id)
                .expect("legacy gmi entry");
            assert_eq!(entry.provider.as_deref(), Some("gemini"));
            assert!(
                !entry.extra.contains_key("native_resume_command"),
                "{scenario_id}: legacy gmi entry must not carry agy native metadata"
            );
        }
        "mixed" => {
            assert_provider_count(&entries, AgentProvider::Antigravity, 1, scenario_id);
            assert_provider_count(&entries, AgentProvider::Gemini, 1, scenario_id);
            assert_eq!(entries.len(), 2, "{scenario_id}: expected agy + legacy gmi");
            let uuid = agy_uuid.expect("mixed uuid");
            assert_agy_entry_and_resume(scenario, log_jsonl, project_root, &entries, uuid, false);
            let legacy_id = legacy_session_id.expect("mixed legacy id");
            let legacy_entry = entries
                .iter()
                .find(|entry| entry.session_id == legacy_id)
                .expect("legacy gmi entry");
            let legacy_path = legacy_entry.path.as_deref().expect("legacy path");
            assert!(
                legacy_path.contains("/.gemini/tmp/"),
                "{scenario_id}: legacy gmi path must stay under tmp/chats"
            );
            assert!(
                !legacy_path.contains("antigravity-cli"),
                "{scenario_id}: legacy gmi path must not cross-list agy root"
            );
        }
        "malformed-irrelevant" => {
            assert_provider_count(&entries, AgentProvider::Antigravity, 1, scenario_id);
            assert_eq!(
                entries.len(),
                1,
                "{scenario_id}: malformed files/dirs/non-uuid db names must not become sessions"
            );
            let uuid = agy_uuid.expect("malformed uuid");
            assert_agy_entry_and_resume(scenario, log_jsonl, project_root, &entries, uuid, false);
        }
        "missing-agy-binary" => {
            assert_provider_count(&entries, AgentProvider::Antigravity, 1, scenario_id);
            let uuid = agy_uuid.expect("missing binary uuid");
            assert_agy_entry_and_resume(scenario, log_jsonl, project_root, &entries, uuid, true);
            let plan = antigravity_native_resume_plan(uuid).expect("agy plan");
            let err = plan
                .require_binary_available_in_path(Some(path_env))
                .expect_err("missing agy binary must fail closed");
            assert!(
                matches!(
                    err,
                    SessionResumeError::NativeProviderNotFound {
                        ref provider_slug,
                        ref binary,
                        ..
                    } if provider_slug == "agy" && binary == "agy"
                ),
                "{scenario_id}: expected provider-specific missing agy error, got {err:?}"
            );
            log_jsonl_record(
                log_jsonl,
                json!({
                    "bead_id": BEAD_ID,
                    "scenario_id": scenario_id,
                    "step": "missing_binary_fail_closed",
                    "command": "NativeResumePlan::require_binary_available_in_path",
                    "cwd": project_root.display().to_string(),
                    "temp_home": home.display().to_string(),
                    "provider": "agy",
                    "session_id": uuid,
                    "path": path_env,
                    "exit_code": 1,
                    "expected": "NativeProviderNotFound(provider=agy,binary=agy)",
                    "actual": err.to_string(),
                    "duration_ms": duration_ms(started),
                    "status": "pass",
                }),
            );
        }
        other => panic!("unexpected Antigravity e2e scenario: {other}"),
    }
}

fn assert_agy_entry_and_resume(
    scenario: &Value,
    log_jsonl: &Path,
    project_root: &Path,
    entries: &[frankenterm_core::casr_types::CasrListEntry],
    uuid: &str,
    skip_execute: bool,
) {
    let started = Instant::now();
    let scenario_id = str_field(scenario, "scenario_id");
    let home = path_field(scenario, "home");
    let path_env = str_field(scenario, "path_env");
    let expected_argv = scenario
        .get("expected_resume_argv")
        .and_then(Value::as_array)
        .expect("expected agy resume argv")
        .iter()
        .map(|value| value.as_str().expect("argv string").to_string())
        .collect::<Vec<_>>();
    let entry = entries
        .iter()
        .find(|entry| provider_from_list_entry(entry) == AgentProvider::Antigravity)
        .expect("agy entry");
    assert_eq!(entry.session_id, uuid);
    assert_eq!(entry.provider.as_deref(), Some("agy"));
    let agy_path = entry.path.as_deref().expect("agy path");
    assert!(
        agy_path.contains("/.gemini/antigravity-cli/conversations/"),
        "{scenario_id}: agy path must stay under antigravity-cli conversations root"
    );
    assert!(
        !agy_path.contains("/.gemini/tmp/"),
        "{scenario_id}: agy path must not cross-list legacy tmp/chats root"
    );

    let plan = antigravity_native_resume_plan(uuid).expect("agy native plan");
    assert_eq!(plan.argv, expected_argv, "{scenario_id}: pinned agy argv");
    assert_eq!(plan.model_name.as_deref(), Some(ANTIGRAVITY_MODEL));

    log_jsonl_record(
        log_jsonl,
        json!({
            "bead_id": BEAD_ID,
            "scenario_id": scenario_id,
            "step": "resume_plan",
            "command": plan.argv,
            "cwd": project_root.display().to_string(),
            "temp_home": home.display().to_string(),
            "provider": "agy",
            "session_id": uuid,
            "path": agy_path,
            "exit_code": 0,
            "expected": ["agy", "--conversation", uuid, "--model", ANTIGRAVITY_MODEL],
            "actual": expected_argv,
            "duration_ms": duration_ms(started),
            "status": "pass",
        }),
    );

    if skip_execute {
        return;
    }

    plan.require_binary_available_in_path(Some(path_env))
        .unwrap_or_else(|err| panic!("{scenario_id}: fake agy binary should be available: {err}"));
    let fake_stdout = path_field(scenario, "fake_agy_argv_log")
        .parent()
        .expect("fake agy log parent")
        .join("fake-agy.stdout.log");
    let fake_stderr = fake_stdout.with_file_name("fake-agy.stderr.log");
    let output = Command::new(&plan.binary)
        .args(&plan.argv[1..])
        .env("PATH", path_env)
        .output()
        .unwrap_or_else(|err| panic!("{scenario_id}: run fake agy: {err}"));
    fs::write(&fake_stdout, &output.stdout).expect("write fake agy stdout");
    fs::write(&fake_stderr, &output.stderr).expect("write fake agy stderr");
    assert!(
        output.status.success(),
        "{scenario_id}: fake agy rejected argv\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    log_jsonl_record(
        log_jsonl,
        json!({
            "bead_id": BEAD_ID,
            "scenario_id": scenario_id,
            "step": "fake_agy_exec",
            "command": plan.argv,
            "cwd": project_root.display().to_string(),
            "temp_home": home.display().to_string(),
            "provider": "agy",
            "session_id": uuid,
            "path": agy_path,
            "stdout_path": fake_stdout.display().to_string(),
            "stderr_path": fake_stderr.display().to_string(),
            "exit_code": output.status.code(),
            "expected": "fake agy accepts only pinned model argv",
            "actual": String::from_utf8_lossy(&output.stdout).trim(),
            "duration_ms": duration_ms(started),
            "status": "pass",
        }),
    );
}

fn assert_provider_count(
    entries: &[frankenterm_core::casr_types::CasrListEntry],
    provider: AgentProvider,
    expected: usize,
    scenario_id: &str,
) {
    let actual = entries
        .iter()
        .filter(|entry| provider_from_list_entry(entry) == provider)
        .count();
    assert_eq!(
        actual,
        expected,
        "{scenario_id}: expected {expected} {} entries, got {actual}",
        provider.slug()
    );
}

fn log_optional_real_smoke(log_jsonl: &Path, project_root: &Path) {
    let Some(home) = std::env::var_os("FT_AGY_E2E_REAL_HOME").map(PathBuf::from) else {
        log_jsonl_record(
            log_jsonl,
            json!({
                "bead_id": BEAD_ID,
                "scenario_id": "optional-real-smoke",
                "step": "real_home_discovery",
                "command": "discover_antigravity_conversations_from_home",
                "cwd": project_root.display().to_string(),
                "temp_home": null,
                "provider": "agy",
                "session_id": null,
                "path": null,
                "exit_code": 0,
                "expected": "optional opt-in env var",
                "actual": "FT_AGY_E2E_REAL_HOME not set",
                "duration_ms": 0,
                "status": "skip",
                "skip_reason": "optional_real_smoke_not_requested",
            }),
        );
        return;
    };
    let started = Instant::now();
    let entries =
        frankenterm_core::session_resume::discover_antigravity_conversations_from_home(&home);
    for entry in &entries {
        log_jsonl_record(
            log_jsonl,
            json!({
                "bead_id": BEAD_ID,
                "scenario_id": "optional-real-smoke",
                "step": "real_home_discovery",
                "command": "discover_antigravity_conversations_from_home",
                "cwd": project_root.display().to_string(),
                "temp_home": home.display().to_string(),
                "provider": "agy",
                "session_id": entry.session_id,
                "path": entry.path,
                "exit_code": 0,
                "expected": "read-only discovery with redacted metadata",
                "actual": "discovered",
                "duration_ms": duration_ms(started),
                "status": "pass",
            }),
        );
    }
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read JSON {}: {err}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("parse JSON {}: {err}", path.display()))
}

fn path_field(value: &Value, field: &str) -> PathBuf {
    PathBuf::from(str_field(value, field))
}

fn str_field<'a>(value: &'a Value, field: &str) -> &'a str {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string field {field}: {value}"))
}

fn duration_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

fn log_jsonl_record(path: &Path, record: Value) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|err| panic!("open JSONL {}: {err}", path.display()));
    serde_json::to_writer(&mut file, &record)
        .unwrap_or_else(|err| panic!("write JSONL {}: {err}", path.display()));
    file.write_all(b"\n")
        .unwrap_or_else(|err| panic!("newline JSONL {}: {err}", path.display()));
}
