#![cfg(feature = "mcp")]

//! Conformance + golden-artifact coverage for `wa.rules_test`.
//!
//! `mcp_conformance.rs` covers `wa.rules_list`; `mcp_conformance_core_tools.rs`
//! covers `wa.search / wa.get_text / wa.send / wa.wait_for`. This file pins the
//! remaining `wa.rules_*` tool the manifest advertises: `wa.rules_test`.
//!
//! Scope:
//!   * envelope shape (FT-MCP-0001 contract via parse_invalid_args_response)
//!   * golden artifact freeze (deterministic input → canonical output)
//!   * TOON-vs-JSON parity on semantics
//!   * manifest schema parity (advertised input_schema matches tool definition)
//!
//! Known drift recorded by the golden file: the MCP `McpRuleMatchItem` emits
//! `agent_type`, `event_type`, `severity`, `confidence`, and `extracted` fields
//! not present in `docs/json-schema/wa-robot-rules-test.json`. The documented
//! schema also lists `start`/`end` byte offsets that the MCP handler does NOT
//! emit today. The golden encodes whatever the implementation actually returns;
//! any change requires an explicit `UPDATE_GOLDEN=1` regeneration pass.

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkMcpError, FrameworkTestClient, FrameworkTool,
    framework_create_memory_transport_pair,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct RulesTestGoldenCapture {
    tool: String,
    input_schema: Value,
    anchor_success_envelope: Value,
    anchor_success_envelope_with_trace: Value,
    empty_text_success_envelope: Value,
    invalid_format_response: Value,
    missing_required_response: Value,
}

fn spawn_client() -> FrameworkTestClient {
    let mut config = Config::default();
    config.safety.require_prompt_active = false;
    let server = build_server_with_db(&config, None).expect("build MCP server");
    let (client_transport, server_transport) = framework_create_memory_transport_pair();
    std::thread::spawn(move || {
        let _ = server.run_transport(server_transport);
    });
    let mut client = FrameworkTestClient::new(client_transport);
    client
        .initialize()
        .expect("initialize in-memory MCP client");
    client
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

fn parse_toon_envelope(contents: &[FrameworkContent]) -> Value {
    let text = first_text_content(contents);
    let decoded = toon_rust::try_decode(text, None).expect("decode TOON payload");
    let json_text = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    serde_json::from_str(&json_text).expect("TOON payload should stringify back to JSON")
}

fn parse_invalid_args_response(
    result: Result<Vec<FrameworkContent>, FrameworkMcpError>,
) -> Value {
    match result {
        Ok(contents) => json!({
            "kind": "tool_envelope",
            "payload": parse_tool_envelope(&contents),
        }),
        Err(err) => json!({
            "kind": "framework_error",
            "code": format!("{:?}", err.code),
            "message": err.message,
            "data": err.data,
        }),
    }
}

fn assert_common_envelope_fields(envelope: &Value, ok: bool) {
    assert_eq!(envelope["ok"], Value::Bool(ok));
    assert!(
        envelope["elapsed_ms"].is_number(),
        "elapsed_ms must be numeric; got {:?}",
        envelope["elapsed_ms"]
    );
    assert!(
        envelope["version"].is_string(),
        "version must be present; got {:?}",
        envelope["version"]
    );
    assert!(envelope["now"].is_number(), "now must be numeric");
    assert_eq!(envelope["mcp_version"], "v1");
}

fn assert_success_envelope_shape(envelope: &Value) {
    assert_common_envelope_fields(envelope, true);
    assert!(envelope["data"].is_object(), "data must be object");
    assert!(envelope.get("error").is_none());
    assert!(envelope.get("error_code").is_none());
    assert!(envelope.get("hint").is_none());
}

fn assert_rules_test_data_shape(data: &Value) {
    let obj = data.as_object().expect("rules_test data must be object");
    assert!(
        obj.contains_key("text_length"),
        "data missing text_length: {data}"
    );
    assert!(
        obj.contains_key("match_count"),
        "data missing match_count: {data}"
    );
    assert!(obj.contains_key("matches"), "data missing matches: {data}");
    assert!(
        data["text_length"].is_number(),
        "text_length must be integer"
    );
    assert!(
        data["match_count"].is_number(),
        "match_count must be integer"
    );
    assert!(data["matches"].is_array(), "matches must be array");
    for (index, m) in data["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .enumerate()
    {
        let item = m
            .as_object()
            .unwrap_or_else(|| panic!("matches[{index}] must be object; got {m}"));
        assert!(
            item.contains_key("rule_id"),
            "matches[{index}] missing rule_id"
        );
        assert!(
            item.contains_key("matched_text"),
            "matches[{index}] missing matched_text"
        );
        assert!(
            m["rule_id"].is_string(),
            "matches[{index}].rule_id must be string"
        );
        assert!(
            m["matched_text"].is_string(),
            "matches[{index}].matched_text must be string"
        );
    }
}

fn canonicalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "now" | "elapsed_ms" => *child = Value::from(0_u64),
                    "confidence" if child.is_number() => {
                        // Confidence varies with internal PatternEngine scoring heuristics
                        // — freeze to 0.0 to keep the golden insensitive to tuning.
                        *child = Value::from(0.0_f64);
                    }
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
    let canon = canonical_value(value);
    format!(
        "{}\n",
        serde_json::to_string_pretty(&canon).expect("serialize canonical JSON")
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
            "missing MCP rules_test conformance golden at {}: {err}. \
             Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test \
             mcp_conformance_rules_test --features mcp,asupersync-runtime",
            path.display()
        )
    })
}

fn assert_matches_golden(name: &str, capture: &RulesTestGoldenCapture) {
    let actual_value = serde_json::to_value(capture).expect("serialize capture");
    let actual_text = pretty_canonical(&actual_value);
    let path = golden_path(name);
    let expected = read_or_update_golden(&path, &actual_text);
    if expected.trim_end_matches('\n') != actual_text.trim_end_matches('\n') {
        let actual_path = path.with_extension("actual.json");
        let _ = fs::write(&actual_path, &actual_text);
        panic!(
            "wa.rules_test conformance golden drift detected. Review diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test \
             mcp_conformance_rules_test --features mcp,asupersync-runtime",
            path.display(),
            actual_path.display()
        );
    }
}

/// Deterministic anchor text: `"Conversation compacted"` is a single-anchor
/// rule (`claude_code.compaction`) whose regex is present but optional for
/// anchor firing. Chosen to keep match output deterministic byte-for-byte.
const ANCHOR_TEXT_COMPACTION: &str = "Conversation compacted";

#[test]
fn mcp_conformance_rules_test_json_success_envelope_is_well_formed() {
    let mut client = spawn_client();
    let reply = client
        .call_tool(
            "wa.rules_test",
            json!({ "text": ANCHOR_TEXT_COMPACTION, "format": "json" }),
        )
        .expect("call wa.rules_test");
    let envelope = parse_tool_envelope(&reply);
    assert_success_envelope_shape(&envelope);
    assert_rules_test_data_shape(&envelope["data"]);
    assert_eq!(
        envelope["data"]["text_length"],
        json!(ANCHOR_TEXT_COMPACTION.len())
    );
    assert!(
        envelope["data"]["match_count"].as_u64().unwrap_or(0) >= 1,
        "expected at least one match for anchor text; envelope = {envelope}"
    );
}

#[test]
fn mcp_conformance_rules_test_empty_text_returns_zero_matches() {
    let mut client = spawn_client();
    let reply = client
        .call_tool("wa.rules_test", json!({ "text": "", "format": "json" }))
        .expect("call wa.rules_test");
    let envelope = parse_tool_envelope(&reply);
    assert_success_envelope_shape(&envelope);
    assert_rules_test_data_shape(&envelope["data"]);
    assert_eq!(envelope["data"]["text_length"], json!(0));
    assert_eq!(envelope["data"]["match_count"], json!(0));
    assert_eq!(envelope["data"]["matches"], json!([]));
}

#[test]
fn mcp_conformance_rules_test_trace_flag_adds_debug_trace_to_matches() {
    let mut client = spawn_client();
    let reply = client
        .call_tool(
            "wa.rules_test",
            json!({
                "text": ANCHOR_TEXT_COMPACTION,
                "trace": true,
                "format": "json",
            }),
        )
        .expect("call wa.rules_test");
    let envelope = parse_tool_envelope(&reply);
    assert_success_envelope_shape(&envelope);
    assert_rules_test_data_shape(&envelope["data"]);
    let matches = envelope["data"]["matches"]
        .as_array()
        .expect("matches array");
    assert!(!matches.is_empty(), "expected at least one match");
    for (index, m) in matches.iter().enumerate() {
        let trace = m
            .get("trace")
            .unwrap_or_else(|| panic!("matches[{index}] missing trace when trace=true"));
        let trace_obj = trace
            .as_object()
            .unwrap_or_else(|| panic!("matches[{index}].trace must be object"));
        assert!(
            trace_obj.contains_key("anchors_checked"),
            "trace missing anchors_checked"
        );
        assert!(
            trace_obj.contains_key("regex_matched"),
            "trace missing regex_matched"
        );
        assert!(trace["anchors_checked"].is_boolean());
        assert!(trace["regex_matched"].is_boolean());
    }
}

#[test]
fn mcp_conformance_rules_test_omits_trace_when_flag_absent() {
    let mut client = spawn_client();
    let reply = client
        .call_tool(
            "wa.rules_test",
            json!({ "text": ANCHOR_TEXT_COMPACTION, "format": "json" }),
        )
        .expect("call wa.rules_test");
    let envelope = parse_tool_envelope(&reply);
    for (index, m) in envelope["data"]["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .enumerate()
    {
        assert!(
            m.get("trace").is_none(),
            "matches[{index}] emitted trace without trace=true; got {m}"
        );
    }
}

#[test]
fn mcp_conformance_rules_test_toon_success_matches_json_semantics() {
    let mut client = spawn_client();
    let json_reply = client
        .call_tool(
            "wa.rules_test",
            json!({ "text": ANCHOR_TEXT_COMPACTION, "format": "json" }),
        )
        .expect("call wa.rules_test json");
    let toon_reply = client
        .call_tool(
            "wa.rules_test",
            json!({ "text": ANCHOR_TEXT_COMPACTION, "format": "toon" }),
        )
        .expect("call wa.rules_test toon");
    let json_envelope = parse_tool_envelope(&json_reply);
    let toon_envelope = parse_toon_envelope(&toon_reply);
    assert_success_envelope_shape(&toon_envelope);
    assert_eq!(
        canonical_value(&json_envelope),
        canonical_value(&toon_envelope),
        "wa.rules_test TOON envelope drifted from JSON semantics"
    );
}

#[test]
fn mcp_conformance_rules_test_invalid_format_returns_documented_envelope() {
    let mut client = spawn_client();
    let reply = client
        .call_tool(
            "wa.rules_test",
            json!({ "text": ANCHOR_TEXT_COMPACTION, "format": "yaml" }),
        )
        .expect("call wa.rules_test");
    let envelope = parse_tool_envelope(&reply);
    assert_common_envelope_fields(&envelope, false);
    assert_eq!(envelope["error_code"], "FT-MCP-0001");
    assert!(
        envelope["error"]
            .as_str()
            .expect("error string")
            .contains("Invalid format"),
        "expected 'Invalid format' hint; got {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .expect("hint string")
            .contains("json"),
        "hint must mention 'json'; got {envelope}"
    );
}

#[test]
fn mcp_conformance_rules_test_missing_required_text_is_rejected() {
    let mut client = spawn_client();
    // No `text` field — required per manifest + tool definition. FastMCP enforces
    // required-key presence at the JSON-RPC boundary, so this surfaces as a
    // framework-level InvalidParams error rather than an FT-MCP-0001 envelope.
    let response = parse_invalid_args_response(
        client.call_tool("wa.rules_test", json!({ "trace": false, "format": "json" })),
    );
    assert_eq!(
        response["kind"], "framework_error",
        "missing required `text` should surface as framework_error; got {response}"
    );
    assert!(
        response["message"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("text")
            || response["message"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("required")
            || response["message"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("invalid"),
        "framework error should reference missing `text`; got {response}"
    );
}

#[test]
fn mcp_conformance_rules_test_input_schema_matches_advertised_manifest() {
    let mut client = spawn_client();
    let live = tool_input_schema(&mut client, "wa.rules_test");
    let manifest = manifest_tool_schema("wa.rules_test");
    // Compare canonicalized form (sorted keys, whitespace-normalized) to ignore
    // serializer ordering drift.
    assert_eq!(
        canonical_value(&live),
        canonical_value(&manifest),
        "wa.rules_test input_schema drifted from advertised manifest fixture"
    );
}

#[test]
fn mcp_conformance_rules_test_contract_matches_golden() {
    let mut client = spawn_client();
    let input_schema = tool_input_schema(&mut client, "wa.rules_test");

    let anchor_json = parse_tool_envelope(
        &client
            .call_tool(
                "wa.rules_test",
                json!({ "text": ANCHOR_TEXT_COMPACTION, "format": "json" }),
            )
            .expect("call wa.rules_test anchor"),
    );
    let anchor_with_trace_json = parse_tool_envelope(
        &client
            .call_tool(
                "wa.rules_test",
                json!({
                    "text": ANCHOR_TEXT_COMPACTION,
                    "trace": true,
                    "format": "json",
                }),
            )
            .expect("call wa.rules_test anchor+trace"),
    );
    let empty_json = parse_tool_envelope(
        &client
            .call_tool("wa.rules_test", json!({ "text": "", "format": "json" }))
            .expect("call wa.rules_test empty"),
    );
    let invalid_format_json = parse_tool_envelope(
        &client
            .call_tool(
                "wa.rules_test",
                json!({ "text": ANCHOR_TEXT_COMPACTION, "format": "yaml" }),
            )
            .expect("call wa.rules_test invalid format"),
    );
    let missing_required = parse_invalid_args_response(
        client.call_tool("wa.rules_test", json!({ "trace": false })),
    );

    let capture = RulesTestGoldenCapture {
        tool: "wa.rules_test".to_string(),
        input_schema,
        anchor_success_envelope: anchor_json,
        anchor_success_envelope_with_trace: anchor_with_trace_json,
        empty_text_success_envelope: empty_json,
        invalid_format_response: invalid_format_json,
        missing_required_response: missing_required,
    };
    assert_matches_golden("wa_rules_test", &capture);
}
