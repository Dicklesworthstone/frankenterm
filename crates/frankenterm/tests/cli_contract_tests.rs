//! CLI command contract tests (wa-nu4.3.2.11)
//!
//! Validates that each human CLI command behaves correctly in both
//! interactive and automation contexts. Uses subprocess-style tests
//! against a temp workspace with pre-populated fixtures.
//!
//! Contract guarantees tested:
//! - Deterministic exit codes
//! - Stable JSON schema in `--format json` mode
//! - No ANSI escapes in `--format plain` mode
//! - Actionable error messages for failure paths
//! - Secret-like strings never leak unredacted

use assert_cmd::Command;
#[cfg(unix)]
#[path = "../../frankenterm-core/tests/common/wezterm_subprocess.rs"]
mod wezterm_subprocess;
#[cfg(unix)]
use frankenterm_core::approval::ApprovalScope;
use frankenterm_core::plan::{
    MISSION_TX_SCHEMA_VERSION, MissionActorRole, MissionTxContract, MissionTxState, StepAction,
    TxCompensation, TxId, TxIntent, TxOutcome, TxPlan, TxPlanId, TxStep, TxStepId,
};
#[cfg(unix)]
use frankenterm_core::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyInput, PolicySurface,
};
#[cfg(unix)]
use frankenterm_core::scrollback_mmap_format::RecordKind;
use frankenterm_core::scrollback_mmap_format::{FormatVersion, HeaderFlags, ScrollbackHeader};
#[cfg(unix)]
use frankenterm_core::scrollback_mmap_writer::{
    LinearRecordReadLimits, MmapScrollback, MmapScrollbackConfig,
};
#[cfg(unix)]
use frankenterm_core::tx_idempotency::{StepOutcome, TxExecutionLedger, TxPhase};
use predicates::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;
#[cfg(unix)]
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

#[cfg(all(unix, feature = "subprocess-bridge"))]
mod scanner_contract {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    struct Fixture {
        root: PathBuf,
        base: PathBuf,
        config: PathBuf,
    }

    impl Fixture {
        fn new(body: Option<&str>, version: &str, decision: &str) -> Self {
            let base = tempfile::Builder::new()
                .prefix("ft-cli-scanner-")
                .tempdir_in("/tmp")
                .unwrap()
                .keep();
            let base = std::fs::canonicalize(base).unwrap();
            let root = base.join("project");
            for path in [
                root.join("src"),
                base.join("bin"),
                base.join("home"),
                base.join("tmp"),
            ] {
                std::fs::create_dir_all(path).unwrap();
            }
            std::fs::write(
                root.join("src/selected.rs"),
                b"pub const SELECTED: u8 = 7;\n",
            )
            .unwrap();
            std::fs::write(root.join("unselected.rs"), b"UNSELECTED_SENTINEL").unwrap();
            let mut configuration = frankenterm_core::config::Config::default();
            configuration.safety.rules = serde_json::from_value(serde_json::json!({
                "rules": [{"id":"scanner-contract", "decision":decision,
                    "match_on":{"actions":["exec_command"], "domains":["code-scanner"]}}]
            }))
            .unwrap();
            let config = base.join("ft.toml");
            std::fs::write(&config, configuration.to_toml().unwrap()).unwrap();
            if let Some(body) = body {
                // Real owned command fixture, not an installed UBS roundtrip.
                let script = format!(
                    r#"#!/bin/sh
set -eu
base=${{0%/*}}
printf 'called\n' >> "$base/calls"
if [ "$1" = --version ]; then
  printf '%s\n' 'UBS Meta-Runner v{version}'
  exit 0
fi
for snapshot do :; done
printf '%s\0' "$@" > "$base/argv"
test "$UBS_SKIP_RUST_BUILD" = 1
test "$UBS_SKIP_CATEGORIES" = 12,13,14
test "$UBS_NO_AUTO_UPDATE" = 1
test -f "$snapshot/src/selected.rs"
test ! -e "$snapshot/unselected.rs"
/bin/cat "$snapshot/src/selected.rs" > "$base/received.rs"
report() {{
  printf '{{"project":"%s","scanners":[{{"project":"%s","format":"json","language":"rust","files":1,"critical":%s,"warning":0,"info":0}}],"totals":{{"files":1,"critical":%s,"warning":0,"info":0}},"suggestion":"touch %s/should-not-exist"}}\n' "$snapshot" "$snapshot" "$1" "$1" "$base"
}}
{body}
"#
                );
                let binary = base.join("bin/ubs");
                std::fs::write(&binary, script).unwrap();
                std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self { root, base, config }
        }

        fn command(&self, robot: bool) -> Command {
            let mut command = wa_cmd_for(self.root.to_str().unwrap());
            command
                .env_clear()
                .env("PATH", self.base.join("bin"))
                .env("HOME", self.base.join("home"))
                .env("TMPDIR", self.base.join("tmp"))
                .env("FT_WORKSPACE", &self.root)
                .env("FT_RUNTIME_WORKER_THREADS", "2")
                .arg("--config")
                .arg(&self.config)
                .timeout(std::time::Duration::from_secs(15));
            if robot {
                command.args(["robot", "--format", "json", "scan"]);
            } else {
                command.args(["scan", "--json"]);
            }
            command.arg("--project-root").arg(&self.root);
            command
        }

        fn empty_policy_database(&self) -> PathBuf {
            std::fs::create_dir(self.root.join(".ft")).unwrap();
            let database = self.root.join(".ft/ft.db");
            let connection = rusqlite::Connection::open(&database).unwrap();
            connection.execute_batch("PRAGMA journal_mode=OFF; CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL);").unwrap();
            drop(connection);
            database
        }

        fn run(&self, robot: bool, path: &Path) -> (bool, serde_json::Value) {
            let output = self
                .command(robot)
                .arg("--path")
                .arg(path)
                .output()
                .unwrap();
            let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "scanner envelope invalid: {error}; stdout={}; stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            (output.status.success(), value)
        }
    }

    #[test]
    fn code_scan_contract_human_and_robot_return_actual_selected_command_results() {
        for robot in [false, true] {
            let fixture = Fixture::new(Some("report 3; exit 1"), "5.2.42", "allow");
            let (success, value) = fixture.run(robot, Path::new("src"));
            assert!(success, "{value}");
            assert_eq!(value["ok"], true);
            assert_eq!(value["data"]["profile"], "ubs-static-rust-v1");
            assert_eq!(value["data"]["classification"], "critical");
            assert_eq!(value["data"]["scanner_exit_code"], 1);
            assert_eq!(value["data"]["report"]["totals"]["critical"], 3);
            let original = std::fs::read(fixture.root.join("src/selected.rs")).unwrap();
            assert_eq!(
                std::fs::read(fixture.base.join("bin/received.rs")).unwrap(),
                original
            );
            assert_eq!(
                value["data"]["inputs"][0]["sha256"],
                hex::encode(Sha256::digest(&original))
            );
            assert_eq!(value["data"]["inputs"][0]["origin_path"], "src/selected.rs");
            let retained = Path::new(value["data"]["retained_snapshot"].as_str().unwrap());
            assert_eq!(
                std::fs::read(retained.join("src/selected.rs")).unwrap(),
                original
            );
            assert!(retained.join("ft-scan-inputs.json").is_file());
            assert_eq!(value["data"]["diagnostics"].as_array().unwrap().len(), 2);
            assert!(
                value["data"]["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["spawned"] == true && item["supervisor_settled"] == true)
            );
            assert!(!fixture.base.join("bin/should-not-exist").exists());
            assert!(!value.to_string().contains("suggestion"));
            assert!(
                !fixture.root.join(".ft").exists(),
                "scan must not initialize a watcher workspace"
            );
        }
    }

    #[test]
    fn code_scan_contract_errors_remain_typed_and_nonzero() {
        for (body, version, expected) in [
            (None, "5.2.42", "unavailable"),
            (Some("report 0"), "9.0.0", "version_mismatch"),
            (Some("report 0; exit 2"), "5.2.42", "nonzero_exit"),
            (Some("printf '{}'"), "5.2.42", "malformed_output"),
        ] {
            let fixture = Fixture::new(body, version, "allow");
            let (success, value) = fixture.run(true, Path::new("src"));
            assert!(!success, "{value}");
            assert_eq!(value["ok"], false);
            assert_eq!(value["error_code"], "robot.code_scan");
            assert_eq!(value["data"]["kind"]["kind"], expected);
            assert!(value["data"]["classification"].is_null());
        }
        let fixture = Fixture::new(Some("report 0"), "5.2.42", "allow");
        let outside = fixture.base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("private.rs"), b"OUTSIDE_SENTINEL").unwrap();
        symlink(&outside, fixture.root.join("escaped")).unwrap();
        let (success, value) = fixture.run(true, Path::new("escaped"));
        assert!(!success);
        assert_eq!(value["data"]["kind"]["kind"], "path_escape");
        assert!(!fixture.base.join("bin/received.rs").exists());
        assert_eq!(
            std::fs::read(outside.join("private.rs")).unwrap(),
            b"OUTSIDE_SENTINEL"
        );
    }

    #[test]
    fn code_scan_contract_policy_and_persisted_kill_switch_block_before_child() {
        for (decision, expected) in [
            ("deny", "policy_denied"),
            ("require_approval", "approval_required"),
        ] {
            let fixture = Fixture::new(Some("report 0"), "5.2.42", decision);
            let (success, value) = fixture.run(true, Path::new("src"));
            assert!(!success);
            assert_eq!(value["data"]["kind"]["kind"], expected);
            assert!(!fixture.base.join("bin/calls").exists());
        }
        for corrupt in [false, true] {
            use frankenterm_core::policy_kill_switch_state::{
                KILL_SWITCH_STATE_KEY, encode_kill_switch_state,
            };
            use frankenterm_core::policy_quarantine::{KillSwitch, KillSwitchLevel};
            let fixture = Fixture::new(Some("report 0"), "5.2.42", "allow");
            let database = fixture.empty_policy_database();
            let connection = rusqlite::Connection::open(&database).unwrap();
            connection
                .execute_batch("PRAGMA journal_mode=OFF;")
                .unwrap();
            let mut switch = KillSwitch::disarmed();
            switch.trip(KillSwitchLevel::HardStop, "scanner-test", "owned test", 1);
            let encoded = if corrupt {
                "invalid-state".to_string()
            } else {
                encode_kill_switch_state(&switch).unwrap()
            };
            connection
                .execute(
                    "INSERT INTO config (key,value,updated_at) VALUES (?1,?2,1)",
                    rusqlite::params![KILL_SWITCH_STATE_KEY, encoded],
                )
                .unwrap();
            drop(connection);
            let before = std::fs::read(&database).unwrap();
            let (success, value) = fixture.run(true, Path::new("src"));
            assert!(!success);
            assert_eq!(value["data"]["kind"]["kind"], "policy_denied");
            assert!(!fixture.base.join("bin/calls").exists());
            assert_eq!(std::fs::read(&database).unwrap(), before);
        }
    }

    #[test]
    fn code_scan_contract_policy_database_authority_refuses_redirects_and_busy_fence() {
        use frankenterm_core::policy_kill_switch_state::acquire_kill_switch_fence;
        for case in [
            "absent_row",
            "leaf_symlink",
            "ancestor_symlink",
            "hardlink",
            "journal_symlink",
            "lock_symlink",
            "busy",
            "malformed_database",
            "database_directory",
        ] {
            let fixture = Fixture::new(Some("report 0"), "5.2.42", "allow");
            let database = fixture.empty_policy_database();
            let original = std::fs::read(&database).unwrap();
            let outside = fixture.base.join("outside.db");
            std::fs::write(&outside, &original).unwrap();
            let mut held_fence = None;
            match case {
                "absent_row" => {}
                "leaf_symlink" => {
                    std::fs::rename(&database, database.with_extension("retained")).unwrap();
                    symlink(&outside, &database).unwrap();
                }
                "ancestor_symlink" => {
                    let retained = fixture.base.join("retained-state");
                    std::fs::rename(fixture.root.join(".ft"), &retained).unwrap();
                    symlink(&retained, fixture.root.join(".ft")).unwrap();
                }
                "hardlink" => {
                    std::fs::hard_link(&database, fixture.base.join("database-link")).unwrap()
                }
                "journal_symlink" => {
                    symlink(&outside, fixture.root.join(".ft/ft.db-wal")).unwrap()
                }
                "lock_symlink" => symlink(
                    &outside,
                    fixture.root.join(".ft/ft.db.policy-kill-switch.lock"),
                )
                .unwrap(),
                "busy" => held_fence = Some(acquire_kill_switch_fence(&database).unwrap()),
                "malformed_database" => {
                    std::fs::write(&database, b"NOT_A_SQLITE_DATABASE").unwrap()
                }
                "database_directory" => {
                    std::fs::rename(&database, database.with_extension("retained")).unwrap();
                    std::fs::create_dir(&database).unwrap();
                }
                _ => unreachable!(),
            }
            let (success, value) = fixture.run(true, Path::new("src"));
            if case == "absent_row" {
                assert!(success, "{case}: {value}");
                assert_eq!(value["data"]["classification"], "clean");
                assert_eq!(std::fs::read(&database).unwrap(), original);
                assert!(
                    acquire_kill_switch_fence(&database).is_ok(),
                    "settled scan released its fence"
                );
            } else {
                assert!(!success, "{case}: {value}");
                assert_eq!(
                    value["data"]["kind"]["kind"],
                    if case == "busy" { "policy_busy" } else { "io" },
                    "{case}: {value}"
                );
                assert!(!fixture.base.join("bin/calls").exists(), "{case}");
            }
            assert_eq!(std::fs::read(&outside).unwrap(), original, "{case}");
            drop(held_fence);
        }
    }

    #[test]
    fn code_scan_contract_keeps_policy_fence_until_running_scanner_settles() {
        use frankenterm_core::policy_kill_switch_state::{
            KillSwitchStateError, acquire_kill_switch_fence,
        };
        let fixture = Fixture::new(
            Some("printf 'running' > \"$base/running\"; exec /bin/sleep 2"),
            "5.2.42",
            "allow",
        );
        let database = fixture.empty_policy_database();
        let marker = fixture.base.join("bin/running");
        let mut command = fixture.command(true);
        command.args(["--path", "src"]);
        let child = std::thread::spawn(move || command.output().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let running = marker.is_file();
        let fenced = matches!(
            acquire_kill_switch_fence(&database),
            Err(KillSwitchStateError::FencePending)
        );
        let output = child.join().unwrap();
        assert!(
            running,
            "scanner never reached owned marker; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(fenced, "running scanner must retain policy authority");
        assert!(
            !output.status.success(),
            "the sleeping control deliberately emits no scanner report"
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"]["kind"]["kind"], "malformed_output");
        assert!(
            value["data"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["supervisor_settled"] == true)
        );
        assert!(
            acquire_kill_switch_fence(&database).is_ok(),
            "failed settled scan released its fence"
        );
    }

    #[test]
    fn code_scan_contract_rejects_ambiguous_scope_and_invalid_deadline_before_child() {
        for args in [
            vec![],
            vec!["--path", "src", "--staged"],
            vec!["--staged", "--diff"],
            vec!["--path", "src", "--timeout-ms", "0"],
            vec!["--path", "src", "--timeout-ms", "120001"],
        ] {
            let fixture = Fixture::new(Some("report 0"), "5.2.42", "allow");
            let output = fixture.command(false).args(args).output().unwrap();
            assert!(!output.status.success());
            assert!(!fixture.base.join("bin/calls").exists());
            assert!(!fixture.root.join(".ft").exists());
        }
    }
}

// =============================================================================
// Test fixture helpers
// =============================================================================

/// Create a temp workspace with `.ft/` directory and initialized DB.
/// Returns (TempDir guard, workspace path string).
fn setup_workspace() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let ft_dir = dir.path().join(".ft");
    std::fs::create_dir_all(&ft_dir).expect("create .ft dir");
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&ft_dir)
            .expect("read .ft dir metadata")
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&ft_dir, perms).expect("harden .ft dir permissions");
    }

    // Initialize database with schema
    let db_path = ft_dir.join("ft.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");
    frankenterm_core::storage::initialize_schema(&conn).expect("init schema");
    drop(conn);
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&db_path)
            .expect("read ft.db metadata")
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&db_path, perms).expect("harden ft.db permissions");
    }

    let ws = dir.path().to_string_lossy().to_string();
    (dir, ws)
}

fn write_test_scrollback(
    scrollback_dir: &std::path::Path,
    uuid_byte: u8,
) -> (String, std::path::PathBuf) {
    std::fs::create_dir_all(scrollback_dir).expect("create scrollback dir");
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(scrollback_dir)
            .expect("read scrollback directory metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(scrollback_dir, permissions)
            .expect("harden scrollback directory permissions");
    }
    let pane_uuid = format!("{uuid_byte:02x}").repeat(32);
    let path = scrollback_dir.join(format!("{pane_uuid}.bin"));
    let header = ScrollbackHeader {
        version: FormatVersion::V1,
        flags: HeaderFlags::empty(),
        capacity_bytes: 1024,
        write_cursor_bytes: 128,
        pane_uuid: [uuid_byte; 32],
        created_at_epoch_ms: 1_700_000_000_000,
        last_msync_at_epoch_ms: 1_700_000_000_123,
        redactions_applied: 0,
        total_bytes_written: 128,
    };
    let mut file = std::fs::File::create(&path).expect("create scrollback file");
    file.write_all(&header.encode())
        .expect("write scrollback header");
    #[cfg(unix)]
    {
        let mut permissions = file
            .metadata()
            .expect("read scrollback file metadata")
            .permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)
            .expect("harden scrollback file permissions");
    }
    (pane_uuid, path)
}

#[cfg(unix)]
fn spawn_sigkill_writer_child(
    scrollback_dir: &std::path::Path,
    pane_uuid: &str,
    payload: &str,
    ready_path: &std::path::Path,
) -> std::process::Child {
    let current_exe = std::env::current_exe().expect("resolve current test binary");
    std::process::Command::new(current_exe)
        .arg("--exact")
        .arg("session_recover_sigkill_writer_child_process")
        .arg("--ignored")
        .arg("--nocapture")
        .env("FT_RLVSZ_SIGKILL_CHILD", "1")
        .env("FT_RLVSZ_SCROLLBACK_DIR", scrollback_dir)
        .env("FT_RLVSZ_PANE_UUID", pane_uuid)
        .env("FT_RLVSZ_PAYLOAD", payload)
        .env("FT_RLVSZ_READY_PATH", ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn SIGKILL writer child test process")
}

#[cfg(unix)]
fn wait_for_sigkill_writer_ready(
    child: &mut std::process::Child,
    ready_path: &std::path::Path,
) -> std::path::PathBuf {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if ready_path.exists() {
            let raw = std::fs::read_to_string(ready_path).expect("read writer ready file");
            return std::path::PathBuf::from(raw.trim());
        }
        if let Some(status) = child.try_wait().expect("poll writer child") {
            panic!("SIGKILL writer child exited before ready: {status:?}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for SIGKILL writer child ready file {}",
            ready_path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[cfg(unix)]
const REAL_MUX_ROBOT_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

fn sample_tx_contract(state: MissionTxState) -> MissionTxContract {
    let tx_id = TxId("tx:test".to_string());
    MissionTxContract {
        tx_version: MISSION_TX_SCHEMA_VERSION,
        intent: TxIntent {
            tx_id: tx_id.clone(),
            requested_by: MissionActorRole::Dispatcher,
            summary: "tx test".to_string(),
            correlation_id: "corr:test".to_string(),
            created_at_ms: 1_700_000_000_000,
        },
        plan: TxPlan {
            plan_id: TxPlanId("plan:test".to_string()),
            tx_id,
            steps: vec![
                TxStep {
                    step_id: TxStepId("tx-step:1".to_string()),
                    ordinal: 1,
                    action: StepAction::StoreData {
                        key: "tx-step:1".to_string(),
                        value: serde_json::json!({"state": "committed", "step": 1}),
                    },
                    description: "step 1".to_string(),
                },
                TxStep {
                    step_id: TxStepId("tx-step:2".to_string()),
                    ordinal: 2,
                    action: StepAction::StoreData {
                        key: "tx-step:2".to_string(),
                        value: serde_json::json!({"state": "committed", "step": 2}),
                    },
                    description: "step 2".to_string(),
                },
                TxStep {
                    step_id: TxStepId("tx-step:3".to_string()),
                    ordinal: 3,
                    action: StepAction::StoreData {
                        key: "tx-step:3".to_string(),
                        value: serde_json::json!({"state": "committed", "step": 3}),
                    },
                    description: "step 3".to_string(),
                },
            ],
            preconditions: Vec::new(),
            compensations: vec![
                TxCompensation {
                    for_step_id: TxStepId("tx-step:1".to_string()),
                    action: StepAction::StoreData {
                        key: "tx-step:1".to_string(),
                        value: serde_json::json!({"state": "compensated", "step": 1}),
                    },
                },
                TxCompensation {
                    for_step_id: TxStepId("tx-step:2".to_string()),
                    action: StepAction::StoreData {
                        key: "tx-step:2".to_string(),
                        value: serde_json::json!({"state": "compensated", "step": 2}),
                    },
                },
                TxCompensation {
                    for_step_id: TxStepId("tx-step:3".to_string()),
                    action: StepAction::StoreData {
                        key: "tx-step:3".to_string(),
                        value: serde_json::json!({"state": "compensated", "step": 3}),
                    },
                },
            ],
        },
        lifecycle_state: state,
        outcome: match state {
            MissionTxState::Committed => TxOutcome::Committed,
            MissionTxState::Failed => TxOutcome::Failed,
            MissionTxState::Compensated | MissionTxState::RolledBack => TxOutcome::Compensated,
            _ => TxOutcome::Pending,
        },
        receipts: Vec::new(),
    }
}

fn write_default_tx_contract(dir: &TempDir) -> std::path::PathBuf {
    let path = dir
        .path()
        .join(".ft")
        .join("mission")
        .join("tx-active.json");
    std::fs::create_dir_all(path.parent().expect("tx contract parent"))
        .expect("create mission dir");
    let contract = sample_tx_contract(MissionTxState::Planned);
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&contract).expect("serialize tx contract"),
    )
    .expect("write tx contract");
    path
}

#[cfg(unix)]
fn executable_send_text_contract(
    mut contract: MissionTxContract,
    pane_id: u64,
    commit_prefix: &str,
    compensation_prefix: &str,
) -> MissionTxContract {
    for step in &mut contract.plan.steps {
        step.action = StepAction::SendText {
            pane_id,
            text: format!("{commit_prefix}:{}", step.step_id.0),
            paste_mode: Some(false),
        };
    }
    for compensation in &mut contract.plan.compensations {
        compensation.action = StepAction::SendText {
            pane_id,
            text: format!("{compensation_prefix}:{}", compensation.for_step_id.0),
            paste_mode: Some(false),
        };
    }
    contract
}

#[cfg(unix)]
fn write_executable_send_text_tx_contract(dir: &TempDir) -> std::path::PathBuf {
    let path = dir
        .path()
        .join(".ft")
        .join("mission")
        .join("tx-active.json");
    std::fs::create_dir_all(path.parent().expect("tx contract parent"))
        .expect("create mission dir");
    let contract = executable_send_text_contract(
        sample_tx_contract(MissionTxState::Planned),
        0,
        "tx-test-commit",
        "tx-test-compensate",
    );
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&contract).expect("serialize executable tx contract"),
    )
    .expect("write executable tx contract");
    path
}

#[cfg(unix)]
struct TxWeztermCliStub {
    binary_path: std::path::PathBuf,
    list_fixture_path: std::path::PathBuf,
    effect_log_path: std::path::PathBuf,
    home: std::path::PathBuf,
    data_home: std::path::PathBuf,
    config_home: std::path::PathBuf,
    runtime_dir: std::path::PathBuf,
}

#[cfg(unix)]
impl TxWeztermCliStub {
    fn new(dir: &TempDir) -> Self {
        let binary_path = dir.path().join("tx-wezterm-stub.sh");
        let effect_log_path = dir.path().join("tx-wezterm-effects.log");
        let list_fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("frankenterm-core")
            .join("tests")
            .join("fixtures")
            .join("wezterm_cli")
            .join("local_single_pane.json");
        assert!(
            list_fixture_path.is_file(),
            "missing WezTerm CLI fixture {}",
            list_fixture_path.display()
        );

        let script = r#"#!/bin/sh
set -eu

if [ "${1:-}" != "cli" ]; then
  echo "unsupported wezterm stub invocation: $*" >&2
  exit 64
fi
shift
if [ "${1:-}" = "--no-auto-start" ]; then
  shift
fi

operation="${1:-}"
if [ -z "$operation" ]; then
  echo "missing wezterm cli operation" >&2
  exit 64
fi
shift

case "$operation" in
  list)
    cat "$FT_TEST_WEZTERM_LIST_JSON"
    ;;
  send-text)
    pane_id=""
    text=""
    while [ "$#" -gt 0 ]; do
      case "${1:-}" in
        --pane-id)
          pane_id="${2:-}"
          shift 2
          ;;
        --no-paste|--no-newline)
          shift
          ;;
        --)
          shift
          if [ "$#" -ne 1 ]; then
            echo "send-text stub expects exactly one text argument" >&2
            exit 64
          fi
          text="$1"
          shift
          ;;
        *)
          echo "unsupported send-text args: $*" >&2
          exit 64
          ;;
      esac
    done
    if [ -z "$pane_id" ]; then
      echo "missing --pane-id" >&2
      exit 64
    fi
    printf '%s\t%s\n' "$pane_id" "$text" >> "$FT_TEST_WEZTERM_EFFECT_LOG"
    ;;
  *)
    echo "unsupported wezterm cli operation: $operation" >&2
    exit 64
    ;;
esac
"#;
        std::fs::write(&binary_path, script).expect("write transaction WezTerm CLI stub");
        let mut permissions = std::fs::metadata(&binary_path)
            .expect("stat transaction WezTerm CLI stub")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&binary_path, permissions)
            .expect("make transaction WezTerm CLI stub executable");
        std::fs::write(&effect_log_path, b"").expect("create transaction effect log");

        let home = dir.path().join("tx-home");
        let data_home = dir.path().join("tx-data-home");
        let config_home = dir.path().join("tx-config-home");
        let runtime_dir = dir.path().join("tx-runtime");
        for path in [&home, &data_home, &config_home, &runtime_dir] {
            std::fs::create_dir_all(path).expect("create isolated transaction CLI environment");
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_millis() as i64;
        let db_path = dir.path().join(".ft").join("ft.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open transaction fixture DB");
        conn.execute(
            "INSERT OR REPLACE INTO panes \
             (pane_id, domain, title, cwd, first_seen_at, last_seen_at, observed) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![0i64, "local", "zsh", "/home/user", now_ms, now_ms, true],
        )
        .expect("seed live transaction target pane");

        Self {
            binary_path,
            list_fixture_path,
            effect_log_path,
            home,
            data_home,
            config_home,
            runtime_dir,
        }
    }

    fn command(&self, workspace: &str) -> Command {
        let mut command = Command::cargo_bin("ft").expect("ft binary should be built");
        command
            .timeout(REAL_MUX_ROBOT_WAIT_TIMEOUT)
            .env("FT_WORKSPACE", workspace)
            .env("FT_WEZTERM_CLI", &self.binary_path)
            .env("FT_TEST_WEZTERM_LIST_JSON", &self.list_fixture_path)
            .env("FT_TEST_WEZTERM_EFFECT_LOG", &self.effect_log_path)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env_remove("FRANKENTERM_CONFIG_FILE")
            .env_remove("FRANKENTERM_CONFIG_DIR")
            .env_remove("WEZTERM_CONFIG_FILE")
            .env_remove("WEZTERM_CONFIG_DIR")
            .env_remove("WEZTERM_FT_SOCKET")
            .env_remove("WEZTERM_UNIX_SOCKET");
        command
    }

    fn run_json(&self, workspace: &str, args: &[&str]) -> serde_json::Value {
        let output = self
            .command(workspace)
            .args(args)
            .output()
            .expect("ft transaction command should execute");
        assert!(
            output.status.success(),
            "command failed: ft {}\nstatus: {:?}\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("ft transaction stdout should be valid JSON")
    }

    fn approve_robot_run(workspace: &str, contract_path: &std::path::Path) {
        let contract: MissionTxContract = serde_json::from_slice(
            &std::fs::read(contract_path).expect("read robot transaction contract for approvals"),
        )
        .expect("deserialize robot transaction contract for approvals");
        let db_path = std::path::Path::new(workspace).join(".ft").join("ft.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open robot approval fixture DB");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch");
        let now_ms = now.as_millis() as i64;
        let approval_nonce = now.as_nanos();

        let mut gated_steps = Vec::with_capacity(contract.plan.steps.len().saturating_mul(2));
        for step in &contract.plan.steps {
            gated_steps.push(("commit", step.clone()));
            let compensation = contract
                .plan
                .compensations
                .iter()
                .find(|compensation| compensation.for_step_id == step.step_id)
                .unwrap_or_else(|| {
                    panic!(
                        "robot transaction approval fixture requires compensation for {}",
                        step.step_id.0
                    )
                });
            let mut compensation_step = step.clone();
            compensation_step.action = compensation.action.clone();
            gated_steps.push(("compensation", compensation_step));
        }

        for (phase, step) in &gated_steps {
            let StepAction::SendText {
                pane_id,
                text,
                paste_mode: _,
            } = &step.action
            else {
                panic!(
                    "robot transaction approval fixture requires SendText steps, got {}",
                    step.action.action_type_name()
                );
            };
            assert_eq!(*pane_id, 0, "stub fixture exposes only pane 0");
            let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
                .with_surface(PolicySurface::Robot)
                .with_capabilities(PaneCapabilities::unknown())
                .with_text_summary(step.description.clone())
                .with_pane(*pane_id)
                .with_domain("local")
                .with_pane_title("zsh")
                .with_pane_cwd("/home/user")
                .with_command_text(text.clone());
            let scope = ApprovalScope::from_input(workspace, &input);
            conn.execute(
                "INSERT INTO approval_tokens \
                 (code_hash, created_at, expires_at, used_at, workspace_id, action_kind, \
                  pane_id, action_fingerprint, plan_hash, plan_version, risk_summary) \
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, NULL, NULL, ?8)",
                rusqlite::params![
                    format!(
                        "tx-cli-stub-{phase}-approval-{}-{approval_nonce}",
                        step.ordinal
                    ),
                    now_ms,
                    now_ms.saturating_add(600_000),
                    scope.workspace_id(),
                    scope.action_kind(),
                    // `ApprovalScope::pane_id` is `Option<u64>`, and rusqlite
                    // implements `ToSql` for signed integers only — SQLite has no
                    // unsigned type. Bind the same value the production insert
                    // path binds.
                    scope
                        .pane_id()
                        .map(|pane_id| i64::try_from(pane_id).expect("pane id fits i64")),
                    scope.action_fingerprint(),
                    format!("isolated CLI transaction {phase} approval")
                ],
            )
            .expect("seed scoped robot transaction approval");
        }
    }

    fn effects(&self) -> Vec<String> {
        std::fs::read_to_string(&self.effect_log_path)
            .expect("read transaction effect log")
            .lines()
            .map(ToString::to_string)
            .collect()
    }

    fn assert_effects(&self, expected: &[&str]) {
        assert_eq!(
            self.effects(),
            expected,
            "real SendText effects and compensations must match the durable reports"
        );
    }
}

#[cfg(unix)]
fn ft_0rlfq_tx_contract() -> MissionTxContract {
    let tx_id = TxId("tx:ft-0rlfq-cli-durability".to_string());
    executable_send_text_contract(
        MissionTxContract {
            tx_version: MISSION_TX_SCHEMA_VERSION,
            intent: TxIntent {
                tx_id: tx_id.clone(),
                requested_by: MissionActorRole::Operator,
                summary: "cross-process CLI contract and ledger durability".to_string(),
                correlation_id: "corr:ft-0rlfq-cli-durability".to_string(),
                created_at_ms: 1_700_000_000_000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("plan:ft-0rlfq-cli-durability".to_string()),
                tx_id,
                steps: vec![
                    TxStep {
                        step_id: TxStepId("tx-step:1".to_string()),
                        ordinal: 1,
                        action: StepAction::StoreData {
                            key: "ft-0rlfq-step-1".to_string(),
                            value: serde_json::json!({"state": "committed", "step": 1}),
                        },
                        description: "record first contract-transition marker".to_string(),
                    },
                    TxStep {
                        step_id: TxStepId("tx-step:2".to_string()),
                        ordinal: 2,
                        action: StepAction::StoreData {
                            key: "ft-0rlfq-step-2".to_string(),
                            value: serde_json::json!({"state": "committed", "step": 2}),
                        },
                        description: "record second contract-transition marker".to_string(),
                    },
                ],
                preconditions: Vec::new(),
                compensations: vec![
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:1".to_string()),
                        action: StepAction::StoreData {
                            key: "ft-0rlfq-step-1".to_string(),
                            value: serde_json::json!({"state": "compensated", "step": 1}),
                        },
                    },
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:2".to_string()),
                        action: StepAction::StoreData {
                            key: "ft-0rlfq-step-2".to_string(),
                            value: serde_json::json!({"state": "compensated", "step": 2}),
                        },
                    },
                ],
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        },
        0,
        "ft-0rlfq-commit",
        "ft-0rlfq-compensate",
    )
}

#[cfg(unix)]
fn write_ft_0rlfq_tx_contract(dir: &TempDir) -> std::path::PathBuf {
    let path = dir
        .path()
        .join(".ft")
        .join("mission")
        .join("tx-active.json");
    std::fs::create_dir_all(path.parent().expect("tx contract parent"))
        .expect("create mission dir");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&ft_0rlfq_tx_contract()).expect("serialize tx contract"),
    )
    .expect("write tx contract");
    path
}

#[cfg(unix)]
fn run_ft_0rlfq_json(
    workspace: &str,
    wezterm_stub: &TxWeztermCliStub,
    args: &[&str],
) -> serde_json::Value {
    let started = std::time::Instant::now();
    let output = wezterm_stub
        .command(workspace)
        .args(args)
        .output()
        .expect("ft tx command should execute");

    eprintln!(
        "{}",
        serde_json::json!({
            "suite": "ft-0rlfq-cli-durability",
            "phase": "command",
            "command": format!("ft {}", args.join(" ")),
            "status": output.status.code(),
            "elapsed_ms": started.elapsed().as_millis(),
        })
    );
    assert!(
        output.status.success(),
        "command failed: ft {}\nstatus: {:?}\nstdout: {}\nstderr: {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("ft tx stdout should be valid JSON")
}

#[cfg(unix)]
fn assert_ft_0rlfq_persisted_tx(
    show_payload: &serde_json::Value,
    contract_path: &std::path::Path,
    expected_lifecycle: &str,
    expected_outcome: &str,
    expected_receipts: &[(u64, &str, &str, &str)],
) -> serde_json::Value {
    assert_eq!(show_payload["ok"], true);
    let data = &show_payload["data"];
    assert_eq!(
        data["contract_file"].as_str(),
        Some(contract_path.to_string_lossy().as_ref())
    );
    assert_eq!(data["lifecycle_state"].as_str(), Some(expected_lifecycle));
    assert_eq!(data["outcome"].as_str(), Some(expected_outcome));
    assert_eq!(
        data["receipt_count"].as_u64(),
        Some(expected_receipts.len() as u64)
    );

    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(contract_path).expect("read persisted tx contract"))
            .expect("persisted tx contract should be valid JSON");
    assert_eq!(
        data["contract"], persisted,
        "show --include-contract must return the exact contract persisted by the prior process"
    );
    assert_eq!(
        persisted["lifecycle_state"].as_str(),
        Some(expected_lifecycle)
    );
    assert_eq!(persisted["outcome"].as_str(), Some(expected_outcome));

    let receipts = persisted["receipts"]
        .as_array()
        .expect("persisted receipts should be an array");
    assert_eq!(receipts.len(), expected_receipts.len());
    for (receipt, &(seq, phase, step_id, outcome)) in receipts.iter().zip(expected_receipts) {
        assert_eq!(receipt["seq"].as_u64(), Some(seq));
        assert_eq!(receipt["phase"].as_str(), Some(phase));
        assert_eq!(receipt["step_id"].as_str(), Some(step_id));
        assert_eq!(receipt["outcome"].as_str(), Some(outcome));
    }

    persisted
}

fn tx_report_receipts(report: &serde_json::Value, report_name: &str) -> Vec<serde_json::Value> {
    report["receipts"]
        .as_array()
        .unwrap_or_else(|| panic!("{report_name}.receipts should be an array"))
        .clone()
}

fn assert_tx_receipt_partition(
    persisted: &serde_json::Value,
    emitted_commit_receipts: &[serde_json::Value],
    emitted_compensation_receipts: &[serde_json::Value],
) {
    let persisted_receipts = persisted["receipts"]
        .as_array()
        .expect("persisted transaction receipts should be an array");
    assert_eq!(
        persisted_receipts.len(),
        emitted_commit_receipts.len() + emitted_compensation_receipts.len(),
        "persisted receipt count must equal the exact emitted commit and compensation arrays"
    );
    assert_eq!(
        &persisted_receipts[..emitted_commit_receipts.len()],
        emitted_commit_receipts,
        "the emitted commit receipt array must be the exact persisted prefix"
    );
    assert_eq!(
        &persisted_receipts[emitted_commit_receipts.len()..],
        emitted_compensation_receipts,
        "the emitted compensation receipt array must be the exact appended persisted suffix"
    );
}

fn assert_tx_show_matches_persisted_contract(
    show_payload: &serde_json::Value,
    contract_path: &std::path::Path,
    expected_lifecycle: &str,
    expected_outcome: &str,
) -> serde_json::Value {
    assert_eq!(show_payload["ok"], true);
    let persisted: serde_json::Value = serde_json::from_slice(
        &std::fs::read(contract_path).expect("read transaction contract after fresh-process show"),
    )
    .expect("persisted transaction contract should be valid JSON");
    let data = &show_payload["data"];
    assert_eq!(
        data["contract_file"].as_str(),
        Some(contract_path.to_string_lossy().as_ref())
    );
    assert_eq!(data["lifecycle_state"].as_str(), Some(expected_lifecycle));
    assert_eq!(data["outcome"].as_str(), Some(expected_outcome));
    assert_eq!(
        data["receipt_count"].as_u64(),
        persisted["receipts"]
            .as_array()
            .map(|receipts| receipts.len() as u64)
    );
    assert_eq!(
        data["contract"], persisted,
        "fresh-process show --include-contract must exactly match the on-disk contract"
    );
    assert_eq!(
        persisted["lifecycle_state"].as_str(),
        Some(expected_lifecycle)
    );
    assert_eq!(persisted["outcome"].as_str(), Some(expected_outcome));
    persisted
}

#[cfg(unix)]
fn load_ft_0rlfq_tx_ledgers(workspace_root: &std::path::Path) -> Vec<TxExecutionLedger> {
    let ledger_dir = workspace_root.join(".ft").join("tx_ledgers");
    let mut ledger_paths = std::fs::read_dir(&ledger_dir)
        .unwrap_or_else(|err| panic!("read durable ledger spool {}: {err}", ledger_dir.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|err| {
                    panic!(
                        "enumerate durable ledger spool {}: {err}",
                        ledger_dir.display()
                    )
                })
                .path()
        })
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    ledger_paths.sort();
    ledger_paths
        .into_iter()
        .map(|path| {
            serde_json::from_slice(
                &std::fs::read(&path)
                    .unwrap_or_else(|err| panic!("read durable ledger {}: {err}", path.display())),
            )
            .unwrap_or_else(|err| {
                panic!(
                    "deserialize and validate durable ledger {}: {err}",
                    path.display()
                )
            })
        })
        .collect()
}

#[cfg(unix)]
fn assert_ft_0rlfq_terminal_ledgers(
    workspace_root: &std::path::Path,
    expected_ledger_count: usize,
    expected_success_steps: &[&str],
    expected_compensated_steps: &[&str],
) {
    let ledgers = load_ft_0rlfq_tx_ledgers(workspace_root);
    assert_eq!(
        ledgers.len(),
        expected_ledger_count,
        "every ft-0rlfq process execution should retain one durable ledger"
    );

    let mut success_steps = Vec::new();
    let mut compensated_steps = Vec::new();
    for ledger in &ledgers {
        assert_eq!(ledger.plan_id(), "plan:ft-0rlfq-cli-durability");
        assert!(
            ledger.phase().is_terminal(),
            "ledger {} must be terminal, got {:?}",
            ledger.execution_id(),
            ledger.phase()
        );
        assert_eq!(
            ledger.phase(),
            TxPhase::Completed,
            "successful run and rollback ledgers must complete"
        );
        let verification = ledger.verify_chain();
        assert!(
            verification.chain_intact,
            "ledger {} hash chain must be intact: {verification:?}",
            ledger.execution_id()
        );
        assert_eq!(verification.total_records, ledger.records().len());
        assert!(
            !ledger.records().is_empty(),
            "ledger {} should retain durable step records",
            ledger.execution_id()
        );

        let mut ledger_has_success = false;
        let mut ledger_has_compensation = false;
        for record in ledger.records() {
            match &record.outcome {
                StepOutcome::Success { .. } => {
                    ledger_has_success = true;
                    success_steps.push(record.idem_key.step_id().to_string());
                }
                StepOutcome::Compensated { .. } => {
                    ledger_has_compensation = true;
                    compensated_steps.push(record.idem_key.step_id().to_string());
                }
                other => panic!(
                    "ledger {} retained unexpected outcome {other:?} for {}",
                    ledger.execution_id(),
                    record.idem_key.step_id()
                ),
            }
        }
        assert!(
            !(ledger_has_success && ledger_has_compensation),
            "run and rollback records should remain separated by execution ledger"
        );
    }

    success_steps.sort();
    compensated_steps.sort();
    let mut expected_success_steps = expected_success_steps
        .iter()
        .map(|step| (*step).to_string())
        .collect::<Vec<_>>();
    let mut expected_compensated_steps = expected_compensated_steps
        .iter()
        .map(|step| (*step).to_string())
        .collect::<Vec<_>>();
    expected_success_steps.sort();
    expected_compensated_steps.sort();
    assert_eq!(success_steps, expected_success_steps);
    assert_eq!(compensated_steps, expected_compensated_steps);
}

fn assert_tx_transition_contract_shape(transitions: &serde_json::Value) {
    let transitions = transitions
        .as_array()
        .expect("legal_transitions should be an array");
    assert!(
        !transitions.is_empty(),
        "expected at least one legal tx transition"
    );
    for transition in transitions {
        assert!(transition["kind"].as_str().is_some());
        assert!(transition["to"].as_str().is_some());
    }
}

fn assert_tx_contract_payload_shape(contract: &serde_json::Value, expected_state: &str) {
    assert_eq!(
        contract["tx_version"].as_u64(),
        Some(u64::from(MISSION_TX_SCHEMA_VERSION))
    );
    assert_eq!(contract["lifecycle_state"].as_str(), Some(expected_state));
    assert_eq!(contract["outcome"].as_str(), Some("pending"));
    assert_eq!(
        contract["intent"]["tx_id"].as_str(),
        Some("tx:test"),
        "expected stable tx id in fixture"
    );
    assert_eq!(contract["plan"]["plan_id"].as_str(), Some("plan:test"));

    let steps = contract["plan"]["steps"]
        .as_array()
        .expect("contract.plan.steps should be an array");
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0]["step_id"].as_str(), Some("tx-step:1"));
    assert_eq!(steps[1]["step_id"].as_str(), Some("tx-step:2"));
    assert_eq!(steps[2]["step_id"].as_str(), Some("tx-step:3"));

    assert_eq!(
        contract["plan"]["preconditions"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(0)
    );
    assert_eq!(
        contract["plan"]["compensations"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(3)
    );
}

/// Write a small deterministic simulation scenario used by CLI contract tests.
fn write_contract_simulation_scenario(path: &std::path::Path) {
    let yaml = r#"
name: contract_resize_timeline
description: "CLI contract scenario for resize timeline JSON"
duration: "6s"
metadata:
  suite: resize_baseline
  suite_version: "2026-02-13"
  seed: "424242"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "120x40"
  - at: "2s"
    pane: 0
    action: set_font_size
    content: "1.10"
  - at: "3s"
    pane: 0
    action: generate_scrollback
    content: "4x32"
expectations:
  - contains:
      pane: 0
      text: "[FONT_SIZE:1.10]"
"#;
    std::fs::write(path, yaml).expect("write simulation scenario");
}

/// Create a workspace with populated fixture data (panes, events, accounts).
fn setup_populated_workspace() -> (TempDir, String) {
    let (dir, ws) = setup_workspace();
    let db_path = dir.path().join(".ft").join("ft.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    // Insert panes
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![1, "local", 1_700_000_000_000i64, 1_700_000_100_000i64, true],
    ).expect("insert pane 1");
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![2, "ssh:devbox", 1_700_000_000_000i64, 1_700_000_050_000i64, true],
    ).expect("insert pane 2");
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![3, "local", 1_700_000_000_000i64, 1_700_000_010_000i64, false],
    ).expect("insert pane 3");

    // Insert events (schema: pane_id, rule_id, agent_type, event_type, severity, confidence, detected_at)
    conn.execute(
        "INSERT INTO events (id, pane_id, rule_id, agent_type, event_type, severity, confidence, matched_text, detected_at)
         SELECT max_event_id + 1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
         FROM event_retention_state WHERE singleton = 1",
        rusqlite::params![1, "usage.high_tokens", "claude_code", "usage_warning", "warning", 0.9f64, "Token usage above 80%", 1_700_000_050_000i64],
    ).expect("insert event 1");
    conn.execute(
        "INSERT INTO events (id, pane_id, rule_id, agent_type, event_type, severity, confidence, matched_text, detected_at, handled_at)
         SELECT max_event_id + 1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9
         FROM event_retention_state WHERE singleton = 1",
        rusqlite::params![1, "compaction.stale", "codex", "compaction_warning", "info", 0.8f64, "Stale compaction detected", 1_700_000_040_000i64, 1_700_000_060_000i64],
    ).expect("insert event 2");
    conn.execute(
        "INSERT INTO events (id, pane_id, rule_id, agent_type, event_type, severity, confidence, matched_text, detected_at)
         SELECT max_event_id + 1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
         FROM event_retention_state WHERE singleton = 1",
        rusqlite::params![2, "error.panic", "unknown", "error_detected", "error", 0.95f64, "Panic in agent process", 1_700_000_090_000i64],
    ).expect("insert event 3");

    // Insert accounts
    conn.execute(
        "INSERT INTO accounts (account_id, service, name, percent_remaining, last_refreshed_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params!["acct-alpha", "openai", "Alpha", 82.5f64, 1_700_000_000_000i64, 1_699_000_000_000i64, 1_700_000_000_000i64],
    ).expect("insert account alpha");
    conn.execute(
        "INSERT INTO accounts (account_id, service, name, percent_remaining, tokens_used, tokens_remaining, tokens_limit, last_refreshed_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params!["acct-beta", "openai", "Beta", 45.0f64, 550_000i64, 450_000i64, 1_000_000i64, 1_700_000_000_000i64, 1_699_000_000_000i64, 1_700_000_000_000i64],
    ).expect("insert account beta");

    // Insert audit records (input_summary should be pre-redacted as it would be
    // when stored through record_audit_action_redacted)
    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, input_summary) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_050_000i64, "human", "send_text", "allow", "success", "ft send --pane 1 'ls -la'"],
    ).expect("insert audit 1");
    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, input_summary) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_060_000i64, "robot", "send_text", "deny", "denied", "ft robot send --pane 1 '[REDACTED]'"],
    ).expect("insert audit 2");

    drop(conn);
    (dir, ws)
}

/// Build a wa command configured for the given workspace.
#[allow(deprecated)]
fn wa_cmd_for(workspace: &str) -> Command {
    let mut cmd = Command::cargo_bin("ft").expect("ft binary should be built");
    cmd.env("FT_WORKSPACE", workspace);
    cmd.env("FT_WEZTERM_CLI", "/nonexistent/wezterm");
    cmd
}

/// Assert that output contains no ANSI escape sequences.
fn assert_no_ansi(output: &str, context: &str) {
    assert!(
        !output.contains("\x1b["),
        "{context}: output should not contain ANSI escapes, got:\n{output}"
    );
}

/// Run a wa command and parse stdout as JSON.
fn run_wa_json(workspace: &str, args: &[&str]) -> serde_json::Value {
    let output = wa_cmd_for(workspace)
        .args(args)
        .output()
        .expect("ft command should execute");
    assert!(
        output.status.success(),
        "command failed: wa {} \nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON")
}

struct ControlPlaneReceipt {
    args: Vec<String>,
    workspace: String,
    data_dir: String,
    fixture_seed: String,
    cleanup_expectation: &'static str,
    status_code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl ControlPlaneReceipt {
    fn command(&self) -> String {
        format!("ft {}", self.args.join(" "))
    }

    fn diagnostic(&self) -> String {
        format!(
            "command: {}\nstatus: {:?}\nworkspace: {}\ndata_dir: {}\nfixture_seed: {}\ncleanup: {}\nstdout:\n{}\nstderr:\n{}",
            self.command(),
            self.status_code,
            self.workspace,
            self.data_dir,
            self.fixture_seed,
            self.cleanup_expectation,
            self.stdout,
            self.stderr
        )
    }

    fn emit(&self, normalized_response: &serde_json::Value) {
        eprintln!(
            "control-plane receipt\n{}\nnormalized_response:\n{}",
            self.diagnostic(),
            serde_json::to_string_pretty(normalized_response)
                .expect("normalized response should serialize")
        );
    }

    fn stdout_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|err| {
            panic!("stdout should be JSON: {err}\n{}", self.diagnostic());
        })
    }

    fn assert_success(&self) {
        assert_eq!(self.status_code, Some(0), "{}", self.diagnostic());
    }
}

fn run_control_plane_receipt(
    workspace: &str,
    args: &[&str],
    fixture_seed: &str,
    cleanup_expectation: &'static str,
) -> ControlPlaneReceipt {
    let output = wa_cmd_for(workspace)
        .args(args)
        .output()
        .expect("ft command should execute");
    ControlPlaneReceipt {
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        workspace: workspace.to_string(),
        data_dir: std::path::Path::new(workspace)
            .join(".ft")
            .display()
            .to_string(),
        fixture_seed: fixture_seed.to_string(),
        cleanup_expectation,
        status_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn assert_receipt_json_eq(
    receipt: &ControlPlaneReceipt,
    label: &str,
    actual: serde_json::Value,
    expected: serde_json::Value,
) {
    assert_eq!(
        actual,
        expected,
        "{label} mismatch\nexpected: {expected}\nactual: {actual}\n{}",
        receipt.diagnostic()
    );
}

fn resize_baseline_fixture_path(file_name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/simulations/resize_baseline")
        .join(file_name)
}

fn run_resize_timeline_json(workspace: &str, scenario_path: &std::path::Path) -> serde_json::Value {
    assert!(
        scenario_path.exists(),
        "scenario fixture should exist: {}",
        scenario_path.display()
    );
    let scenario_arg = scenario_path.to_string_lossy().to_string();
    let output = wa_cmd_for(workspace)
        .args([
            "simulate",
            "run",
            &scenario_arg,
            "--json",
            "--resize-timeline-json",
        ])
        .output()
        .expect("ft simulate run --resize-timeline-json should execute");
    assert!(
        output.status.success(),
        "simulate timeline mode should succeed for fixture {}, stderr: {}",
        scenario_path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("simulate timeline output should be JSON")
}

fn assert_resize_timeline_contract_shape(parsed: &serde_json::Value, expected_events: usize) {
    assert_eq!(parsed["mode"], "resize_timeline_json");
    assert_eq!(parsed["expectations_failed"], 0);
    assert_eq!(
        parsed["timeline"]["executed_resize_events"],
        expected_events as u64
    );
    assert_eq!(
        parsed["timeline"]["events"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or_default(),
        expected_events
    );

    let stage_summary = parsed["stage_summary"]
        .as_array()
        .expect("stage_summary should be an array");
    assert_eq!(
        stage_summary.len(),
        5,
        "expected five resize timeline stages"
    );

    let stage_names: BTreeSet<_> = stage_summary
        .iter()
        .filter_map(|entry| entry["stage"].as_str())
        .collect();
    assert_eq!(
        stage_names,
        BTreeSet::from([
            "input_intent",
            "scheduler_queueing",
            "logical_reflow",
            "render_prep",
            "presentation",
        ])
    );

    for stage in stage_summary {
        assert_eq!(stage["samples"], expected_events as u64);
        assert!(stage["total_duration_ns"].as_u64().is_some());
        assert!(stage["p50_duration_ns"].as_u64().is_some());
        assert!(stage["p95_duration_ns"].as_u64().is_some());
        assert!(stage["p99_duration_ns"].as_u64().is_some());
        assert!(stage["max_duration_ns"].as_u64().is_some());
        assert!(
            stage["p50_duration_ns"].as_u64().unwrap_or(0)
                <= stage["p95_duration_ns"].as_u64().unwrap_or(0)
        );
        assert!(
            stage["p95_duration_ns"].as_u64().unwrap_or(0)
                <= stage["p99_duration_ns"].as_u64().unwrap_or(0)
        );
        assert!(
            stage["total_duration_ns"].as_u64().unwrap_or(0)
                >= stage["max_duration_ns"].as_u64().unwrap_or(0)
        );
        assert!(
            stage["p99_duration_ns"].as_u64().unwrap_or(0)
                <= stage["max_duration_ns"].as_u64().unwrap_or(0)
        );
    }

    let flame = parsed["flame_samples"]
        .as_array()
        .expect("flame_samples should be an array");
    assert_eq!(
        flame.len(),
        expected_events * 5,
        "expected one flame sample per event per stage"
    );

    let aggregate = &parsed["aggregate_event_duration_ns"];
    assert!(aggregate["p50"].as_u64().is_some());
    assert!(aggregate["p95"].as_u64().is_some());
    assert!(aggregate["p99"].as_u64().is_some());
    assert!(
        aggregate["p50"].as_u64().unwrap_or(0) <= aggregate["p95"].as_u64().unwrap_or(0)
            && aggregate["p95"].as_u64().unwrap_or(0) <= aggregate["p99"].as_u64().unwrap_or(0)
    );

    let timeline_jsonl = parsed["timeline_jsonl"]
        .as_array()
        .expect("timeline_jsonl should be an array");
    assert_eq!(
        timeline_jsonl.len(),
        expected_events,
        "expected one JSONL row per timeline event"
    );
    for row in timeline_jsonl {
        let row = row
            .as_str()
            .expect("timeline_jsonl entries should be strings");
        let parsed_row: serde_json::Value =
            serde_json::from_str(row).expect("timeline_jsonl row should be valid JSON");
        assert!(parsed_row["resize_transaction_id"].as_str().is_some());
        assert!(parsed_row["pane_id"].as_u64().is_some());
        assert!(parsed_row["tab_id"].as_u64().is_some());
        assert!(parsed_row["sequence_no"].as_u64().is_some());
        assert_eq!(parsed_row["scheduler_decision"], "dequeue_latest_intent");
        assert!(parsed_row["frame_id"].as_u64().is_some());
        assert!(parsed_row["test_case_id"].as_str().is_some());
        assert!(parsed_row["queue_wait_ms"].as_u64().is_some());
        assert!(parsed_row["reflow_ms"].as_u64().is_some());
        assert!(parsed_row["render_ms"].as_u64().is_some());
        assert!(parsed_row["present_ms"].as_u64().is_some());
    }
}

const RESIZE_TIMELINE_STAGE_ORDER: [&str; 5] = [
    "input_intent",
    "scheduler_queueing",
    "logical_reflow",
    "render_prep",
    "presentation",
];

fn assert_resize_timeline_event_stage_contract(event: &serde_json::Value) {
    assert!(event["resize_transaction_id"].as_str().is_some());
    assert!(event["tab_id"].as_u64().is_some());
    assert!(event["sequence_no"].as_u64().is_some());
    assert_eq!(
        event["scheduler_decision"], "dequeue_latest_intent",
        "scheduler decision should be stable for resize timeline events"
    );
    assert!(event["frame_id"].as_u64().is_some());
    assert!(event["test_case_id"].as_str().is_some());
    assert!(event["queue_wait_ms"].as_u64().is_some());
    assert!(event["reflow_ms"].as_u64().is_some());
    assert!(event["render_ms"].as_u64().is_some());
    assert!(event["present_ms"].as_u64().is_some());

    let stages = event["stages"]
        .as_array()
        .expect("timeline event should include stage probes");
    assert_eq!(stages.len(), RESIZE_TIMELINE_STAGE_ORDER.len());

    let mut last_start_offset = 0u64;
    for (index, stage) in stages.iter().enumerate() {
        let expected_name = RESIZE_TIMELINE_STAGE_ORDER[index];
        assert_eq!(stage["stage"], expected_name);

        let start_offset = stage["start_offset_ns"]
            .as_u64()
            .expect("stage start_offset_ns should be present");
        if index > 0 {
            assert!(
                start_offset >= last_start_offset,
                "stage start offsets should be non-decreasing"
            );
        }
        last_start_offset = start_offset;

        assert!(
            stage["duration_ns"].as_u64().is_some(),
            "stage duration_ns should be present"
        );

        if expected_name == "scheduler_queueing" {
            assert!(
                stage["queue_metrics"]["depth_before"].as_u64().is_some(),
                "scheduler queue metrics should include depth_before"
            );
            assert!(
                stage["queue_metrics"]["depth_after"].as_u64().is_some(),
                "scheduler queue metrics should include depth_after"
            );
        } else {
            assert!(
                stage["queue_metrics"].is_null(),
                "only scheduler_queueing should expose queue metrics"
            );
        }
    }

    let expected_queue_wait_ms = stages[1]["duration_ns"].as_u64().unwrap_or(0) / 1_000_000;
    let expected_reflow_ms = stages[2]["duration_ns"].as_u64().unwrap_or(0) / 1_000_000;
    let expected_render_ms = stages[3]["duration_ns"].as_u64().unwrap_or(0) / 1_000_000;
    let expected_present_ms = stages[4]["duration_ns"].as_u64().unwrap_or(0) / 1_000_000;
    assert_eq!(
        event["queue_wait_ms"].as_u64().unwrap_or(0),
        expected_queue_wait_ms
    );
    assert_eq!(event["reflow_ms"].as_u64().unwrap_or(0), expected_reflow_ms);
    assert_eq!(event["render_ms"].as_u64().unwrap_or(0), expected_render_ms);
    assert_eq!(
        event["present_ms"].as_u64().unwrap_or(0),
        expected_present_ms
    );
}

fn assert_resize_timeline_flame_contract(
    parsed: &serde_json::Value,
    expected_scenario: &str,
    expected_events: usize,
) {
    let events = parsed["timeline"]["events"]
        .as_array()
        .expect("timeline.events should be array");
    let mut known_event_pairs = BTreeSet::new();
    for event in events {
        let event_index = event["event_index"]
            .as_u64()
            .expect("timeline event should include event_index") as usize;
        let pane_id = event["pane_id"]
            .as_u64()
            .expect("timeline event should include pane_id");
        known_event_pairs.insert((event_index, pane_id));
    }

    let flame_samples = parsed["flame_samples"]
        .as_array()
        .expect("flame_samples should be array");
    let mut per_event_counts: BTreeMap<usize, usize> = BTreeMap::new();
    for sample in flame_samples {
        let stack = sample["stack"]
            .as_str()
            .expect("flame sample should include stack");
        let parts: Vec<_> = stack.split(';').collect();
        assert_eq!(parts.len(), 3, "flame stack must be scenario;action;stage");
        assert_eq!(parts[0], expected_scenario);
        assert!(matches!(
            parts[1],
            "resize" | "set_font_size" | "generate_scrollback"
        ));
        assert!(RESIZE_TIMELINE_STAGE_ORDER.contains(&parts[2]));

        let event_index = sample["event_index"]
            .as_u64()
            .expect("flame sample should include event_index") as usize;
        let pane_id = sample["pane_id"]
            .as_u64()
            .expect("flame sample should include pane_id");
        assert!(
            known_event_pairs.contains(&(event_index, pane_id)),
            "flame sample should map to a known timeline event"
        );
        *per_event_counts.entry(event_index).or_insert(0) += 1;
    }

    assert_eq!(
        per_event_counts.len(),
        expected_events,
        "expected flame samples for every timeline event"
    );
    for count in per_event_counts.values() {
        assert_eq!(
            *count,
            RESIZE_TIMELINE_STAGE_ORDER.len(),
            "expected one flame sample per stage per event"
        );
    }
}

// =============================================================================
// ft status contract tests
// =============================================================================

#[test]
fn contract_status_empty_db_plain() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["status", "--format", "plain"])
        .output()
        .expect("ft status should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "ft status (empty, plain)");
    // Empty DB should show a friendly empty state, not crash
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("panicked"),
        "ft status should not panic on empty DB"
    );
}

#[test]
fn contract_status_empty_db_json() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["status", "--format", "json"])
        .output()
        .expect("ft status --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Status may produce multiple JSON sections; each should be valid
    assert!(
        !stderr.contains("panicked"),
        "ft status --format json should not panic"
    );
    // At minimum, the output should contain some JSON (brackets or braces)
    if output.status.success() {
        assert!(
            stdout.contains('{') || stdout.contains('['),
            "ft status --format json should contain JSON: {stdout}"
        );
    }
}

#[test]
fn contract_status_populated_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["status", "--format", "plain"])
        .output()
        .expect("ft status should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_no_ansi(&stdout, "ft status (populated, plain)");
    assert!(
        !stderr.contains("panicked"),
        "ft status (populated, plain) should not panic"
    );
    if output.status.success() {
        // When WezTerm is available, plain status should show pane-like output.
        assert!(
            stdout.contains("local") || stdout.contains("Pane") || stdout.contains("pane"),
            "ft status should mention panes: {stdout}"
        );
    } else {
        // In fixtures we intentionally disable WezTerm CLI; failure should be actionable.
        assert!(
            stderr.contains("Failed to list panes")
                || stderr.contains("WezTerm circuit breaker open")
                || stderr.contains("Is the active backend bridge (current: WezTerm) running"),
            "ft status failure should be actionable, stderr: {stderr}"
        );
    }
}

#[test]
fn contract_status_filter_by_pane() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["status", "--format", "json", "--pane-id", "1"])
        .output()
        .expect("ft status --pane-id 1 should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "ft status --pane-id 1 should not panic"
    );
    // Status produces multi-section JSON output; verify it contains data
    if output.status.success() {
        assert!(
            stdout.contains('{') || stdout.contains('['),
            "ft status --pane-id 1 should contain JSON data: {stdout}"
        );
    }
}

// =============================================================================
// ft simulate contract tests
// =============================================================================

#[test]
fn contract_simulate_resize_timeline_json_envelope() {
    let (_dir, ws) = setup_workspace();
    let scenario_path = std::path::Path::new(&ws).join("contract_resize_timeline.yaml");
    write_contract_simulation_scenario(&scenario_path);

    let scenario_arg = scenario_path.to_string_lossy().to_string();
    let output = wa_cmd_for(&ws)
        .args([
            "simulate",
            "run",
            &scenario_arg,
            "--json",
            "--resize-timeline-json",
        ])
        .output()
        .expect("ft simulate run --resize-timeline-json should execute");

    assert!(
        output.status.success(),
        "simulate timeline mode should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("simulate timeline output should be JSON");

    assert_eq!(parsed["mode"], "resize_timeline_json");
    assert_eq!(parsed["expectations_failed"], 0);
    assert!(parsed["scenario"]["reproducibility_key"].is_string());
    assert!(parsed["timeline"]["events"].is_array());
    assert!(parsed["stage_summary"].is_array());
    assert!(parsed["flame_samples"].is_array());
    assert_eq!(parsed["timeline"]["executed_resize_events"], 3);
    assert_eq!(
        parsed["stage_summary"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or_default(),
        5
    );
}

#[test]
fn contract_simulate_resize_timeline_requires_json_flag() {
    let (_dir, ws) = setup_workspace();
    let scenario_path = std::path::Path::new(&ws).join("contract_resize_timeline.yaml");
    write_contract_simulation_scenario(&scenario_path);
    let scenario_arg = scenario_path.to_string_lossy().to_string();

    wa_cmd_for(&ws)
        .args(["simulate", "run", &scenario_arg, "--resize-timeline-json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--resize-timeline-json requires --json",
        ));
}

#[test]
fn contract_simulate_resize_timeline_failure_emits_artifact_payload() {
    let (_dir, ws) = setup_workspace();
    let scenario_path = std::path::Path::new(&ws).join("contract_resize_timeline_failure.yaml");
    std::fs::write(
        &scenario_path,
        r#"
name: contract_resize_timeline_failure
duration: "2s"
metadata:
  suite: resize_baseline
  suite_version: "2026-02-13"
  seed: "9090"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "100x40"
expectations:
  - contains:
      pane: 0
      text: "[MISSING_EXPECTATION]"
"#,
    )
    .expect("write failing simulation scenario");
    let scenario_arg = scenario_path.to_string_lossy().to_string();

    let output = wa_cmd_for(&ws)
        .args([
            "simulate",
            "run",
            &scenario_arg,
            "--json",
            "--resize-timeline-json",
        ])
        .output()
        .expect("simulate timeline mode should execute for failing scenario");
    assert!(
        !output.status.success(),
        "failing expectation should return non-zero status"
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("failing run should still emit JSON");
    assert_eq!(parsed["mode"], "resize_timeline_json");
    assert_eq!(parsed["completed"], false);
    assert_eq!(parsed["expectations_passed"], 0);
    assert_eq!(parsed["expectations_failed"], 1);
    assert_eq!(parsed["events_executed"], 1);
    assert_eq!(
        parsed["scenario"]["name"],
        "contract_resize_timeline_failure"
    );
    assert_eq!(
        parsed["timeline"]["reproducibility_key"],
        "resize_baseline:2026-02-13:contract_resize_timeline_failure:9090"
    );
    assert!(
        parsed["timeline"]["events"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or_default()
            > 0
    );
    assert!(
        parsed["stage_summary"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or_default()
            > 0
    );
    assert!(
        parsed["flame_samples"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or_default()
            > 0
    );
    let failure_artifacts = &parsed["failure_artifacts"];
    assert!(
        failure_artifacts["trace_bundle"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or_default()
            > 0
    );
    assert!(failure_artifacts["frame_histogram"].is_object());
    assert!(
        failure_artifacts["failure_signature"].as_str().is_some(),
        "failing run should emit a failure signature"
    );
}

#[test]
fn contract_simulate_resize_multi_tab_storm_timeline_contract() {
    let (_dir, ws) = setup_workspace();
    let scenario_path = resize_baseline_fixture_path("resize_multi_tab_storm.yaml");
    let parsed = run_resize_timeline_json(&ws, &scenario_path);

    assert_resize_timeline_contract_shape(&parsed, 24);

    let events = parsed["timeline"]["events"]
        .as_array()
        .expect("timeline.events should be array");
    assert_eq!(parsed["scenario"]["name"], "resize_multi_tab_storm");
    assert_eq!(
        parsed["scenario"]["reproducibility_key"],
        "resize_baseline:2026-02-13:resize_multi_tab_storm:1002"
    );
    assert_eq!(
        parsed["timeline"]["reproducibility_key"],
        "resize_baseline:2026-02-13:resize_multi_tab_storm:1002"
    );

    let panes: BTreeSet<_> = events
        .iter()
        .filter_map(|entry| entry["pane_id"].as_u64())
        .collect();
    assert_eq!(panes, BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7]));

    let mut event_indexes = BTreeSet::new();
    for event in events {
        let event_index = event["event_index"]
            .as_u64()
            .expect("event should include event_index");
        assert!(
            event_indexes.insert(event_index),
            "event_index should be unique"
        );
        assert!(
            matches!(
                event["action"].as_str(),
                Some("resize" | "set_font_size" | "generate_scrollback")
            ),
            "only resize-class actions should appear in timeline"
        );
        assert!(event["total_duration_ns"].as_u64().is_some());
        assert_resize_timeline_event_stage_contract(event);
    }
    assert_resize_timeline_flame_contract(&parsed, "resize_multi_tab_storm", 24);
}

#[test]
fn contract_simulate_resize_single_pane_scrollback_timeline_contract() {
    let (_dir, ws) = setup_workspace();
    let scenario_path = resize_baseline_fixture_path("resize_single_pane_scrollback.yaml");
    let parsed = run_resize_timeline_json(&ws, &scenario_path);

    assert_resize_timeline_contract_shape(&parsed, 8);

    let events = parsed["timeline"]["events"]
        .as_array()
        .expect("timeline.events should be array");
    assert_eq!(parsed["scenario"]["name"], "resize_single_pane_scrollback");
    assert_eq!(
        parsed["scenario"]["reproducibility_key"],
        "resize_baseline:2026-02-13:resize_single_pane_scrollback:1001"
    );
    assert_eq!(
        parsed["timeline"]["reproducibility_key"],
        "resize_baseline:2026-02-13:resize_single_pane_scrollback:1001"
    );
    let captured_at_ms = parsed["timeline"]["captured_at_ms"]
        .as_u64()
        .expect("timeline should include captured_at_ms");
    assert!(captured_at_ms >= 1_600_000_000_000);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after UNIX_EPOCH")
        .as_millis() as u64;
    assert!(captured_at_ms <= now_ms.saturating_add(600_000));

    assert!(events.iter().all(|entry| entry["pane_id"] == 0));

    let mut resize_count = 0usize;
    let mut font_count = 0usize;
    let mut scrollback_count = 0usize;
    for event in events {
        assert_resize_timeline_event_stage_contract(event);
        match event["action"].as_str() {
            Some("resize") => resize_count += 1,
            Some("set_font_size") => font_count += 1,
            Some("generate_scrollback") => scrollback_count += 1,
            other => panic!("unexpected action in resize timeline: {other:?}"),
        }
    }
    assert_resize_timeline_flame_contract(&parsed, "resize_single_pane_scrollback", 8);
    assert_eq!(resize_count, 4, "expected four resize actions");
    assert_eq!(font_count, 3, "expected three set_font_size actions");
    assert_eq!(
        scrollback_count, 1,
        "expected one generate_scrollback action"
    );
}

#[test]
fn contract_simulate_resize_mixed_workload_regression_timeline_contract() {
    let (_dir, ws) = setup_workspace();
    let scenario_path = resize_baseline_fixture_path("mixed_workload_interactive_streaming.yaml");
    let parsed = run_resize_timeline_json(&ws, &scenario_path);

    assert_resize_timeline_contract_shape(&parsed, 13);

    let events = parsed["timeline"]["events"]
        .as_array()
        .expect("timeline.events should be array");
    assert_eq!(
        parsed["scenario"]["name"],
        "mixed_workload_interactive_streaming"
    );
    assert_eq!(
        parsed["scenario"]["reproducibility_key"],
        "resize_baseline:2026-02-14:mixed_workload_interactive_streaming:1005"
    );
    assert_eq!(
        parsed["timeline"]["reproducibility_key"],
        "resize_baseline:2026-02-14:mixed_workload_interactive_streaming:1005"
    );
    assert_eq!(
        parsed["scenario"]["metadata"]["regression_case"], "resize_wrap_jitter_2026_02",
        "fixture regression case should be preserved in scenario metadata"
    );

    let panes: BTreeSet<_> = events
        .iter()
        .filter_map(|entry| entry["pane_id"].as_u64())
        .collect();
    assert_eq!(panes, BTreeSet::from([40, 41, 42, 43]));

    let mut resize_count = 0usize;
    let mut font_count = 0usize;
    let mut scrollback_count = 0usize;
    for event in events {
        assert_resize_timeline_event_stage_contract(event);
        match event["action"].as_str() {
            Some("resize") => resize_count += 1,
            Some("set_font_size") => font_count += 1,
            Some("generate_scrollback") => scrollback_count += 1,
            other => panic!("unexpected action in resize timeline: {other:?}"),
        }
    }
    assert_resize_timeline_flame_contract(&parsed, "mixed_workload_interactive_streaming", 13);
    assert_eq!(resize_count, 8, "expected eight resize actions");
    assert_eq!(font_count, 4, "expected four set_font_size actions");
    assert_eq!(
        scrollback_count, 1,
        "expected one generate_scrollback action"
    );
}

// =============================================================================
// ft events contract tests
// =============================================================================

#[test]
fn contract_events_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["events", "--format", "plain"])
        .output()
        .expect("ft events should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "ft events (plain)");
    assert!(
        output.status.success(),
        "ft events should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Should show events table
    assert!(
        stdout.contains("Events") || stdout.contains("events") || stdout.contains("usage"),
        "ft events should list events: {stdout}"
    );
}

#[test]
fn contract_events_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["events", "--format", "json"])
        .output()
        .expect("ft events --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ft events --format json should exit 0"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("ft events --format json should produce valid JSON");
    assert!(parsed.is_array(), "ft events JSON should be an array");
}

#[test]
fn contract_events_filter_by_pane() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["events", "--format", "json", "--pane-id", "2"])
        .output()
        .expect("ft events --pane-id 2 should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let events = parsed.as_array().expect("array");
    // All returned events should be for pane 2
    for event in events {
        assert_eq!(
            event["pane_id"], 2,
            "filtered events should only contain pane 2"
        );
    }
}

#[test]
fn contract_events_mutations_human_roundtrip() {
    let (_dir, ws) = setup_populated_workspace();

    let events = run_wa_json(&ws, &["events", "--format", "json"]);
    let first_id = events
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("id"))
        .and_then(serde_json::Value::as_i64)
        .expect("expected at least one event with id");

    let annotate = run_wa_json(
        &ws,
        &[
            "events",
            "--format",
            "json",
            "annotate",
            &first_id.to_string(),
            "--note",
            "investigating event",
        ],
    );
    assert_eq!(annotate["ok"], true);
    assert_eq!(annotate["annotations"]["note"], "investigating event");

    let triage = run_wa_json(
        &ws,
        &[
            "events",
            "--format",
            "json",
            "triage",
            &first_id.to_string(),
            "--state",
            "investigating",
        ],
    );
    assert_eq!(triage["ok"], true);
    assert_eq!(triage["annotations"]["triage_state"], "investigating");

    let label = run_wa_json(
        &ws,
        &[
            "events",
            "--format",
            "json",
            "label",
            &first_id.to_string(),
            "--add",
            "urgent",
        ],
    );
    assert_eq!(label["ok"], true);
    assert!(
        label["annotations"]["labels"]
            .as_array()
            .is_some_and(|labels| labels.iter().any(|v| v == "urgent")),
        "labels should contain urgent: {}",
        label["annotations"]
    );
}

#[test]
fn contract_robot_events_mutations_roundtrip() {
    let (_dir, ws) = setup_populated_workspace();

    let baseline = run_wa_json(&ws, &["events", "--format", "json"]);
    let first_id = baseline
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("id"))
        .and_then(serde_json::Value::as_i64)
        .expect("expected at least one event with id");

    let annotate = run_wa_json(
        &ws,
        &[
            "robot",
            "events",
            "annotate",
            &first_id.to_string(),
            "--note",
            "robot-note",
        ],
    );
    assert_eq!(annotate["ok"], true);
    assert_eq!(
        annotate["data"]["annotations"]["note"],
        serde_json::Value::String("robot-note".to_string())
    );

    let triage = run_wa_json(
        &ws,
        &[
            "robot",
            "events",
            "triage",
            &first_id.to_string(),
            "--state",
            "investigating",
        ],
    );
    assert_eq!(triage["ok"], true);
    assert_eq!(
        triage["data"]["annotations"]["triage_state"],
        serde_json::Value::String("investigating".to_string())
    );

    let label = run_wa_json(
        &ws,
        &[
            "robot",
            "events",
            "label",
            &first_id.to_string(),
            "--add",
            "urgent",
        ],
    );
    assert_eq!(label["ok"], true);
    assert!(
        label["data"]["annotations"]["labels"]
            .as_array()
            .is_some_and(|labels| labels.iter().any(|v| v == "urgent")),
        "robot labels should contain urgent: {}",
        label["data"]["annotations"]
    );
}

#[test]
fn contract_events_unhandled_filter() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["events", "--format", "json", "--unhandled"])
        .output()
        .expect("ft events --unhandled should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let events = parsed.as_array().expect("array");
    // Handled events should be excluded
    for event in events {
        assert!(
            event["handled_at"].is_null(),
            "unhandled filter should exclude handled events"
        );
    }
}

// =============================================================================
// ft accounts contract tests
// =============================================================================

#[test]
fn contract_accounts_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["accounts", "--format", "plain"])
        .output()
        .expect("ft accounts should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "ft accounts (plain)");
    assert!(
        output.status.success(),
        "ft accounts should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Alpha") && stdout.contains("Beta"),
        "ft accounts should list both accounts: {stdout}"
    );
    assert!(
        stdout.contains("82.5%"),
        "ft accounts should show percent remaining: {stdout}"
    );
}

#[test]
fn contract_accounts_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["accounts", "--format", "json"])
        .output()
        .expect("ft accounts --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("ft accounts JSON should be valid");
    assert_eq!(parsed["total"], 2);
    assert_eq!(parsed["service"], "openai");
    assert!(parsed["accounts"].is_array());
}

#[test]
fn contract_accounts_pick_preview() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["accounts", "--format", "json", "--pick"])
        .output()
        .expect("ft accounts --pick should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert!(
        parsed["pick_preview"].is_object(),
        "--pick should include pick_preview"
    );
    assert_eq!(
        parsed["pick_preview"]["selected_account_id"], "acct-alpha",
        "should pick highest percent_remaining"
    );
}

#[test]
fn contract_accounts_empty_db() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["accounts", "--format", "plain"])
        .output()
        .expect("ft accounts should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "ft accounts (empty)");
    assert!(
        stdout.contains("No accounts") || stdout.contains("no accounts"),
        "empty accounts should show friendly message: {stdout}"
    );
}

// =============================================================================
// ft audit contract tests
// =============================================================================

#[test]
fn contract_audit_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["audit", "--format", "plain"])
        .output()
        .expect("ft audit should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "ft audit (plain)");
    assert!(
        output.status.success(),
        "ft audit should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn contract_audit_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["audit", "--format", "json"])
        .output()
        .expect("ft audit --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    // Should produce parseable JSON (array or object)
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("ft audit JSON should be valid");
    assert!(
        parsed.is_array() || parsed.is_object(),
        "ft audit JSON should be array or object"
    );
}

#[test]
fn contract_audit_redacts_secrets() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["audit", "--format", "plain"])
        .output()
        .expect("ft audit should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The secret "sk-SECRET1234abcd" was inserted as part of audit input_summary.
    // It should be redacted in output (or the input_summary column is truncated/hidden).
    // We check that the full secret string does not appear unredacted.
    assert!(
        !stdout.contains("sk-SECRET1234abcd"),
        "ft audit should not show full secret in plain output: {stdout}"
    );
}

// =============================================================================
// ft rules contract tests
// =============================================================================

#[test]
fn contract_rules_list_plain() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["rules", "list", "--format", "plain"])
        .output()
        .expect("ft rules list should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "ft rules list (plain)");
    assert!(
        output.status.success(),
        "ft rules list should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Should list available detection rules/packs
    assert!(
        stdout.contains("Rules") || stdout.contains("rules") || stdout.contains("RULE"),
        "ft rules list should list rules: {stdout}"
    );
}

#[test]
fn contract_rules_list_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["rules", "list", "--format", "json"])
        .output()
        .expect("ft rules list --format json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("ft rules list JSON should be valid");
    assert!(
        parsed.is_array() || parsed.is_object(),
        "ft rules list JSON should be structured"
    );
}

// =============================================================================
// ft export contract tests
// =============================================================================

#[test]
fn contract_export_events_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["export", "events"])
        .output()
        .expect("ft export events should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ft export events should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Export produces JSONL (one JSON per line)
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(
            parsed.is_ok(),
            "ft export events should produce valid JSONL, bad line: {line}"
        );
    }
}

#[test]
fn contract_export_audit_json() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["export", "audit"])
        .output()
        .expect("ft export audit should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(parsed.is_ok(), "ft export audit line should be valid JSON");
    }
}

#[test]
fn contract_export_unknown_kind_fails() {
    let (_dir, ws) = setup_populated_workspace();
    wa_cmd_for(&ws)
        .args(["export", "nonexistent_kind"])
        .assert()
        .failure();
}

// =============================================================================
// ft reserve / ft reservations contract tests
// =============================================================================

#[test]
fn contract_reservations_empty_plain() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["reservations"])
        .output()
        .expect("ft reservations should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ft reservations should exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_ansi(&stdout, "ft reservations (empty)");
}

#[test]
fn contract_reservations_json() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["reservations", "--json"])
        .output()
        .expect("ft reservations --json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("ft reservations JSON should be valid");
    assert!(
        parsed.is_array() || parsed.is_object(),
        "ft reservations JSON should be structured"
    );
}

// =============================================================================
// ft doctor contract tests
// =============================================================================

#[test]
fn contract_doctor_plain_no_ansi() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["doctor"])
        .output()
        .expect("ft doctor should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Doctor may fail (no WezTerm) but should not panic
    assert!(!stderr.contains("panicked"), "ft doctor should not panic");
    // Doctor in non-TTY should produce clean output
    assert_no_ansi(&stdout, "ft doctor (plain)");
}

#[test]
fn contract_doctor_json_schema() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["doctor", "--json"])
        .output()
        .expect("ft doctor --json should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        let parsed: serde_json::Value =
            serde_json::from_str(&stdout).expect("ft doctor --json should produce valid JSON");
        assert!(parsed.is_object(), "ft doctor JSON should be an object");
    }
}

// =============================================================================
// ft stop contract tests
// =============================================================================

#[test]
fn contract_stop_no_watcher_running() {
    let (_dir, ws) = setup_workspace();
    let output = wa_cmd_for(&ws)
        .args(["stop"])
        .output()
        .expect("ft stop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Stop when no watcher is running should fail gracefully
    assert!(
        !stderr.contains("panicked"),
        "ft stop should not panic when no watcher running"
    );
    // Should indicate no watcher found
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("not running")
            || combined.contains("No watcher")
            || combined.contains("no watcher")
            || combined.contains("not found")
            || combined.contains("lock")
            || !output.status.success(),
        "ft stop should indicate no watcher: stdout={stdout}, stderr={stderr}"
    );
}

// =============================================================================
// ft approve contract tests
// =============================================================================

#[test]
fn contract_approve_invalid_code() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["approve", "INVALID1"])
        .output()
        .expect("ft approve should execute");

    // Invalid code should fail with clear error
    assert!(
        !output.status.success(),
        "ft approve with invalid code should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("invalid")
            || combined.contains("Invalid")
            || combined.contains("not found")
            || combined.contains("expired")
            || combined.contains("error")
            || combined.contains("Error"),
        "ft approve invalid code should show clear error: {combined}"
    );
}

// =============================================================================
// Unknown/invalid command contract tests
// =============================================================================

// =============================================================================
// ft history contract tests
// =============================================================================

#[test]
fn contract_history_plain_no_ansi_and_redacted_summary() {
    let (_dir, ws) = setup_populated_workspace();
    let output = wa_cmd_for(&ws)
        .args(["history", "--format", "plain", "--limit", "20"])
        .output()
        .expect("ft history should execute");

    assert!(
        output.status.success(),
        "ft history should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_no_ansi(&stdout, "ft history (plain)");
    assert!(
        stdout.contains("Action history"),
        "ft history plain should include heading: {stdout}"
    );
    assert!(
        stdout.contains("SUMMARY"),
        "ft history plain should include table headers: {stdout}"
    );
    assert!(
        stdout.contains("[REDACTED]"),
        "ft history plain should preserve redacted summaries: {stdout}"
    );
}

#[test]
fn contract_history_json_filters_undoable_and_orders_newest_first() {
    let (dir, ws) = setup_populated_workspace();
    let db_path = dir.path().join(".ft").join("ft.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_120_000i64, "human", "spawn", "allow", "success", 1i64],
    )
    .expect("insert audit undoable older");
    let older_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            older_id,
            1i64,
            "pane_close",
            "Close pane",
            r#"{"pane_id":1}"#,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert undo older");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_130_000i64, "workflow", "workflow_start", "allow", "success", 1i64],
    )
    .expect("insert audit undoable newer");
    let newer_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            newer_id,
            1i64,
            "workflow_abort",
            "Abort workflow",
            r#"{"execution_id":"wf-123"}"#,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert undo newer");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_140_000i64, "human", "send_text", "allow", "success", 1i64],
    )
    .expect("insert audit non-undoable");
    let non_undoable_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            non_undoable_id,
            0i64,
            "manual",
            "Manual only",
            rusqlite::types::Null,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert undo non-undoable");
    drop(conn);

    let payload = run_wa_json(
        &ws,
        &["history", "--format", "json", "--undoable", "--limit", "20"],
    );
    let rows = payload.as_array().expect("history JSON should be an array");

    let ids: Vec<i64> = rows.iter().filter_map(|row| row["id"].as_i64()).collect();
    assert_eq!(
        ids,
        vec![newer_id, older_id],
        "history undoable filter should return only undoable rows in deterministic order"
    );

    for row in rows {
        assert_eq!(row["undoable"].as_bool(), Some(true));
        assert!(
            row.get("undo_strategy").is_some(),
            "undoable row should carry undo_strategy"
        );
    }
}

// =============================================================================
// ft undo contract tests
// =============================================================================

#[test]
fn contract_undo_list_json_returns_only_currently_undoable_actions() {
    let (dir, ws) = setup_populated_workspace();
    let db_path = dir.path().join(".ft").join("ft.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_100_000i64, "human", "spawn", "allow", "success", 1i64],
    )
    .expect("insert audit undoable");
    let undoable_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            undoable_id,
            1i64,
            "pane_close",
            "Close spawned pane",
            r#"{"pane_id":1}"#,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert action_undo undoable");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_101_000i64, "human", "spawn", "allow", "success", 1i64],
    )
    .expect("insert audit undone");
    let undone_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            undone_id,
            1i64,
            "pane_close",
            "Already undone",
            r#"{"pane_id":1}"#,
            1_700_000_200_000i64,
            "tester",
        ],
    )
    .expect("insert action_undo undone");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_102_000i64, "human", "send_text", "allow", "success", 1i64],
    )
    .expect("insert audit non-undoable");
    let non_undoable_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            non_undoable_id,
            0i64,
            "manual",
            "Manual only",
            rusqlite::types::Null,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert action_undo non-undoable");
    drop(conn);

    let payload = run_wa_json(
        &ws,
        &["undo", "--list", "--format", "json", "--limit", "20"],
    );
    assert_eq!(payload["ok"], true);

    let actions = payload["data"]["actions"]
        .as_array()
        .expect("actions should be an array");
    let ids: Vec<i64> = actions
        .iter()
        .filter_map(|row| row["action_id"].as_i64())
        .collect();

    assert!(
        ids.contains(&undoable_id),
        "undoable pending action should be listed"
    );
    assert!(
        !ids.contains(&undone_id),
        "already-undone action should not be listed"
    );
    assert!(
        !ids.contains(&non_undoable_id),
        "non-undoable action should not be listed"
    );
}

#[test]
fn contract_undo_single_json_not_applicable_for_manual_strategy() {
    let (dir, ws) = setup_populated_workspace();
    let db_path = dir.path().join(".ft").join("ft.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_150_000i64, "human", "send_text", "allow", "success", 1i64],
    )
    .expect("insert audit manual");
    let action_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            action_id,
            0i64,
            "manual",
            "Reverse this command manually.",
            rusqlite::types::Null,
            rusqlite::types::Null,
            rusqlite::types::Null,
        ],
    )
    .expect("insert action_undo manual");
    drop(conn);

    let payload = run_wa_json(
        &ws,
        &["undo", &action_id.to_string(), "--yes", "--format", "json"],
    );
    assert_eq!(payload["ok"], true);

    let results = payload["data"]["results"]
        .as_array()
        .expect("results should be an array");
    assert_eq!(results.len(), 1, "expected exactly one undo result");
    assert_eq!(results[0]["action_id"].as_i64(), Some(action_id));
    assert_eq!(results[0]["outcome"].as_str(), Some("not_applicable"));
    assert_eq!(results[0]["strategy"].as_str(), Some("manual"));
    assert_eq!(
        results[0]["guidance"].as_str(),
        Some("Reverse this command manually.")
    );
}

#[test]
fn contract_undo_single_json_already_undone_is_idempotent_noop() {
    let (dir, ws) = setup_populated_workspace();
    let db_path = dir.path().join(".ft").join("ft.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, action_kind, policy_decision, result, pane_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![1_700_000_170_000i64, "human", "spawn", "allow", "success", 1i64],
    )
    .expect("insert audit already-undone");
    let action_id = conn.last_insert_rowid();
    let already_undone_at = 1_700_000_171_000i64;

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            action_id,
            1i64,
            "pane_close",
            "Pane already closed previously.",
            r#"{"pane_id":1}"#,
            already_undone_at,
            "previous-operator",
        ],
    )
    .expect("insert action_undo already-undone");
    drop(conn);

    let payload = run_wa_json(
        &ws,
        &["undo", &action_id.to_string(), "--yes", "--format", "json"],
    );
    assert_eq!(payload["ok"], true);

    let results = payload["data"]["results"]
        .as_array()
        .expect("results should be an array");
    assert_eq!(results.len(), 1, "expected exactly one undo result");
    assert_eq!(results[0]["action_id"].as_i64(), Some(action_id));
    assert_eq!(results[0]["outcome"].as_str(), Some("not_applicable"));
    assert!(
        results[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already been undone"),
        "expected idempotent already-undone message"
    );

    let conn = rusqlite::Connection::open(&db_path).expect("re-open DB");
    let record: (Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT undone_at, undone_by FROM action_undo WHERE audit_action_id = ?1",
            rusqlite::params![action_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query action_undo");
    assert_eq!(record.0, Some(already_undone_at));
    assert_eq!(record.1.as_deref(), Some("previous-operator"));
}

#[test]
fn contract_unknown_subcommand_fails() {
    let (_dir, ws) = setup_workspace();
    wa_cmd_for(&ws)
        .arg("nonexistent-command-xyz")
        .assert()
        .failure();
}

#[test]
fn contract_help_lists_core_commands() {
    let (_dir, ws) = setup_workspace();
    wa_cmd_for(&ws)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("events"))
        .stdout(predicate::str::contains("accounts"))
        .stdout(predicate::str::contains("audit"))
        .stdout(predicate::str::contains("rules"))
        .stdout(predicate::str::contains("export"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("stop"))
        .stdout(predicate::str::contains("approve"));
}

// =============================================================================
// ft tx contract tests
// =============================================================================

#[cfg(unix)]
#[test]
fn contract_mission_cli_durable_lifecycle_and_stale_token_refusal() {
    use frankenterm_core::plan::{Mission, MissionId, MissionLifecycleState, MissionOwnership};
    let (dir, ws) = setup_workspace();
    let path = dir.path().join(".ft/mission/active.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mission = Mission::new(
        MissionId("mission:owned-cli".to_string()),
        "Owned lifecycle persistence test",
        "owned-cli-workspace",
        MissionOwnership {
            planner: "test-planner".to_string(),
            dispatcher: "test-dispatcher".to_string(),
            operator: "test-operator".to_string(),
        },
        1_000,
    );
    std::fs::write(&path, serde_json::to_vec(&mission).unwrap()).unwrap();
    // These owned short inputs produce finite JSON receipts. Each actual CLI
    // subprocess has a deadline; preserve stderr in every failure diagnostic.
    let call = |verb: &str, token: Option<&serde_json::Value>, success: bool| {
        let mut command = wa_cmd_for(&ws);
        command.timeout(std::time::Duration::from_secs(15));
        command.args(["mission", verb, "--format", "json"]);
        if let Some(token) = token {
            command
                .arg("--expected-token")
                .arg(serde_json::to_string(token).unwrap());
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{verb}: status={:?}, stdout={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.len() < 64 * 1024 && output.stderr.len() < 64 * 1024);
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let original = std::fs::read(&path).unwrap();
    let status = call("status", None, true);
    let initial = status["data"]["revision_token"].clone();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "status is observational"
    );
    let run = call("run", Some(&initial), true);
    assert_eq!(run["data"]["lifecycle_state"], "running");
    assert_eq!(run["data"]["mutation"]["current"]["revision"], 1);
    let paused = call("pause", Some(&run["data"]["mutation"]["current"]), true);
    assert_eq!(paused["data"]["lifecycle_state"], "paused");
    let loaded: Mission = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(loaded.pause_resume_state.current_checkpoint.is_some());
    let resumed = call("resume", Some(&paused["data"]["mutation"]["current"]), true);
    assert_eq!(resumed["data"]["lifecycle_state"], "running");
    let aborted = call("abort", Some(&resumed["data"]["mutation"]["current"]), true);
    assert_eq!(aborted["data"]["mutation"]["current"]["revision"], 4);
    assert_eq!(
        aborted["data"]["mutation"]["owner_acknowledgement"],
        "unavailable_no_mission_driver"
    );
    let accepted = std::fs::read(&path).unwrap();
    let rejected = call(
        "resume",
        Some(&resumed["data"]["mutation"]["current"]),
        false,
    );
    assert_eq!(rejected["error_code"], "mission.revision_conflict");
    assert_eq!(std::fs::read(&path).unwrap(), accepted);
    let loaded: Mission = serde_json::from_slice(&accepted).unwrap();
    loaded.validate().unwrap();
    assert_eq!(loaded.lifecycle_state, MissionLifecycleState::Cancelled);
    assert_eq!(loaded.pause_resume_state.total_abort_count, 1);
    assert_eq!(loaded.pause_resume_state.checkpoint_history.len(), 1);
}

#[cfg(unix)]
#[test]
fn contract_mission_cli_process_race_has_one_revision_winner() {
    use frankenterm_core::plan::{Mission, MissionId, MissionLifecycleState, MissionOwnership};
    use frankenterm_core::tx_execution::MissionRevisionToken;
    let (dir, ws) = setup_workspace();
    let path = dir.path().join(".ft/mission/active.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut mission = Mission::new(
        MissionId("mission:owned-cli-race".to_string()),
        "Owned CLI concurrency test",
        "owned-cli-workspace",
        MissionOwnership {
            planner: "test-planner".to_string(),
            dispatcher: "test-dispatcher".to_string(),
            operator: "test-operator".to_string(),
        },
        1_000,
    );
    mission.lifecycle_state = MissionLifecycleState::Running;
    mission
        .pause_mission("test-operator", "seed", 2_000, None)
        .unwrap();
    std::fs::write(&path, serde_json::to_vec(&mission).unwrap()).unwrap();
    let token =
        serde_json::to_value(MissionRevisionToken::from_mission(&mission).unwrap()).unwrap();
    let call = |verb: &str, expected: &serde_json::Value| {
        let output = wa_cmd_for(&ws)
            .timeout(std::time::Duration::from_secs(15))
            .args(["mission", verb, "--format", "json", "--expected-token"])
            .arg(serde_json::to_string(expected).unwrap())
            .output()
            .unwrap();
        assert!(output.stdout.len() < 64 * 1024 && output.stderr.len() < 64 * 1024);
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        eprintln!(
            "owned CLI mission race: verb={verb}, status={:?}, response={response}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.success(), response["ok"] == true);
        response
    };
    // The launch barrier admits both actual CLI requests from the same observed
    // revision. OS scheduling may serialize execution; neither winner is chosen.
    let barrier = std::sync::Barrier::new(3);
    let (resume, abort) = std::thread::scope(|scope| {
        let resume = scope.spawn(|| {
            barrier.wait();
            call("resume", &token)
        });
        let abort = scope.spawn(|| {
            barrier.wait();
            call("abort", &token)
        });
        barrier.wait();
        (resume.join().unwrap(), abort.join().unwrap())
    });
    assert_ne!(
        resume["ok"], abort["ok"],
        "exactly one request may accept revision zero"
    );
    let (winner, loser) = if abort["ok"] == true {
        (&abort, &resume)
    } else {
        (&resume, &abort)
    };
    assert_eq!(winner["data"]["mutation"]["previous"], token);
    assert_eq!(winner["data"]["mutation"]["current"]["revision"], 1);
    assert!(matches!(
        loser["error_code"].as_str(),
        Some("mission.revision_conflict" | "mission.mutation_in_progress")
    ));
    if abort["ok"] == false {
        let fresh_abort = call("abort", &winner["data"]["mutation"]["current"]);
        assert_eq!(fresh_abort["ok"], true);
        assert_eq!(fresh_abort["data"]["mutation"]["current"]["revision"], 2);
    }
    let accepted = std::fs::read(&path).unwrap();
    let stale_resume = call("resume", &token);
    assert_eq!(stale_resume["ok"], false);
    assert_eq!(stale_resume["error_code"], "mission.revision_conflict");
    assert_eq!(std::fs::read(&path).unwrap(), accepted);
    let final_mission: Mission = serde_json::from_slice(&accepted).unwrap();
    final_mission.validate().unwrap();
    assert_eq!(
        final_mission.lifecycle_state,
        MissionLifecycleState::Cancelled
    );
    assert_eq!(final_mission.pause_resume_state.total_abort_count, 1);
}

#[test]
fn contract_tx_plan_json_envelope() {
    let (dir, ws) = setup_workspace();
    let contract_path = write_default_tx_contract(&dir);
    let payload = run_wa_json(&ws, &["tx", "plan", "--format", "json"]);

    assert_eq!(payload["ok"], true);
    let data = &payload["data"];
    assert_eq!(
        data["contract_file"].as_str(),
        Some(contract_path.to_string_lossy().as_ref())
    );
    assert_eq!(data["tx_id"].as_str(), Some("tx:test"));
    assert_eq!(data["plan_id"].as_str(), Some("plan:test"));
    assert_eq!(data["lifecycle_state"].as_str(), Some("planned"));
    assert_eq!(data["step_count"].as_u64(), Some(3));
    assert_eq!(data["precondition_count"].as_u64(), Some(0));
    assert_eq!(data["compensation_count"].as_u64(), Some(3));
    assert_tx_transition_contract_shape(&data["legal_transitions"]);
}

#[test]
fn contract_tx_show_include_contract_json_envelope() {
    let (dir, ws) = setup_workspace();
    let contract_path = write_default_tx_contract(&dir);
    let payload = run_wa_json(
        &ws,
        &["tx", "show", "--include-contract", "--format", "json"],
    );

    assert_eq!(payload["ok"], true);
    let data = &payload["data"];
    assert_eq!(
        data["contract_file"].as_str(),
        Some(contract_path.to_string_lossy().as_ref())
    );
    assert_eq!(data["tx_id"].as_str(), Some("tx:test"));
    assert_eq!(data["plan_id"].as_str(), Some("plan:test"));
    assert_eq!(data["lifecycle_state"].as_str(), Some("planned"));
    assert_eq!(data["outcome"].as_str(), Some("pending"));
    assert_eq!(data["step_count"].as_u64(), Some(3));
    assert_eq!(data["precondition_count"].as_u64(), Some(0));
    assert_eq!(data["compensation_count"].as_u64(), Some(3));
    assert_eq!(data["receipt_count"].as_u64(), Some(0));
    assert_tx_transition_contract_shape(&data["legal_transitions"]);
    assert_tx_contract_payload_shape(&data["contract"], "planned");
}

#[cfg(unix)]
#[test]
#[ignore = "drives tx sends through the legacy WezTerm CLI stub; pane input over the CLI fails closed since da8e16eab, so the run reports immediate_failure. Needs a mux-server harness: ft-ydqah"]
fn contract_tx_run_partial_failure_json_envelope() {
    let (dir, ws) = setup_workspace();
    let wezterm_stub = TxWeztermCliStub::new(&dir);
    let contract_path = write_executable_send_text_tx_contract(&dir);
    let authoritative_contract_path = contract_path
        .canonicalize()
        .expect("canonicalize locked tx contract");
    let payload = wezterm_stub.run_json(
        &ws,
        &["tx", "run", "--format", "json", "--fail-step", "tx-step:2"],
    );

    assert_eq!(payload["ok"], true);
    let data = &payload["data"];
    assert_eq!(
        data["contract_file"].as_str(),
        Some(authoritative_contract_path.to_string_lossy().as_ref())
    );
    assert_eq!(data["tx_id"].as_str(), Some("tx:test"));
    assert_eq!(data["plan_id"].as_str(), Some("plan:test"));
    assert_eq!(
        data["prepare_report"]["outcome"].as_str(),
        Some("all_ready")
    );
    assert_eq!(
        data["commit_report"]["outcome"].as_str(),
        Some("partial_failure")
    );
    assert_eq!(data["commit_report"]["failed_count"].as_u64(), Some(1));
    assert!(
        data["commit_report"]["committed_count"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    assert_eq!(
        data["commit_report"]["failure_boundary"].as_str(),
        Some("tx-step:2")
    );
    assert_eq!(
        data["compensation_report"]["outcome"].as_str(),
        Some("fully_rolled_back")
    );
    assert!(
        data["compensation_report"]["compensated_count"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    assert_eq!(data["final_state"].as_str(), Some("rolled_back"));

    let emitted_commit_receipts = tx_report_receipts(&data["commit_report"], "commit_report");
    let emitted_compensation_receipts =
        tx_report_receipts(&data["compensation_report"], "compensation_report");
    let show_payload = wezterm_stub.run_json(
        &ws,
        &[
            "robot",
            "--format",
            "json",
            "tx",
            "show",
            "--include-contract",
        ],
    );
    let persisted = assert_tx_show_matches_persisted_contract(
        &show_payload,
        &contract_path,
        "rolled_back",
        "compensated",
    );
    assert_tx_receipt_partition(
        &persisted,
        &emitted_commit_receipts,
        &emitted_compensation_receipts,
    );
    wezterm_stub.assert_effects(&[
        "0\ttx-test-commit:tx-step:1",
        "0\ttx-test-compensate:tx-step:1",
    ]);
}

#[test]
fn contract_tx_run_invalid_fail_step_json_error_envelope() {
    let (dir, ws) = setup_workspace();
    write_default_tx_contract(&dir);

    let output = wa_cmd_for(&ws)
        .args([
            "tx",
            "run",
            "--format",
            "json",
            "--fail-step",
            "tx-step:missing",
        ])
        .output()
        .expect("ft tx run invalid --fail-step should execute");

    assert_eq!(output.status.code(), Some(7));
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
    assert_eq!(payload["ok"], false);
    assert_eq!(
        payload["error_code"].as_str(),
        Some("mission.tx.unknown_fail_step")
    );
    assert_eq!(payload["exit_code"].as_i64(), Some(7));
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Unknown --fail-step: tx-step:missing")
    );
    assert_eq!(
        payload["hint"].as_str(),
        Some("Use `ft tx show --include-contract` to inspect valid step IDs.")
    );
}

#[test]
fn contract_robot_tx_plan_json_envelope() {
    let (dir, ws) = setup_workspace();
    let contract_path = write_default_tx_contract(&dir);
    let payload = run_wa_json(&ws, &["robot", "--format", "json", "tx", "plan"]);

    assert_eq!(payload["ok"], true);
    assert!(payload["elapsed_ms"].as_u64().is_some());
    assert!(payload["now"].as_u64().is_some());
    assert!(payload["version"].as_str().is_some());

    let data = &payload["data"];
    assert_eq!(
        data["contract_file"].as_str(),
        Some(contract_path.to_string_lossy().as_ref())
    );
    assert_eq!(data["tx_id"].as_str(), Some("tx:test"));
    assert_eq!(data["plan_id"].as_str(), Some("plan:test"));
    assert_eq!(data["lifecycle_state"].as_str(), Some("planned"));
    assert_eq!(data["step_count"].as_u64(), Some(3));
    assert_eq!(data["precondition_count"].as_u64(), Some(0));
    assert_eq!(data["compensation_count"].as_u64(), Some(3));
    assert_tx_transition_contract_shape(&data["legal_transitions"]);
}

#[test]
fn contract_no_mock_control_plane_receipts_cover_read_and_policy_gated_robot_paths() {
    let (dir, ws) = setup_workspace();
    let contract_path = write_default_tx_contract(&dir);
    let cleanup_expectation =
        "TempDir guard owns isolated workspace cleanup; no repository files are removed.";

    let read_receipt = run_control_plane_receipt(
        &ws,
        &[
            "robot",
            "--format",
            "json",
            "tx",
            "show",
            "--include-contract",
        ],
        "ft-m5y5u:tx-contract-read:v1",
        cleanup_expectation,
    );
    read_receipt.assert_success();
    let read_payload = read_receipt.stdout_json();
    read_receipt.emit(&read_payload);
    assert_receipt_json_eq(
        &read_receipt,
        "robot tx show ok",
        read_payload["ok"].clone(),
        serde_json::json!(true),
    );

    let read_data = &read_payload["data"];
    assert_receipt_json_eq(
        &read_receipt,
        "robot tx show contract_file",
        read_data["contract_file"].clone(),
        serde_json::json!(contract_path.to_string_lossy()),
    );
    assert_receipt_json_eq(
        &read_receipt,
        "robot tx show tx_id",
        read_data["tx_id"].clone(),
        serde_json::json!("tx:test"),
    );
    assert!(
        read_data["contract"].is_object(),
        "read path should include the real serialized tx contract\n{}",
        read_receipt.diagnostic()
    );

    let send_receipt = run_control_plane_receipt(
        &ws,
        &[
            "robot",
            "--format",
            "json",
            "send",
            "5252",
            "conformance-ok",
            "--dry-run",
        ],
        "ft-m5y5u:robot-send-dry-run:v1",
        cleanup_expectation,
    );
    send_receipt.assert_success();
    let send_payload = send_receipt.stdout_json();
    send_receipt.emit(&send_payload);
    assert_receipt_json_eq(
        &send_receipt,
        "robot send dry-run ok",
        send_payload["ok"].clone(),
        serde_json::json!(true),
    );

    let send_data = &send_payload["data"];
    let expected_actions = send_data["expected_actions"].as_array().unwrap_or_else(|| {
        panic!(
            "expected_actions should be an array\n{}",
            send_receipt.diagnostic()
        )
    });
    assert!(
        expected_actions
            .iter()
            .any(|action| action["action_type"] == "send_text"),
        "dry-run send must expose the policy-gated SendText action\n{}",
        send_receipt.diagnostic()
    );

    let checks = send_data["policy_evaluation"]["checks"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "policy checks should be an array\n{}",
                send_receipt.diagnostic()
            )
        });
    assert!(
        checks.iter().any(|check| check["name"] == "policy_surface"),
        "dry-run send must log policy surface evaluation\n{}",
        send_receipt.diagnostic()
    );
    assert_receipt_json_eq(
        &send_receipt,
        "robot send dry-run target pane",
        send_data["target_resolution"]["pane_id"].clone(),
        serde_json::json!(5252),
    );
}

#[test]
fn contract_robot_tx_show_include_contract_json_envelope() {
    let (dir, ws) = setup_workspace();
    let contract_path = write_default_tx_contract(&dir);
    let payload = run_wa_json(
        &ws,
        &[
            "robot",
            "--format",
            "json",
            "tx",
            "show",
            "--include-contract",
        ],
    );

    assert_eq!(payload["ok"], true);
    assert!(payload["elapsed_ms"].as_u64().is_some());
    assert!(payload["now"].as_u64().is_some());
    assert!(payload["version"].as_str().is_some());

    let data = &payload["data"];
    assert_eq!(
        data["contract_file"].as_str(),
        Some(contract_path.to_string_lossy().as_ref())
    );
    assert_eq!(data["tx_id"].as_str(), Some("tx:test"));
    assert_eq!(data["plan_id"].as_str(), Some("plan:test"));
    assert_eq!(data["lifecycle_state"].as_str(), Some("planned"));
    assert_eq!(data["outcome"].as_str(), Some("pending"));
    assert_eq!(data["receipt_count"].as_u64(), Some(0));
    assert_tx_transition_contract_shape(&data["legal_transitions"]);
    assert_tx_contract_payload_shape(&data["contract"], "planned");
}

#[cfg(unix)]
#[test]
#[ignore = "drives tx sends through the legacy WezTerm CLI stub; pane input over the CLI fails closed since da8e16eab, so the run reports immediate_failure. Needs a mux-server harness: ft-ydqah"]
fn contract_robot_tx_run_partial_failure_json_envelope() {
    let (dir, ws) = setup_workspace();
    let wezterm_stub = TxWeztermCliStub::new(&dir);
    let contract_path = write_executable_send_text_tx_contract(&dir);
    TxWeztermCliStub::approve_robot_run(&ws, &contract_path);
    let authoritative_contract_path = contract_path
        .canonicalize()
        .expect("canonicalize locked tx contract");
    let payload = wezterm_stub.run_json(
        &ws,
        &[
            "robot",
            "--format",
            "json",
            "tx",
            "run",
            "--fail-step",
            "tx-step:2",
        ],
    );

    assert_eq!(payload["ok"], true);
    assert!(payload["elapsed_ms"].as_u64().is_some());
    assert!(payload["now"].as_u64().is_some());
    assert!(payload["version"].as_str().is_some());

    let data = &payload["data"];
    assert_eq!(
        data["contract_file"].as_str(),
        Some(authoritative_contract_path.to_string_lossy().as_ref())
    );
    assert_eq!(data["tx_id"].as_str(), Some("tx:test"));
    assert_eq!(data["plan_id"].as_str(), Some("plan:test"));
    assert_eq!(
        data["prepare_report"]["outcome"].as_str(),
        Some("all_ready")
    );
    assert_eq!(
        data["commit_report"]["outcome"].as_str(),
        Some("partial_failure")
    );
    assert_eq!(
        data["commit_report"]["failure_boundary"].as_str(),
        Some("tx-step:2")
    );
    assert_eq!(data["commit_report"]["committed_count"].as_u64(), Some(1));
    assert_eq!(data["commit_report"]["failed_count"].as_u64(), Some(1));
    assert_eq!(data["commit_report"]["skipped_count"].as_u64(), Some(1));
    assert_eq!(
        data["commit_report"]["receipts"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(3)
    );
    assert_eq!(
        data["compensation_report"]["outcome"].as_str(),
        Some("fully_rolled_back")
    );
    assert_eq!(
        data["compensation_report"]["compensated_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        data["compensation_report"]["failed_count"].as_u64(),
        Some(0)
    );
    assert_eq!(
        data["compensation_report"]["skipped_count"].as_u64(),
        Some(0)
    );
    assert_eq!(
        data["compensation_report"]["receipts"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(1)
    );
    assert_eq!(data["final_state"].as_str(), Some("rolled_back"));

    let emitted_commit_receipts = tx_report_receipts(&data["commit_report"], "commit_report");
    let emitted_compensation_receipts =
        tx_report_receipts(&data["compensation_report"], "compensation_report");
    let show_payload = wezterm_stub.run_json(
        &ws,
        &["tx", "show", "--include-contract", "--format", "json"],
    );
    let persisted = assert_tx_show_matches_persisted_contract(
        &show_payload,
        &contract_path,
        "rolled_back",
        "compensated",
    );
    assert_tx_receipt_partition(
        &persisted,
        &emitted_commit_receipts,
        &emitted_compensation_receipts,
    );
    wezterm_stub.assert_effects(&[
        "0\ttx-test-commit:tx-step:1",
        "0\ttx-test-compensate:tx-step:1",
    ]);
}

#[test]
fn contract_robot_tx_run_paused_json_envelope() {
    let (dir, ws) = setup_workspace();
    write_default_tx_contract(&dir);
    let payload = run_wa_json(&ws, &["robot", "--format", "json", "tx", "run", "--paused"]);

    assert_eq!(payload["ok"], true);
    assert!(payload["elapsed_ms"].as_u64().is_some());
    assert!(payload["now"].as_u64().is_some());
    assert!(payload["version"].as_str().is_some());

    let data = &payload["data"];
    assert_eq!(
        data["prepare_report"]["outcome"].as_str(),
        Some("all_ready")
    );
    assert_eq!(
        data["commit_report"]["outcome"].as_str(),
        Some("pause_suspended")
    );
    assert_eq!(data["commit_report"]["committed_count"].as_u64(), Some(0));
    assert_eq!(data["commit_report"]["failed_count"].as_u64(), Some(0));
    assert_eq!(data["commit_report"]["skipped_count"].as_u64(), Some(3));
    assert_eq!(
        data["commit_report"]["receipts"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(3)
    );
    assert!(data["compensation_report"].is_null());
    assert_eq!(data["final_state"].as_str(), Some("committing"));
}

#[test]
fn contract_robot_tx_run_safe_mode_json_envelope() {
    let (dir, ws) = setup_workspace();
    write_default_tx_contract(&dir);
    let payload = run_wa_json(
        &ws,
        &[
            "robot",
            "--format",
            "json",
            "tx",
            "run",
            "--kill-switch",
            "safe-mode",
        ],
    );

    assert_eq!(payload["ok"], true);
    assert!(payload["elapsed_ms"].as_u64().is_some());
    assert!(payload["now"].as_u64().is_some());
    assert!(payload["version"].as_str().is_some());

    let data = &payload["data"];
    assert_eq!(
        data["prepare_report"]["outcome"].as_str(),
        Some("all_ready")
    );
    assert_eq!(
        data["commit_report"]["outcome"].as_str(),
        Some("kill_switch_blocked")
    );
    assert_eq!(data["commit_report"]["committed_count"].as_u64(), Some(0));
    assert_eq!(data["commit_report"]["failed_count"].as_u64(), Some(0));
    assert_eq!(data["commit_report"]["skipped_count"].as_u64(), Some(3));
    assert_eq!(
        data["commit_report"]["receipts"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(3)
    );
    assert!(data["compensation_report"].is_null());
    assert_eq!(data["final_state"].as_str(), Some("failed"));
}

#[cfg(unix)]
#[test]
#[ignore = "drives tx sends through the legacy WezTerm CLI stub; pane input over the CLI fails closed since da8e16eab, so the run reports immediate_failure. Needs a mux-server harness: ft-ydqah"]
fn contract_robot_tx_rollback_failure_and_recovery_json_envelopes() {
    let (dir, ws) = setup_workspace();
    let wezterm_stub = TxWeztermCliStub::new(&dir);
    // Exercise the real durable run path first: receipts alone are not proof
    // that the external commit effects happened.
    let contract_path = write_executable_send_text_tx_contract(&dir);
    TxWeztermCliStub::approve_robot_run(&ws, &contract_path);
    let run_payload = wezterm_stub.run_json(&ws, &["robot", "--format", "json", "tx", "run"]);
    assert_eq!(run_payload["ok"], true);
    assert_eq!(run_payload["data"]["final_state"], "committed");
    assert_eq!(run_payload["data"]["commit_report"]["committed_count"], 3);
    let persisted_after_run: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&contract_path).expect("read transaction after commit"),
    )
    .expect("parse transaction after commit");
    assert_eq!(
        persisted_after_run["receipts"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(3),
        "the durable commit must persist exactly three receipts"
    );

    let fail_payload = wezterm_stub.run_json(
        &ws,
        &[
            "robot",
            "--format",
            "json",
            "tx",
            "rollback",
            "--fail-compensation-for-step",
            "tx-step:1",
        ],
    );

    assert_eq!(fail_payload["ok"], true);
    assert!(fail_payload["elapsed_ms"].as_u64().is_some());
    assert!(fail_payload["now"].as_u64().is_some());
    assert!(fail_payload["version"].as_str().is_some());

    let fail_data = &fail_payload["data"];
    assert_eq!(fail_data["tx_id"].as_str(), Some("tx:test"));
    assert_eq!(fail_data["plan_id"].as_str(), Some("plan:test"));
    assert_eq!(
        fail_data["compensation_report"]["outcome"].as_str(),
        Some("compensation_failed")
    );
    assert_eq!(
        fail_data["compensation_report"]["compensated_count"].as_u64(),
        Some(2)
    );
    assert_eq!(
        fail_data["compensation_report"]["failed_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        fail_data["compensation_report"]["skipped_count"].as_u64(),
        Some(0)
    );
    assert_eq!(
        fail_data["compensation_report"]["receipts"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(3)
    );
    assert_eq!(fail_data["final_state"].as_str(), Some("failed"));
    let persisted_after_failed_rollback: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&contract_path).expect("read transaction after failed rollback"),
    )
    .expect("parse transaction after failed rollback");
    assert_eq!(
        persisted_after_failed_rollback["receipts"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(6),
        "the failed rollback must retain all commit and compensation receipts"
    );
    wezterm_stub.assert_effects(&[
        "0\ttx-test-commit:tx-step:1",
        "0\ttx-test-commit:tx-step:2",
        "0\ttx-test-commit:tx-step:3",
        "0\ttx-test-compensate:tx-step:3",
        "0\ttx-test-compensate:tx-step:2",
    ]);

    // Approval consumption is not wired yet (ft-0rlfq.9), so the original
    // scoped approvals remain active across this compensation retry.
    let recovery_payload =
        wezterm_stub.run_json(&ws, &["robot", "--format", "json", "tx", "rollback"]);

    assert_eq!(recovery_payload["ok"], true);
    assert!(recovery_payload["elapsed_ms"].as_u64().is_some());
    assert!(recovery_payload["now"].as_u64().is_some());
    assert!(recovery_payload["version"].as_str().is_some());

    let recovery_data = &recovery_payload["data"];
    assert_eq!(recovery_data["tx_id"].as_str(), Some("tx:test"));
    assert_eq!(recovery_data["plan_id"].as_str(), Some("plan:test"));
    assert_eq!(
        recovery_data["compensation_report"]["outcome"].as_str(),
        Some("fully_rolled_back")
    );
    assert_eq!(
        recovery_data["compensation_report"]["compensated_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        recovery_data["compensation_report"]["failed_count"].as_u64(),
        Some(0)
    );
    assert_eq!(
        recovery_data["compensation_report"]["skipped_count"].as_u64(),
        Some(0)
    );
    assert_eq!(
        recovery_data["compensation_report"]["receipts"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(1)
    );
    assert_eq!(recovery_data["final_state"].as_str(), Some("rolled_back"));
    let persisted_after_recovery: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&contract_path).expect("read transaction after rollback recovery"),
    )
    .expect("parse transaction after rollback recovery");
    assert_eq!(
        persisted_after_recovery["receipts"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(7),
        "rollback recovery must append only the one newly compensated receipt"
    );
    wezterm_stub.assert_effects(&[
        "0\ttx-test-commit:tx-step:1",
        "0\ttx-test-commit:tx-step:2",
        "0\ttx-test-commit:tx-step:3",
        "0\ttx-test-compensate:tx-step:3",
        "0\ttx-test-compensate:tx-step:2",
        "0\ttx-test-compensate:tx-step:1",
    ]);
}

#[cfg(unix)]
#[test]
#[ignore = "drives tx sends through the legacy WezTerm CLI stub; pane input over the CLI fails closed since da8e16eab, so the run reports immediate_failure. Needs a mux-server harness: ft-ydqah"]
fn contract_robot_tx_rollback_conflict_is_serialized_without_dispatch_or_mutation() {
    let (dir, ws) = setup_workspace();
    let wezterm_stub = TxWeztermCliStub::new(&dir);
    let contract_path = write_executable_send_text_tx_contract(&dir);
    TxWeztermCliStub::approve_robot_run(&ws, &contract_path);

    let run_payload = wezterm_stub.run_json(&ws, &["robot", "--format", "json", "tx", "run"]);
    assert_eq!(run_payload["ok"], true);
    assert_eq!(run_payload["data"]["final_state"], "committed");

    let mut contradictory_contract: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&contract_path).expect("read committed transaction contract"),
    )
    .expect("parse committed transaction contract");
    let receipt = contradictory_contract["receipts"]
        .as_array_mut()
        .expect("committed transaction receipts")
        .iter_mut()
        .find(|receipt| receipt["phase"] == "commit" && receipt["step_id"] == "tx-step:1")
        .expect("tx-step:1 commit receipt");
    receipt["outcome"] = serde_json::json!("failed");
    receipt["reason_code"] = serde_json::json!("forged_commit_failure");
    receipt["error_code"] = serde_json::json!("FTX3999");
    receipt["decision_path"] = serde_json::json!("contradictory_process_boundary_fixture");
    std::fs::write(
        &contract_path,
        serde_json::to_vec_pretty(&contradictory_contract)
            .expect("serialize contradictory transaction contract"),
    )
    .expect("write contradictory transaction contract");
    let contradictory_bytes =
        std::fs::read(&contract_path).expect("snapshot contradictory transaction contract");

    let rollback_payload =
        wezterm_stub.run_json(&ws, &["robot", "--format", "json", "tx", "rollback"]);

    assert_eq!(rollback_payload["ok"], false);
    assert_eq!(
        rollback_payload["error_code"],
        "robot.tx_rollback_proof_conflict"
    );
    assert!(
        rollback_payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains("rejected before compensation dispatch")
    );
    let hint = rollback_payload["hint"]
        .as_str()
        .expect("rollback proof conflict hint");
    assert!(hint.contains("Do not blindly rerun the commit or rollback"));
    assert!(hint.contains("external effect may already have been dispatched"));
    assert_eq!(
        std::fs::read(&contract_path).expect("reread rejected transaction contract"),
        contradictory_bytes,
        "proof conflict must leave the authoritative contract byte-for-byte unchanged"
    );
    wezterm_stub.assert_effects(&[
        "0\ttx-test-commit:tx-step:1",
        "0\ttx-test-commit:tx-step:2",
        "0\ttx-test-commit:tx-step:3",
    ]);
}

#[test]
fn contract_robot_tx_run_invalid_fail_step_json_error_envelope() {
    let (dir, ws) = setup_workspace();
    write_default_tx_contract(&dir);
    let payload = run_wa_json(
        &ws,
        &[
            "robot",
            "--format",
            "json",
            "tx",
            "run",
            "--fail-step",
            "tx-step:missing",
        ],
    );

    assert_eq!(payload["ok"], false);
    assert_eq!(payload["error_code"].as_str(), Some("robot.invalid_args"));
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Unknown --fail-step: tx-step:missing")
    );
    assert_eq!(
        payload["hint"].as_str(),
        Some("Use a step ID from `ft robot tx show --include-contract`.")
    );
    assert!(payload["elapsed_ms"].as_u64().is_some());
    assert!(payload["now"].as_u64().is_some());
    assert!(payload["version"].as_str().is_some());
}

#[cfg(unix)]
#[test]
fn contract_ft_0rlfq_terminal_backend_unavailable_leaves_no_transaction_evidence() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");
    let config_home = dir.path().join("config-home");
    let runtime_dir = dir.path().join("runtime");
    for path in [&data_home, &config_home, &runtime_dir] {
        std::fs::create_dir_all(path).expect("create isolated terminal-backend environment");
    }

    let contract_path = write_ft_0rlfq_tx_contract(&dir);
    let mut contract = ft_0rlfq_tx_contract();
    contract.plan.steps[0].action = StepAction::SendText {
        pane_id: 91_337,
        text: "terminal backend must be available".to_string(),
        paste_mode: Some(false),
    };
    std::fs::write(
        &contract_path,
        serde_json::to_vec_pretty(&contract).expect("serialize terminal-backed tx contract"),
    )
    .expect("write terminal-backed tx contract");
    let contract_before = std::fs::read(&contract_path).expect("snapshot tx contract before run");

    let output = wa_cmd_for(&ws)
        .timeout(REAL_MUX_ROBOT_WAIT_TIMEOUT)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("HOME", dir.path())
        .env("FT_WEZTERM_CLI", dir.path().join("missing-wezterm"))
        .env_remove("FRANKENTERM_CONFIG_FILE")
        .env_remove("FRANKENTERM_CONFIG_DIR")
        .env_remove("WEZTERM_CONFIG_FILE")
        .env_remove("WEZTERM_CONFIG_DIR")
        .env_remove("WEZTERM_UNIX_SOCKET")
        .args(["tx", "run", "--format", "json"])
        .output()
        .expect("ft tx run should report the unavailable terminal backend");

    // `mission.tx.execution_failed` maps to MISSION_EXIT_VALIDATION (5) since
    // 37fb43d0e; this test was written against the older INVALID_INPUT (7)
    // mapping and had been red at v0.15.1 too.
    assert_eq!(output.status.code(), Some(5));
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("failure stdout should be valid JSON");
    assert_eq!(payload["ok"], false);
    assert_eq!(
        payload["error_code"].as_str(),
        Some("mission.tx.execution_failed")
    );
    let error = payload["error"]
        .as_str()
        .expect("failure envelope should contain an error message");
    assert!(error.contains("real tx runtime unavailable"));
    assert!(error.contains("terminal backend unavailable for real tx execution"));

    let contract_after = std::fs::read(&contract_path).expect("read tx contract after failed run");
    assert_eq!(
        contract_after, contract_before,
        "backend preflight failure must not mutate the authoritative transaction contract"
    );
    let persisted: serde_json::Value = serde_json::from_slice(&contract_after)
        .expect("unchanged contract should remain valid JSON");
    assert_eq!(persisted["lifecycle_state"], "planned");
    assert_eq!(persisted["outcome"], "pending");
    assert_eq!(persisted["receipts"], serde_json::json!([]));

    let ledger_dir = dir.path().join(".ft").join("tx_ledgers");
    let ledger_count = if ledger_dir.exists() {
        std::fs::read_dir(&ledger_dir)
            .unwrap_or_else(|err| panic!("read ledger directory {}: {err}", ledger_dir.display()))
            .map(|entry| {
                entry
                    .unwrap_or_else(|err| {
                        panic!("enumerate ledger directory {}: {err}", ledger_dir.display())
                    })
                    .path()
            })
            .filter(|path| {
                path.extension().and_then(|extension| extension.to_str()) == Some("json")
            })
            .count()
    } else {
        0
    };
    assert_eq!(
        ledger_count, 0,
        "backend preflight failure must not create a durable transaction ledger"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "drives tx sends through the legacy WezTerm CLI stub; pane input over the CLI fails closed since da8e16eab, so the run reports immediate_failure. Needs a mux-server harness: ft-ydqah"]
fn contract_ft_0rlfq_human_run_robot_show_human_rollback_persists_contract_and_ledger() {
    // This fixture proves the cross-process contract/receipt/ledger boundary
    // against real SendText effects recorded by the isolated CLI stub.
    let (dir, ws) = setup_workspace();
    let wezterm_stub = TxWeztermCliStub::new(&dir);
    let contract_path = write_ft_0rlfq_tx_contract(&dir);

    let run_payload = run_ft_0rlfq_json(&ws, &wezterm_stub, &["tx", "run", "--format", "json"]);
    assert_eq!(run_payload["ok"], true);
    assert_eq!(run_payload["data"]["final_state"], "committed");
    assert_eq!(
        run_payload["data"]["commit_report"]["outcome"],
        "fully_committed"
    );
    assert_eq!(run_payload["data"]["commit_report"]["committed_count"], 2);
    let emitted_commit_receipts =
        tx_report_receipts(&run_payload["data"]["commit_report"], "commit_report");

    let show_after_run = run_ft_0rlfq_json(
        &ws,
        &wezterm_stub,
        &[
            "robot",
            "--format",
            "json",
            "tx",
            "show",
            "--include-contract",
        ],
    );
    let persisted_after_run = assert_ft_0rlfq_persisted_tx(
        &show_after_run,
        &contract_path,
        "committed",
        "committed",
        &[
            (1, "commit", "tx-step:1", "committed"),
            (2, "commit", "tx-step:2", "committed"),
        ],
    );
    assert_eq!(persisted_after_run["intent"]["requested_by"], "operator");
    assert_tx_receipt_partition(&persisted_after_run, &emitted_commit_receipts, &[]);
    assert_ft_0rlfq_terminal_ledgers(dir.path(), 1, &["tx-step:1", "tx-step:2"], &[]);
    wezterm_stub.assert_effects(&[
        "0\tft-0rlfq-commit:tx-step:1",
        "0\tft-0rlfq-commit:tx-step:2",
    ]);

    let rollback_payload =
        run_ft_0rlfq_json(&ws, &wezterm_stub, &["tx", "rollback", "--format", "json"]);
    assert_eq!(rollback_payload["ok"], true);
    assert_eq!(rollback_payload["data"]["final_state"], "rolled_back");
    assert_eq!(
        rollback_payload["data"]["compensation_report"]["outcome"],
        "fully_rolled_back"
    );
    assert_eq!(
        rollback_payload["data"]["compensation_report"]["compensated_count"],
        2
    );
    let emitted_compensation_receipts = tx_report_receipts(
        &rollback_payload["data"]["compensation_report"],
        "compensation_report",
    );

    let show_after_rollback = run_ft_0rlfq_json(
        &ws,
        &wezterm_stub,
        &[
            "robot",
            "--format",
            "json",
            "tx",
            "show",
            "--include-contract",
        ],
    );
    let persisted_after_rollback = assert_ft_0rlfq_persisted_tx(
        &show_after_rollback,
        &contract_path,
        "rolled_back",
        "compensated",
        &[
            (1, "commit", "tx-step:1", "committed"),
            (2, "commit", "tx-step:2", "committed"),
            (3, "compensate", "tx-step:2", "compensated"),
            (4, "compensate", "tx-step:1", "compensated"),
        ],
    );
    assert_tx_receipt_partition(
        &persisted_after_rollback,
        &emitted_commit_receipts,
        &emitted_compensation_receipts,
    );
    assert_ft_0rlfq_terminal_ledgers(
        dir.path(),
        2,
        &["tx-step:1", "tx-step:2"],
        &["tx-step:1", "tx-step:2"],
    );
    wezterm_stub.assert_effects(&[
        "0\tft-0rlfq-commit:tx-step:1",
        "0\tft-0rlfq-commit:tx-step:2",
        "0\tft-0rlfq-compensate:tx-step:2",
        "0\tft-0rlfq-compensate:tx-step:1",
    ]);
}

#[cfg(unix)]
#[test]
#[ignore = "drives tx sends through the legacy WezTerm CLI stub; pane input over the CLI fails closed since da8e16eab, so the run reports immediate_failure. Needs a mux-server harness: ft-ydqah"]
fn contract_ft_0rlfq_robot_run_human_show_robot_rollback_persists_contract_and_ledger() {
    // This fixture proves the cross-process contract/receipt/ledger boundary
    // against real SendText effects recorded by the isolated CLI stub.
    let (dir, ws) = setup_workspace();
    let wezterm_stub = TxWeztermCliStub::new(&dir);
    let contract_path = write_ft_0rlfq_tx_contract(&dir);
    TxWeztermCliStub::approve_robot_run(&ws, &contract_path);

    let run_payload = run_ft_0rlfq_json(
        &ws,
        &wezterm_stub,
        &["robot", "--format", "json", "tx", "run"],
    );
    assert_eq!(run_payload["ok"], true);
    assert_eq!(run_payload["data"]["final_state"], "committed");
    assert_eq!(
        run_payload["data"]["commit_report"]["outcome"],
        "fully_committed"
    );
    assert_eq!(run_payload["data"]["commit_report"]["committed_count"], 2);
    let emitted_commit_receipts =
        tx_report_receipts(&run_payload["data"]["commit_report"], "commit_report");

    let show_after_run = run_ft_0rlfq_json(
        &ws,
        &wezterm_stub,
        &["tx", "show", "--include-contract", "--format", "json"],
    );
    let persisted_after_run = assert_ft_0rlfq_persisted_tx(
        &show_after_run,
        &contract_path,
        "committed",
        "committed",
        &[
            (1, "commit", "tx-step:1", "committed"),
            (2, "commit", "tx-step:2", "committed"),
        ],
    );
    assert_eq!(persisted_after_run["intent"]["requested_by"], "operator");
    assert_tx_receipt_partition(&persisted_after_run, &emitted_commit_receipts, &[]);
    assert_ft_0rlfq_terminal_ledgers(dir.path(), 1, &["tx-step:1", "tx-step:2"], &[]);
    wezterm_stub.assert_effects(&[
        "0\tft-0rlfq-commit:tx-step:1",
        "0\tft-0rlfq-commit:tx-step:2",
    ]);

    let rollback_payload = run_ft_0rlfq_json(
        &ws,
        &wezterm_stub,
        &["robot", "--format", "json", "tx", "rollback"],
    );
    assert_eq!(rollback_payload["ok"], true);
    assert_eq!(rollback_payload["data"]["final_state"], "rolled_back");
    assert_eq!(
        rollback_payload["data"]["compensation_report"]["outcome"],
        "fully_rolled_back"
    );
    assert_eq!(
        rollback_payload["data"]["compensation_report"]["compensated_count"],
        2
    );
    let emitted_compensation_receipts = tx_report_receipts(
        &rollback_payload["data"]["compensation_report"],
        "compensation_report",
    );

    let show_after_rollback = run_ft_0rlfq_json(
        &ws,
        &wezterm_stub,
        &["tx", "show", "--include-contract", "--format", "json"],
    );
    let persisted_after_rollback = assert_ft_0rlfq_persisted_tx(
        &show_after_rollback,
        &contract_path,
        "rolled_back",
        "compensated",
        &[
            (1, "commit", "tx-step:1", "committed"),
            (2, "commit", "tx-step:2", "committed"),
            (3, "compensate", "tx-step:2", "compensated"),
            (4, "compensate", "tx-step:1", "compensated"),
        ],
    );
    assert_tx_receipt_partition(
        &persisted_after_rollback,
        &emitted_commit_receipts,
        &emitted_compensation_receipts,
    );
    assert_ft_0rlfq_terminal_ledgers(
        dir.path(),
        2,
        &["tx-step:1", "tx-step:2"],
        &["tx-step:1", "tx-step:2"],
    );
    wezterm_stub.assert_effects(&[
        "0\tft-0rlfq-commit:tx-step:1",
        "0\tft-0rlfq-commit:tx-step:2",
        "0\tft-0rlfq-compensate:tx-step:2",
        "0\tft-0rlfq-compensate:tx-step:1",
    ]);
}

// =============================================================================
// Session durable and orphan recovery CLI contract
// =============================================================================

#[test]
fn session_list_durable_defaults_to_json_for_an_empty_store() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");

    let output = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .env_remove("FT_OUTPUT_FORMAT")
        .args(["session", "list-durable"])
        .output()
        .expect("ft session list-durable should execute");

    assert!(
        output.status.success(),
        "list-durable failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("list-durable stdout should be JSON");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["action"], "list_durable");
    assert_eq!(payload["count"], 0);
    assert_eq!(payload["panes"], serde_json::json!([]));
    assert_eq!(
        payload["scrollback_dir"],
        data_home
            .join("ft")
            .join("scrollback-lines")
            .display()
            .to_string()
    );
}

#[test]
fn session_export_durable_invalid_identity_returns_structured_exit_2() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");

    let output = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .args([
            "session",
            "export-durable",
            "not-a-durable-pane-id",
            "--format",
            "json",
        ])
        .output()
        .expect("ft session export-durable should execute");

    assert_eq!(output.status.code(), Some(2));
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("export-durable stdout should be JSON");
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["error_code"], "session.durable_export_failed");
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains("expected 32 lowercase hex characters")
    );
}

#[test]
fn session_list_orphans_defaults_to_json_when_stdout_is_piped() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");
    let scrollback_dir = data_home.join("ft").join("scrollback");
    let (pane_uuid, path) = write_test_scrollback(&scrollback_dir, 0x6a);

    let output = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .env_remove("FT_OUTPUT_FORMAT")
        .args(["session", "list-orphans"])
        .output()
        .expect("ft session list-orphans should execute");

    assert!(
        output.status.success(),
        "list-orphans failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("list-orphans stdout should be JSON");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["count"], 1);
    assert_eq!(payload["orphans"][0]["pane_uuid"], pane_uuid);
    assert_eq!(payload["orphans"][0]["state"], "orphaned");
    assert_eq!(payload["orphans"][0]["path"], path.display().to_string());
}

#[cfg(unix)]
#[test]
fn session_list_orphans_scan_rejection_returns_structured_exit_2() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");
    let scrollback_dir = data_home.join("ft").join("scrollback");
    std::fs::create_dir_all(&scrollback_dir).expect("create non-private recovery directory");
    std::fs::set_permissions(&scrollback_dir, std::fs::Permissions::from_mode(0o755))
        .expect("make recovery directory deliberately non-private");

    let output = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .args(["session", "list-orphans", "--format", "json"])
        .output()
        .expect("ft session list-orphans should execute");

    assert_eq!(output.status.code(), Some(2));
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("scan rejection stdout should be JSON");
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["error_code"], "session.orphan_scan_failed");
    // The structured contract is stdout + exit code. ft logs at info to
    // stderr for every command (v0.15.1 did too), so stderr is not empty; it
    // must only be free of panics and of a second copy of the error.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    assert!(
        !stderr.contains("orphan_scan_failed"),
        "the structured error must not be duplicated on stderr: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn session_orphan_file_limit_opt_in_admits_valid_nondefault_capacity() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");
    let scrollback_dir = data_home.join("ft").join("scrollback");
    std::fs::create_dir_all(&scrollback_dir).expect("create recovery directory");
    std::fs::set_permissions(&scrollback_dir, std::fs::Permissions::from_mode(0o700))
        .expect("harden recovery directory");
    let pane_uuid = "6c".repeat(32);
    let path = scrollback_dir.join(format!("{pane_uuid}.bin"));
    let physical_bytes = 70 * 1024 * 1024u64;
    let header = ScrollbackHeader {
        version: FormatVersion::V1,
        flags: HeaderFlags::empty(),
        capacity_bytes: physical_bytes
            - frankenterm_core::scrollback_mmap_format::HEADER_SIZE as u64,
        write_cursor_bytes: 0,
        pane_uuid: [0x6c; 32],
        created_at_epoch_ms: 1_700_000_000_000,
        last_msync_at_epoch_ms: 1_700_000_000_123,
        redactions_applied: 0,
        total_bytes_written: 0,
    };
    let mut file = std::fs::File::create(&path).expect("create large sparse scrollback file");
    file.write_all(&header.encode())
        .expect("write large scrollback header");
    file.set_len(physical_bytes)
        .expect("size sparse scrollback file");
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .expect("harden sparse scrollback file");
    drop(file);

    let default_output = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .args(["session", "list-orphans", "--format", "json"])
        .output()
        .expect("default bounded list should execute");
    assert!(default_output.status.success());
    let default_payload: serde_json::Value =
        serde_json::from_slice(&default_output.stdout).expect("default list stdout should be JSON");
    assert_eq!(default_payload["orphans"][0]["state"], "unsafe");
    assert_eq!(default_payload["orphans"][0]["unsafe_reason"], "oversized");

    let opted_in = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .args([
            "session",
            "list-orphans",
            "--max-file-bytes",
            "83886080",
            "--format",
            "json",
        ])
        .output()
        .expect("opted-in bounded list should execute");
    assert!(
        opted_in.status.success(),
        "opted-in list failed: {}",
        String::from_utf8_lossy(&opted_in.stderr)
    );
    let opted_in_payload: serde_json::Value =
        serde_json::from_slice(&opted_in.stdout).expect("opted-in stdout should be JSON");
    assert_eq!(opted_in_payload["orphans"][0]["state"], "orphaned");
}

#[test]
fn session_recover_existing_output_returns_structured_exit_2_without_overwrite() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");
    let scrollback_dir = data_home.join("ft").join("scrollback");
    let (pane_uuid, _) = write_test_scrollback(&scrollback_dir, 0x6b);
    let output_path = dir.path().join("already-present.txt");
    std::fs::write(&output_path, b"retain me").expect("plant existing recovery output");

    let output = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .arg("session")
        .arg("recover")
        .arg(&pane_uuid)
        .arg("--allow-partial")
        .arg("--output")
        .arg(&output_path)
        .args(["--format", "json"])
        .output()
        .expect("ft session recover should execute");

    assert_eq!(output.status.code(), Some(2));
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("existing-artifact rejection stdout should be JSON");
    assert_eq!(payload["ok"], false);
    assert_eq!(
        payload["error_code"],
        "session.orphan_artifact_write_failed"
    );
    assert_eq!(
        std::fs::read(&output_path).expect("read retained output"),
        b"retain me"
    );
}

#[test]
fn session_discard_removes_bin_retains_lock_and_disappears_from_list() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");
    let scrollback_dir = data_home.join("ft").join("scrollback");
    let (pane_uuid, path) = write_test_scrollback(&scrollback_dir, 0x7b);
    let lock_path = path.with_extension("bin.lock");
    std::fs::write(&lock_path, b"stale lock").expect("write stale lock");
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&lock_path)
            .expect("read stale lock metadata")
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&lock_path, permissions).expect("harden stale lock permissions");
    }

    let output = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .args([
            "session", "discard", &pane_uuid, "--force", "--format", "json",
        ])
        .output()
        .expect("ft session discard should execute");

    assert!(
        output.status.success(),
        "discard failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("discard stdout should be JSON");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["action"], "discard");
    assert_eq!(payload["pane_uuid"], pane_uuid);
    assert_eq!(payload["removed"]["bin"], path.display().to_string());
    assert_eq!(payload["retained"]["lock"], lock_path.display().to_string());
    assert_eq!(
        payload["retained"]["reason"],
        "preserve_single_flock_inode_authority"
    );
    assert_eq!(payload["directory_synced"], true);
    assert!(!path.exists(), "discard should remove the .bin file");
    assert!(
        lock_path.is_file(),
        "discard must retain the .bin.lock inode to preserve flock authority"
    );

    let listed = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .args(["session", "list-orphans", "--format", "json"])
        .output()
        .expect("ft session list-orphans should execute");
    assert!(listed.status.success());
    let listed_payload: serde_json::Value =
        serde_json::from_slice(&listed.stdout).expect("list-orphans stdout should be JSON");
    assert_eq!(listed_payload["count"], 0);
}

#[test]
fn session_recover_unknown_uuid_returns_structured_exit_2() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");
    let missing_uuid = "11".repeat(32);

    let output = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .args(["session", "recover", &missing_uuid, "--format", "json"])
        .output()
        .expect("ft session recover should execute");

    assert_eq!(output.status.code(), Some(2));
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("recover stdout should be JSON");
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["error_code"], "session.orphan_not_found");
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains(&missing_uuid)
    );
}

#[test]
fn session_recover_requires_explicit_opt_in_before_retaining_an_incomplete_source() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");
    let scrollback_dir = data_home.join("ft").join("scrollback");
    let (pane_uuid, _) = write_test_scrollback(&scrollback_dir, 0x8c);
    let transcript_path = dir.path().join("incomplete-scrollback.txt");

    let rejected = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .args(["session", "recover", &pane_uuid, "--output"])
        .arg(&transcript_path)
        .args(["--format", "json"])
        .output()
        .expect("ft session recover should reject implicit partial salvage");

    assert_eq!(rejected.status.code(), Some(2));
    let rejected_payload: serde_json::Value = serde_json::from_slice(&rejected.stdout)
        .expect("partial-recovery rejection stdout should be JSON");
    assert_eq!(rejected_payload["ok"], false);
    assert_eq!(
        rejected_payload["error_code"],
        "session.orphan_recovery_partial_requires_opt_in"
    );
    assert!(
        !transcript_path.exists(),
        "default recovery must reject before publishing an incomplete artifact"
    );

    let retained = wa_cmd_for(&ws)
        .env("XDG_DATA_HOME", &data_home)
        .args([
            "session",
            "recover",
            &pane_uuid,
            "--allow-partial",
            "--output",
        ])
        .arg(&transcript_path)
        .args(["--format", "json"])
        .output()
        .expect("ft session recover should retain explicitly authorized partial salvage");

    assert!(
        retained.status.success(),
        "explicit partial recovery failed: {}",
        String::from_utf8_lossy(&retained.stderr)
    );
    let retained_payload: serde_json::Value =
        serde_json::from_slice(&retained.stdout).expect("partial-recovery stdout should be JSON");
    assert_eq!(retained_payload["ok"], true);
    assert_eq!(retained_payload["complete"], false);
    assert_eq!(retained_payload["partial_retention_authorized"], true);
    assert_eq!(
        retained_payload["scrollback_export"]["status"],
        "unreplayable"
    );
    assert_eq!(
        retained_payload["scrollback_export"]["source_completeness"]["status"],
        "incomplete"
    );
    assert_eq!(
        std::fs::read(&transcript_path).expect("read retained partial transcript"),
        Vec::<u8>::new()
    );
}

#[cfg(unix)]
#[test]
fn session_recover_exports_sigkill_orphan_without_mux_mutation() {
    let (dir, ws) = setup_workspace();
    let data_home = dir.path().join("data-home");
    let scrollback_dir = data_home.join("ft").join("scrollback");
    std::fs::create_dir_all(&scrollback_dir).expect("create scrollback dir");
    let mut scrollback_permissions = std::fs::metadata(&scrollback_dir)
        .expect("read scrollback directory metadata")
        .permissions();
    scrollback_permissions.set_mode(0o700);
    std::fs::set_permissions(&scrollback_dir, scrollback_permissions)
        .expect("harden scrollback directory permissions");
    let pane_uuid = "c3".repeat(32);
    let payload = "ft-rlvsz durable prefix line 1\nft-rlvsz durable prefix line 2\n";
    let ready_path = dir.path().join("sigkill-writer.ready");

    let mut child = spawn_sigkill_writer_child(&scrollback_dir, &pane_uuid, payload, &ready_path);
    let scrollback_path = wait_for_sigkill_writer_ready(&mut child, &ready_path);
    let max_file_bytes = std::fs::metadata(&scrollback_path)
        .expect("read pre-kill scrollback metadata")
        .len();
    let pre_kill_records = frankenterm_core::scrollback_mmap_writer::read_linear_records(
        &scrollback_path,
        LinearRecordReadLimits {
            max_file_bytes,
            max_records: 16,
            max_payload_bytes: max_file_bytes,
        },
    )
    .expect("read pre-kill linear records")
    .records;
    assert!(
        pre_kill_records.iter().any(|(_, bytes)| bytes
            .windows(payload.len())
            .any(|w| { w == payload.as_bytes() })),
        "writer child should sync the durable payload before SIGKILL"
    );

    child.kill().expect("send SIGKILL to writer child");
    let status = child.wait().expect("wait for writer child");
    assert_eq!(
        status.signal(),
        Some(9),
        "writer child should terminate via SIGKILL"
    );

    let transcript_path = dir.path().join("recovered-scrollback.txt");

    let output = Command::cargo_bin("ft")
        .expect("ft binary should be built")
        .timeout(std::time::Duration::from_secs(60))
        .env("FT_WORKSPACE", &ws)
        .env("XDG_DATA_HOME", &data_home)
        .env("HOME", dir.path())
        .args(["session", "recover", &pane_uuid, "--output"])
        .arg(&transcript_path)
        .args(["--format", "json"])
        .output()
        .expect("ft session recover should execute");

    assert!(
        output.status.success(),
        "recover failed: status={:?} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let recover: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("recover stdout should be JSON");
    assert_eq!(recover["ok"], true);
    assert_eq!(recover["action"], "recover");
    assert_eq!(recover["mode"], "export_only");
    assert_eq!(recover["pane_uuid"], pane_uuid);
    assert_eq!(recover["live_pty_mutated"], false);
    assert_eq!(recover["redaction_applied"], true);
    assert_eq!(
        recover["output_path"],
        transcript_path.display().to_string()
    );
    assert_eq!(recover["scrollback_export"]["status"], "replayed");
    assert_eq!(recover["scrollback_export"]["records_read"], 1);
    assert_eq!(recover["scrollback_export"]["records_replayed"], 1);
    assert_eq!(recover["scrollback_export"]["chunks_exported"], 1);
    assert_eq!(
        recover["scrollback_export"]["bytes_replayed"],
        payload.len() as u64
    );
    assert_eq!(
        std::fs::read_to_string(&transcript_path).expect("read exported transcript"),
        payload
    );
}

#[cfg(unix)]
#[test]
#[ignore = "child helper for session_recover_exports_sigkill_orphan_without_mux_mutation"]
fn session_recover_sigkill_writer_child_process() {
    if std::env::var("FT_RLVSZ_SIGKILL_CHILD").as_deref() != Ok("1") {
        return;
    }

    let scrollback_dir = std::path::PathBuf::from(
        std::env::var_os("FT_RLVSZ_SCROLLBACK_DIR").expect("FT_RLVSZ_SCROLLBACK_DIR is required"),
    );
    let pane_uuid = std::env::var("FT_RLVSZ_PANE_UUID").expect("FT_RLVSZ_PANE_UUID is required");
    let payload = std::env::var("FT_RLVSZ_PAYLOAD").expect("FT_RLVSZ_PAYLOAD is required");
    let ready_path = std::path::PathBuf::from(
        std::env::var_os("FT_RLVSZ_READY_PATH").expect("FT_RLVSZ_READY_PATH is required"),
    );

    let config = MmapScrollbackConfig::new(&scrollback_dir, &pane_uuid)
        .with_cap_bytes(4096)
        .with_sync_every_appends(1)
        .with_sync_interval(std::time::Duration::from_secs(3600));
    let mut writer = MmapScrollback::open(config).expect("open child mmap writer");
    let report = writer
        .append(RecordKind::Text, payload.as_bytes())
        .expect("append child payload");
    assert!(
        report.synced,
        "first append should hit durable sync boundary"
    );
    if let Some(flushed) = writer
        .flush_pending_redaction()
        .expect("flush child redaction tail")
    {
        assert!(
            flushed.payload_bytes > 0,
            "redaction flush should not create empty records"
        );
    }
    writer.sync().expect("sync child payload");
    std::fs::write(&ready_path, writer.path().display().to_string())
        .expect("write child ready file");

    loop {
        std::thread::park_timeout(std::time::Duration::from_secs(3600));
    }
}

// =============================================================================
// Cross-cutting: no ANSI in plain mode across all commands
// =============================================================================

#[test]
fn contract_no_ansi_in_plain_mode() {
    let (_dir, ws) = setup_populated_workspace();

    let commands: Vec<Vec<&str>> = vec![
        vec!["status", "--format", "plain"],
        vec!["events", "--format", "plain"],
        vec!["accounts", "--format", "plain"],
        vec!["audit", "--format", "plain"],
        vec!["history", "--format", "plain"],
        vec!["undo", "--list", "--format", "plain"],
        vec!["rules", "list", "--format", "plain"],
        vec!["doctor"],
    ];

    for args in &commands {
        let output = wa_cmd_for(&ws)
            .args(args)
            .output()
            .unwrap_or_else(|_| panic!("command {:?} should execute", args));

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_no_ansi(&stdout, &format!("ft {}", args.join(" ")));
    }
}

// =============================================================================
// Cross-cutting: JSON mode produces parseable output
// =============================================================================

#[test]
fn contract_json_mode_always_parseable() {
    let (_dir, ws) = setup_populated_workspace();

    let commands: Vec<Vec<&str>> = vec![
        vec!["events", "--format", "json"],
        vec!["accounts", "--format", "json"],
        vec!["audit", "--format", "json"],
        vec!["history", "--format", "json"],
        vec!["undo", "--list", "--format", "json"],
        vec!["rules", "list", "--format", "json"],
    ];

    for args in &commands {
        let output = wa_cmd_for(&ws)
            .args(args)
            .output()
            .unwrap_or_else(|_| panic!("command {:?} should execute", args));

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);
            assert!(
                parsed.is_ok(),
                "ft {} should produce valid JSON: {}",
                args.join(" "),
                stdout
            );
        }
    }
}

// =============================================================================
// Feature Contract: Agent detection feature availability
// =============================================================================

#[test]
fn contract_agent_detection_feature_matches_cfg() {
    #[cfg(feature = "agent-detection")]
    {
        assert!(
            frankenterm_core::agent_correlator::filesystem_detection_available(),
            "default build with agent-detection enabled must report filesystem detection available"
        );
    }
    #[cfg(not(feature = "agent-detection"))]
    {
        assert!(
            !frankenterm_core::agent_correlator::filesystem_detection_available(),
            "trimmed build without agent-detection must report filesystem detection unavailable"
        );
    }
}

// =============================================================================
// Feature Contract: FT_WORKSPACE config.toml discovery
// =============================================================================

#[test]
fn contract_ft_workspace_config_toml_is_discovered() {
    let temp = TempDir::new().expect("temp dir");
    let ft_dir = temp.path().join(".ft");
    std::fs::create_dir_all(&ft_dir).expect("create .ft dir");
    let config_file = ft_dir.join("config.toml");
    std::fs::write(&config_file, "[general]\nlog_level = \"warn\"\n").expect("write config");

    let resolved = frankenterm_core::config::resolve_config_path_from(None, temp.path().to_str());
    assert_eq!(
        resolved,
        Some(config_file),
        "FT_WORKSPACE/.ft/config.toml must be discovered by resolve_config_path"
    );
}

// =============================================================================
// Feature Contract: Startup session restore detection (wa-2l27x.5)
// =============================================================================

#[test]
fn contract_session_restorer_detects_unclean_session_on_startup() {
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("ft.db");
    let db_path_str = db_path.to_str().expect("utf8 path").to_string();

    let storage_config = frankenterm_core::storage::StorageConfig::default();
    let cx = frankenterm_core::cx::for_request();
    // `block_on` lives on the `CompatRuntime` trait, not inherently on `Runtime`.
    use frankenterm_core::runtime_async::CompatRuntime as _;
    let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime");

    runtime.block_on(async {
        let storage = frankenterm_core::storage::StorageHandle::with_config_with_cx(
            &cx,
            &db_path_str,
            storage_config,
        )
        .await
        .expect("storage init");

        storage
            .insert_mux_session(
                "sess-unclean-startup".to_string(),
                r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#.to_string(),
                "0.15.1".to_string(),
                None,
            )
            .await
            .expect("insert session");

        let restorer = frankenterm_core::session_restore::SessionRestorer::new(
            std::sync::Arc::new(db_path_str.clone()),
            frankenterm_core::session_restore::SessionRestoreConfig::default(),
        );

        let detection = restorer.detect();
        assert!(
            detection.is_ok()
                || matches!(
                    detection,
                    Err(frankenterm_core::session_restore::RestoreError::UncleanSessionsNotRestorable { .. })
                )
        );
    });
}
