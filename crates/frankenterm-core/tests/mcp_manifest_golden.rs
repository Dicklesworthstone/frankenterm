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
//!     --no-default-features --features mcp,asupersync-runtime
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
    let server = build_server_with_db(&Config::default(), db_path).expect("build MCP server");

    let mut tools: Vec<Value> = server
        .tools()
        .into_iter()
        .map(|tool| {
            let mut entry = Map::new();
            entry.insert("name".to_string(), Value::String(tool.name));
            entry.insert(
                "description".to_string(),
                tool.description.map(Value::String).unwrap_or(Value::Null),
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
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
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

fn no_db_golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mcp_manifest_no_db.json")
}

/// Return a per-test isolated sqlite db path plus the TempDir that owns it.
/// Keep the TempDir alive for the caller's scope (it cleans up on drop).
fn isolated_db_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir for mcp manifest test");
    let path = dir.path().join("mcp_manifest.sqlite3");
    (dir, path)
}

fn read_or_update_golden(path: &PathBuf, actual: &str) -> String {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixtures dir");
        }
        std::fs::write(path, format!("{actual}\n")).expect("write golden");
        return actual.to_string();
    }

    std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing MCP manifest golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test --test mcp_manifest_golden \
             --no-default-features --features mcp,asupersync-runtime",
            path.display()
        )
    })
}

fn assert_matches_golden(actual: &str, golden: &PathBuf) {
    let expected = read_or_update_golden(golden, actual);
    let expected_trimmed = expected.trim_end_matches('\n');
    let actual_trimmed = actual.trim_end_matches('\n');

    if expected_trimmed != actual_trimmed {
        let actual_path = golden.with_extension("actual.json");
        let _ = std::fs::write(&actual_path, format!("{actual}\n"));
        panic!(
            "MCP manifest drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test --test mcp_manifest_golden \
             --no-default-features --features mcp,asupersync-runtime",
            golden.display(),
            actual_path.display()
        );
    }
}

fn string_set(values: &[Value], key: &str) -> std::collections::BTreeSet<String> {
    values
        .iter()
        .map(|value| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("missing `{key}` in manifest entry: {value}"))
                .to_string()
        })
        .collect()
}

/// With `db_path == None`, the server registers only the always-on tool set.
/// The golden lives alongside the in-db manifest so a missing db_path variant
/// has its own focused surface test.
#[test]
fn mcp_manifest_matches_golden_without_db() {
    let manifest = capture_manifest(None);
    let actual = pretty_canonical(&manifest);
    let (_db_dir, db_path) = isolated_db_path();
    let full_manifest = capture_manifest(Some(db_path));

    assert_matches_golden(&actual, &no_db_golden_path());

    let no_db_tools = string_set(
        manifest
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools"),
        "name",
    );
    let full_tools = string_set(
        full_manifest
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools"),
        "name",
    );
    assert!(
        !no_db_tools.is_empty(),
        "no-db MCP server must expose at least one tool"
    );
    assert!(
        no_db_tools.is_subset(&full_tools),
        "no-db tool manifest must be a subset of the db-backed manifest"
    );
    assert!(
        full_tools.len() > no_db_tools.len(),
        "db-backed MCP manifest must advertise at least one db-gated tool"
    );

    let no_db_resources = string_set(
        manifest
            .get("resources")
            .and_then(Value::as_array)
            .expect("resources"),
        "uri",
    );
    let full_resources = string_set(
        full_manifest
            .get("resources")
            .and_then(Value::as_array)
            .expect("resources"),
        "uri",
    );
    assert!(
        no_db_resources.is_subset(&full_resources),
        "no-db resources must be a subset of the db-backed manifest"
    );
    assert!(
        full_resources.len() > no_db_resources.len(),
        "db-backed MCP manifest must advertise at least one db-gated resource"
    );

    let no_db_templates = string_set(
        manifest
            .get("resource_templates")
            .and_then(Value::as_array)
            .expect("resource_templates"),
        "uri_template",
    );
    let full_templates = string_set(
        full_manifest
            .get("resource_templates")
            .and_then(Value::as_array)
            .expect("resource_templates"),
        "uri_template",
    );
    assert!(
        no_db_templates.is_subset(&full_templates),
        "no-db resource templates must be a subset of the db-backed manifest"
    );

    let absent_db_tools: Vec<_> = full_tools.difference(&no_db_tools).cloned().collect();
    assert!(
        !absent_db_tools.is_empty(),
        "expected db-backed manifest to expose db-gated tools absent from no-db mode"
    );
    // Whitelist of tools that MUST be db-gated. Each entry is verified
    // twice (ft-cbeyl): (a) the tool still exists in the db-backed manifest
    // — guards against rename/removal silently making the no-db check pass
    // trivially — and (b) the tool is absent from the no-db manifest.
    for expected_absent_tool in [
        "wa.accounts",
        "wa.events",
        "wa.reserve",
        "wa.release",
        "wa.search",
        "wa.send",
        "wa.workflow_run",
    ] {
        assert!(
            full_tools.contains(expected_absent_tool),
            "whitelist drift (ft-cbeyl): {expected_absent_tool} no longer exists in the db-backed manifest — update the whitelist deliberately instead of letting the no-db check pass trivially"
        );
        assert!(
            !no_db_tools.contains(expected_absent_tool),
            "db-gated tool unexpectedly advertised without db_path: {expected_absent_tool}"
        );
    }

    let absent_db_resources: Vec<_> = full_resources
        .difference(&no_db_resources)
        .cloned()
        .collect();
    assert!(
        !absent_db_resources.is_empty(),
        "expected db-backed manifest to expose db-gated resources absent from no-db mode"
    );
    for expected_absent_resource in ["wa://accounts", "wa://events", "wa://reservations"] {
        assert!(
            full_resources.contains(expected_absent_resource),
            "whitelist drift (ft-cbeyl): {expected_absent_resource} no longer exists in the db-backed manifest — update the whitelist deliberately instead of letting the no-db check pass trivially"
        );
        assert!(
            !no_db_resources.contains(expected_absent_resource),
            "db-gated resource unexpectedly advertised without db_path: {expected_absent_resource}"
        );
    }

    for expected_absent_template in [
        "wa://accounts/{service}",
        "wa://events/{limit}",
        "wa://events/unhandled/{limit}",
        "wa://reservations/{pane_id}",
    ] {
        assert!(
            full_templates.contains(expected_absent_template),
            "whitelist drift (ft-cbeyl): {expected_absent_template} no longer exists in the db-backed manifest — update the whitelist deliberately instead of letting the no-db check pass trivially"
        );
        assert!(
            !no_db_templates.contains(expected_absent_template),
            "db-gated template unexpectedly advertised without db_path: {expected_absent_template}"
        );
    }

    let _: Value = serde_json::from_str(&actual).expect("round-trip no-db manifest");
}

#[test]
fn mcp_manifest_matches_golden_with_db() {
    // The tool handlers only record the path; they do not open the DB during
    // registration, so the file need not exist. Use an isolated per-test path
    // (ft-h164o) so any future code that opens the DB won't race with other
    // concurrent test processes.
    let (_db_dir, path) = isolated_db_path();
    let manifest = capture_manifest(Some(path));
    let actual = pretty_canonical(&manifest);
    assert_matches_golden(&actual, &golden_path());
}

#[test]
fn mcp_manifest_capture_is_deterministic() {
    let (_db_dir, path) = isolated_db_path();
    let db_path = Some(path);
    let first = pretty_canonical(&capture_manifest(db_path.clone()));
    let second = pretty_canonical(&capture_manifest(db_path.clone()));
    let third = pretty_canonical(&capture_manifest(db_path));
    assert_eq!(
        first, second,
        "manifest must be deterministic across captures"
    );
    assert_eq!(
        second, third,
        "manifest must remain deterministic across repeated captures"
    );
}

#[test]
fn mcp_manifest_tool_names_are_unique() {
    let (_db_dir, path) = isolated_db_path();
    let manifest = capture_manifest(Some(path));
    let tools = manifest
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools");
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
    let (_db_dir, path) = isolated_db_path();
    let manifest = capture_manifest(Some(path));
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
    let (_db_dir, path) = isolated_db_path();
    let manifest = capture_manifest(Some(path));
    let tools = manifest
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools");
    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).expect("tool name");
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
