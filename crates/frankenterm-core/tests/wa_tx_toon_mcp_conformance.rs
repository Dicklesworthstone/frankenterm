#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkTestClient, FrameworkTool, framework_create_memory_transport_pair,
};
use frankenterm_core::plan::{
    MissionActorRole, MissionTxContract, MissionTxState, StepAction, TxCompensation, TxId,
    TxIntent, TxOutcome, TxPlan, TxPlanId, TxPrecondition, TxStep, TxStepId,
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
    _cwd_guard: CwdGuard,
    client: FrameworkTestClient,
    workspace: tempfile::TempDir,
}

#[derive(Serialize)]
struct ToolContractCapture {
    tool: String,
    input_schema: Value,
    json_envelope: Value,
    toon_envelope: Value,
    boundary_invalid_params_error: String,
}

fn spawn_client(db_path: Option<PathBuf>) -> FrameworkTestClient {
    let mut config = Config::default();
    config.safety.require_prompt_active = false;
    let (client_transport, server_transport) = framework_create_memory_transport_pair();
    std::thread::spawn(move || {
        let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("build MCP test runtime");
        runtime.block_on(async {
            let cx = frankenterm_core::cx::Cx::current().expect("runtime-owned MCP context");
            let server = build_server_with_db(&cx, &config, db_path)
                .await
                .expect("build MCP server");
            server
                .run_transport_returning_with_cx(&cx, server_transport)
                .expect("run MCP transport");
        });
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
        actual_schema, &expected_schema,
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
    assert_eq!(
        envelope["version"],
        env!("CARGO_PKG_VERSION"),
        "{label} unexpected package version: {envelope}"
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
                    "input_schema" => {}
                    "now" | "elapsed_ms" if child.is_number() => *child = Value::from(0_u64),
                    "contract_file" if child.is_string() => {
                        *child = Value::String("<contract_file>".to_string());
                    }
                    "workspace_id" if child.is_string() => {
                        *child = Value::String("<workspace_id>".to_string());
                    }
                    _ if key.ends_with("_ms") && child.is_number() => *child = Value::from(0_i64),
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
        Value::Number(number) if number.is_f64() => {
            let float_value = number.as_f64().unwrap();
            // TOON counters decode as f64. Do not round integer authorities
            // through f64 or coerce floats outside its exact-integer range.
            if float_value.fract() == 0.0 && float_value.abs() <= 9_007_199_254_740_991.0 {
                *number = serde_json::Number::from(float_value as i64);
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

#[test]
fn canonicalization_preserves_schemas_types_and_large_integer_identity() {
    let schema = json!({"properties": {
        "contract_file": {"type": "string"},
        "elapsed_ms": {"type": "number", "default": 23},
        "workspace_id": {"type": "string"}
    }});
    let value = json!({
        "input_schema": schema,
        "contract_file": {"type": "string"},
        "elapsed_ms": "invalid-number-type",
        "workspace_id": false,
        "small_counter": 7.0,
        "large_id": 9_007_199_254_740_993_u64,
        "max_id": u64::MAX,
        "large_float": 9_007_199_254_740_992.0
    });
    let canonical = canonical_value(&value);
    assert_eq!(canonical["input_schema"], value["input_schema"]);
    for key in [
        "contract_file",
        "elapsed_ms",
        "workspace_id",
        "large_id",
        "max_id",
        "large_float",
    ] {
        assert_eq!(canonical[key], value[key], "must preserve {key}");
    }
    assert_eq!(canonical["small_counter"], json!(7));
    assert_ne!(canonical["large_id"], json!(9_007_199_254_740_992_u64));
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
            "missing MCP tx TOON conformance golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test wa_tx_toon_mcp_conformance \
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
    let mut expected_value: Value = serde_json::from_str(&expected).expect("parse tx golden");
    // Only these expected envelope leaves vary with a release. Live success
    // and rejection envelopes already assert the exact current package version.
    // Never normalize versions inside schemas, contracts, or response data.
    for capture in expected_value
        .as_array_mut()
        .expect("golden captures array")
    {
        for field in ["json_envelope", "toon_envelope"] {
            let version = capture
                .get_mut(field)
                .and_then(|envelope| envelope.get_mut("version"))
                .expect("golden envelope version leaf");
            assert!(version.is_string(), "golden version must remain a string");
            *version = Value::from(env!("CARGO_PKG_VERSION"));
        }
    }
    let expected = pretty_canonical(&expected_value);

    if expected.trim_end_matches('\n') != actual_text.trim_end_matches('\n') {
        let actual_path = path.with_extension("actual.json");
        let _ = fs::write(&actual_path, &actual_text);
        panic!(
            "MCP tx TOON conformance golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test wa_tx_toon_mcp_conformance \
             --features mcp,asupersync-runtime",
            path.display(),
            actual_path.display()
        );
    }
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

fn seed_planned_tx(harness: &mut TestHarness) {
    write_json(&tx_file_path(harness.workspace.path()), &make_tx_contract());
}

fn assert_persisted_prepare_denial(harness: &TestHarness) {
    let contract: MissionTxContract = serde_json::from_slice(
        &fs::read(tx_file_path(harness.workspace.path())).expect("read persisted transaction"),
    )
    .expect("parse persisted transaction");
    assert_eq!(contract.lifecycle_state, MissionTxState::Failed);
    assert_eq!(contract.outcome, TxOutcome::Failed);
    assert!(
        contract
            .receipts
            .iter()
            .all(|receipt| { receipt["phase"] != "commit" && receipt["phase"] != "compensate" }),
        "prepare denial must not produce commit or compensation receipts"
    );
}

fn assert_workspace_ids(value: &Value, expected: &str) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "workspace_id" {
                    assert_eq!(child.as_str(), Some(expected), "wrong workspace authority");
                } else {
                    assert_workspace_ids(child, expected);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_workspace_ids(item, expected);
            }
        }
        _ => {}
    }
}

fn assert_response_outcome(harness: &TestHarness, tool_name: &str, envelope: &Value) {
    let workspace = Config::default()
        .workspace_layout(None)
        .expect("workspace layout");
    assert_eq!(
        workspace.root.canonicalize().unwrap(),
        harness.workspace.path().canonicalize().unwrap(),
        "response must be scoped to this harness workspace"
    );
    assert_workspace_ids(envelope, &workspace.root.to_string_lossy());
    if tool_name == "wa.tx_rollback" {
        assert_common_envelope_fields(envelope, false, tool_name);
        assert_eq!(envelope["error_code"], "FT-MCP-0001");
        assert_eq!(
            envelope["error"],
            "rollback requires commit receipts, got none for tx state failed"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .unwrap()
                .contains("do not fabricate receipts")
        );
        assert!(envelope.get("data").is_none());
        assert_persisted_prepare_denial(harness);
    } else {
        assert_success_envelope_shape(envelope, tool_name);
        if tool_name == "wa.tx_run" {
            // Transport success is a valid execution report, not a committed
            // transaction. Missing panes must stop this fixture at prepare.
            assert_eq!(envelope["data"]["final_state"], "failed");
            assert_eq!(envelope["data"]["prepare_report"]["outcome"], "denied");
            let gates = envelope["data"]["prepare_report"]["gate_inputs"]
                .as_array()
                .unwrap();
            assert_eq!(gates.len(), 2);
            for gate in gates {
                assert_eq!(gate["target_liveness"], false);
                assert_eq!(gate["preconditions_satisfied"], false);
            }
            assert!(envelope["data"].get("commit_report").is_none());
            assert_persisted_prepare_denial(harness);
        }
    }
}

fn seed_prepare_denied_tx(harness: &mut TestHarness) {
    seed_planned_tx(harness);
    let contents = harness
        .client
        .call_tool(
            "wa.tx_run",
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
            }),
        )
        .expect("execute transaction prepare");
    let envelope = parse_tool_envelope(&contents, "json");
    assert_response_outcome(harness, "wa.tx_run", &envelope);
}

fn capture_tool_contract(
    tool_name: &str,
    setup: impl Fn(&mut TestHarness),
    args: impl Fn(&TestHarness, &str) -> Value,
    boundary_setup: impl Fn(&mut TestHarness),
    boundary_args: impl Fn(&TestHarness) -> Value,
    boundary_hint: &str,
) -> ToolContractCapture {
    let (input_schema, json_envelope) = {
        let mut json_harness = new_harness();
        setup(&mut json_harness);
        let input_schema = tool_input_schema(&mut json_harness.client, tool_name);
        assert_schema_matches_manifest(tool_name, &input_schema);
        let before = fs::read(tx_file_path(json_harness.workspace.path())).unwrap();
        let json_envelope = parse_tool_envelope(
            &json_harness
                .client
                .call_tool(tool_name, args(&json_harness, "json"))
                .unwrap_or_else(|err| panic!("call {tool_name} json case: {err}")),
            "json",
        );
        assert_response_outcome(&json_harness, tool_name, &json_envelope);
        if tool_name == "wa.tx_rollback" {
            assert_eq!(
                fs::read(tx_file_path(json_harness.workspace.path())).unwrap(),
                before
            );
        }
        (input_schema, json_envelope)
    };

    let toon_envelope = {
        let mut toon_harness = new_harness();
        setup(&mut toon_harness);
        let before = fs::read(tx_file_path(toon_harness.workspace.path())).unwrap();
        let envelope = parse_tool_envelope(
            &toon_harness
                .client
                .call_tool(tool_name, args(&toon_harness, "toon"))
                .unwrap_or_else(|err| panic!("call {tool_name} toon case: {err}")),
            "toon",
        );
        assert_response_outcome(&toon_harness, tool_name, &envelope);
        if tool_name == "wa.tx_rollback" {
            assert_eq!(
                fs::read(tx_file_path(toon_harness.workspace.path())).unwrap(),
                before
            );
        }
        envelope
    };

    assert_eq!(
        canonical_value(&json_envelope),
        canonical_value(&toon_envelope),
        "{tool_name} TOON envelope drifted from JSON outcome semantics"
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
        json_envelope,
        toon_envelope,
        boundary_invalid_params_error,
    }
}

#[test]
fn mcp_conformance_wa_tx_toon_and_boundary_contract_matches_golden() {
    let captures = vec![
        capture_tool_contract(
            "wa.tx_plan",
            seed_planned_tx,
            |harness, format| {
                json!({
                    "format": format,
                    "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
                })
            },
            |_| {},
            |_| {
                json!({
                    "format": "json",
                    "contract_file": 7
                })
            },
            "root.contract_file",
        ),
        capture_tool_contract(
            "wa.tx_show",
            seed_planned_tx,
            |harness, format| {
                json!({
                    "format": format,
                    "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                    "include_contract": true
                })
            },
            |_| {},
            |_| {
                json!({
                    "format": "json",
                    "include_contract": "yes"
                })
            },
            "root.include_contract",
        ),
        capture_tool_contract(
            "wa.tx_run",
            seed_planned_tx,
            |harness, format| {
                json!({
                    "format": format,
                    "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
                })
            },
            seed_planned_tx,
            |harness| {
                json!({
                    "format": "json",
                    "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                    "paused": "true"
                })
            },
            "root.paused",
        ),
        capture_tool_contract(
            "wa.tx_rollback",
            seed_prepare_denied_tx,
            |harness, format| {
                json!({
                    "format": format,
                    "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
                })
            },
            seed_prepare_denied_tx,
            |harness| {
                json!({
                    "format": "json",
                    "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                    "fail_compensation_for_step": 7
                })
            },
            "root.fail_compensation_for_step",
        ),
    ];

    assert_matches_golden("wa_tx_toon_conformance", &captures);
}
