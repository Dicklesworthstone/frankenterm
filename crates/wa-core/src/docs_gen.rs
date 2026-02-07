//! Schema-driven API reference documentation generator.
//!
//! Consumes JSON Schema files from `docs/json-schema/` and
//! [`SchemaRegistry`](crate::api_schema::SchemaRegistry) metadata to produce
//! deterministic Markdown reference pages.
//!
//! # Design
//!
//! - **No I/O**: all functions take parsed data in, return strings out.
//! - **Deterministic**: output order is fixed by registry order + sorted
//!   properties, so golden-file tests can diff without flakes.
//! - **Grouped by category**: endpoints are grouped into logical sections
//!   (panes, events, workflows, rules, accounts, reservations, meta).
//!
//! # Usage
//!
//! ```rust,no_run
//! use wa_core::api_schema::SchemaRegistry;
//! use wa_core::docs_gen::{DocGenConfig, generate_reference};
//!
//! let registry = SchemaRegistry::canonical();
//! let schemas = vec![]; // load from docs/json-schema/
//! let config = DocGenConfig::default();
//! let pages = generate_reference(&registry, &schemas, &config);
//! for page in &pages {
//!     println!("## {} ({} bytes)", page.filename, page.content.len());
//! }
//! ```

use std::collections::BTreeMap;
use std::fmt::Write;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api_schema::{EndpointMeta, SchemaRegistry};

// ───────────────────────────────────────────────────────────────────────────
// Configuration
// ───────────────────────────────────────────────────────────────────────────

/// Configuration for documentation generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocGenConfig {
    /// Include the response envelope documentation.
    pub include_envelope: bool,
    /// Include experimental (unstable) endpoints.
    pub include_experimental: bool,
    /// Include error code reference section.
    pub include_error_codes: bool,
}

impl Default for DocGenConfig {
    fn default() -> Self {
        Self {
            include_envelope: true,
            include_experimental: true,
            include_error_codes: true,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Parsed schema types
// ───────────────────────────────────────────────────────────────────────────

/// A documented property extracted from a JSON Schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyDoc {
    /// Property name.
    pub name: String,
    /// Human-readable type string (e.g., "integer", "string", "object").
    pub type_str: String,
    /// Whether this property is required.
    pub required: bool,
    /// Description from the schema.
    pub description: String,
    /// Enum values if the property has a fixed set.
    pub enum_values: Vec<String>,
    /// Minimum value constraint (for numbers).
    pub minimum: Option<f64>,
    /// Maximum value constraint (for numbers).
    pub maximum: Option<f64>,
    /// Pattern constraint (for strings).
    pub pattern: Option<String>,
}

/// A parsed schema definition (top-level or `$defs` entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDoc {
    /// Schema title.
    pub title: String,
    /// Schema description.
    pub description: String,
    /// Top-level properties.
    pub properties: Vec<PropertyDoc>,
    /// Sub-definitions from `$defs`.
    pub definitions: Vec<(String, SchemaDoc)>,
}

/// Endpoint category for grouping in reference documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointCategory {
    /// Pane state and text operations.
    PaneOperations,
    /// Search and event queries.
    SearchAndEvents,
    /// Workflow execution and management.
    Workflows,
    /// Detection rule operations.
    Rules,
    /// Account management.
    Accounts,
    /// Pane reservation management.
    Reservations,
    /// Help, approval, and diagnostics.
    Meta,
}

impl EndpointCategory {
    /// Human-readable category title.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::PaneOperations => "Pane Operations",
            Self::SearchAndEvents => "Search & Events",
            Self::Workflows => "Workflows",
            Self::Rules => "Rules",
            Self::Accounts => "Accounts",
            Self::Reservations => "Reservations",
            Self::Meta => "Meta",
        }
    }

    /// All categories in display order.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::PaneOperations,
            Self::SearchAndEvents,
            Self::Workflows,
            Self::Rules,
            Self::Accounts,
            Self::Reservations,
            Self::Meta,
        ]
    }
}

/// A generated documentation page.
#[derive(Debug, Clone)]
pub struct DocPage {
    /// Output filename (e.g., "api-reference.md").
    pub filename: String,
    /// Page title.
    pub title: String,
    /// Markdown content.
    pub content: String,
}

// ───────────────────────────────────────────────────────────────────────────
// Schema parsing
// ───────────────────────────────────────────────────────────────────────────

/// Parse a JSON Schema value into structured documentation.
///
/// Extracts title, description, properties (with types and constraints),
/// and `$defs` sub-definitions.
#[must_use]
pub fn parse_schema(schema: &Value) -> SchemaDoc {
    let title = schema
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let description = schema
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let required_set: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    let properties = parse_properties(schema, &required_set);
    let definitions = parse_defs(schema);

    SchemaDoc {
        title,
        description,
        properties,
        definitions,
    }
}

fn parse_properties(schema: &Value, required_set: &[String]) -> Vec<PropertyDoc> {
    let props = match schema.get("properties").and_then(Value::as_object) {
        Some(p) => p,
        None => return Vec::new(),
    };

    let mut result: Vec<PropertyDoc> = props
        .iter()
        .map(|(name, prop)| {
            let type_str = extract_type_str(prop);
            let description = prop
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let enum_values = prop
                .get("enum")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let minimum = prop.get("minimum").and_then(Value::as_f64);
            let maximum = prop.get("maximum").and_then(Value::as_f64);
            let pattern = prop
                .get("pattern")
                .and_then(Value::as_str)
                .map(String::from);

            PropertyDoc {
                name: name.clone(),
                type_str,
                required: required_set.contains(name),
                description,
                enum_values,
                minimum,
                maximum,
                pattern,
            }
        })
        .collect();

    // Deterministic order: required first (alphabetical), then optional (alphabetical)
    result.sort_by(|a, b| {
        b.required
            .cmp(&a.required)
            .then_with(|| a.name.cmp(&b.name))
    });

    result
}

fn extract_type_str(prop: &Value) -> String {
    // Handle $ref
    if let Some(ref_str) = prop.get("$ref").and_then(Value::as_str) {
        // Extract definition name from "#/$defs/foo"
        if let Some(name) = ref_str.strip_prefix("#/$defs/") {
            return name.to_string();
        }
        return ref_str.to_string();
    }

    // Handle type array like ["integer", "null"]
    if let Some(arr) = prop.get("type").and_then(Value::as_array) {
        let types: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
        return types.join(" | ");
    }

    // Handle simple type
    if let Some(t) = prop.get("type").and_then(Value::as_str) {
        if t == "array" {
            // Check items type
            if let Some(items) = prop.get("items") {
                if let Some(ref_str) = items.get("$ref").and_then(Value::as_str) {
                    if let Some(name) = ref_str.strip_prefix("#/$defs/") {
                        return format!("{name}[]");
                    }
                }
                if let Some(item_type) = items.get("type").and_then(Value::as_str) {
                    return format!("{item_type}[]");
                }
            }
            return "array".to_string();
        }
        return t.to_string();
    }

    "any".to_string()
}

fn parse_defs(schema: &Value) -> Vec<(String, SchemaDoc)> {
    let defs = match schema.get("$defs").and_then(Value::as_object) {
        Some(d) => d,
        None => return Vec::new(),
    };

    let mut result: Vec<(String, SchemaDoc)> = defs
        .iter()
        .map(|(name, def)| (name.clone(), parse_schema(def)))
        .collect();

    // Deterministic order
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

// ───────────────────────────────────────────────────────────────────────────
// Endpoint categorization
// ───────────────────────────────────────────────────────────────────────────

/// Classify an endpoint into a documentation category.
#[must_use]
pub fn categorize_endpoint(endpoint: &EndpointMeta) -> EndpointCategory {
    match endpoint.id.as_str() {
        "state" | "get_text" | "send" | "wait_for" => EndpointCategory::PaneOperations,
        "search" | "events" | "events_annotate" | "events_triage" | "events_label" => {
            EndpointCategory::SearchAndEvents
        }
        "workflow_run" | "workflow_list" | "workflow_status" | "workflow_abort" => {
            EndpointCategory::Workflows
        }
        "rules_list" | "rules_test" | "rules_show" | "rules_lint" => EndpointCategory::Rules,
        "accounts_list" | "accounts_refresh" => EndpointCategory::Accounts,
        "reservations_list" | "reserve" | "release" => EndpointCategory::Reservations,
        _ => EndpointCategory::Meta,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Markdown generation
// ───────────────────────────────────────────────────────────────────────────

/// Generate Markdown reference documentation from the schema registry and
/// parsed JSON Schema files.
///
/// `schemas` maps schema filename → parsed `serde_json::Value`.
/// Returns one or more [`DocPage`]s with deterministic content.
#[must_use]
pub fn generate_reference(
    registry: &SchemaRegistry,
    schemas: &[(String, Value)],
    config: &DocGenConfig,
) -> Vec<DocPage> {
    let schema_map: BTreeMap<&str, &Value> = schemas
        .iter()
        .map(|(name, val)| (name.as_str(), val))
        .collect();

    let mut pages = Vec::new();

    // Main API reference page
    let main_page = generate_main_reference(registry, &schema_map, config);
    pages.push(main_page);

    pages
}

fn generate_main_reference(
    registry: &SchemaRegistry,
    schemas: &BTreeMap<&str, &Value>,
    config: &DocGenConfig,
) -> DocPage {
    let mut out = String::with_capacity(16 * 1024);

    // Header
    writeln!(out, "# wa API Reference").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Auto-generated from JSON Schema files. Version: {}.",
        registry.version
    )
    .unwrap();
    writeln!(out).unwrap();

    // Table of contents
    write_toc(&mut out, registry, config);

    // Response envelope
    if config.include_envelope {
        if let Some(envelope) = schemas.get("wa-robot-envelope.json") {
            write_envelope_section(&mut out, envelope);
        }
    }

    // Error codes
    if config.include_error_codes {
        if let Some(envelope) = schemas.get("wa-robot-envelope.json") {
            write_error_codes_section(&mut out, envelope);
        }
    }

    // Group endpoints by category
    let grouped = group_endpoints(registry, config);

    for category in EndpointCategory::all() {
        if let Some(endpoints) = grouped.get(category) {
            if endpoints.is_empty() {
                continue;
            }
            writeln!(out, "---").unwrap();
            writeln!(out).unwrap();
            writeln!(out, "## {}", category.title()).unwrap();
            writeln!(out).unwrap();

            for ep in endpoints {
                write_endpoint_section(&mut out, ep, schemas);
            }
        }
    }

    DocPage {
        filename: "api-reference.md".to_string(),
        title: "wa API Reference".to_string(),
        content: out,
    }
}

fn write_toc(out: &mut String, registry: &SchemaRegistry, config: &DocGenConfig) {
    writeln!(out, "## Table of Contents").unwrap();
    writeln!(out).unwrap();

    if config.include_envelope {
        writeln!(out, "- [Response Envelope](#response-envelope)").unwrap();
    }
    if config.include_error_codes {
        writeln!(out, "- [Error Codes](#error-codes)").unwrap();
    }

    let grouped = group_endpoints(registry, config);

    for category in EndpointCategory::all() {
        if let Some(endpoints) = grouped.get(category) {
            if endpoints.is_empty() {
                continue;
            }
            writeln!(out, "- [{}](#{})", category.title(), slug(category.title())).unwrap();
            for ep in endpoints {
                writeln!(out, "  - [{}](#{})", ep.title, slug(&ep.title)).unwrap();
            }
        }
    }
    writeln!(out).unwrap();
}

fn write_envelope_section(out: &mut String, envelope: &Value) {
    writeln!(out, "---").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "## Response Envelope").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Every robot command returns a JSON envelope with this structure:"
    )
    .unwrap();
    writeln!(out).unwrap();

    let doc = parse_schema(envelope);
    write_properties_table(out, &doc.properties);
    writeln!(out).unwrap();

    writeln!(
        out,
        "When `ok` is `true`, the `data` field contains the command-specific response."
    )
    .unwrap();
    writeln!(
        out,
        "When `ok` is `false`, `error` and `error_code` are present."
    )
    .unwrap();
    writeln!(out).unwrap();
}

fn write_error_codes_section(out: &mut String, envelope: &Value) {
    let codes = envelope
        .pointer("/$defs/error_codes/enum")
        .and_then(Value::as_array);

    if let Some(codes) = codes {
        writeln!(out, "## Error Codes").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "| Code | Description |").unwrap();
        writeln!(out, "|------|-------------|").unwrap();

        for code in codes {
            if let Some(code_str) = code.as_str() {
                let desc = error_code_description(code_str);
                writeln!(out, "| `{code_str}` | {desc} |").unwrap();
            }
        }
        writeln!(out).unwrap();
    }
}

fn error_code_description(code: &str) -> &'static str {
    match code {
        "robot.invalid_args" => "Invalid or missing command arguments",
        "robot.unknown_subcommand" => "Unrecognized robot subcommand",
        "robot.not_implemented" => "Command is not yet implemented",
        "robot.config_error" => "Configuration error (missing or invalid config)",
        "robot.wezterm_error" => "Error communicating with WezTerm",
        "robot.storage_error" => "Database or storage layer error",
        "robot.policy_denied" => "Action denied by safety policy",
        "robot.pane_not_found" => "Specified pane does not exist",
        "robot.workflow_error" => "Workflow execution failed",
        "robot.timeout" => "Operation timed out",
        _ => "Unknown error code",
    }
}

fn write_endpoint_section(
    out: &mut String,
    endpoint: &EndpointMeta,
    schemas: &BTreeMap<&str, &Value>,
) {
    writeln!(out, "### {}", endpoint.title).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{}", endpoint.description).unwrap();
    writeln!(out).unwrap();

    // Surfaces
    if let Some(ref cmd) = endpoint.robot_command {
        writeln!(out, "**Robot:** `wa {cmd}`").unwrap();
    }
    if let Some(ref tool) = endpoint.mcp_tool {
        writeln!(out, "**MCP:** `{tool}`").unwrap();
    }

    // Stability
    if !endpoint.stable {
        writeln!(out).unwrap();
        writeln!(out, "> **Experimental** — this endpoint may change.").unwrap();
    }

    writeln!(out).unwrap();
    writeln!(out, "**Since:** v{}", endpoint.since).unwrap();
    writeln!(out, "**Schema:** `{}`", endpoint.schema_file).unwrap();
    writeln!(out).unwrap();

    // Parse and render schema
    if let Some(schema) = schemas.get(endpoint.schema_file.as_str()) {
        let doc = parse_schema(schema);

        if !doc.properties.is_empty() {
            writeln!(out, "#### Response Fields").unwrap();
            writeln!(out).unwrap();
            write_properties_table(out, &doc.properties);
            writeln!(out).unwrap();
        }

        // Render definitions
        for (def_name, def_doc) in &doc.definitions {
            if !def_doc.properties.is_empty() {
                writeln!(out, "#### `{def_name}`").unwrap();
                writeln!(out).unwrap();
                if !def_doc.description.is_empty() {
                    writeln!(out, "{}", def_doc.description).unwrap();
                    writeln!(out).unwrap();
                }
                write_properties_table(out, &def_doc.properties);
                writeln!(out).unwrap();
            }
        }
    }
}

fn write_properties_table(out: &mut String, properties: &[PropertyDoc]) {
    writeln!(out, "| Field | Type | Required | Description |").unwrap();
    writeln!(out, "|-------|------|----------|-------------|").unwrap();

    for prop in properties {
        let req = if prop.required { "**yes**" } else { "no" };
        let type_str = format_type_with_constraints(prop);
        let desc = escape_markdown_table(&prop.description);
        writeln!(
            out,
            "| `{}` | {} | {} | {} |",
            prop.name, type_str, req, desc
        )
        .unwrap();
    }
}

fn format_type_with_constraints(prop: &PropertyDoc) -> String {
    let mut parts = vec![format!("`{}`", prop.type_str)];

    if !prop.enum_values.is_empty() {
        let vals: Vec<String> = prop
            .enum_values
            .iter()
            .map(|v| format!("`\"{v}\"`"))
            .collect();
        parts.push(format!("({})", vals.join(", ")));
    }

    parts.join(" ")
}

fn escape_markdown_table(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn slug(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .replace("--", "-")
        .trim_matches('-')
        .to_string()
}

fn group_endpoints<'a>(
    registry: &'a SchemaRegistry,
    config: &DocGenConfig,
) -> BTreeMap<EndpointCategory, Vec<&'a EndpointMeta>> {
    let mut grouped: BTreeMap<EndpointCategory, Vec<&EndpointMeta>> = BTreeMap::new();

    for ep in &registry.endpoints {
        if !config.include_experimental && !ep.stable {
            continue;
        }
        let cat = categorize_endpoint(ep);
        grouped.entry(cat).or_default().push(ep);
    }

    grouped
}

// ───────────────────────────────────────────────────────────────────────────
// Summary generation (for quick overview)
// ───────────────────────────────────────────────────────────────────────────

/// Generate a compact endpoint summary table (useful for README or overview).
#[must_use]
pub fn generate_endpoint_summary(registry: &SchemaRegistry) -> String {
    let mut out = String::with_capacity(4096);

    writeln!(out, "| Endpoint | Robot Command | MCP Tool | Stable |").unwrap();
    writeln!(out, "|----------|---------------|----------|--------|").unwrap();

    for ep in &registry.endpoints {
        let robot = ep
            .robot_command
            .as_deref()
            .map(|c| format!("`wa {c}`"))
            .unwrap_or_else(|| "—".to_string());
        let mcp = ep
            .mcp_tool
            .as_deref()
            .map(|t| format!("`{t}`"))
            .unwrap_or_else(|| "—".to_string());
        let stable = if ep.stable { "yes" } else { "no" };
        writeln!(out, "| {} | {} | {} | {} |", ep.title, robot, mcp, stable).unwrap();
    }

    out
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_schema() -> Value {
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.com/test.json",
            "title": "Test Schema",
            "description": "A test schema for unit tests",
            "type": "object",
            "required": ["id", "name"],
            "properties": {
                "id": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Unique identifier"
                },
                "name": {
                    "type": "string",
                    "description": "Human-readable name"
                },
                "status": {
                    "type": "string",
                    "enum": ["active", "inactive"],
                    "description": "Current status"
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tags"
                },
                "nullable_field": {
                    "type": ["string", "null"],
                    "description": "A nullable string"
                }
            },
            "additionalProperties": false,
            "$defs": {
                "sub_object": {
                    "type": "object",
                    "description": "A sub-object definition",
                    "required": ["value"],
                    "properties": {
                        "value": {
                            "type": "number",
                            "description": "The value"
                        }
                    }
                }
            }
        })
    }

    fn sample_envelope() -> Value {
        serde_json::json!({
            "title": "Response Envelope",
            "description": "Standard response wrapper",
            "type": "object",
            "required": ["ok", "version"],
            "properties": {
                "ok": { "type": "boolean", "description": "Success flag" },
                "data": { "description": "Response data" },
                "error": { "type": "string", "description": "Error message" },
                "error_code": {
                    "type": "string",
                    "description": "Machine error code",
                    "pattern": "^robot\\.[a-z_]+$"
                },
                "version": { "type": "string", "description": "Version string" }
            },
            "$defs": {
                "error_codes": {
                    "enum": [
                        "robot.invalid_args",
                        "robot.wezterm_error",
                        "robot.policy_denied"
                    ]
                }
            }
        })
    }

    // --- Schema parsing ---

    #[test]
    fn parse_schema_extracts_title_and_description() {
        let schema = sample_schema();
        let doc = parse_schema(&schema);
        assert_eq!(doc.title, "Test Schema");
        assert_eq!(doc.description, "A test schema for unit tests");
    }

    #[test]
    fn parse_schema_extracts_properties() {
        let schema = sample_schema();
        let doc = parse_schema(&schema);
        assert_eq!(doc.properties.len(), 5);
    }

    #[test]
    fn parse_schema_marks_required_fields() {
        let schema = sample_schema();
        let doc = parse_schema(&schema);

        let id_prop = doc.properties.iter().find(|p| p.name == "id").unwrap();
        assert!(id_prop.required);

        let status_prop = doc.properties.iter().find(|p| p.name == "status").unwrap();
        assert!(!status_prop.required);
    }

    #[test]
    fn parse_schema_extracts_enum_values() {
        let schema = sample_schema();
        let doc = parse_schema(&schema);

        let status = doc.properties.iter().find(|p| p.name == "status").unwrap();
        assert_eq!(status.enum_values, vec!["active", "inactive"]);
    }

    #[test]
    fn parse_schema_extracts_type_strings() {
        let schema = sample_schema();
        let doc = parse_schema(&schema);

        let id = doc.properties.iter().find(|p| p.name == "id").unwrap();
        assert_eq!(id.type_str, "integer");

        let tags = doc.properties.iter().find(|p| p.name == "tags").unwrap();
        assert_eq!(tags.type_str, "string[]");

        let nullable = doc
            .properties
            .iter()
            .find(|p| p.name == "nullable_field")
            .unwrap();
        assert_eq!(nullable.type_str, "string | null");
    }

    #[test]
    fn parse_schema_extracts_minimum() {
        let schema = sample_schema();
        let doc = parse_schema(&schema);

        let id = doc.properties.iter().find(|p| p.name == "id").unwrap();
        assert_eq!(id.minimum, Some(0.0));
    }

    #[test]
    fn parse_schema_extracts_definitions() {
        let schema = sample_schema();
        let doc = parse_schema(&schema);
        assert_eq!(doc.definitions.len(), 1);
        assert_eq!(doc.definitions[0].0, "sub_object");
        assert_eq!(doc.definitions[0].1.properties.len(), 1);
    }

    #[test]
    fn parse_schema_handles_ref() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "result": {
                    "$ref": "#/$defs/wait_result",
                    "description": "The wait result"
                }
            }
        });
        let doc = parse_schema(&schema);
        let result = doc.properties.iter().find(|p| p.name == "result").unwrap();
        assert_eq!(result.type_str, "wait_result");
    }

    #[test]
    fn parse_schema_handles_array_items_ref() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "events": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/event" }
                }
            }
        });
        let doc = parse_schema(&schema);
        let events = doc.properties.iter().find(|p| p.name == "events").unwrap();
        assert_eq!(events.type_str, "event[]");
    }

    #[test]
    fn parse_empty_schema() {
        let schema = serde_json::json!({});
        let doc = parse_schema(&schema);
        assert!(doc.title.is_empty());
        assert!(doc.properties.is_empty());
        assert!(doc.definitions.is_empty());
    }

    // --- Property ordering ---

    #[test]
    fn properties_sorted_required_first() {
        let schema = sample_schema();
        let doc = parse_schema(&schema);

        // Required fields should come first
        let required_indices: Vec<usize> = doc
            .properties
            .iter()
            .enumerate()
            .filter(|(_, p)| p.required)
            .map(|(i, _)| i)
            .collect();
        let optional_indices: Vec<usize> = doc
            .properties
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.required)
            .map(|(i, _)| i)
            .collect();

        if let (Some(&last_req), Some(&first_opt)) =
            (required_indices.last(), optional_indices.first())
        {
            assert!(
                last_req < first_opt,
                "required fields must come before optional"
            );
        }
    }

    // --- Categorization ---

    #[test]
    fn categorize_pane_endpoints() {
        let ep = EndpointMeta {
            id: "state".into(),
            title: "Pane State".into(),
            description: String::new(),
            robot_command: Some("robot state".into()),
            mcp_tool: Some("wa.state".into()),
            schema_file: "wa-robot-state.json".into(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::PaneOperations);
    }

    #[test]
    fn categorize_workflow_endpoints() {
        let ep = EndpointMeta {
            id: "workflow_run".into(),
            title: "Run Workflow".into(),
            description: String::new(),
            robot_command: Some("robot workflow run".into()),
            mcp_tool: Some("wa.workflow_run".into()),
            schema_file: "wa-robot-workflow-run.json".into(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Workflows);
    }

    #[test]
    fn categorize_unknown_as_meta() {
        let ep = EndpointMeta {
            id: "unknown_new_thing".into(),
            title: "New Thing".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: "wa-robot-new.json".into(),
            stable: false,
            since: "0.2.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Meta);
    }

    #[test]
    fn all_categories_ordered() {
        let cats = EndpointCategory::all();
        assert_eq!(cats.len(), 7);
        assert_eq!(cats[0], EndpointCategory::PaneOperations);
        assert_eq!(cats[6], EndpointCategory::Meta);
    }

    // --- Markdown generation ---

    #[test]
    fn generate_reference_produces_page() {
        let registry = SchemaRegistry::canonical();
        let schemas = vec![];
        let config = DocGenConfig::default();
        let pages = generate_reference(&registry, &schemas, &config);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].filename, "api-reference.md");
        assert!(pages[0].content.contains("# wa API Reference"));
    }

    #[test]
    fn generate_reference_includes_toc() {
        let registry = SchemaRegistry::canonical();
        let config = DocGenConfig::default();
        let pages = generate_reference(&registry, &[], &config);
        assert!(pages[0].content.contains("## Table of Contents"));
        assert!(pages[0].content.contains("Pane Operations"));
        assert!(pages[0].content.contains("Workflows"));
    }

    #[test]
    fn generate_reference_with_schemas() {
        let registry = SchemaRegistry::canonical();
        let schema = sample_schema();
        let schemas = vec![("wa-robot-state.json".to_string(), schema)];
        let config = DocGenConfig::default();
        let pages = generate_reference(&registry, &schemas, &config);

        // Should include the endpoint section with parsed schema
        assert!(pages[0].content.contains("### Pane State"));
        assert!(pages[0].content.contains("Response Fields"));
    }

    #[test]
    fn generate_reference_with_envelope() {
        let registry = SchemaRegistry::canonical();
        let envelope = sample_envelope();
        let schemas = vec![("wa-robot-envelope.json".to_string(), envelope)];
        let config = DocGenConfig::default();
        let pages = generate_reference(&registry, &schemas, &config);

        assert!(pages[0].content.contains("## Response Envelope"));
        assert!(pages[0].content.contains("## Error Codes"));
        assert!(pages[0].content.contains("`robot.invalid_args`"));
    }

    #[test]
    fn generate_reference_excludes_experimental() {
        let registry = SchemaRegistry::canonical();
        let config = DocGenConfig {
            include_experimental: false,
            ..Default::default()
        };
        let pages = generate_reference(&registry, &[], &config);

        // rules_show is experimental
        assert!(!pages[0].content.contains("### Show Rule"));
    }

    #[test]
    fn generate_reference_deterministic() {
        let registry = SchemaRegistry::canonical();
        let config = DocGenConfig::default();
        let pages1 = generate_reference(&registry, &[], &config);
        let pages2 = generate_reference(&registry, &[], &config);
        assert_eq!(pages1[0].content, pages2[0].content);
    }

    // --- Summary generation ---

    #[test]
    fn endpoint_summary_includes_all() {
        let registry = SchemaRegistry::canonical();
        let summary = generate_endpoint_summary(&registry);
        assert!(summary.contains("Pane State"));
        assert!(summary.contains("Send Text"));
        assert!(summary.contains("`wa.state`"));
    }

    #[test]
    fn endpoint_summary_marks_robot_only() {
        let registry = SchemaRegistry::canonical();
        let summary = generate_endpoint_summary(&registry);
        // help is robot-only, should have dash for MCP
        assert!(summary.contains("Robot Help"));
    }

    // --- Helpers ---

    #[test]
    fn slug_generation() {
        assert_eq!(slug("Pane Operations"), "pane-operations");
        assert_eq!(slug("Search & Events"), "search--events");
        assert_eq!(slug("Meta"), "meta");
    }

    #[test]
    fn escape_markdown_table_pipes() {
        assert_eq!(escape_markdown_table("a|b"), "a\\|b");
        assert_eq!(escape_markdown_table("a\nb"), "a b");
    }

    #[test]
    fn format_type_basic() {
        let prop = PropertyDoc {
            name: "x".into(),
            type_str: "integer".into(),
            required: true,
            description: String::new(),
            enum_values: vec![],
            minimum: None,
            maximum: None,
            pattern: None,
        };
        assert_eq!(format_type_with_constraints(&prop), "`integer`");
    }

    #[test]
    fn format_type_with_enum() {
        let prop = PropertyDoc {
            name: "x".into(),
            type_str: "string".into(),
            required: true,
            description: String::new(),
            enum_values: vec!["a".into(), "b".into()],
            minimum: None,
            maximum: None,
            pattern: None,
        };
        let formatted = format_type_with_constraints(&prop);
        assert!(formatted.contains("`\"a\"`"));
        assert!(formatted.contains("`\"b\"`"));
    }

    #[test]
    fn error_code_descriptions_complete() {
        let known_codes = [
            "robot.invalid_args",
            "robot.unknown_subcommand",
            "robot.not_implemented",
            "robot.config_error",
            "robot.wezterm_error",
            "robot.storage_error",
            "robot.policy_denied",
            "robot.pane_not_found",
            "robot.workflow_error",
            "robot.timeout",
        ];
        for code in &known_codes {
            let desc = error_code_description(code);
            assert_ne!(desc, "Unknown error code", "missing description for {code}");
        }
    }

    #[test]
    fn config_default_includes_everything() {
        let config = DocGenConfig::default();
        assert!(config.include_envelope);
        assert!(config.include_experimental);
        assert!(config.include_error_codes);
    }

    #[test]
    fn config_roundtrip_serde() {
        let config = DocGenConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: DocGenConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.include_envelope, parsed.include_envelope);
    }

    // --- Full integration test with realistic schema ---

    #[test]
    fn full_generation_with_realistic_schemas() {
        let registry = SchemaRegistry::canonical();
        let send_schema = serde_json::json!({
            "title": "WA Robot Send Response",
            "description": "Confirms text was sent",
            "type": "object",
            "required": ["pane_id", "sent", "policy_decision"],
            "properties": {
                "pane_id": { "type": "integer", "minimum": 0, "description": "Target pane" },
                "sent": { "type": "boolean", "description": "Whether text was sent" },
                "policy_decision": {
                    "type": "string",
                    "enum": ["allow", "deny", "require_approval"],
                    "description": "Policy decision"
                },
                "wait_for_result": {
                    "$ref": "#/$defs/wait_result",
                    "description": "Wait result if requested"
                }
            },
            "$defs": {
                "wait_result": {
                    "type": "object",
                    "description": "Wait-for result",
                    "required": ["condition", "matched"],
                    "properties": {
                        "condition": { "type": "string", "description": "Condition" },
                        "matched": { "type": "boolean", "description": "Matched?" }
                    }
                }
            }
        });

        let schemas = vec![("wa-robot-send.json".to_string(), send_schema)];
        let config = DocGenConfig::default();
        let pages = generate_reference(&registry, &schemas, &config);

        let content = &pages[0].content;

        // Endpoint section present
        assert!(content.contains("### Send Text"));
        assert!(content.contains("**Robot:** `wa robot send`"));
        assert!(content.contains("**MCP:** `wa.send`"));

        // Properties table
        assert!(content.contains("| `pane_id`"));
        assert!(content.contains("| `policy_decision`"));
        assert!(content.contains("`\"allow\"`"));

        // Sub-definition
        assert!(content.contains("#### `wait_result`"));
        assert!(content.contains("| `condition`"));
    }
}
