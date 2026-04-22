//! Conformance harness for MCP manifest list payloads.
//!
//! Verifies the combined JSON shape used to expose MCP list surfaces for
//! tools, resources, and prompts. The harness is intentionally stricter than
//! the golden freeze: it rejects unknown fields, malformed optional values,
//! and duplicate logical identifiers so drift shows up as a contract break.

#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestListResponse {
    tools: Vec<ToolListEntry>,
    resources: Vec<ResourceListEntry>,
    prompts: Vec<PromptListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolListEntry {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Map<String, Value>,
    #[serde(rename = "outputSchema", default)]
    output_schema: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    annotations: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceListEntry {
    uri: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptListEntry {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    arguments: Vec<PromptArgumentEntry>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptArgumentEntry {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    required: bool,
}

fn capture_manifest_lists(db_path: Option<PathBuf>) -> Value {
    let server = build_server_with_db(&Config::default(), db_path).expect("build MCP server");

    let mut tools: Vec<Value> = server
        .tools()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema,
                "outputSchema": tool.output_schema,
                "version": tool.version,
                "tags": tool.tags,
                "annotations": tool.annotations.map(|a| serde_json::to_value(a).expect("serialize annotations")),
            })
        })
        .collect();
    tools.sort_by(|a, b| {
        let na = a.get("name").and_then(Value::as_str).unwrap_or("");
        let nb = b.get("name").and_then(Value::as_str).unwrap_or("");
        na.cmp(nb)
    });

    let mut resources: Vec<Value> = server
        .resources()
        .into_iter()
        .map(|resource| {
            json!({
                "uri": resource.uri,
                "name": resource.name,
                "description": resource.description,
                "mimeType": resource.mime_type,
                "version": resource.version,
                "tags": resource.tags,
            })
        })
        .collect();
    resources.sort_by(|a, b| {
        let ua = a.get("uri").and_then(Value::as_str).unwrap_or("");
        let ub = b.get("uri").and_then(Value::as_str).unwrap_or("");
        ua.cmp(ub)
    });

    let mut prompts: Vec<Value> = server
        .prompts()
        .into_iter()
        .map(|prompt| {
            json!({
                "name": prompt.name,
                "description": prompt.description,
                "arguments": prompt.arguments.into_iter().map(|arg| {
                    json!({
                        "name": arg.name,
                        "description": arg.description,
                        "required": arg.required,
                    })
                }).collect::<Vec<_>>(),
                "version": prompt.version,
                "tags": prompt.tags,
            })
        })
        .collect();
    prompts.sort_by(|a, b| {
        let na = a.get("name").and_then(Value::as_str).unwrap_or("");
        let nb = b.get("name").and_then(Value::as_str).unwrap_or("");
        na.cmp(nb)
    });

    json!({
        "tools": tools,
        "resources": resources,
        "prompts": prompts,
    })
}

fn parse_manifest(value: Value) -> Result<ManifestListResponse, String> {
    let manifest: ManifestListResponse =
        serde_json::from_value(value).map_err(|err| err.to_string())?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &ManifestListResponse) -> Result<(), String> {
    let mut tool_names = BTreeSet::new();
    for tool in &manifest.tools {
        if !tool_names.insert(tool.name.clone()) {
            return Err(format!("duplicate tool name: {}", tool.name));
        }
    }

    let mut resource_uris = BTreeSet::new();
    for resource in &manifest.resources {
        if !resource_uris.insert(resource.uri.clone()) {
            return Err(format!("duplicate resource uri: {}", resource.uri));
        }
    }

    let mut prompt_names = BTreeSet::new();
    for prompt in &manifest.prompts {
        if !prompt_names.insert(prompt.name.clone()) {
            return Err(format!("duplicate prompt name: {}", prompt.name));
        }
    }

    Ok(())
}

fn isolated_db_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir for conformance manifest");
    let path = dir.path().join("conformance_mcp_manifest.sqlite3");
    (dir, path)
}

#[test]
fn live_server_manifests_conform_with_and_without_db() {
    let no_db = parse_manifest(capture_manifest_lists(None)).expect("no-db manifest should parse");
    assert!(
        no_db.prompts.is_empty(),
        "current frankenterm MCP server does not advertise prompts yet"
    );

    let (_dir, db_path) = isolated_db_path();
    let with_db = parse_manifest(capture_manifest_lists(Some(db_path)))
        .expect("db-backed manifest should parse");

    assert!(
        !with_db.tools.is_empty(),
        "db-backed manifest should advertise at least one tool"
    );
    assert!(
        !with_db.resources.is_empty(),
        "db-backed manifest should advertise at least one resource"
    );
}

#[test]
fn conformance_valid_cases_cover_minimal_and_optional_fields() {
    let cases = vec![
        (
            "minimal_empty_lists",
            json!({
                "tools": [],
                "resources": [],
                "prompts": [],
            }),
        ),
        (
            "all_optional_fields_set",
            json!({
                "tools": [{
                    "name": "tool.echo",
                    "description": "Echo text",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
                    "outputSchema": {"type": "object", "properties": {"ok": {"type": "boolean"}}},
                    "version": "1.2.3",
                    "tags": ["diagnostics", "echo"],
                    "annotations": {"destructive": false, "readOnly": true}
                }],
                "resources": [{
                    "uri": "ft://workspace/status",
                    "name": "workspace-status",
                    "description": "Current workspace state",
                    "mimeType": "application/json",
                    "version": "2026.04",
                    "tags": ["workspace", "status"]
                }],
                "prompts": [{
                    "name": "summarize-workspace",
                    "description": "Summarize workspace health",
                    "arguments": [{
                        "name": "scope",
                        "description": "Scope to summarize",
                        "required": true
                    }],
                    "version": "0.9.0",
                    "tags": ["summary", "analysis"]
                }]
            }),
        ),
        (
            "null_optional_fields",
            json!({
                "tools": [{
                    "name": "tool.nulls",
                    "description": null,
                    "inputSchema": {"type": "object"},
                    "outputSchema": null,
                    "version": null,
                    "tags": [],
                    "annotations": null
                }],
                "resources": [{
                    "uri": "ft://resource/nulls",
                    "name": "resource-nulls",
                    "description": null,
                    "mimeType": null,
                    "version": null,
                    "tags": []
                }],
                "prompts": [{
                    "name": "prompt-nulls",
                    "description": null,
                    "arguments": [],
                    "version": null,
                    "tags": []
                }]
            }),
        ),
    ];

    for (name, payload) in cases {
        parse_manifest(payload).unwrap_or_else(|err| panic!("{name} should conform: {err}"));
    }
}

#[test]
fn conformance_invalid_cases_reject_shape_and_uniqueness_breaks() {
    let cases = vec![
        (
            "missing_tools_field",
            json!({
                "resources": [],
                "prompts": [],
            }),
            "missing field `tools`",
        ),
        (
            "wrong_tools_type",
            json!({
                "tools": {},
                "resources": [],
                "prompts": [],
            }),
            "invalid type",
        ),
        (
            "wrong_resources_type",
            json!({
                "tools": [],
                "resources": "nope",
                "prompts": [],
            }),
            "invalid type",
        ),
        (
            "wrong_prompts_type",
            json!({
                "tools": [],
                "resources": [],
                "prompts": 1,
            }),
            "invalid type",
        ),
        (
            "extra_top_level_field",
            json!({
                "tools": [],
                "resources": [],
                "prompts": [],
                "resourceTemplates": [],
            }),
            "unknown field `resourceTemplates`",
        ),
        (
            "duplicate_tool_names",
            json!({
                "tools": [
                    {"name": "tool.dupe", "inputSchema": {"type": "object"}},
                    {"name": "tool.dupe", "inputSchema": {"type": "object"}}
                ],
                "resources": [],
                "prompts": [],
            }),
            "duplicate tool name",
        ),
        (
            "duplicate_resource_uris",
            json!({
                "tools": [],
                "resources": [
                    {"uri": "ft://same", "name": "one"},
                    {"uri": "ft://same", "name": "two"}
                ],
                "prompts": [],
            }),
            "duplicate resource uri",
        ),
        (
            "duplicate_prompt_names",
            json!({
                "tools": [],
                "resources": [],
                "prompts": [
                    {"name": "prompt.same"},
                    {"name": "prompt.same"}
                ],
            }),
            "duplicate prompt name",
        ),
        (
            "tool_missing_name",
            json!({
                "tools": [{"inputSchema": {"type": "object"}}],
                "resources": [],
                "prompts": [],
            }),
            "missing field `name`",
        ),
        (
            "tool_input_schema_must_be_object",
            json!({
                "tools": [{"name": "tool.bad", "inputSchema": "string-schema"}],
                "resources": [],
                "prompts": [],
            }),
            "invalid type",
        ),
        (
            "tool_tags_wrong_type",
            json!({
                "tools": [{
                    "name": "tool.bad-tags",
                    "inputSchema": {"type": "object"},
                    "tags": "not-an-array"
                }],
                "resources": [],
                "prompts": [],
            }),
            "invalid type",
        ),
        (
            "tool_extra_field",
            json!({
                "tools": [{
                    "name": "tool.extra",
                    "inputSchema": {"type": "object"},
                    "extra": true
                }],
                "resources": [],
                "prompts": [],
            }),
            "unknown field `extra`",
        ),
        (
            "resource_missing_uri",
            json!({
                "tools": [],
                "resources": [{"name": "resource-no-uri"}],
                "prompts": [],
            }),
            "missing field `uri`",
        ),
        (
            "resource_mime_type_wrong_type",
            json!({
                "tools": [],
                "resources": [{
                    "uri": "ft://bad",
                    "name": "bad",
                    "mimeType": 12
                }],
                "prompts": [],
            }),
            "invalid type",
        ),
        (
            "resource_extra_field",
            json!({
                "tools": [],
                "resources": [{
                    "uri": "ft://extra",
                    "name": "extra",
                    "oops": false
                }],
                "prompts": [],
            }),
            "unknown field `oops`",
        ),
        (
            "prompt_missing_name",
            json!({
                "tools": [],
                "resources": [],
                "prompts": [{"description": "missing name"}],
            }),
            "missing field `name`",
        ),
        (
            "prompt_arguments_wrong_type",
            json!({
                "tools": [],
                "resources": [],
                "prompts": [{
                    "name": "prompt.bad-args",
                    "arguments": "not-an-array"
                }],
            }),
            "invalid type",
        ),
        (
            "prompt_argument_required_wrong_type",
            json!({
                "tools": [],
                "resources": [],
                "prompts": [{
                    "name": "prompt.bad-required",
                    "arguments": [{
                        "name": "scope",
                        "required": "yes"
                    }]
                }],
            }),
            "invalid type",
        ),
        (
            "prompt_argument_extra_field",
            json!({
                "tools": [],
                "resources": [],
                "prompts": [{
                    "name": "prompt.extra-arg",
                    "arguments": [{
                        "name": "scope",
                        "default": "all"
                    }]
                }],
            }),
            "unknown field `default`",
        ),
    ];

    for (name, payload, expected) in cases {
        let err =
            parse_manifest(payload).unwrap_err_or_else(|| panic!("{name} should be rejected"));
        assert!(
            err.contains(expected),
            "{name} expected error containing {expected:?}, got {err:?}"
        );
    }
}

trait ResultExt<T> {
    fn unwrap_err_or_else(self, f: impl FnOnce() -> !) -> <Self as ResultExt<T>>::Err
    where
        Self: Sized;

    type Err;
}

impl<T, E> ResultExt<T> for Result<T, E> {
    type Err = E;

    fn unwrap_err_or_else(self, f: impl FnOnce() -> !) -> E {
        match self {
            Ok(_) => f(),
            Err(err) => err,
        }
    }
}
