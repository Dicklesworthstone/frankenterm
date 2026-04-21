#![cfg(feature = "mcp")]

use frankenterm_core::VERSION;
use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkResource, FrameworkResourceContent, FrameworkResourceTemplate,
    FrameworkTestClient, FrameworkTool, framework_create_memory_transport_pair,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;

const RULES_LIST_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/json-schema/wa-robot-rules-list.json"
));

fn spawn_client(db_path: Option<PathBuf>) -> FrameworkTestClient {
    let server = build_server_with_db(&Config::default(), db_path).expect("build MCP server");
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

fn first_text_content(contents: &[FrameworkContent]) -> &str {
    contents
        .first()
        .and_then(|content| match content {
            FrameworkContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("expected first MCP content to be text")
}

fn first_resource_text(resources: &[FrameworkResourceContent]) -> &str {
    let resource = resources.first().expect("at least one resource payload");
    resource
        .text
        .as_deref()
        .expect("resource payload should have JSON text")
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

fn parse_resource_envelope(resources: &[FrameworkResourceContent]) -> Value {
    parse_json_value(first_resource_text(resources))
}

fn assert_common_envelope_fields(envelope: &Value, ok: bool) {
    assert_eq!(envelope["ok"], ok);
    assert!(envelope["elapsed_ms"].is_number());
    assert_eq!(envelope["version"], VERSION);
    assert!(envelope["now"].is_number());
    assert_eq!(envelope["mcp_version"], "v1");
}

fn assert_success_envelope_shape(envelope: &Value) {
    assert_common_envelope_fields(envelope, true);
    assert!(envelope["data"].is_object());
    assert!(envelope.get("error").is_none());
    assert!(envelope.get("error_code").is_none());
    assert!(envelope.get("hint").is_none());
}

fn assert_invalid_args_envelope_shape(envelope: &Value, hint_substring: &str) {
    assert_common_envelope_fields(envelope, false);
    assert!(envelope.get("data").is_none());
    assert_eq!(envelope["error_code"], "FT-MCP-0001");
    assert!(envelope["error"].is_string());
    assert!(
        envelope["hint"]
            .as_str()
            .expect("hint string")
            .contains(hint_substring)
    );
}

fn assert_object_matches_documented_schema(value: &Value, schema: &Value, label: &str) {
    let object = value.as_object().expect("schema target should be object");
    let properties = schema["properties"]
        .as_object()
        .expect("schema should expose properties");

    for required in schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        assert!(
            object.contains_key(required),
            "{label} missing required key {required}"
        );
    }

    if schema["additionalProperties"] == Value::Bool(false) {
        let allowed: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
        let actual: BTreeSet<&str> = object.keys().map(String::as_str).collect();
        let unexpected: Vec<&str> = actual.difference(&allowed).copied().collect();
        assert!(
            unexpected.is_empty(),
            "{label} had undocumented keys: {unexpected:?}"
        );
    }
}

fn assert_rules_list_data_matches_documented_schema(data: &Value) {
    let schema: Value = serde_json::from_str(RULES_LIST_SCHEMA).expect("parse rules list schema");
    assert_object_matches_documented_schema(data, &schema, "wa.rules_list data");

    let rules = data["rules"].as_array().expect("rules array");
    assert!(!rules.is_empty(), "rules list should not be empty");

    let item_schema = &schema["$defs"]["rule_item"];
    for (index, rule) in rules.iter().enumerate() {
        assert_object_matches_documented_schema(rule, item_schema, &format!("rule[{index}]"));
        assert!(rule["id"].is_string());
        assert!(rule["agent_type"].is_string());
        assert!(rule["event_type"].is_string());
        assert!(rule["severity"].is_string());
        assert!(rule["description"].is_string());
        assert!(rule["anchor_count"].is_number());
        assert!(rule["has_regex"].is_boolean());
    }
}

fn assert_client_schema_has_optional_format(tool: &FrameworkTool) {
    let schema_object = tool
        .input_schema
        .as_object()
        .expect("tool input schema should be an object");
    assert_eq!(
        schema_object.get("type").and_then(Value::as_str),
        Some("object"),
        "tool {} should expose an object schema",
        tool.name
    );

    let properties = schema_object
        .get("properties")
        .and_then(Value::as_object)
        .expect("tool schema should have properties");
    let format = properties
        .get("format")
        .expect("tool schema missing format property");

    assert_eq!(format["type"], "string");
    assert_eq!(
        format["enum"]
            .as_array()
            .expect("format enum")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>(),
        vec!["json", "toon"],
        "tool {} should expose json/toon output negotiation",
        tool.name
    );

    let required = schema_object
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        !required
            .iter()
            .any(|entry| entry.as_str() == Some("format")),
        "tool {} should keep format optional",
        tool.name
    );
}

fn assert_resource_metadata(resource: &FrameworkResource) {
    assert!(resource.uri.starts_with("wa://"));
    assert!(!resource.name.trim().is_empty());
    assert!(
        resource
            .description
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    );
    assert_eq!(resource.mime_type.as_deref(), Some("application/json"));
    assert_eq!(resource.version.as_deref(), Some(VERSION));
}

fn assert_resource_template_metadata(template: &FrameworkResourceTemplate) {
    assert!(template.uri_template.starts_with("wa://"));
    assert!(!template.name.trim().is_empty());
    assert!(
        template
            .description
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    );
    assert_eq!(template.mime_type.as_deref(), Some("application/json"));
    assert_eq!(template.version.as_deref(), Some(VERSION));
}

#[test]
fn mcp_conformance_tool_schemas_advertise_optional_format_negotiation() {
    let mut client = spawn_client(None);
    let tools = client.list_tools().expect("list tools");
    assert!(!tools.is_empty(), "server should advertise MCP tools");

    for tool in &tools {
        assert_client_schema_has_optional_format(tool);
    }
}

#[test]
fn mcp_conformance_rules_list_json_success_matches_documented_envelope_and_schema() {
    let mut client = spawn_client(None);
    let reply = client
        .call_tool("wa.rules_list", json!({"verbose": true, "format": "json"}))
        .expect("call wa.rules_list");

    let envelope = parse_tool_envelope(&reply, "json");
    assert_success_envelope_shape(&envelope);
    assert_rules_list_data_matches_documented_schema(&envelope["data"]);
}

#[test]
fn mcp_conformance_rules_list_toon_success_preserves_documented_semantics() {
    let mut client = spawn_client(None);
    let reply = client
        .call_tool("wa.rules_list", json!({"verbose": false, "format": "toon"}))
        .expect("call wa.rules_list");

    let envelope = parse_tool_envelope(&reply, "toon");
    assert_success_envelope_shape(&envelope);
    assert_rules_list_data_matches_documented_schema(&envelope["data"]);
}

#[test]
fn mcp_conformance_invalid_format_returns_documented_error_envelope() {
    let mut client = spawn_client(None);
    let reply = client
        .call_tool("wa.rules_list", json!({"verbose": true, "format": "yaml"}))
        .expect("call wa.rules_list");

    let envelope = parse_tool_envelope(&reply, "json");
    assert_invalid_args_envelope_shape(&envelope, "json");
    assert!(
        envelope["error"]
            .as_str()
            .expect("error string")
            .contains("Invalid format")
    );
}

#[test]
fn mcp_conformance_inner_invalid_args_still_return_documented_error_envelope() {
    let mut client = spawn_client(None);
    let reply = client
        .call_tool("wa.rules_list", json!({"agent_type": 7, "format": "json"}))
        .expect("call wa.rules_list");

    let envelope = parse_tool_envelope(&reply, "json");
    assert_invalid_args_envelope_shape(&envelope, "Expected object with optional agent_type");
    assert!(
        envelope["error"]
            .as_str()
            .expect("error string")
            .contains("Invalid params")
    );
}

#[test]
fn mcp_conformance_resource_catalog_is_versioned_json_for_clients() {
    let mut client = spawn_client(None);
    let resources = client.list_resources().expect("list resources");
    let templates = client
        .list_resource_templates()
        .expect("list resource templates");

    assert!(!resources.is_empty(), "server should advertise resources");
    assert!(!templates.is_empty(), "server should advertise templates");

    for resource in &resources {
        assert_resource_metadata(resource);
    }
    for template in &templates {
        assert_resource_template_metadata(template);
    }
}

#[test]
fn mcp_conformance_rules_resource_returns_well_formed_json_envelope() {
    let mut client = spawn_client(None);
    let resources = client.read_resource("wa://rules").expect("read wa://rules");
    let resource = resources.first().expect("rules resource entry");

    assert_eq!(resource.uri, "wa://rules");
    assert_eq!(resource.mime_type.as_deref(), Some("application/json"));
    assert!(resource.blob.is_none());

    let envelope = parse_resource_envelope(&resources);
    assert_success_envelope_shape(&envelope);
    assert_rules_list_data_matches_documented_schema(&envelope["data"]);
}

#[test]
fn mcp_conformance_rules_template_resource_returns_filtered_json_envelope() {
    let mut client = spawn_client(None);
    let resources = client
        .read_resource("wa://rules/codex")
        .expect("read wa://rules/codex");
    let resource = resources.first().expect("rules template resource entry");

    assert_eq!(resource.uri, "wa://rules/codex");
    assert_eq!(resource.mime_type.as_deref(), Some("application/json"));
    assert!(resource.blob.is_none());

    let envelope = parse_resource_envelope(&resources);
    assert_success_envelope_shape(&envelope);
    assert_eq!(envelope["data"]["agent_type_filter"], "codex");
    assert_rules_list_data_matches_documented_schema(&envelope["data"]);
}

#[test]
fn mcp_conformance_workflows_resource_returns_counted_json_payload() {
    let mut client = spawn_client(None);
    let resources = client
        .read_resource("wa://workflows")
        .expect("read wa://workflows");
    let resource = resources.first().expect("workflows resource entry");

    assert_eq!(resource.uri, "wa://workflows");
    assert_eq!(resource.mime_type.as_deref(), Some("application/json"));
    assert!(resource.blob.is_none());

    let envelope = parse_resource_envelope(&resources);
    assert_success_envelope_shape(&envelope);

    let data = envelope["data"]
        .as_object()
        .expect("workflow payload object");
    let workflows = data["workflows"].as_array().expect("workflow list");
    let total = data["total"].as_u64().expect("workflow count");
    assert_eq!(total as usize, workflows.len());
    assert!(
        workflows.iter().all(|workflow| workflow["name"].is_string()
            && workflow["description"].is_string()
            && workflow["step_count"].is_number()),
        "workflow items should expose stable metadata"
    );
}
