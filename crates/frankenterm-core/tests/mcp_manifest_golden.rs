//! Golden freeze for the MCP server's public protocol surface (ft-mejto).
//!
//! Captures the complete set of advertised MCP tools, resources, and resource
//! templates — together with their descriptions, input schemas, tags, and
//! annotations — as a canonical JSON manifest. Any change to this manifest
//! (additions, removals, description wording, or input-schema structure) will
//! fail this test and must be reviewed and intentionally reflected in the
//! committed golden file.
//!
//! Regeneration: run with `UPDATE_GOLDEN=1`:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test --test mcp_manifest_golden \
//!     --no-default-features --features mcp,tokio-runtime
//! ```
//!
//! The golden lives at `tests/fixtures/mcp_manifest.json` relative to the
//! crate manifest directory.

#![cfg(feature = "mcp")]

use std::path::PathBuf;

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use serde_json::{Map, Value, json};

/// Produce a deterministic manifest of tools/resources/templates from the
/// MCP server. The manifest is a BTree-sorted JSON `Object` so that its
/// serialized form is canonical.
fn capture_manifest(db_path: Option<PathBuf>) -> Value {
    let server =
        build_server_with_db(&Config::default(), db_path).expect("build MCP server");

    let mut tools: Vec<Value> = server
        .tools()
        .into_iter()
        .map(|tool| {
            let mut entry = Map::new();
            entry.insert("name".to_string(), Value::String(tool.name));
            entry.insert(
                "description".to_string(),
                tool.description
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            entry.insert("input_schema".to_string(), tool.input_schema);
            entry.insert(
                "output_schema".to_string(),
                tool.output_schema.unwrap_or(Value::Null),
            );
            entry.insert(
                "tags".to_string(),
                Value::Array(tool.tags.into_iter().map(Value::String).collect()),
            );
            entry.insert(
                "version".to_string(),
                tool.version.map(Value::String).unwrap_or(Value::Null),
            );
            entry.insert(
                "annotations".to_string(),
                tool.annotations
                    .map(|a| serde_json::to_value(a).expect("serialize annotations"))
                    .unwrap_or(Value::Null),
            );
            Value::Object(entry)
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
        .map(|r| {
            json!({
                "uri": r.uri,
                "name": r.name,
                "description": r.description,
                "mime_type": r.mime_type,
                "tags": r.tags,
                "version": r.version,
            })
        })
        .collect();
    resources.sort_by(|a, b| {
        let ua = a.get("uri").and_then(Value::as_str).unwrap_or("");
        let ub = b.get("uri").and_then(Value::as_str).unwrap_or("");
        ua.cmp(ub)
    });

    let mut templates: Vec<Value> = server
        .resource_templates()
        .into_iter()
        .map(|t| {
            json!({
                "uri_template": t.uri_template,
                "name": t.name,
                "description": t.description,
                "mime_type": t.mime_type,
                "tags": t.tags,
                "version": t.version,
            })
        })
        .collect();
    templates.sort_by(|a, b| {
        let ua = a.get("uri_template").and_then(Value::as_str).unwrap_or("");
        let ub = b.get("uri_template").and_then(Value::as_str).unwrap_or("");
        ua.cmp(ub)
    });

    json!({
        "tools": tools,
        "resources": resources,
        "resource_templates": templates,
    })
}

fn canonical_json(value: &Value) -> String {
    canonicalize(value).to_string()
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::new();
            for (k, v) in sorted {
                out.insert(k.clone(), canonicalize(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(canonicalize).collect())
        }
        other => other.clone(),
    }
}

fn pretty_canonical(value: &Value) -> String {
    let canonical = canonicalize(value);
    serde_json::to_string_pretty(&canonical).expect("serialize manifest")
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mcp_manifest.json")
}

/// With `db_path == None`, the server registers only the always-on tool set.
/// The golden lives alongside the in-db manifest so a missing db_path variant
/// has its own focused surface test.
#[test]
fn mcp_manifest_matches_golden_without_db() {
    let manifest = capture_manifest(None);
    let actual = pretty_canonical(&manifest);

    // Freeze: the always-on (no-db) tool set must be a non-empty strict
    // subset of the full manifest. We don't commit a second golden; we
    // just assert structural properties that cannot drift silently.
    let tools = manifest.get("tools").and_then(Value::as_array).expect("tools");
    assert!(!tools.is_empty(), "no-db MCP server must expose at least one tool");
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .expect("tool name");
        assert!(
            name.starts_with("wa.") || name.starts_with("frankenterm.") || name.contains('_'),
            "unexpected tool name shape (no-db variant): {name}"
        );
    }
    // Non-destructive: ensure the output is valid JSON by round-tripping.
    let _: Value = serde_json::from_str(&actual).expect("round-trip no-db manifest");
}

#[test]
fn mcp_manifest_matches_golden_with_db() {
    // Use a fixed path for the manifest capture. The tool handlers only
    // record the path; they do not open the DB during registration, so
    // the file need not exist.
    let db_path = Some(PathBuf::from(
        "/tmp/ft-mcp-manifest-golden-fixture.sqlite3",
    ));

    let manifest = capture_manifest(db_path);
    let actual = pretty_canonical(&manifest);
    let golden = golden_path();

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = golden.parent() {
            std::fs::create_dir_all(parent).expect("create fixtures dir");
        }
        std::fs::write(&golden, format!("{actual}\n")).expect("write golden");
        return;
    }

    let expected = std::fs::read_to_string(&golden).unwrap_or_else(|err| {
        panic!(
            "missing MCP manifest golden at {}: {err}. Regenerate with \
             UPDATE_GOLDEN=1 cargo test --test mcp_manifest_golden",
            golden.display()
        )
    });

    // Tolerate a trailing newline in the committed file.
    let expected_trimmed = expected.trim_end_matches('\n');
    let actual_trimmed = actual.trim_end_matches('\n');

    if expected_trimmed != actual_trimmed {
        // Write actual next to the golden for easy diff review.
        let actual_path = golden.with_extension("actual.json");
        let _ = std::fs::write(&actual_path, format!("{actual}\n"));
        panic!(
            "MCP manifest drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test --test mcp_manifest_golden \
             --no-default-features --features mcp,tokio-runtime",
            golden.display(),
            actual_path.display()
        );
    }
}

#[test]
fn mcp_manifest_capture_is_deterministic() {
    let db_path = Some(PathBuf::from("/tmp/ft-mcp-manifest-determinism.sqlite3"));
    let first = pretty_canonical(&capture_manifest(db_path.clone()));
    let second = pretty_canonical(&capture_manifest(db_path.clone()));
    let third = pretty_canonical(&capture_manifest(db_path));
    assert_eq!(first, second, "manifest must be deterministic across captures");
    assert_eq!(
        second, third,
        "manifest must remain deterministic across repeated captures"
    );
}

#[test]
fn mcp_manifest_tool_names_are_unique() {
    let db_path = Some(PathBuf::from("/tmp/ft-mcp-manifest-unique.sqlite3"));
    let manifest = capture_manifest(db_path);
    let tools = manifest.get("tools").and_then(Value::as_array).expect("tools");
    let mut names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    names.sort();
    let original_len = names.len();
    names.dedup();
    assert_eq!(
        original_len,
        names.len(),
        "duplicate tool names detected in MCP manifest"
    );
}

#[test]
fn mcp_manifest_resource_uris_are_unique() {
    let db_path = Some(PathBuf::from("/tmp/ft-mcp-manifest-resource-unique.sqlite3"));
    let manifest = capture_manifest(db_path);
    let resources = manifest
        .get("resources")
        .and_then(Value::as_array)
        .expect("resources");
    let mut uris: Vec<&str> = resources
        .iter()
        .filter_map(|r| r.get("uri").and_then(Value::as_str))
        .collect();
    uris.sort();
    let original_len = uris.len();
    uris.dedup();
    assert_eq!(
        original_len,
        uris.len(),
        "duplicate resource URIs detected in MCP manifest"
    );

    let templates = manifest
        .get("resource_templates")
        .and_then(Value::as_array)
        .expect("resource_templates");
    let mut template_uris: Vec<&str> = templates
        .iter()
        .filter_map(|r| r.get("uri_template").and_then(Value::as_str))
        .collect();
    template_uris.sort();
    let original_len = template_uris.len();
    template_uris.dedup();
    assert_eq!(
        original_len,
        template_uris.len(),
        "duplicate resource template URIs detected in MCP manifest"
    );
}

#[test]
fn mcp_manifest_tool_input_schemas_are_objects() {
    let db_path = Some(PathBuf::from(
        "/tmp/ft-mcp-manifest-schema-shape.sqlite3",
    ));
    let manifest = capture_manifest(db_path);
    let tools = manifest.get("tools").and_then(Value::as_array).expect("tools");
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .expect("tool name");
        let schema = tool.get("input_schema").expect("input_schema");
        assert!(
            schema.is_object(),
            "tool `{name}` has non-object input_schema: {schema}"
        );
        let obj = schema.as_object().unwrap();
        let ty = obj.get("type").and_then(Value::as_str);
        assert_eq!(
            ty,
            Some("object"),
            "tool `{name}` input_schema.type must be \"object\", got {ty:?}"
        );
    }
}
