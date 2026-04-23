#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkTestClient, FrameworkTool, framework_create_memory_transport_pair,
};
use frankenterm_core::plan::{
    ApprovalState, Assignment, AssignmentId, CandidateAction, CandidateActionId, Mission,
    MissionActorRole, MissionId, MissionLifecycleState, MissionOwnership, Outcome, StepAction,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};

struct CwdGuard {
    original_cwd: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.original_cwd).expect("restore original cwd");
    }
}

struct TestHarness {
    workspace: tempfile::TempDir,
    client: FrameworkTestClient,
    _cwd_guard: CwdGuard,
}

#[derive(Serialize)]
struct ToolContractCapture {
    tool: String,
    input_schema: Value,
    json_success_envelope: Value,
    toon_success_envelope: Value,
    boundary_invalid_params_error: String,
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
    let workspace = tempfile::tempdir().expect("create temp workspace");
    fs::create_dir_all(workspace.path().join(".ft/mission")).expect("create mission dir");
    let original_cwd = std::env::current_dir().expect("capture current cwd");
    std::env::set_current_dir(workspace.path()).expect("enter temp workspace");
    let client = spawn_client(Some(workspace.path().join("mcp.sqlite3")));
    TestHarness {
        workspace,
        client,
        _cwd_guard: CwdGuard { original_cwd },
    }
}

fn mission_file_path(workspace: &Path) -> PathBuf {
    workspace.join(".ft/mission/active.json")
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
        .find(|tool: &FrameworkTool| tool.name == tool_name)
        .map(|tool| tool.input_schema)
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

fn assert_schema_matches_manifest(tool_name: &str, actual_schema: &Value) {
    let expected_schema = manifest_tool_schema(tool_name);
    assert_eq!(
        pretty_canonical(actual_schema),
        pretty_canonical(&expected_schema),
        "schema drift vs tests/fixtures/mcp_manifest.json for {tool_name}"
    );
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

fn parse_json_value(text: &str) -> Value {
    serde_json::from_str(text).expect("parse JSON payload")
}

fn parse_toon_value(text: &str) -> Value {
    let decoded = toon_rust::try_decode(text, None).expect("decode TOON payload");
    let json_text = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    serde_json::from_str(&json_text).expect("TOON payload should stringify back to JSON")
}

fn parse_tool_envelope(contents: &[FrameworkContent], format: &str) -> Value {
    let text = first_text_content(contents);
    if format == "json" {
        parse_json_value(text)
    } else {
        parse_toon_value(text)
    }
}

fn assert_common_envelope_fields(envelope: &Value, ok: bool, label: &str) {
    assert_eq!(
        envelope["ok"],
        Value::Bool(ok),
        "{label} unexpected ok field: {envelope}"
    );
    assert!(
        envelope["elapsed_ms"].is_number(),
        "{label} missing elapsed_ms: {envelope}"
    );
    assert!(
        envelope["now"].is_number(),
        "{label} missing now: {envelope}"
    );
    assert_eq!(
        envelope["mcp_version"], "v1",
        "{label} unexpected mcp_version: {envelope}"
    );
    assert!(
        envelope["version"].is_string(),
        "{label} missing version: {envelope}"
    );
}

fn assert_success_envelope_shape(envelope: &Value, label: &str) {
    assert_common_envelope_fields(envelope, true, label);
    assert!(
        envelope["data"].is_object(),
        "{label} missing data: {envelope}"
    );
    assert!(
        envelope.get("error").is_none(),
        "{label} unexpected error: {envelope}"
    );
    assert!(
        envelope.get("error_code").is_none(),
        "{label} unexpected error_code: {envelope}"
    );
    assert!(
        envelope.get("hint").is_none(),
        "{label} unexpected hint: {envelope}"
    );
}

fn assert_boundary_error_contains(error: &str, field_hint: &str) {
    assert!(
        error.contains("[-32602]"),
        "expected framework invalid-params code in error: {error}"
    );
    assert!(
        error.contains(field_hint),
        "expected field hint '{field_hint}' in error: {error}"
    );
}

fn canonicalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "now" | "elapsed_ms" => *child = Value::from(0_u64),
                    "mission_file" => *child = Value::String("<mission_file>".to_string()),
                    _ if key.ends_with("_ms") => *child = Value::from(0_i64),
                    _ => canonicalize(child),
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
                canonicalize(item);
            }
        }
        _ => {}
    }
}

fn canonical_value(value: &Value) -> Value {
    let mut cloned = value.clone();
    canonicalize(&mut cloned);
    cloned
}

fn pretty_canonical(value: &Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&canonical_value(value)).expect("serialize canonical JSON")
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
            "missing MCP mission TOON conformance golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test wa_mission_toon_mcp_conformance \
             --features mcp,asupersync-runtime",
            path.display()
        )
    })
}

fn assert_matches_golden(name: &str, captures: &[ToolContractCapture]) {
    let actual_value = serde_json::to_value(captures).expect("serialize capture");
    let actual_text = pretty_canonical(&actual_value);
    let path = golden_path(name);
    let expected = read_or_update_golden(&path, &actual_text);

    if expected.trim_end_matches('\n') != actual_text.trim_end_matches('\n') {
        let actual_path = path.with_extension("actual.json");
        let _ = fs::write(&actual_path, &actual_text);
        panic!(
            "MCP mission TOON conformance golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test wa_mission_toon_mcp_conformance \
             --features mcp,asupersync-runtime",
            path.display(),
            actual_path.display()
        );
    }
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

fn seed_running_mission(harness: &mut TestHarness) {
    write_json(
        &mission_file_path(harness.workspace.path()),
        &make_running_mission(),
    );
}

fn seed_paused_mission(harness: &mut TestHarness) {
    write_json(
        &mission_file_path(harness.workspace.path()),
        &make_paused_mission(),
    );
}

fn capture_tool_contract(
    tool_name: &str,
    success_setup: impl Fn(&mut TestHarness),
    success_args: impl Fn(&TestHarness, &str) -> Value,
    boundary_setup: impl Fn(&mut TestHarness),
    boundary_args: impl Fn(&TestHarness) -> Value,
    boundary_hint: &str,
) -> ToolContractCapture {
    let (input_schema, json_success_envelope) = {
        let mut json_harness = new_harness();
        success_setup(&mut json_harness);
        let input_schema = tool_input_schema(&mut json_harness.client, tool_name);
        assert_schema_matches_manifest(tool_name, &input_schema);
        let json_success_envelope = parse_tool_envelope(
            &json_harness
                .client
                .call_tool(tool_name, success_args(&json_harness, "json"))
                .unwrap_or_else(|err| panic!("call {tool_name} json success case: {err}")),
            "json",
        );
        (input_schema, json_success_envelope)
    };

    let toon_success_envelope = {
        let mut toon_harness = new_harness();
        success_setup(&mut toon_harness);
        parse_tool_envelope(
            &toon_harness
                .client
                .call_tool(tool_name, success_args(&toon_harness, "toon"))
                .unwrap_or_else(|err| panic!("call {tool_name} toon success case: {err}")),
            "toon",
        )
    };

    assert_success_envelope_shape(&json_success_envelope, &format!("{tool_name} json"));
    assert_success_envelope_shape(&toon_success_envelope, &format!("{tool_name} toon"));
    assert_eq!(
        canonical_value(&json_success_envelope),
        canonical_value(&toon_success_envelope),
        "{tool_name} TOON envelope drifted from JSON success semantics"
    );

    let boundary_invalid_params_error = {
        let mut boundary_harness = new_harness();
        boundary_setup(&mut boundary_harness);
        boundary_harness
            .client
            .call_tool(tool_name, boundary_args(&boundary_harness))
            .err()
            .map(|err| err.to_string())
            .unwrap_or_else(|| panic!("expected {tool_name} boundary-invalid case to fail"))
    };
    assert_boundary_error_contains(&boundary_invalid_params_error, boundary_hint);

    ToolContractCapture {
        tool: tool_name.to_string(),
        input_schema,
        json_success_envelope,
        toon_success_envelope,
        boundary_invalid_params_error,
    }
}

#[test]
fn mcp_conformance_wa_mission_toon_and_boundary_contract_matches_golden() {
    let captures = vec![
        capture_tool_contract(
            "wa.mission_state",
            seed_running_mission,
            |harness, format| {
                json!({
                    "format": format,
                    "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                    "limit": 10
                })
            },
            |_| {},
            |_| {
                json!({
                    "format": "json",
                    "limit": 0
                })
            },
            "root.limit",
        ),
        capture_tool_contract(
            "wa.mission_explain",
            seed_running_mission,
            |harness, format| {
                json!({
                    "format": format,
                    "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                    "assignment_id": "assignment:alpha"
                })
            },
            |_| {},
            |_| {
                json!({
                    "format": "json",
                    "assignment_id": 7
                })
            },
            "root.assignment_id",
        ),
        capture_tool_contract(
            "wa.mission_pause",
            seed_running_mission,
            |harness, format| {
                json!({
                    "format": format,
                    "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                    "reason": "maintenance_window",
                    "requested_by": "operator-a"
                })
            },
            seed_running_mission,
            |harness| {
                json!({
                    "format": "json",
                    "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                    "reason": 7,
                    "requested_by": "operator-a"
                })
            },
            "root.reason",
        ),
        capture_tool_contract(
            "wa.mission_resume",
            seed_paused_mission,
            |harness, format| {
                json!({
                    "format": format,
                    "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                    "requested_by": "operator-a"
                })
            },
            seed_paused_mission,
            |harness| {
                json!({
                    "format": "json",
                    "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                    "requested_by": 7
                })
            },
            "root.requested_by",
        ),
        capture_tool_contract(
            "wa.mission_abort",
            seed_running_mission,
            |harness, format| {
                json!({
                    "format": format,
                    "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                    "reason": "operator_abort",
                    "requested_by": "operator-a",
                    "error_code": "mission.failure.manual_abort"
                })
            },
            seed_running_mission,
            |harness| {
                json!({
                    "format": "json",
                    "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                    "reason": 7,
                    "requested_by": "operator-a"
                })
            },
            "root.reason",
        ),
    ];

    assert_matches_golden("wa_mission_toon_conformance", &captures);
}
