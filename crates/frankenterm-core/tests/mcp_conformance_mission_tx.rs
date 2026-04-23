#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkTestClient, FrameworkTool, framework_create_memory_transport_pair,
};
use frankenterm_core::plan::{
    ApprovalState, Assignment, AssignmentId, CandidateAction, CandidateActionId, Mission,
    MissionActorRole, MissionId, MissionLifecycleState, MissionOwnership, MissionTxContract,
    MissionTxState, Outcome, StepAction, TxCompensation, TxId, TxIntent, TxOutcome, TxPlan,
    TxPlanId, TxPrecondition, TxStep, TxStepId,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

struct TestHarness {
    _cwd_lock: MutexGuard<'static, ()>,
    _workspace_guard: WorkspaceRootGuard,
    workspace: tempfile::TempDir,
    client: FrameworkTestClient,
}

#[derive(Serialize)]
struct ToolGoldenCapture {
    #[serde(skip_serializing)]
    workspace_root: PathBuf,
    tool: String,
    input_schema: Value,
    success_envelope: Value,
    invalid_args_envelope: Value,
}

struct WorkspaceRootGuard {
    previous_cwd: PathBuf,
}

impl WorkspaceRootGuard {
    fn new(workspace_root: &Path) -> Self {
        let previous_cwd = std::env::current_dir().expect("capture current dir");
        std::env::set_current_dir(workspace_root).expect("enter conformance workspace root");
        Self { previous_cwd }
    }
}

impl Drop for WorkspaceRootGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous_cwd).expect("restore current dir");
    }
}

fn workspace_root_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_client(db_path: Option<PathBuf>) -> FrameworkTestClient {
    let mut config = Config::default();
    config.safety.require_prompt_active = false;
    let server = build_server_with_db(&config, db_path).expect("build MCP server");
    let (client_transport, server_transport) = framework_create_memory_transport_pair();
    std::thread::spawn(move || {
        server.run_transport_returning(server_transport);
    });

    let mut client = FrameworkTestClient::new(client_transport);
    client
        .initialize()
        .expect("initialize in-memory MCP client");
    client
}

fn new_harness() -> TestHarness {
    let cwd_lock = workspace_root_lock();
    let workspace = tempfile::tempdir().expect("create temp workspace");
    fs::create_dir_all(workspace.path().join(".ft/mission")).expect("create mission dir");
    let workspace_guard = WorkspaceRootGuard::new(workspace.path());
    let client = spawn_client(Some(workspace.path().join("mcp.sqlite3")));
    TestHarness {
        _cwd_lock: cwd_lock,
        _workspace_guard: workspace_guard,
        workspace,
        client,
    }
}

fn mission_file_path(workspace: &Path) -> PathBuf {
    workspace.join(".ft/mission/active.json")
}

fn tx_file_path(workspace: &Path) -> PathBuf {
    workspace.join(".ft/mission/tx-active.json")
}

fn write_json<T: Serialize>(path: &Path, value: &T) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    let text = serde_json::to_string_pretty(value).expect("serialize fixture");
    fs::write(path, text).expect("write fixture");
}

fn tool_input_schema(client: &mut FrameworkTestClient, tool_name: &str) -> Value {
    client
        .list_tools()
        .expect("list tools")
        .into_iter()
        .find(|tool| tool.name == tool_name)
        .map(|tool: FrameworkTool| tool.input_schema)
        .unwrap_or_else(|| panic!("missing tool {tool_name}"))
}

fn manifest_tool_schema(tool_name: &str) -> Value {
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mcp_manifest.json");
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path).expect("read mcp manifest fixture"),
    )
    .expect("parse mcp manifest fixture");
    manifest["tools"]
        .as_array()
        .expect("manifest tools array")
        .iter()
        .find(|tool| tool["name"] == tool_name)
        .and_then(|tool| tool.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| panic!("missing manifest schema for {tool_name}"))
}

fn first_text_content(contents: &[FrameworkContent]) -> &str {
    contents
        .first()
        .and_then(|content| match content {
            FrameworkContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("expected first MCP content to be text")
}

fn parse_tool_envelope(contents: &[FrameworkContent]) -> Value {
    serde_json::from_str(first_text_content(contents)).expect("parse JSON envelope")
}

fn canonicalize(value: &mut Value, workspace_root: &Path) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "now" | "elapsed_ms" => *child = Value::from(0_u64),
                    "mission_file" => *child = Value::String("<mission_file>".to_string()),
                    "contract_file" => *child = Value::String("<contract_file>".to_string()),
                    "checkpoint_id" if !child.is_null() => {
                        *child = Value::String("<checkpoint_id>".to_string())
                    }
                    _ if key.ends_with("_ms") => *child = Value::from(0_i64),
                    _ => canonicalize(child, workspace_root),
                }
            }

            let mut sorted = std::collections::BTreeMap::new();
            for (key, child) in std::mem::take(map) {
                sorted.insert(key, child);
            }
            let mut rebuilt = Map::new();
            for (key, child) in sorted {
                rebuilt.insert(key, child);
            }
            *map = rebuilt;
        }
        Value::Array(items) => {
            for item in items {
                canonicalize(item, workspace_root);
            }
        }
        Value::String(text) => {
            let workspace_root = workspace_root.display().to_string();
            let private_workspace_root = format!("/private{workspace_root}");
            if text.contains(&private_workspace_root) {
                *text = text.replace(&private_workspace_root, "<workspace_root>");
            }
            if text.contains(&workspace_root) {
                *text = text.replace(&workspace_root, "<workspace_root>");
            }
        }
        _ => {}
    }
}

fn pretty_canonical(value: &Value, workspace_root: &Path) -> String {
    let mut cloned = value.clone();
    canonicalize(&mut cloned, workspace_root);
    format!(
        "{}\n",
        serde_json::to_string_pretty(&cloned).expect("serialize canonical JSON")
    )
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden_robot_envelope")
        .join(format!("{name}.json"))
}

fn read_or_update_golden(path: &Path, actual: &str) -> String {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create golden dir");
        }
        fs::write(path, actual).expect("write golden");
        return actual.to_string();
    }

    fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing MCP conformance golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test mcp_conformance_mission_tx \
             --features mcp,asupersync-runtime",
            path.display()
        )
    })
}

fn assert_matches_golden(name: &str, capture: &ToolGoldenCapture) {
    let actual_value = serde_json::to_value(capture).expect("serialize capture");
    let actual_text = pretty_canonical(&actual_value, &capture.workspace_root);
    if std::env::var("DEBUG_GOLDEN_CAPTURE").is_ok() {
        eprintln!("DEBUG_GOLDEN_CAPTURE {name}\n{actual_text}");
    }
    let path = golden_path(name);
    let expected = read_or_update_golden(&path, &actual_text);

    if expected.trim_end_matches('\n') != actual_text.trim_end_matches('\n') {
        let actual_path = path.with_extension("actual.json");
        let _ = fs::write(&actual_path, &actual_text);
        panic!(
            "MCP mission/tx golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test mcp_conformance_mission_tx \
             --features mcp,asupersync-runtime",
            path.display(),
            actual_path.display()
        );
    }
}

fn assert_schema_matches_manifest(tool_name: &str, actual_schema: &Value) {
    let expected_schema = manifest_tool_schema(tool_name);
    assert_eq!(
        pretty_canonical(actual_schema, Path::new("<workspace_root>")),
        pretty_canonical(&expected_schema, Path::new("<workspace_root>")),
        "schema drift vs tests/fixtures/mcp_manifest.json for {tool_name}"
    );
}

fn make_candidate(id: &str, pane_id: u64, text: &str, created_at_ms: i64) -> CandidateAction {
    CandidateAction {
        candidate_id: CandidateActionId(id.to_string()),
        requested_by: MissionActorRole::Planner,
        action: StepAction::SendText {
            pane_id,
            text: text.to_string(),
            paste_mode: Some(false),
        },
        rationale: format!("dispatch {text}"),
        score: Some(0.95),
        created_at_ms,
    }
}

fn make_assignment(
    assignment_id: &str,
    candidate_id: &str,
    assignee: &str,
    approval_state: ApprovalState,
    outcome: Option<Outcome>,
    created_at_ms: i64,
) -> Assignment {
    Assignment {
        assignment_id: AssignmentId(assignment_id.to_string()),
        candidate_id: CandidateActionId(candidate_id.to_string()),
        assigned_by: MissionActorRole::Dispatcher,
        assignee: assignee.to_string(),
        reservation_intent: None,
        approval_state,
        outcome,
        escalation: None,
        created_at_ms,
        updated_at_ms: None,
    }
}

fn make_running_mission() -> Mission {
    let mut mission = Mission::new(
        MissionId("mission:qiba0".to_string()),
        "Mission MCP Conformance",
        "ws-qiba0",
        MissionOwnership {
            planner: "planner-a".to_string(),
            dispatcher: "dispatcher-a".to_string(),
            operator: "operator-a".to_string(),
        },
        1_700_000_000_000,
    );
    mission.lifecycle_state = MissionLifecycleState::Running;
    mission.candidates = vec![
        make_candidate("candidate:alpha", 1, "/approve alpha", 1_700_000_000_010),
        make_candidate("candidate:beta", 2, "/run beta", 1_700_000_000_020),
    ];
    mission.assignments = vec![
        make_assignment(
            "assignment:alpha",
            "candidate:alpha",
            "agent-alpha",
            ApprovalState::Pending {
                requested_by: "dispatcher-a".to_string(),
                requested_at_ms: 1_700_000_000_100,
            },
            None,
            1_700_000_000_110,
        ),
        make_assignment(
            "assignment:beta",
            "candidate:beta",
            "agent-beta",
            ApprovalState::Approved {
                approved_by: "operator-a".to_string(),
                approved_at_ms: 1_700_000_000_120,
                approval_code_hash: "sha256:approved".to_string(),
            },
            Some(Outcome::Success {
                reason_code: "step_completed".to_string(),
                completed_at_ms: 1_700_000_000_130,
            }),
            1_700_000_000_115,
        ),
    ];
    mission
}

fn make_paused_mission() -> Mission {
    let mut mission = make_running_mission();
    mission
        .pause_mission("operator-a", "maintenance_window", 1_700_000_000_200, None)
        .expect("pause seed mission");
    mission
}

fn make_cancelled_mission() -> Mission {
    let mut mission = make_running_mission();
    mission
        .abort_mission(
            "operator-a",
            "operator_abort",
            Some("mission.failure.manual_abort".to_string()),
            1_700_000_000_300,
            None,
        )
        .expect("abort seed mission");
    mission
}

fn make_tx_contract() -> MissionTxContract {
    let tx_id = TxId("tx:qiba0".to_string());
    MissionTxContract {
        tx_version: frankenterm_core::plan::MISSION_TX_SCHEMA_VERSION,
        intent: TxIntent {
            tx_id: tx_id.clone(),
            requested_by: MissionActorRole::Dispatcher,
            summary: "qiba0 tx contract".to_string(),
            correlation_id: "corr-qiba0".to_string(),
            created_at_ms: 1_700_000_001_000,
        },
        plan: TxPlan {
            plan_id: TxPlanId("tx-plan:qiba0".to_string()),
            tx_id,
            steps: vec![
                TxStep {
                    step_id: TxStepId("tx-step:1".to_string()),
                    ordinal: 1,
                    action: StepAction::SendText {
                        pane_id: 11,
                        text: "/do-step-1".to_string(),
                        paste_mode: Some(false),
                    },
                    description: "prepare alpha".to_string(),
                },
                TxStep {
                    step_id: TxStepId("tx-step:2".to_string()),
                    ordinal: 2,
                    action: StepAction::SendText {
                        pane_id: 12,
                        text: "/do-step-2".to_string(),
                        paste_mode: Some(true),
                    },
                    description: "commit beta".to_string(),
                },
            ],
            preconditions: vec![TxPrecondition::PromptActive { pane_id: 11 }],
            compensations: vec![
                TxCompensation {
                    for_step_id: TxStepId("tx-step:1".to_string()),
                    action: StepAction::SendText {
                        pane_id: 11,
                        text: "/undo-step-1".to_string(),
                        paste_mode: Some(false),
                    },
                },
                TxCompensation {
                    for_step_id: TxStepId("tx-step:2".to_string()),
                    action: StepAction::SendText {
                        pane_id: 12,
                        text: "/undo-step-2".to_string(),
                        paste_mode: Some(true),
                    },
                },
            ],
        },
        lifecycle_state: MissionTxState::Planned,
        outcome: TxOutcome::Pending,
        receipts: Vec::new(),
    }
}

fn capture_tool_contract(
    tool_name: &str,
    success_setup: impl FnOnce(&mut TestHarness),
    success_args: impl FnOnce(&TestHarness) -> Value,
    invalid_setup: impl FnOnce(&mut TestHarness),
    invalid_args: impl FnOnce(&TestHarness) -> Value,
) -> ToolGoldenCapture {
    let mut harness = new_harness();
    success_setup(&mut harness);
    let input_schema = tool_input_schema(&mut harness.client, tool_name);
    assert_schema_matches_manifest(tool_name, &input_schema);
    let success_args = success_args(&harness);
    let success_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(tool_name, success_args)
            .unwrap_or_else(|err| panic!("call {tool_name} success case: {err}")),
    );

    invalid_setup(&mut harness);
    let invalid_args = invalid_args(&harness);
    let invalid_args_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(tool_name, invalid_args)
            .unwrap_or_else(|err| panic!("call {tool_name} invalid case: {err}")),
    );

    ToolGoldenCapture {
        workspace_root: harness.workspace.path().to_path_buf(),
        tool: tool_name.to_string(),
        input_schema,
        success_envelope,
        invalid_args_envelope,
    }
}

fn seed_running_mission(harness: &TestHarness) {
    write_json(
        &mission_file_path(harness.workspace.path()),
        &make_running_mission(),
    );
}

fn seed_paused_mission(harness: &TestHarness) {
    write_json(
        &mission_file_path(harness.workspace.path()),
        &make_paused_mission(),
    );
}

fn seed_cancelled_mission(harness: &TestHarness) {
    write_json(
        &mission_file_path(harness.workspace.path()),
        &make_cancelled_mission(),
    );
}

fn seed_planned_tx(harness: &TestHarness) {
    write_json(&tx_file_path(harness.workspace.path()), &make_tx_contract());
}

fn seed_committed_tx(harness: &mut TestHarness) {
    seed_planned_tx(harness);
    let _ = harness
        .client
        .call_tool(
            "wa.tx_run",
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
            }),
        )
        .expect("seed tx_run success");
}

fn read_tx_contract(workspace: &Path) -> MissionTxContract {
    serde_json::from_str(
        &fs::read_to_string(tx_file_path(workspace)).expect("read persisted tx contract"),
    )
    .expect("parse persisted tx contract")
}

fn log_tx_roundtrip_phase(phase: &str, envelope: &Value, persisted: &MissionTxContract) {
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "test": "mcp_wa_tx_roundtrip_plan_run_rollback_persists_expected_state",
            "phase": phase,
            "envelope": envelope,
            "persisted_state": persisted.lifecycle_state,
            "persisted_outcome": persisted.outcome,
            "receipt_count": persisted.receipts.len(),
        }))
        .expect("serialize tx roundtrip log")
    );
}

#[test]
fn mcp_conformance_wa_mission_state_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_state",
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "limit": 10
            })
        },
        |_| {},
        |_harness| {
            json!({
                "format": "json",
                "mission_file": "missing-mission.json",
                "limit": 10
            })
        },
    );
    assert_matches_golden("wa_mission_state", &capture);
}

#[test]
fn mcp_conformance_wa_mission_explain_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_explain",
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "assignment_id": "assignment:alpha"
            })
        },
        |_| {},
        |_harness| {
            json!({
                "format": "json",
                "mission_file": "missing-mission.json",
                "assignment_id": "assignment:missing"
            })
        },
    );
    assert_matches_golden("wa_mission_explain", &capture);
}

#[test]
fn mcp_conformance_wa_mission_pause_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_pause",
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "reason": "maintenance_window",
                "requested_by": "operator-a"
            })
        },
        |harness| seed_paused_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "reason": "maintenance_window",
                "requested_by": "operator-a"
            })
        },
    );
    assert_matches_golden("wa_mission_pause", &capture);
}

#[test]
fn mcp_conformance_wa_mission_resume_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_resume",
        |harness| seed_paused_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "requested_by": "operator-a"
            })
        },
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "requested_by": "operator-a"
            })
        },
    );
    assert_matches_golden("wa_mission_resume", &capture);
}

#[test]
fn mcp_conformance_wa_mission_abort_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_abort",
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "reason": "operator_abort",
                "requested_by": "operator-a",
                "error_code": "mission.failure.manual_abort"
            })
        },
        |harness| seed_cancelled_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "reason": "operator_abort",
                "requested_by": "operator-a"
            })
        },
    );
    assert_matches_golden("wa_mission_abort", &capture);
}

#[test]
fn mcp_conformance_wa_tx_plan_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.tx_plan",
        |harness| seed_planned_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
            })
        },
        |_| {},
        |_harness| {
            json!({
                "format": "json",
                "contract_file": "missing-tx.json"
            })
        },
    );
    assert_matches_golden("wa_tx_plan", &capture);
}

#[test]
fn mcp_conformance_wa_tx_show_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.tx_show",
        |harness| seed_planned_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                "include_contract": true
            })
        },
        |_| {},
        |_harness| {
            json!({
                "format": "json",
                "contract_file": "missing-tx.json",
                "include_contract": true
            })
        },
    );
    assert_matches_golden("wa_tx_show", &capture);
}

#[test]
fn mcp_conformance_wa_tx_run_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.tx_run",
        |harness| seed_planned_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                "fail_step": "tx-step:2"
            })
        },
        |harness| seed_planned_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                "fail_step": "tx-step:missing"
            })
        },
    );
    assert_matches_golden("wa_tx_run", &capture);
}

#[test]
fn mcp_conformance_wa_tx_rollback_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.tx_rollback",
        |harness| seed_committed_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
            })
        },
        |harness| seed_committed_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                "fail_compensation_for_step": "tx-step:missing"
            })
        },
    );
    assert_matches_golden("wa_tx_rollback", &capture);
}

#[test]
fn mcp_wa_tx_roundtrip_plan_run_rollback_persists_expected_state() {
    let mut harness = new_harness();
    seed_planned_tx(&harness);

    let contract_file = tx_file_path(harness.workspace.path());
    let contract_file_text = contract_file.display().to_string();

    let plan_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.tx_plan",
                json!({
                    "format": "json",
                    "contract_file": contract_file_text,
                }),
            )
            .expect("wa.tx_plan roundtrip success"),
    );
    let planned_contract = read_tx_contract(harness.workspace.path());
    log_tx_roundtrip_phase("plan", &plan_envelope, &planned_contract);
    assert_eq!(plan_envelope["ok"], Value::Bool(true));
    assert_eq!(plan_envelope["data"]["lifecycle_state"], "planned");
    assert_eq!(plan_envelope["data"]["step_count"], 2);
    assert_eq!(plan_envelope["data"]["compensation_count"], 2);
    assert_eq!(planned_contract.lifecycle_state, MissionTxState::Planned);
    assert_eq!(planned_contract.outcome, TxOutcome::Pending);
    assert_eq!(planned_contract.receipts.len(), 0);

    let run_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.tx_run",
                json!({
                    "format": "json",
                    "contract_file": contract_file.display().to_string(),
                }),
            )
            .expect("wa.tx_run roundtrip success"),
    );
    let committed_contract = read_tx_contract(harness.workspace.path());
    log_tx_roundtrip_phase("run", &run_envelope, &committed_contract);
    assert_eq!(run_envelope["ok"], Value::Bool(true));
    assert_eq!(run_envelope["data"]["final_state"], "committed");
    assert_eq!(
        run_envelope["data"]["prepare_report"]["outcome"],
        "all_ready"
    );
    assert_eq!(
        run_envelope["data"]["commit_report"]["outcome"],
        "committed"
    );
    assert!(run_envelope["data"]["compensation_report"].is_null());
    assert_eq!(
        committed_contract.lifecycle_state,
        MissionTxState::Committed
    );
    assert_eq!(committed_contract.outcome, TxOutcome::Committed);
    assert_eq!(committed_contract.receipts.len(), 2);

    let rollback_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.tx_rollback",
                json!({
                    "format": "json",
                    "contract_file": contract_file.display().to_string(),
                }),
            )
            .expect("wa.tx_rollback roundtrip success"),
    );
    let rolled_back_contract = read_tx_contract(harness.workspace.path());
    log_tx_roundtrip_phase("rollback", &rollback_envelope, &rolled_back_contract);
    assert_eq!(rollback_envelope["ok"], Value::Bool(true));
    assert_eq!(rollback_envelope["data"]["final_state"], "rolled_back");
    assert_eq!(
        rollback_envelope["data"]["compensation_report"]["outcome"],
        "fully_rolled_back"
    );
    assert_eq!(
        rollback_envelope["data"]["compensation_report"]["compensated_count"],
        2
    );
    assert_eq!(
        rolled_back_contract.lifecycle_state,
        MissionTxState::RolledBack
    );
    assert_eq!(rolled_back_contract.outcome, TxOutcome::RolledBack);
    assert_eq!(rolled_back_contract.receipts.len(), 4);
}
