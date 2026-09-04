//! E2E coverage for `ft steer plan` / `ft steer run` — drives the real binary
//! against a temp workspace and asserts deterministic receipts plus bound,
//! durable transaction execution.

use assert_cmd::Command;
#[cfg(unix)]
use frankenterm_core::approval::ApprovalScope;
use frankenterm_core::config::Config;
use frankenterm_core::plan::{
    MISSION_TX_SCHEMA_VERSION, MissionActorRole, MissionTxContract, MissionTxState, StepAction,
    TxCompensation, TxId, TxIntent, TxOutcome, TxPlan, TxPlanId, TxStep, TxStepId,
};
#[cfg(unix)]
use frankenterm_core::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyInput, PolicySurface,
};
#[cfg(unix)]
use frankenterm_core::steer_receipt_store::persist_receipt;
#[cfg(unix)]
use frankenterm_core::steering::SteeringReceipt;
#[cfg(unix)]
use frankenterm_core::tx_idempotency::{StepOutcome, TxExecutionLedger, TxPhase};
use std::path::Path;
#[cfg(unix)]
use std::{
    os::unix::fs::PermissionsExt as _,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

fn ft(ws: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ft").expect("ft binary");
    cmd.env("FT_WORKSPACE", ws)
        .env("FT_WEZTERM_CLI", "/nonexistent/wezterm");
    cmd
}

fn workspace() -> TempDir {
    let ws = TempDir::new().expect("temp workspace");
    let db = Config::default().effective_db_path(ws.path());
    let ft_dir = db.parent().expect("db parent");
    std::fs::create_dir_all(ft_dir).expect("create .ft dir");
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(ft_dir)
            .expect("stat .ft fixture directory")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(ft_dir, permissions).expect("harden .ft fixture directory");
    }
    let conn = rusqlite::Connection::open(&db).expect("open fixture DB");
    frankenterm_core::storage::initialize_schema(&conn).expect("initialize fixture schema");
    drop(conn);
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&db)
            .expect("stat fixture DB")
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&db, permissions).expect("harden fixture DB");
    }
    ws
}

fn stdout(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).expect("utf8")
}

fn executable_tx_contract() -> MissionTxContract {
    let tx_id = TxId("tx:steer-bound-e2e".to_string());
    let step_id = TxStepId("tx-step:steer-send".to_string());
    MissionTxContract {
        tx_version: MISSION_TX_SCHEMA_VERSION,
        intent: TxIntent {
            tx_id: tx_id.clone(),
            requested_by: MissionActorRole::Operator,
            summary: "execute the tx contract bound to a steering receipt".to_string(),
            correlation_id: "corr:steer-bound-e2e".to_string(),
            created_at_ms: 1_700_000_000_000,
        },
        plan: TxPlan {
            plan_id: TxPlanId("plan:steer-bound-e2e".to_string()),
            tx_id,
            steps: vec![TxStep {
                step_id: step_id.clone(),
                ordinal: 1,
                action: StepAction::SendText {
                    pane_id: 0,
                    text: "steer-bound-commit".to_string(),
                    paste_mode: Some(false),
                },
                description: "send the admitted steering action".to_string(),
            }],
            preconditions: Vec::new(),
            compensations: vec![TxCompensation {
                for_step_id: step_id,
                action: StepAction::SendText {
                    pane_id: 0,
                    text: "steer-bound-compensate".to_string(),
                    paste_mode: Some(false),
                },
            }],
        },
        lifecycle_state: MissionTxState::Planned,
        outcome: TxOutcome::Pending,
        receipts: Vec::new(),
    }
}

fn write_executable_tx_contract(w: &TempDir) -> (std::path::PathBuf, MissionTxContract) {
    let path = w.path().join(".ft/mission/tx-active.json");
    std::fs::create_dir_all(path.parent().expect("contract parent"))
        .expect("create contract parent");
    let contract = executable_tx_contract();
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&contract).expect("serialize executable tx contract"),
    )
    .expect("write executable tx contract");
    (path, contract)
}

#[cfg(unix)]
struct SteerWeztermCliStub {
    binary_path: std::path::PathBuf,
    list_fixture_path: std::path::PathBuf,
    effect_log_path: std::path::PathBuf,
    home: std::path::PathBuf,
    data_home: std::path::PathBuf,
    config_home: std::path::PathBuf,
    runtime_dir: std::path::PathBuf,
}

#[cfg(unix)]
impl SteerWeztermCliStub {
    fn new(w: &TempDir) -> Self {
        let binary_path = w.path().join("steer-wezterm-stub.sh");
        let effect_log_path = w.path().join("steer-wezterm-effects.log");
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
        std::fs::write(&binary_path, script).expect("write steering WezTerm CLI stub");
        let mut permissions = std::fs::metadata(&binary_path)
            .expect("stat steering WezTerm CLI stub")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&binary_path, permissions)
            .expect("make steering WezTerm CLI stub executable");
        std::fs::write(&effect_log_path, b"").expect("create steering effect log");

        let home = w.path().join("steer-home");
        let data_home = w.path().join("steer-data-home");
        let config_home = w.path().join("steer-config-home");
        let runtime_dir = w.path().join("steer-runtime");
        for path in [&home, &data_home, &config_home, &runtime_dir] {
            std::fs::create_dir_all(path).expect("create isolated steering CLI environment");
        }

        let now_ms = system_now_ms();
        let db_path = Config::default().effective_db_path(w.path());
        let conn = rusqlite::Connection::open(&db_path).expect("open steering fixture DB");
        conn.execute(
            "INSERT OR REPLACE INTO panes \
             (pane_id, domain, title, cwd, first_seen_at, last_seen_at, observed) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![0i64, "local", "zsh", "/home/user", now_ms, now_ms, true],
        )
        .expect("seed live steering target pane");

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

    fn command(&self, workspace: &Path) -> Command {
        let mut command = Command::cargo_bin("ft").expect("ft binary");
        command
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

    fn approve_contract(workspace: &Path, contract: &MissionTxContract) {
        let db_path = Config::default().effective_db_path(workspace);
        let conn = rusqlite::Connection::open(&db_path).expect("open steering approval DB");
        let now_ms = system_now_ms();
        let workspace = workspace.to_string_lossy();

        for step in &contract.plan.steps {
            let StepAction::SendText {
                pane_id,
                text,
                paste_mode: _,
            } = &step.action
            else {
                panic!("steering fixture requires SendText steps");
            };
            let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
                .with_surface(PolicySurface::Robot)
                .with_capabilities(PaneCapabilities::unknown())
                .with_text_summary(step.description.clone())
                .with_pane(*pane_id)
                .with_domain("local")
                .with_pane_title("zsh")
                .with_pane_cwd("/home/user")
                .with_command_text(text.clone());
            let scope = ApprovalScope::from_input(workspace.as_ref(), &input);
            conn.execute(
                "INSERT INTO approval_tokens \
                 (code_hash, created_at, expires_at, used_at, workspace_id, action_kind, \
                  pane_id, action_fingerprint, plan_hash, plan_version, risk_summary) \
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, NULL, NULL, ?8)",
                rusqlite::params![
                    format!("steer-bound-approval-{}", step.ordinal),
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
                    "isolated bound steering transaction fixture"
                ],
            )
            .expect("seed scoped steering approval");
        }
    }

    fn effects(&self) -> Vec<String> {
        std::fs::read_to_string(&self.effect_log_path)
            .expect("read steering effect log")
            .lines()
            .map(ToString::to_string)
            .collect()
    }
}

#[cfg(unix)]
fn system_now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_millis(),
    )
    .expect("fixture timestamp must fit in i64")
}

#[test]
fn steer_plan_clean_ready_json_receipt() {
    let w = workspace();
    let out = stdout(
        ft(w.path())
            .args([
                "steer",
                "plan",
                "--objective",
                "ship the W3 family",
                "--scenario",
                "clean-ready",
                "--format",
                "json",
            ])
            .assert()
            .success(),
    );
    let receipt: serde_json::Value = serde_json::from_str(&out).expect("json receipt");
    assert!(
        receipt["receipt_id"]
            .as_str()
            .is_some_and(|receipt_id| receipt_id.starts_with("steer:")),
        "receipt id not content-addressed: {out}"
    );
    assert!(
        receipt["envelope_verdict"] == "envelope.admit",
        "wrong verdict: {out}"
    );
    assert_eq!(
        receipt["rehearsal_score"],
        serde_json::json!(1000),
        "wrong clean-ready rehearsal score: {out}"
    );
}

#[test]
fn steer_plan_rch_blocked_plain() {
    let w = workspace();
    let out = stdout(
        ft(w.path())
            .args([
                "steer",
                "plan",
                "--objective",
                "x",
                "--scenario",
                "rch-blocked",
            ])
            .assert()
            .success(),
    );
    assert!(out.contains("rch_substrate_blocked"), "wrong status: {out}");
    assert!(
        out.contains("envelope.blocked.rch_substrate"),
        "wrong verdict: {out}"
    );
}

#[test]
fn steer_plan_approval_required_lists_approval() {
    let w = workspace();
    let out = stdout(
        ft(w.path())
            .args([
                "steer",
                "plan",
                "--objective",
                "x",
                "--scenario",
                "approval-required",
            ])
            .assert()
            .success(),
    );
    assert!(out.contains("requires_approval"), "{out}");
    assert!(out.contains("owner_handoff"), "approval not listed: {out}");
}

#[test]
fn steer_plan_deterministic_receipt_id() {
    let w = workspace();
    let run = || {
        stdout(
            ft(w.path())
                .args([
                    "steer",
                    "plan",
                    "--objective",
                    "same",
                    "--scenario",
                    "clean-ready",
                    "--format",
                    "json",
                ])
                .assert()
                .success(),
        )
    };
    let a: serde_json::Value = serde_json::from_str(&run()).expect("json a");
    let b: serde_json::Value = serde_json::from_str(&run()).expect("json b");
    assert_eq!(
        a["receipt_id"], b["receipt_id"],
        "receipt_id must be deterministic (content-addressed)"
    );
}

#[test]
fn steer_plan_persists_receipt_artifact() {
    let w = workspace();
    let out = stdout(
        ft(w.path())
            .args([
                "steer",
                "plan",
                "--objective",
                "persist me",
                "--scenario",
                "clean-ready",
                "--format",
                "json",
            ])
            .assert()
            .success(),
    );
    let receipt: serde_json::Value = serde_json::from_str(&out).expect("json receipt");
    let id = receipt["receipt_id"].as_str().expect("receipt_id");
    let safe = id.replace(':', "_");
    let path = w
        .path()
        .join(".ft/steer_receipts")
        .join(format!("{safe}.json"));
    assert!(
        path.exists(),
        "receipt artifact must be persisted at {}",
        path.display()
    );
    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read stored")).expect("json");
    assert_eq!(
        stored["receipt_id"], receipt["receipt_id"],
        "persisted receipt must match the printed one"
    );
}

#[test]
fn steer_plan_rejects_unknown_scenario() {
    let w = workspace();
    ft(w.path())
        .args(["steer", "plan", "--objective", "x", "--scenario", "bogus"])
        .assert()
        .failure();
}

/// Plan a clean-ready receipt and return its content-addressed id.
fn plan_receipt_id(w: &Path, extra: &[&str]) -> String {
    let mut args = vec![
        "steer",
        "plan",
        "--objective",
        "run me",
        "--scenario",
        "clean-ready",
        "--format",
        "json",
    ];
    args.extend_from_slice(extra);
    let out = stdout(ft(w).args(args).assert().success());
    let v: serde_json::Value = serde_json::from_str(&out).expect("json receipt");
    v["receipt_id"].as_str().expect("receipt_id").to_string()
}

#[cfg(unix)]
fn persist_bound_receipt(w: &TempDir, contract: &MissionTxContract) -> SteeringReceipt {
    // Standard-scenario `steer plan` receipts do not accept a contract input.
    // Seed the execute-half fixture through the same content-addressed store
    // with the immutable tx hash that `steer run` must revalidate.
    let now_ms = system_now_ms();
    let receipt = SteeringReceipt::new(
        "run the admitted executable transaction",
        w.path().to_string_lossy(),
        None,
        Some(contract.compute_hash()),
        "envelope.admit",
        Some(950),
        Vec::new(),
        now_ms,
        Some(600_000),
    );
    let stored_path = persist_receipt(&w.path().join(".ft"), &receipt)
        .expect("persist tx-bound steering receipt");
    assert!(stored_path.is_file(), "bound receipt must be durable");
    receipt
}

#[cfg(unix)]
#[test]
fn steer_run_cli_only_backend_fails_closed_and_uses_workspace_global_ledger() {
    let w = workspace();
    let stub = SteerWeztermCliStub::new(&w);
    let (contract_path, contract) = write_executable_tx_contract(&w);
    let authoritative_contract_path = contract_path
        .canonicalize()
        .expect("canonicalize locked steering transaction contract");
    let tx_hash = contract.compute_hash();
    let receipt = persist_bound_receipt(&w, &contract);
    assert_eq!(receipt.tx_contract_hash.as_deref(), Some(tx_hash.as_str()));
    SteerWeztermCliStub::approve_contract(w.path(), &contract);

    let out = stdout(
        stub.command(w.path())
            .args([
                "steer",
                "run",
                "--receipt",
                &receipt.receipt_id,
                "--format",
                "json",
            ])
            .assert()
            .success(),
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["valid"], serde_json::json!(true), "{out}");
    assert_eq!(v["executed"], serde_json::json!(true), "{out}");
    assert_eq!(
        v["receipt_id"].as_str(),
        Some(receipt.receipt_id.as_str()),
        "{out}"
    );
    assert_eq!(v["live_tx_hash"].as_str(), Some(tx_hash.as_str()), "{out}");
    assert_eq!(
        v["contract_file"].as_str(),
        Some(authoritative_contract_path.to_string_lossy().as_ref()),
        "{out}"
    );
    assert_eq!(v["tx"]["final_state"], "compensated", "{out}");
    assert_eq!(
        v["tx"]["commit_report"]["outcome"], "immediate_failure",
        "{out}"
    );
    assert_eq!(
        v["tx"]["commit_report"]["step_results"][0]["outcome"]["failed"]["reason_code"],
        "send_text_failed",
        "{out}"
    );
    assert!(
        v["tx"]["commit_report"]["error_code"]
            .as_str()
            .is_some_and(|error| error.contains("backend_failure")),
        "CLI-only pane mutation must fail closed with a typed backend error: {out}"
    );
    assert_eq!(
        v["tx"]["steering_receipt_id"].as_str(),
        Some(receipt.receipt_id.as_str()),
        "{out}"
    );
    assert_eq!(
        v["tx"]["steering_tx_hash"].as_str(),
        Some(tx_hash.as_str()),
        "{out}"
    );

    assert_eq!(
        stub.effects(),
        Vec::<String>::new(),
        "CLI pane-input fallback is forbidden, so the stub must observe no mutation"
    );

    let persisted: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&contract_path).expect("read settled steering contract"),
    )
    .expect("settled steering contract JSON");
    assert_eq!(persisted["lifecycle_state"], "compensated");
    assert_eq!(persisted["outcome"], "compensated");
    let attachment = persisted["receipts"]
        .as_array()
        .and_then(|receipts| receipts.first())
        .expect("steering receipt attachment");
    assert_eq!(attachment["kind"], "ft.steering_receipt.run");
    assert_eq!(
        attachment["receipt_id"].as_str(),
        Some(receipt.receipt_id.as_str())
    );
    assert_eq!(
        attachment["tx_contract_hash"].as_str(),
        Some(tx_hash.as_str())
    );
    assert_eq!(
        attachment["live_tx_contract_hash"].as_str(),
        Some(tx_hash.as_str())
    );

    let ledger_dir = w.path().join(".ft/tx_ledgers");
    let mut ledger_paths = std::fs::read_dir(&ledger_dir)
        .unwrap_or_else(|err| {
            panic!(
                "read workspace-global ledger spool {}: {err}",
                ledger_dir.display()
            )
        })
        .map(|entry| entry.expect("ledger directory entry").path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    ledger_paths.sort();
    assert_eq!(
        ledger_paths.len(),
        1,
        "one steer run must retain one workspace-global durable ledger"
    );
    assert!(
        !w.path().join(".ft/mission/tx_ledgers").exists(),
        "the ledger spool must not follow the contract into its mission subdirectory"
    );
    let ledger: TxExecutionLedger = serde_json::from_slice(
        &std::fs::read(&ledger_paths[0]).expect("read workspace-global steering ledger"),
    )
    .expect("deserialize workspace-global steering ledger");
    assert_eq!(ledger.plan_id(), "plan:steer-bound-e2e");
    assert_eq!(ledger.phase(), TxPhase::Completed);
    assert!(
        ledger.verify_chain().chain_intact,
        "ledger chain must verify"
    );
    assert_eq!(ledger.records().len(), 1);
    assert!(
        matches!(&ledger.records()[0].outcome, StepOutcome::Failure { .. }),
        "the durable ledger must prove that the forbidden CLI SendText was not applied"
    );
}

#[test]
fn steer_run_refuses_expired_receipt_typed() {
    let w = workspace();
    let _ = write_executable_tx_contract(&w);
    // A zero TTL is expired deterministically when the separate run process loads it.
    let id = plan_receipt_id(w.path(), &["--ttl-ms", "0"]);
    let out = stdout(
        ft(w.path())
            .args([
                "steer",
                "run",
                "--receipt",
                &id,
                "--dry-run",
                "--format",
                "json",
            ])
            .assert()
            .failure(),
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["valid"], serde_json::json!(false), "{out}");
    assert_eq!(
        v["error_code"],
        serde_json::json!("robot.steer_receipt_expired"),
        "{out}"
    );
}

#[test]
fn steer_run_refuses_unknown_receipt() {
    let w = workspace();
    let out = stdout(
        ft(w.path())
            .args([
                "steer",
                "run",
                "--receipt",
                "steer:deadbeefdeadbeefdeadbeefdeadbeef",
                "--format",
                "json",
            ])
            .assert()
            .failure(),
    );
    assert!(out.contains("no stored receipt"), "{out}");
}
