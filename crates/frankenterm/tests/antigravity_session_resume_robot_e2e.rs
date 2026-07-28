//! Public robot e2e coverage for Antigravity session-resume.
//!
//! The fixture generator lives in `scripts/e2e_antigravity_session_resume.sh`
//! so shell users and Cargo/RCH proof use the same isolated HOME/PATH trees.
//! This test drives the compiled `ft` binary through `ft robot session-resume`
//! and appends detailed JSONL records to the retained artifact directory.

#![cfg(feature = "session-resume")]

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

use serde_json::{Value, json};

const BEAD_ID: &str = "ft-agy-provider-q8o4y-685af.5";
const ANTIGRAVITY_MODEL: &str = "Gemini 3.1 Pro (High)";
const MISSING_AGY_ERROR_CODE: &str = "robot.session_resume.native_provider_not_found";

#[test]
fn robot_session_resume_public_surface_lists_and_resumes_agy_without_cross_listing_gmi() {
    let project_root = project_root();
    let artifact_dir = project_root
        .join("target")
        .join("e2e-logs")
        .join("antigravity-session-resume")
        .join(format!("robot-wrapper-{}", std::process::id()));
    let fixture_root = artifact_dir.join("fixtures");
    let stdout_dir = artifact_dir.join("robot-public-surface").join("stdout");
    let stderr_dir = artifact_dir.join("robot-public-surface").join("stderr");
    fs::create_dir_all(&stdout_dir).expect("create public stdout dir");
    fs::create_dir_all(&stderr_dir).expect("create public stderr dir");

    prepare_fixtures(&project_root, &artifact_dir, &fixture_root);
    let manifest = read_json(&fixture_root.join("manifest.json"));
    let log_jsonl = path_field(&manifest, "log_jsonl");
    let ft_bin = PathBuf::from(env!("CARGO_BIN_EXE_ft"));

    log_jsonl_record(
        &log_jsonl,
        json!({
            "bead_id": BEAD_ID,
            "scenario_id": "robot-wrapper",
            "step": "public_robot_validation_start",
            "command": format!("{} robot --format json session-resume ...", ft_bin.display()),
            "cwd": project_root.display().to_string(),
            "temp_home": null,
            "provider": null,
            "session_id": null,
            "path": fixture_root.display().to_string(),
            "exit_code": null,
            "expected": "ft robot session-resume exercises all retained scenarios",
            "actual": "starting",
            "duration_ms": 0,
            "status": "running",
        }),
    );

    let scenarios = manifest
        .get("scenarios")
        .and_then(Value::as_array)
        .expect("manifest scenarios array");
    for scenario in scenarios {
        validate_public_surface_scenario(
            scenario,
            &project_root,
            &ft_bin,
            &stdout_dir,
            &stderr_dir,
            &log_jsonl,
        );
    }

    log_jsonl_record(
        &log_jsonl,
        json!({
            "bead_id": BEAD_ID,
            "scenario_id": "robot-wrapper",
            "step": "public_robot_validation_complete",
            "command": "ft robot --format json session-resume list|resume",
            "cwd": project_root.display().to_string(),
            "temp_home": null,
            "provider": null,
            "session_id": null,
            "path": artifact_dir.display().to_string(),
            "exit_code": 0,
            "expected": "agy/gmi public robot scenarios pass",
            "actual": format!("{} scenarios passed", scenarios.len()),
            "duration_ms": 0,
            "status": "pass",
        }),
    );
}

fn validate_public_surface_scenario(
    scenario: &Value,
    project_root: &Path,
    ft_bin: &Path,
    stdout_dir: &Path,
    stderr_dir: &Path,
    log_jsonl: &Path,
) {
    let scenario_id = str_field(scenario, "scenario_id");
    let home = path_field(scenario, "home");
    let casr_binary = str_field(scenario, "casr_binary");
    let agy_uuid = scenario.get("agy_uuid").and_then(Value::as_str);
    let legacy_id = scenario.get("legacy_session_id").and_then(Value::as_str);
    let expect_missing_agy_binary = bool_field(scenario, "expect_missing_agy_binary");

    let list_all = run_robot_json(RobotRun {
        scenario,
        project_root,
        ft_bin,
        stdout_dir,
        stderr_dir,
        log_jsonl,
        step: "public-list-all",
        expect_ok: true,
        args: vec![
            "session-resume".into(),
            "list".into(),
            "--home".into(),
            home.display().to_string(),
            "--casr-binary".into(),
            casr_binary.into(),
        ],
    });
    if let Some(uuid) = agy_uuid {
        assert_has_session(&list_all, "agy", uuid, scenario_id, "list-all");
    }
    if let Some(session_id) = legacy_id {
        assert_has_session(&list_all, "gemini", session_id, scenario_id, "list-all");
    }

    let list_agy = run_robot_json(RobotRun {
        scenario,
        project_root,
        ft_bin,
        stdout_dir,
        stderr_dir,
        log_jsonl,
        step: "public-list-agy",
        expect_ok: true,
        args: vec![
            "session-resume".into(),
            "list".into(),
            "--provider".into(),
            "antigravity-cli".into(),
            "--home".into(),
            home.display().to_string(),
            "--casr-binary".into(),
            casr_binary.into(),
        ],
    });
    assert_lacks_provider(&list_agy, "gemini", scenario_id, "list-agy");
    if let Some(uuid) = agy_uuid {
        let entry = assert_has_session(&list_agy, "agy", uuid, scenario_id, "list-agy");
        let expected_argv = expected_agy_argv(uuid);
        assert_eq!(
            value_string_array(entry.get("native_resume_command")),
            expected_argv,
            "{scenario_id}: Antigravity list entry must expose the model-pinned native argv"
        );
        assert_eq!(
            entry.get("model_name").and_then(Value::as_str),
            Some(ANTIGRAVITY_MODEL),
            "{scenario_id}: Antigravity list entry must expose the pinned model name"
        );
        let source_path = entry
            .get("source_path")
            .and_then(Value::as_str)
            .expect("agy source_path");
        assert!(
            source_path.contains("/.gemini/antigravity-cli/conversations/"),
            "{scenario_id}: agy source_path must stay under antigravity-cli conversations: {source_path}"
        );
        assert!(
            !source_path.contains("/.gemini/tmp/"),
            "{scenario_id}: agy source_path must not cross-list legacy Gemini tmp/chats: {source_path}"
        );
    }

    if legacy_id.is_some() || !expect_missing_agy_binary {
        let list_gmi = run_robot_json(RobotRun {
            scenario,
            project_root,
            ft_bin,
            stdout_dir,
            stderr_dir,
            log_jsonl,
            step: "public-list-gmi",
            expect_ok: true,
            args: vec![
                "session-resume".into(),
                "list".into(),
                "--provider".into(),
                "gmi".into(),
                "--home".into(),
                home.display().to_string(),
                "--casr-binary".into(),
                casr_binary.into(),
            ],
        });
        assert_lacks_provider(&list_gmi, "agy", scenario_id, "list-gmi");
        if let Some(session_id) = legacy_id {
            let entry =
                assert_has_session(&list_gmi, "gemini", session_id, scenario_id, "list-gmi");
            let source_path = entry
                .get("source_path")
                .and_then(Value::as_str)
                .expect("legacy gmi source_path");
            assert!(
                source_path.contains("/.gemini/tmp/"),
                "{scenario_id}: legacy Gemini source_path must stay under tmp/chats: {source_path}"
            );
            assert!(
                !source_path.contains("antigravity-cli"),
                "{scenario_id}: legacy Gemini source_path must not cross-list agy root: {source_path}"
            );
        }
    }

    if let Some(uuid) = agy_uuid {
        if expect_missing_agy_binary {
            let missing = run_robot_json(RobotRun {
                scenario,
                project_root,
                ft_bin,
                stdout_dir,
                stderr_dir,
                log_jsonl,
                step: "public-resume-agy-missing-binary",
                expect_ok: false,
                args: vec![
                    "session-resume".into(),
                    "resume".into(),
                    uuid.into(),
                    "--provider".into(),
                    "agy".into(),
                    "--dry-run".into(),
                    "--home".into(),
                    home.display().to_string(),
                    "--casr-binary".into(),
                    casr_binary.into(),
                ],
            });
            assert_eq!(
                missing.get("error_code").and_then(Value::as_str),
                Some(MISSING_AGY_ERROR_CODE),
                "{scenario_id}: missing agy binary must fail closed with provider-specific error"
            );
        } else {
            let dry_run = run_robot_json(RobotRun {
                scenario,
                project_root,
                ft_bin,
                stdout_dir,
                stderr_dir,
                log_jsonl,
                step: "public-resume-agy-dry-run",
                expect_ok: true,
                args: vec![
                    "session-resume".into(),
                    "resume".into(),
                    uuid.into(),
                    "--provider".into(),
                    "antigravity".into(),
                    "--dry-run".into(),
                    "--home".into(),
                    home.display().to_string(),
                    "--casr-binary".into(),
                    casr_binary.into(),
                ],
            });
            assert_resume_payload_has_agy_argv(&dry_run, uuid, scenario_id, true);

            let executed = run_robot_json(RobotRun {
                scenario,
                project_root,
                ft_bin,
                stdout_dir,
                stderr_dir,
                log_jsonl,
                step: "public-resume-agy-execute",
                expect_ok: true,
                args: vec![
                    "session-resume".into(),
                    "resume".into(),
                    uuid.into(),
                    "--provider".into(),
                    "agy".into(),
                    "--home".into(),
                    home.display().to_string(),
                    "--casr-binary".into(),
                    casr_binary.into(),
                ],
            });
            assert_resume_payload_has_agy_argv(&executed, uuid, scenario_id, false);
            assert_eq!(
                executed
                    .pointer("/data/native_execution/exit_code")
                    .and_then(Value::as_i64),
                Some(0),
                "{scenario_id}: fake agy process must exit successfully"
            );
        }
    }

    if let Some(session_id) = legacy_id {
        let legacy_resume = run_robot_json(RobotRun {
            scenario,
            project_root,
            ft_bin,
            stdout_dir,
            stderr_dir,
            log_jsonl,
            step: "public-resume-gmi-dry-run",
            expect_ok: true,
            args: vec![
                "session-resume".into(),
                "resume".into(),
                session_id.into(),
                "--provider".into(),
                "gemini".into(),
                "--dry-run".into(),
                "--home".into(),
                home.display().to_string(),
                "--casr-binary".into(),
                casr_binary.into(),
            ],
        });
        assert_eq!(
            legacy_resume
                .pointer("/data/provider")
                .and_then(Value::as_str),
            Some("gemini"),
            "{scenario_id}: legacy resume must preserve the Gemini provider"
        );
        assert_eq!(
            legacy_resume
                .pointer("/data/resume_kind")
                .and_then(Value::as_str),
            Some("casr"),
            "{scenario_id}: legacy Gemini resume must route through casr"
        );
        let argv = value_string_array(legacy_resume.pointer("/data/command_argv"));
        assert_eq!(
            argv.last().map(String::as_str),
            Some("--dry-run"),
            "{scenario_id}: legacy dry-run resume must pass --dry-run through casr"
        );
    }
}

struct RobotRun<'a> {
    scenario: &'a Value,
    project_root: &'a Path,
    ft_bin: &'a Path,
    stdout_dir: &'a Path,
    stderr_dir: &'a Path,
    log_jsonl: &'a Path,
    step: &'a str,
    expect_ok: bool,
    args: Vec<String>,
}

fn run_robot_json(run: RobotRun<'_>) -> Value {
    let scenario_id = str_field(run.scenario, "scenario_id");
    let home = str_field(run.scenario, "home");
    let path_env = str_field(run.scenario, "path_env");
    let stdout_path = run
        .stdout_dir
        .join(format!("{scenario_id}-{}.json", run.step));
    let stderr_path = run
        .stderr_dir
        .join(format!("{scenario_id}-{}.log", run.step));

    let mut command = Command::new(run.ft_bin);
    command
        .current_dir(run.project_root)
        .env("HOME", home)
        .env("PATH", path_env)
        .arg("robot")
        .arg("--format")
        .arg("json")
        .args(&run.args);
    let command_display = command_display(run.ft_bin, &run.args);
    let started = Instant::now();
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("{scenario_id}: run {command_display}: {err}"));
    let duration_ms = started.elapsed().as_millis();
    fs::write(&stdout_path, &output.stdout).expect("write robot stdout artifact");
    fs::write(&stderr_path, &output.stderr).expect("write robot stderr artifact");

    let payload = parse_robot_stdout(&run, &output, &stdout_path, &stderr_path, &command_display);
    let ok = payload.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let status = if output.status.success() && ok == run.expect_ok {
        "pass"
    } else {
        "fail"
    };
    log_jsonl_record(
        run.log_jsonl,
        json!({
            "bead_id": BEAD_ID,
            "scenario_id": scenario_id,
            "step": run.step,
            "command": command_display,
            "cwd": run.project_root.display().to_string(),
            "temp_home": home,
            "provider": null,
            "session_id": run.scenario.get("agy_uuid").or_else(|| run.scenario.get("legacy_session_id")),
            "path": run.scenario.get("agy_db").or_else(|| run.scenario.get("legacy_path")),
            "stdout_path": stdout_path.display().to_string(),
            "stderr_path": stderr_path.display().to_string(),
            "exit_code": output.status.code(),
            "expected": format!("robot envelope ok={}", run.expect_ok),
            "actual": format!(
                "exit={:?} ok={} error_code={}",
                output.status.code(),
                ok,
                payload.get("error_code").and_then(Value::as_str).unwrap_or("")
            ),
            "duration_ms": duration_ms,
            "status": status,
        }),
    );
    assert_eq!(
        status,
        "pass",
        "{scenario_id}:{} expected ok={} with zero exit; stdout artifact {}; stderr artifact {}; payload={payload}",
        run.step,
        run.expect_ok,
        stdout_path.display(),
        stderr_path.display()
    );
    payload
}

fn parse_robot_stdout(
    run: &RobotRun<'_>,
    output: &Output,
    stdout_path: &Path,
    stderr_path: &Path,
    command_display: &str,
) -> Value {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap_or_else(|err| {
        panic!(
            "{}:{} stdout was not UTF-8: {err}",
            str_field(run.scenario, "scenario_id"),
            run.step
        )
    });
    match serde_json::from_str::<Value>(&stdout) {
        Ok(payload) => payload,
        Err(err) => {
            log_jsonl_record(
                run.log_jsonl,
                json!({
                    "bead_id": BEAD_ID,
                    "scenario_id": str_field(run.scenario, "scenario_id"),
                    "step": run.step,
                    "command": command_display,
                    "cwd": run.project_root.display().to_string(),
                    "temp_home": str_field(run.scenario, "home"),
                    "provider": null,
                    "session_id": run.scenario.get("agy_uuid").or_else(|| run.scenario.get("legacy_session_id")),
                    "path": run.scenario.get("agy_db").or_else(|| run.scenario.get("legacy_path")),
                    "stdout_path": stdout_path.display().to_string(),
                    "stderr_path": stderr_path.display().to_string(),
                    "exit_code": output.status.code(),
                    "expected": "parseable JSON robot envelope",
                    "actual": err.to_string(),
                    "duration_ms": 0,
                    "status": "fail",
                }),
            );
            panic!(
                "{}:{} stdout was not JSON for {command_display}; stdout artifact {}; stderr artifact {}; parse error: {err}",
                str_field(run.scenario, "scenario_id"),
                run.step,
                stdout_path.display(),
                stderr_path.display()
            );
        }
    }
}

fn assert_resume_payload_has_agy_argv(
    payload: &Value,
    uuid: &str,
    scenario_id: &str,
    dry_run: bool,
) {
    assert_eq!(
        payload.pointer("/data/provider").and_then(Value::as_str),
        Some("agy"),
        "{scenario_id}: resume payload must identify Antigravity provider"
    );
    assert_eq!(
        payload.pointer("/data/dry_run").and_then(Value::as_bool),
        Some(dry_run),
        "{scenario_id}: resume payload dry_run drift"
    );
    assert_eq!(
        payload.pointer("/data/resume_kind").and_then(Value::as_str),
        Some("native"),
        "{scenario_id}: Antigravity must use native resume"
    );
    assert_eq!(
        payload.pointer("/data/model_name").and_then(Value::as_str),
        Some(ANTIGRAVITY_MODEL),
        "{scenario_id}: Antigravity resume payload must expose the pinned model"
    );
    assert_eq!(
        value_string_array(payload.pointer("/data/command_argv")),
        expected_agy_argv(uuid),
        "{scenario_id}: Antigravity resume command must remain model-pinned"
    );
}

fn assert_has_session<'a>(
    payload: &'a Value,
    provider: &str,
    session_id: &str,
    scenario_id: &str,
    step: &str,
) -> &'a Value {
    payload_sessions(payload)
        .iter()
        .find(|entry| {
            entry.get("provider").and_then(Value::as_str) == Some(provider)
                && entry.get("session_id").and_then(Value::as_str) == Some(session_id)
        })
        .unwrap_or_else(|| {
            panic!("{scenario_id}:{step}: missing {provider}:{session_id} in {payload}")
        })
}

fn assert_lacks_provider(payload: &Value, provider: &str, scenario_id: &str, step: &str) {
    let offenders = payload_sessions(payload)
        .iter()
        .filter(|entry| entry.get("provider").and_then(Value::as_str) == Some(provider))
        .collect::<Vec<_>>();
    assert!(
        offenders.is_empty(),
        "{scenario_id}:{step}: unexpectedly listed provider {provider}: {offenders:?}"
    );
}

fn payload_sessions(payload: &Value) -> &[Value] {
    payload
        .pointer("/data/sessions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .expect("robot session-resume list data.sessions array")
}

fn expected_agy_argv(uuid: &str) -> Vec<String> {
    vec![
        "agy".into(),
        "--conversation".into(),
        uuid.into(),
        "--model".into(),
        ANTIGRAVITY_MODEL.into(),
    ]
}

fn value_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .expect("string array value")
        .iter()
        .map(|value| value.as_str().expect("array string").to_string())
        .collect()
}

fn prepare_fixtures(project_root: &Path, artifact_dir: &Path, fixture_root: &Path) {
    let script = project_root
        .join("scripts")
        .join("e2e_antigravity_session_resume.sh");
    let output = Command::new("bash")
        .arg(&script)
        .arg("--prepare-only")
        .arg("--artifact-dir")
        .arg(artifact_dir)
        .arg("--fixture-root")
        .arg(fixture_root)
        .current_dir(project_root)
        .output()
        .expect("run Antigravity fixture generator");
    assert!(
        output.status.success(),
        "fixture generation failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn command_display(ft_bin: &Path, args: &[String]) -> String {
    std::iter::once(ft_bin.display().to_string())
        .chain([
            "robot".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ])
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("project root from CARGO_MANIFEST_DIR")
        .to_path_buf()
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

fn bool_field(value: &Value, field: &str) -> bool {
    value
        .get(field)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("missing bool field {field}: {value}"))
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
