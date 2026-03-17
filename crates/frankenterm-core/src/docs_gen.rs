//! Schema-driven API reference documentation generator.
//!
//! Consumes JSON Schema files from `docs/json-schema/` and
//! [`SchemaRegistry`] metadata to produce
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
//! use frankenterm_core::api_schema::SchemaRegistry;
//! use frankenterm_core::docs_gen::{DocGenConfig, generate_reference};
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
        title: "ft API Reference".to_string(),
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
        "robot.feature_not_available" => {
            "Feature is unavailable in the current build or runtime configuration"
        }
        "robot.wezterm_error" => {
            "Error communicating with terminal backend bridge (current: WezTerm)"
        }
        "robot.wezterm_not_found" => {
            "Active terminal backend CLI is not installed or not available in PATH"
        }
        "robot.wezterm_not_running" => {
            "Active terminal backend bridge is not running or not reachable"
        }
        "robot.wezterm_socket_not_found" => "Terminal backend socket is missing or was not created",
        "robot.wezterm_command_failed" => {
            "Terminal backend command failed before returning usable output"
        }
        "robot.wezterm_parse_error" => {
            "Terminal backend response could not be parsed into the expected format"
        }
        "robot.circuit_open" => {
            "Terminal backend circuit breaker is open after repeated recent failures"
        }
        "robot.storage_error" => "Database or storage layer error",
        "robot.fts_query_error" => "Full-text search query was invalid or could not be executed",
        "robot.policy_denied" => "Action denied by safety policy",
        "robot.require_approval" => {
            "Action requires an allow-once approval token before it can proceed"
        }
        "robot.approval_error" => "Approval token issuance or approval workflow persistence failed",
        "robot.rate_limited" => "External service or policy rate limit was reached",
        "robot.pane_not_found" => "Specified pane does not exist",
        "robot.reservation_conflict" => "Pane is already reserved by another owner",
        "robot.workflow_error" => "Workflow execution failed",
        "robot.internal_error" => "Unexpected internal error while handling the robot command",
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
        writeln!(out, "**Robot:** `ft {cmd}`").unwrap();
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
            .map(|c| format!("`ft {c}`"))
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
                        "robot.unknown_subcommand",
                        "robot.not_implemented",
                        "robot.config_error",
                        "robot.feature_not_available",
                        "robot.wezterm_error",
                        "robot.wezterm_not_found",
                        "robot.wezterm_not_running",
                        "robot.wezterm_socket_not_found",
                        "robot.wezterm_command_failed",
                        "robot.wezterm_parse_error",
                        "robot.circuit_open",
                        "robot.storage_error",
                        "robot.fts_query_error",
                        "robot.policy_denied",
                        "robot.require_approval",
                        "robot.approval_error",
                        "robot.rate_limited",
                        "robot.pane_not_found",
                        "robot.reservation_conflict",
                        "robot.workflow_error",
                        "robot.internal_error",
                        "robot.timeout"
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
            "robot.feature_not_available",
            "robot.wezterm_error",
            "robot.wezterm_not_found",
            "robot.wezterm_not_running",
            "robot.wezterm_socket_not_found",
            "robot.wezterm_command_failed",
            "robot.wezterm_parse_error",
            "robot.circuit_open",
            "robot.storage_error",
            "robot.fts_query_error",
            "robot.policy_denied",
            "robot.require_approval",
            "robot.approval_error",
            "robot.rate_limited",
            "robot.pane_not_found",
            "robot.reservation_conflict",
            "robot.workflow_error",
            "robot.internal_error",
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
        assert!(content.contains("**Robot:** `ft robot send`"));
        assert!(content.contains("**MCP:** `wa.send`"));

        // Properties table
        assert!(content.contains("| `pane_id`"));
        assert!(content.contains("| `policy_decision`"));
        assert!(content.contains("`\"allow\"`"));

        // Sub-definition
        assert!(content.contains("#### `wait_result`"));
        assert!(content.contains("| `condition`"));
    }

    // =====================================================================
    // NEW TESTS below (wa-1u90p.7.1 expansion)
    // =====================================================================

    // --- DocGenConfig serde edge cases ---

    #[test]
    fn config_serde_all_false() {
        let config = DocGenConfig {
            include_envelope: false,
            include_experimental: false,
            include_error_codes: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: DocGenConfig = serde_json::from_str(&json).unwrap();
        assert!(!parsed.include_envelope);
        assert!(!parsed.include_experimental);
        assert!(!parsed.include_error_codes);
    }

    #[test]
    fn config_serde_roundtrip_preserves_all_fields() {
        let config = DocGenConfig {
            include_envelope: false,
            include_experimental: true,
            include_error_codes: false,
        };
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: DocGenConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.include_envelope, parsed.include_envelope);
        assert_eq!(config.include_experimental, parsed.include_experimental);
        assert_eq!(config.include_error_codes, parsed.include_error_codes);
    }

    #[test]
    fn config_debug_impl() {
        let config = DocGenConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("DocGenConfig"));
        assert!(dbg.contains("include_envelope"));
        assert!(dbg.contains("include_experimental"));
        assert!(dbg.contains("include_error_codes"));
    }

    #[test]
    fn config_clone_is_independent() {
        let config = DocGenConfig::default();
        let mut cloned = config.clone();
        cloned.include_envelope = false;
        // Original unchanged
        assert!(config.include_envelope);
        assert!(!cloned.include_envelope);
    }

    #[test]
    fn config_deserialize_from_partial_json() {
        // serde should fail if fields are missing (no #[serde(default)])
        let result: Result<DocGenConfig, _> = serde_json::from_str("{}");
        assert!(result.is_err(), "should require all fields");
    }

    // --- PropertyDoc serde and traits ---

    #[test]
    fn property_doc_serde_roundtrip() {
        let prop = PropertyDoc {
            name: "test_field".into(),
            type_str: "string".into(),
            required: true,
            description: "A test field".into(),
            enum_values: vec!["a".into(), "b".into()],
            minimum: Some(1.0),
            maximum: Some(100.0),
            pattern: Some("^[a-z]+$".into()),
        };
        let json = serde_json::to_string(&prop).unwrap();
        let parsed: PropertyDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test_field");
        assert_eq!(parsed.type_str, "string");
        assert!(parsed.required);
        assert_eq!(parsed.enum_values.len(), 2);
        assert_eq!(parsed.minimum, Some(1.0));
        assert_eq!(parsed.maximum, Some(100.0));
        assert_eq!(parsed.pattern.as_deref(), Some("^[a-z]+$"));
    }

    #[test]
    fn property_doc_serde_roundtrip_none_fields() {
        let prop = PropertyDoc {
            name: "x".into(),
            type_str: "integer".into(),
            required: false,
            description: String::new(),
            enum_values: vec![],
            minimum: None,
            maximum: None,
            pattern: None,
        };
        let json = serde_json::to_string(&prop).unwrap();
        let parsed: PropertyDoc = serde_json::from_str(&json).unwrap();
        assert!(parsed.minimum.is_none());
        assert!(parsed.maximum.is_none());
        assert!(parsed.pattern.is_none());
        assert!(parsed.enum_values.is_empty());
    }

    #[test]
    fn property_doc_debug_impl() {
        let prop = PropertyDoc {
            name: "dbg_test".into(),
            type_str: "boolean".into(),
            required: false,
            description: "debug test".into(),
            enum_values: vec![],
            minimum: None,
            maximum: None,
            pattern: None,
        };
        let dbg = format!("{:?}", prop);
        assert!(dbg.contains("PropertyDoc"));
        assert!(dbg.contains("dbg_test"));
    }

    #[test]
    fn property_doc_clone_is_deep() {
        let prop = PropertyDoc {
            name: "orig".into(),
            type_str: "string".into(),
            required: true,
            description: "original".into(),
            enum_values: vec!["x".into()],
            minimum: Some(0.0),
            maximum: Some(10.0),
            pattern: Some("pat".into()),
        };
        let mut cloned = prop.clone();
        cloned.name = "cloned".into();
        cloned.enum_values.push("y".into());
        assert_eq!(prop.name, "orig");
        assert_eq!(prop.enum_values.len(), 1);
        assert_eq!(cloned.enum_values.len(), 2);
    }

    // --- SchemaDoc serde and traits ---

    #[test]
    fn schema_doc_serde_roundtrip() {
        let doc = SchemaDoc {
            title: "Test".into(),
            description: "A test".into(),
            properties: vec![PropertyDoc {
                name: "id".into(),
                type_str: "integer".into(),
                required: true,
                description: "ID".into(),
                enum_values: vec![],
                minimum: None,
                maximum: None,
                pattern: None,
            }],
            definitions: vec![(
                "sub".into(),
                SchemaDoc {
                    title: "Sub".into(),
                    description: "sub desc".into(),
                    properties: vec![],
                    definitions: vec![],
                },
            )],
        };
        let json = serde_json::to_string(&doc).unwrap();
        let parsed: SchemaDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, "Test");
        assert_eq!(parsed.properties.len(), 1);
        assert_eq!(parsed.definitions.len(), 1);
        assert_eq!(parsed.definitions[0].0, "sub");
    }

    #[test]
    fn schema_doc_debug_impl() {
        let doc = parse_schema(&serde_json::json!({"title": "DebugTest"}));
        let dbg = format!("{:?}", doc);
        assert!(dbg.contains("SchemaDoc"));
        assert!(dbg.contains("DebugTest"));
    }

    #[test]
    fn schema_doc_clone_deep() {
        let doc = parse_schema(&sample_schema());
        let cloned = doc.clone();
        assert_eq!(doc.title, cloned.title);
        assert_eq!(doc.properties.len(), cloned.properties.len());
        assert_eq!(doc.definitions.len(), cloned.definitions.len());
    }

    // --- EndpointCategory exhaustive tests ---

    #[test]
    fn category_title_all_nonempty() {
        for cat in EndpointCategory::all() {
            let t = cat.title();
            assert!(!t.is_empty(), "category {:?} has empty title", cat);
        }
    }

    #[test]
    fn category_all_returns_seven() {
        assert_eq!(EndpointCategory::all().len(), 7);
    }

    #[test]
    fn category_serde_roundtrip() {
        for cat in EndpointCategory::all() {
            let json = serde_json::to_string(cat).unwrap();
            let parsed: EndpointCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(*cat, parsed);
        }
    }

    #[test]
    fn category_serde_snake_case() {
        let json = serde_json::to_string(&EndpointCategory::PaneOperations).unwrap();
        assert_eq!(json, "\"pane_operations\"");
        let json = serde_json::to_string(&EndpointCategory::SearchAndEvents).unwrap();
        assert_eq!(json, "\"search_and_events\"");
        let json = serde_json::to_string(&EndpointCategory::Meta).unwrap();
        assert_eq!(json, "\"meta\"");
    }

    #[test]
    fn category_debug_impl() {
        let dbg = format!("{:?}", EndpointCategory::Workflows);
        assert_eq!(dbg, "Workflows");
    }

    #[test]
    fn category_clone_copy() {
        let cat = EndpointCategory::Rules;
        let cloned = cat;
        let copied = cat;
        assert_eq!(cat, cloned);
        assert_eq!(cat, copied);
    }

    #[test]
    fn category_eq_and_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        for cat in EndpointCategory::all() {
            set.insert(*cat);
        }
        assert_eq!(set.len(), 7);
        // Inserting duplicates does not change length.
        set.insert(EndpointCategory::Meta);
        assert_eq!(set.len(), 7);
    }

    #[test]
    fn category_ord() {
        // PartialOrd/Ord should be deterministic
        assert!(EndpointCategory::PaneOperations < EndpointCategory::Meta);
        assert!(EndpointCategory::SearchAndEvents < EndpointCategory::Workflows);
    }

    // --- Categorization: exhaustive endpoint ID coverage ---

    #[test]
    fn categorize_get_text() {
        let ep = EndpointMeta {
            id: "get_text".into(),
            title: "Get Pane Text".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::PaneOperations);
    }

    #[test]
    fn categorize_send() {
        let ep = EndpointMeta {
            id: "send".into(),
            title: "Send Text".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::PaneOperations);
    }

    #[test]
    fn categorize_wait_for() {
        let ep = EndpointMeta {
            id: "wait_for".into(),
            title: "Wait For".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::PaneOperations);
    }

    #[test]
    fn categorize_search() {
        let ep = EndpointMeta {
            id: "search".into(),
            title: "Search".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::SearchAndEvents);
    }

    #[test]
    fn categorize_events() {
        let ep = EndpointMeta {
            id: "events".into(),
            title: "Events".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::SearchAndEvents);
    }

    #[test]
    fn categorize_events_annotate() {
        let ep = EndpointMeta {
            id: "events_annotate".into(),
            title: "Annotate Event".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::SearchAndEvents);
    }

    #[test]
    fn categorize_events_triage() {
        let ep = EndpointMeta {
            id: "events_triage".into(),
            title: "Triage".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::SearchAndEvents);
    }

    #[test]
    fn categorize_events_label() {
        let ep = EndpointMeta {
            id: "events_label".into(),
            title: "Label".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::SearchAndEvents);
    }

    #[test]
    fn categorize_workflow_list() {
        let ep = EndpointMeta {
            id: "workflow_list".into(),
            title: "List Workflows".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Workflows);
    }

    #[test]
    fn categorize_workflow_status() {
        let ep = EndpointMeta {
            id: "workflow_status".into(),
            title: "Workflow Status".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Workflows);
    }

    #[test]
    fn categorize_workflow_abort() {
        let ep = EndpointMeta {
            id: "workflow_abort".into(),
            title: "Abort Workflow".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Workflows);
    }

    #[test]
    fn categorize_rules_list() {
        let ep = EndpointMeta {
            id: "rules_list".into(),
            title: "List Rules".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Rules);
    }

    #[test]
    fn categorize_rules_test() {
        let ep = EndpointMeta {
            id: "rules_test".into(),
            title: "Test Rules".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Rules);
    }

    #[test]
    fn categorize_rules_show() {
        let ep = EndpointMeta {
            id: "rules_show".into(),
            title: "Show Rule".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: false,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Rules);
    }

    #[test]
    fn categorize_rules_lint() {
        let ep = EndpointMeta {
            id: "rules_lint".into(),
            title: "Lint Rules".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Rules);
    }

    #[test]
    fn categorize_accounts_list() {
        let ep = EndpointMeta {
            id: "accounts_list".into(),
            title: "List Accounts".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Accounts);
    }

    #[test]
    fn categorize_accounts_refresh() {
        let ep = EndpointMeta {
            id: "accounts_refresh".into(),
            title: "Refresh Accounts".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Accounts);
    }

    #[test]
    fn categorize_reservations_list() {
        let ep = EndpointMeta {
            id: "reservations_list".into(),
            title: "List Reservations".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Reservations);
    }

    #[test]
    fn categorize_reserve() {
        let ep = EndpointMeta {
            id: "reserve".into(),
            title: "Reserve Pane".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Reservations);
    }

    #[test]
    fn categorize_release() {
        let ep = EndpointMeta {
            id: "release".into(),
            title: "Release Reservation".into(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: "0.1.0".into(),
        };
        assert_eq!(categorize_endpoint(&ep), EndpointCategory::Reservations);
    }

    // --- extract_type_str edge cases ---

    #[test]
    fn extract_type_str_no_type_returns_any() {
        let prop = serde_json::json!({
            "description": "no type"
        });
        assert_eq!(extract_type_str(&prop), "any");
    }

    #[test]
    fn extract_type_str_object() {
        let prop = serde_json::json!({
            "type": "object"
        });
        assert_eq!(extract_type_str(&prop), "object");
    }

    #[test]
    fn extract_type_str_boolean() {
        let prop = serde_json::json!({ "type": "boolean" });
        assert_eq!(extract_type_str(&prop), "boolean");
    }

    #[test]
    fn extract_type_str_number() {
        let prop = serde_json::json!({ "type": "number" });
        assert_eq!(extract_type_str(&prop), "number");
    }

    #[test]
    fn extract_type_str_array_no_items() {
        let prop = serde_json::json!({ "type": "array" });
        assert_eq!(extract_type_str(&prop), "array");
    }

    #[test]
    fn extract_type_str_array_with_typed_items() {
        let prop = serde_json::json!({
            "type": "array",
            "items": { "type": "integer" }
        });
        assert_eq!(extract_type_str(&prop), "integer[]");
    }

    #[test]
    fn extract_type_str_array_with_ref_items() {
        let prop = serde_json::json!({
            "type": "array",
            "items": { "$ref": "#/$defs/my_type" }
        });
        assert_eq!(extract_type_str(&prop), "my_type[]");
    }

    #[test]
    fn extract_type_str_ref_without_defs_prefix() {
        let prop = serde_json::json!({
            "$ref": "https://example.com/schema.json"
        });
        assert_eq!(extract_type_str(&prop), "https://example.com/schema.json");
    }

    #[test]
    fn extract_type_str_type_array_three_types() {
        let prop = serde_json::json!({
            "type": ["string", "integer", "null"]
        });
        assert_eq!(extract_type_str(&prop), "string | integer | null");
    }

    #[test]
    fn extract_type_str_array_items_no_type_no_ref() {
        // items present but with neither type nor $ref
        let prop = serde_json::json!({
            "type": "array",
            "items": { "description": "anything" }
        });
        assert_eq!(extract_type_str(&prop), "array");
    }

    // --- parse_schema edge cases ---

    #[test]
    fn parse_schema_null_value() {
        let schema = Value::Null;
        let doc = parse_schema(&schema);
        assert!(doc.title.is_empty());
        assert!(doc.description.is_empty());
        assert!(doc.properties.is_empty());
        assert!(doc.definitions.is_empty());
    }

    #[test]
    fn parse_schema_string_value() {
        let schema = Value::String("not a schema".into());
        let doc = parse_schema(&schema);
        assert!(doc.title.is_empty());
        assert!(doc.properties.is_empty());
    }

    #[test]
    fn parse_schema_number_value() {
        let schema = serde_json::json!(42);
        let doc = parse_schema(&schema);
        assert!(doc.title.is_empty());
        assert!(doc.properties.is_empty());
    }

    #[test]
    fn parse_schema_no_required_array() {
        // Schema with properties but no "required" key
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "integer" }
            }
        });
        let doc = parse_schema(&schema);
        assert_eq!(doc.properties.len(), 2);
        assert!(doc.properties.iter().all(|p| !p.required));
    }

    #[test]
    fn parse_schema_empty_properties_object() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {}
        });
        let doc = parse_schema(&schema);
        assert!(doc.properties.is_empty());
    }

    #[test]
    fn parse_schema_multiple_definitions_sorted() {
        let schema = serde_json::json!({
            "$defs": {
                "zebra": { "type": "object", "properties": { "z": { "type": "string" } } },
                "alpha": { "type": "object", "properties": { "a": { "type": "string" } } },
                "mid": { "type": "object", "properties": { "m": { "type": "string" } } }
            }
        });
        let doc = parse_schema(&schema);
        assert_eq!(doc.definitions.len(), 3);
        assert_eq!(doc.definitions[0].0, "alpha");
        assert_eq!(doc.definitions[1].0, "mid");
        assert_eq!(doc.definitions[2].0, "zebra");
    }

    #[test]
    fn parse_schema_property_with_maximum() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "val": {
                    "type": "number",
                    "minimum": -100.5,
                    "maximum": 100.5
                }
            }
        });
        let doc = parse_schema(&schema);
        let val = &doc.properties[0];
        assert_eq!(val.minimum, Some(-100.5));
        assert_eq!(val.maximum, Some(100.5));
    }

    #[test]
    fn parse_schema_property_with_pattern() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "pattern": "^[A-Z]{3}$"
                }
            }
        });
        let doc = parse_schema(&schema);
        let code = &doc.properties[0];
        assert_eq!(code.pattern.as_deref(), Some("^[A-Z]{3}$"));
    }

    #[test]
    fn parse_schema_enum_with_non_string_values() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "level": {
                    "type": "integer",
                    "enum": [1, 2, 3]
                }
            }
        });
        let doc = parse_schema(&schema);
        let level = &doc.properties[0];
        assert_eq!(level.enum_values, vec!["1", "2", "3"]);
    }

    #[test]
    fn parse_schema_property_no_description() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "bare": { "type": "string" }
            }
        });
        let doc = parse_schema(&schema);
        assert_eq!(doc.properties[0].description, "");
    }

    // --- slug edge cases ---

    #[test]
    fn slug_empty_string() {
        assert_eq!(slug(""), "");
    }

    #[test]
    fn slug_all_special_chars() {
        assert_eq!(slug("!@#$%^&*()"), "");
    }

    #[test]
    fn slug_leading_trailing_spaces() {
        assert_eq!(slug("  hello  "), "hello");
    }

    #[test]
    fn slug_preserves_numbers() {
        assert_eq!(slug("Version 2.0"), "version-2-0");
    }

    #[test]
    fn slug_unicode_chars() {
        // Non-ASCII alphanumeric are kept by is_alphanumeric
        let result = slug("cafe");
        assert_eq!(result, "cafe");
    }

    #[test]
    fn slug_consecutive_special_chars() {
        // Non-alphanumeric chars become '-', then replace("--", "-") runs once.
        // Input "a--b": '-' is not alphanumeric, so mapped to '-', giving "a--b",
        // then replace("--","-") -> "a-b"
        assert_eq!(slug("a--b"), "a-b");
        // Two spaces -> "a--b" -> "a-b"
        assert_eq!(slug("a  b"), "a-b");
        // Three spaces -> "a---b" -> replace("--","-") once -> "a--b"
        // (only one pass of replacement)
        assert_eq!(slug("a   b"), "a--b");
    }

    // --- escape_markdown_table edge cases ---

    #[test]
    fn escape_markdown_table_empty() {
        assert_eq!(escape_markdown_table(""), "");
    }

    #[test]
    fn escape_markdown_table_multiple_pipes() {
        assert_eq!(escape_markdown_table("a|b|c"), "a\\|b\\|c");
    }

    #[test]
    fn escape_markdown_table_multiple_newlines() {
        assert_eq!(escape_markdown_table("a\nb\nc"), "a b c");
    }

    #[test]
    fn escape_markdown_table_mixed_pipe_and_newline() {
        assert_eq!(escape_markdown_table("a|b\nc"), "a\\|b c");
    }

    #[test]
    fn escape_markdown_table_no_special_chars() {
        assert_eq!(escape_markdown_table("hello world"), "hello world");
    }

    // --- format_type_with_constraints edge cases ---

    #[test]
    fn format_type_with_single_enum_value() {
        let prop = PropertyDoc {
            name: "x".into(),
            type_str: "string".into(),
            required: false,
            description: String::new(),
            enum_values: vec!["only".into()],
            minimum: None,
            maximum: None,
            pattern: None,
        };
        let formatted = format_type_with_constraints(&prop);
        assert_eq!(formatted, "`string` (`\"only\"`)");
    }

    #[test]
    fn format_type_with_empty_type_str() {
        let prop = PropertyDoc {
            name: "x".into(),
            type_str: String::new(),
            required: false,
            description: String::new(),
            enum_values: vec![],
            minimum: None,
            maximum: None,
            pattern: None,
        };
        assert_eq!(format_type_with_constraints(&prop), "``");
    }

    #[test]
    fn format_type_with_many_enum_values() {
        let prop = PropertyDoc {
            name: "x".into(),
            type_str: "string".into(),
            required: false,
            description: String::new(),
            enum_values: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
            minimum: None,
            maximum: None,
            pattern: None,
        };
        let formatted = format_type_with_constraints(&prop);
        assert!(formatted.contains("`\"a\"`"));
        assert!(formatted.contains("`\"e\"`"));
        // Check comma separation
        assert!(formatted.contains(", "));
    }

    // --- error_code_description edge cases ---

    #[test]
    fn error_code_unknown_returns_unknown() {
        assert_eq!(
            error_code_description("robot.nonexistent"),
            "Unknown error code"
        );
    }

    #[test]
    fn error_code_empty_returns_unknown() {
        assert_eq!(error_code_description(""), "Unknown error code");
    }

    #[test]
    fn error_code_each_has_unique_description() {
        let codes = [
            "robot.invalid_args",
            "robot.unknown_subcommand",
            "robot.not_implemented",
            "robot.config_error",
            "robot.feature_not_available",
            "robot.wezterm_error",
            "robot.wezterm_not_found",
            "robot.wezterm_not_running",
            "robot.wezterm_socket_not_found",
            "robot.wezterm_command_failed",
            "robot.wezterm_parse_error",
            "robot.circuit_open",
            "robot.storage_error",
            "robot.fts_query_error",
            "robot.policy_denied",
            "robot.require_approval",
            "robot.approval_error",
            "robot.rate_limited",
            "robot.pane_not_found",
            "robot.reservation_conflict",
            "robot.workflow_error",
            "robot.internal_error",
            "robot.timeout",
        ];
        let mut descs: Vec<&str> = codes.iter().map(|c| error_code_description(c)).collect();
        let original_len = descs.len();
        descs.sort();
        descs.dedup();
        assert_eq!(
            descs.len(),
            original_len,
            "all error codes should have unique descriptions"
        );
    }

    // --- DocPage traits ---

    #[test]
    fn doc_page_debug_impl() {
        let page = DocPage {
            filename: "test.md".into(),
            title: "Test".into(),
            content: "# Test".into(),
        };
        let dbg = format!("{:?}", page);
        assert!(dbg.contains("DocPage"));
        assert!(dbg.contains("test.md"));
    }

    #[test]
    fn doc_page_clone() {
        let page = DocPage {
            filename: "a.md".into(),
            title: "A".into(),
            content: "content".into(),
        };
        let cloned = page.clone();
        assert_eq!(page.filename, cloned.filename);
        assert_eq!(page.title, cloned.title);
        assert_eq!(page.content, cloned.content);
    }

    // --- generate_reference config combinations ---

    #[test]
    fn generate_reference_no_envelope_no_errors() {
        let registry = SchemaRegistry::canonical();
        let envelope = sample_envelope();
        let schemas = vec![("wa-robot-envelope.json".to_string(), envelope)];
        let config = DocGenConfig {
            include_envelope: false,
            include_experimental: true,
            include_error_codes: false,
        };
        let pages = generate_reference(&registry, &schemas, &config);
        assert!(!pages[0].content.contains("## Response Envelope"));
        assert!(!pages[0].content.contains("## Error Codes"));
    }

    #[test]
    fn generate_reference_envelope_without_error_codes() {
        let registry = SchemaRegistry::canonical();
        let envelope = sample_envelope();
        let schemas = vec![("wa-robot-envelope.json".to_string(), envelope)];
        let config = DocGenConfig {
            include_envelope: true,
            include_experimental: true,
            include_error_codes: false,
        };
        let pages = generate_reference(&registry, &schemas, &config);
        assert!(pages[0].content.contains("## Response Envelope"));
        assert!(!pages[0].content.contains("## Error Codes"));
    }

    #[test]
    fn generate_reference_error_codes_without_envelope() {
        let registry = SchemaRegistry::canonical();
        let envelope = sample_envelope();
        let schemas = vec![("wa-robot-envelope.json".to_string(), envelope)];
        let config = DocGenConfig {
            include_envelope: false,
            include_experimental: true,
            include_error_codes: true,
        };
        let pages = generate_reference(&registry, &schemas, &config);
        assert!(!pages[0].content.contains("## Response Envelope"));
        // Error codes section should still render since envelope schema is provided
        assert!(pages[0].content.contains("## Error Codes"));
    }

    #[test]
    fn generate_reference_toc_excludes_envelope_when_disabled() {
        let registry = SchemaRegistry::canonical();
        let config = DocGenConfig {
            include_envelope: false,
            include_experimental: true,
            include_error_codes: false,
        };
        let pages = generate_reference(&registry, &[], &config);
        assert!(
            !pages[0]
                .content
                .contains("Response Envelope](#response-envelope)")
        );
        assert!(!pages[0].content.contains("Error Codes](#error-codes)"));
    }

    #[test]
    fn generate_reference_toc_includes_envelope_when_enabled() {
        let registry = SchemaRegistry::canonical();
        let config = DocGenConfig::default();
        let pages = generate_reference(&registry, &[], &config);
        assert!(
            pages[0]
                .content
                .contains("[Response Envelope](#response-envelope)")
        );
        assert!(pages[0].content.contains("[Error Codes](#error-codes)"));
    }

    #[test]
    fn generate_reference_includes_version() {
        let registry = SchemaRegistry::canonical();
        let config = DocGenConfig::default();
        let pages = generate_reference(&registry, &[], &config);
        assert!(
            pages[0]
                .content
                .contains(&format!("Version: {}.", registry.version)),
            "output should contain the registry version"
        );
    }

    // --- write_endpoint_section: unstable marker ---

    #[test]
    fn endpoint_section_unstable_marker() {
        let ep = EndpointMeta {
            id: "experimental_thing".into(),
            title: "Experimental Thing".into(),
            description: "An unstable endpoint".into(),
            robot_command: Some("robot exp".into()),
            mcp_tool: None,
            schema_file: "wa-robot-exp.json".into(),
            stable: false,
            since: "0.3.0".into(),
        };
        let schemas: BTreeMap<&str, &Value> = BTreeMap::new();
        let mut out = String::new();
        write_endpoint_section(&mut out, &ep, &schemas);
        assert!(out.contains("> **Experimental**"));
        assert!(out.contains("**Since:** v0.3.0"));
    }

    #[test]
    fn endpoint_section_stable_no_experimental_marker() {
        let ep = EndpointMeta {
            id: "stable_thing".into(),
            title: "Stable Thing".into(),
            description: "A stable endpoint".into(),
            robot_command: Some("robot stable".into()),
            mcp_tool: Some("wa.stable".into()),
            schema_file: "wa-robot-stable.json".into(),
            stable: true,
            since: "0.1.0".into(),
        };
        let schemas: BTreeMap<&str, &Value> = BTreeMap::new();
        let mut out = String::new();
        write_endpoint_section(&mut out, &ep, &schemas);
        assert!(!out.contains("Experimental"));
        assert!(out.contains("**Robot:** `ft robot stable`"));
        assert!(out.contains("**MCP:** `wa.stable`"));
    }

    #[test]
    fn endpoint_section_no_robot_no_mcp() {
        let ep = EndpointMeta {
            id: "bare".into(),
            title: "Bare".into(),
            description: "No surfaces".into(),
            robot_command: None,
            mcp_tool: None,
            schema_file: "bare.json".into(),
            stable: true,
            since: "0.1.0".into(),
        };
        let schemas: BTreeMap<&str, &Value> = BTreeMap::new();
        let mut out = String::new();
        write_endpoint_section(&mut out, &ep, &schemas);
        assert!(!out.contains("**Robot:**"));
        assert!(!out.contains("**MCP:**"));
    }

    // --- write_properties_table ---

    #[test]
    fn properties_table_empty_properties() {
        let mut out = String::new();
        write_properties_table(&mut out, &[]);
        // Should still have headers
        assert!(out.contains("| Field | Type | Required | Description |"));
        assert!(out.contains("|-------|------|----------|-------------|"));
        // But no data rows (just the 2 header lines)
        assert_eq!(out.trim().lines().count(), 2);
    }

    #[test]
    fn properties_table_required_yes_marker() {
        let props = vec![PropertyDoc {
            name: "req_field".into(),
            type_str: "string".into(),
            required: true,
            description: "required".into(),
            enum_values: vec![],
            minimum: None,
            maximum: None,
            pattern: None,
        }];
        let mut out = String::new();
        write_properties_table(&mut out, &props);
        assert!(out.contains("**yes**"));
    }

    #[test]
    fn properties_table_optional_no_marker() {
        let props = vec![PropertyDoc {
            name: "opt_field".into(),
            type_str: "string".into(),
            required: false,
            description: "optional".into(),
            enum_values: vec![],
            minimum: None,
            maximum: None,
            pattern: None,
        }];
        let mut out = String::new();
        write_properties_table(&mut out, &props);
        // Should contain "no" but not "**yes**"
        assert!(out.contains("| no |"));
        assert!(!out.contains("**yes**"));
    }

    #[test]
    fn properties_table_escapes_description() {
        let props = vec![PropertyDoc {
            name: "tricky".into(),
            type_str: "string".into(),
            required: false,
            description: "has|pipe\nand newline".into(),
            enum_values: vec![],
            minimum: None,
            maximum: None,
            pattern: None,
        }];
        let mut out = String::new();
        write_properties_table(&mut out, &props);
        assert!(out.contains("has\\|pipe and newline"));
    }

    // --- write_error_codes_section edge cases ---

    #[test]
    fn error_codes_section_no_defs() {
        // Envelope without $defs should produce no error codes section
        let envelope = serde_json::json!({
            "type": "object",
            "properties": {
                "ok": { "type": "boolean" }
            }
        });
        let mut out = String::new();
        write_error_codes_section(&mut out, &envelope);
        assert!(!out.contains("Error Codes"));
    }

    #[test]
    fn error_codes_section_empty_enum() {
        let envelope = serde_json::json!({
            "$defs": {
                "error_codes": {
                    "enum": []
                }
            }
        });
        let mut out = String::new();
        write_error_codes_section(&mut out, &envelope);
        // Should still render header and table headers
        assert!(out.contains("## Error Codes"));
        assert!(out.contains("| Code | Description |"));
    }

    #[test]
    fn error_codes_section_non_string_enum_values() {
        let envelope = serde_json::json!({
            "$defs": {
                "error_codes": {
                    "enum": [42, true, null]
                }
            }
        });
        let mut out = String::new();
        write_error_codes_section(&mut out, &envelope);
        // Non-string values should be skipped (as_str returns None for them)
        assert!(out.contains("## Error Codes"));
        // No code rows since none are strings
        assert!(!out.lines().any(|l| l.starts_with("| `")));
    }

    // --- Summary table edge cases ---

    #[test]
    fn endpoint_summary_empty_registry() {
        let registry = SchemaRegistry {
            version: "0.0.0".into(),
            endpoints: vec![],
        };
        let summary = generate_endpoint_summary(&registry);
        // Should have only header rows
        assert!(summary.contains("| Endpoint |"));
        assert_eq!(summary.trim().lines().count(), 2);
    }

    #[test]
    fn endpoint_summary_no_robot_no_mcp_shows_dashes() {
        let registry = SchemaRegistry {
            version: "1.0.0".into(),
            endpoints: vec![EndpointMeta {
                id: "bare".into(),
                title: "Bare Endpoint".into(),
                description: String::new(),
                robot_command: None,
                mcp_tool: None,
                schema_file: "bare.json".into(),
                stable: true,
                since: "1.0.0".into(),
            }],
        };
        let summary = generate_endpoint_summary(&registry);
        // Should contain em-dash for missing robot and MCP
        let lines: Vec<&str> = summary.trim().lines().collect();
        assert_eq!(lines.len(), 3); // header + separator + 1 data row
        // Check the data row contains dashes
        let data_line = lines[2];
        assert!(data_line.contains("Bare Endpoint"));
    }

    // --- group_endpoints ---

    #[test]
    fn group_endpoints_filters_unstable_when_not_included() {
        let registry = SchemaRegistry {
            version: "1.0.0".into(),
            endpoints: vec![
                EndpointMeta {
                    id: "state".into(),
                    title: "Pane State".into(),
                    description: String::new(),
                    robot_command: None,
                    mcp_tool: None,
                    schema_file: String::new(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "state".into(),
                    title: "Unstable State".into(),
                    description: String::new(),
                    robot_command: None,
                    mcp_tool: None,
                    schema_file: String::new(),
                    stable: false,
                    since: "0.1.0".into(),
                },
            ],
        };
        let config = DocGenConfig {
            include_experimental: false,
            ..Default::default()
        };
        let grouped = group_endpoints(&registry, &config);
        let pane_ops = grouped.get(&EndpointCategory::PaneOperations).unwrap();
        assert_eq!(pane_ops.len(), 1);
        assert_eq!(pane_ops[0].title, "Pane State");
    }

    #[test]
    fn group_endpoints_includes_unstable_when_enabled() {
        let registry = SchemaRegistry {
            version: "1.0.0".into(),
            endpoints: vec![
                EndpointMeta {
                    id: "state".into(),
                    title: "Pane State".into(),
                    description: String::new(),
                    robot_command: None,
                    mcp_tool: None,
                    schema_file: String::new(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "state".into(),
                    title: "Unstable State".into(),
                    description: String::new(),
                    robot_command: None,
                    mcp_tool: None,
                    schema_file: String::new(),
                    stable: false,
                    since: "0.1.0".into(),
                },
            ],
        };
        let config = DocGenConfig {
            include_experimental: true,
            ..Default::default()
        };
        let grouped = group_endpoints(&registry, &config);
        let pane_ops = grouped.get(&EndpointCategory::PaneOperations).unwrap();
        assert_eq!(pane_ops.len(), 2);
    }

    #[test]
    fn group_endpoints_empty_registry() {
        let registry = SchemaRegistry {
            version: "0.0.0".into(),
            endpoints: vec![],
        };
        let config = DocGenConfig::default();
        let grouped = group_endpoints(&registry, &config);
        assert!(grouped.is_empty());
    }

    // --- write_envelope_section ---

    #[test]
    fn envelope_section_contains_structure_description() {
        let envelope = sample_envelope();
        let mut out = String::new();
        write_envelope_section(&mut out, &envelope);
        assert!(out.contains("## Response Envelope"));
        assert!(out.contains("Every robot command returns a JSON envelope"));
        assert!(out.contains("When `ok` is `true`"));
        assert!(out.contains("When `ok` is `false`"));
    }

    // --- write_toc ---

    #[test]
    fn toc_empty_registry_only_envelope_and_errors() {
        let registry = SchemaRegistry {
            version: "0.0.0".into(),
            endpoints: vec![],
        };
        let config = DocGenConfig::default();
        let mut out = String::new();
        write_toc(&mut out, &registry, &config);
        assert!(out.contains("## Table of Contents"));
        assert!(out.contains("Response Envelope"));
        assert!(out.contains("Error Codes"));
        // No category links since no endpoints
        assert!(!out.contains("Pane Operations"));
    }

    #[test]
    fn toc_without_envelope_or_errors() {
        let registry = SchemaRegistry {
            version: "0.0.0".into(),
            endpoints: vec![],
        };
        let config = DocGenConfig {
            include_envelope: false,
            include_experimental: true,
            include_error_codes: false,
        };
        let mut out = String::new();
        write_toc(&mut out, &registry, &config);
        assert!(!out.contains("Response Envelope"));
        assert!(!out.contains("Error Codes"));
    }

    // --- Definitions rendering in endpoint section ---

    #[test]
    fn endpoint_section_renders_definitions_with_description() {
        let ep = EndpointMeta {
            id: "test".into(),
            title: "Test".into(),
            description: "A test".into(),
            robot_command: None,
            mcp_tool: None,
            schema_file: "test.json".into(),
            stable: true,
            since: "0.1.0".into(),
        };
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "val": { "type": "string" }
            },
            "$defs": {
                "inner": {
                    "type": "object",
                    "description": "An inner definition",
                    "properties": {
                        "x": { "type": "integer" }
                    }
                }
            }
        });
        let mut schemas_map: BTreeMap<&str, &Value> = BTreeMap::new();
        schemas_map.insert("test.json", &schema);
        let mut out = String::new();
        write_endpoint_section(&mut out, &ep, &schemas_map);
        assert!(out.contains("#### `inner`"));
        assert!(out.contains("An inner definition"));
    }

    #[test]
    fn endpoint_section_skips_empty_definitions() {
        let ep = EndpointMeta {
            id: "test".into(),
            title: "Test".into(),
            description: "A test".into(),
            robot_command: None,
            mcp_tool: None,
            schema_file: "test.json".into(),
            stable: true,
            since: "0.1.0".into(),
        };
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "val": { "type": "string" }
            },
            "$defs": {
                "empty_def": {
                    "type": "object",
                    "description": "Empty definition"
                }
            }
        });
        let mut schemas_map: BTreeMap<&str, &Value> = BTreeMap::new();
        schemas_map.insert("test.json", &schema);
        let mut out = String::new();
        write_endpoint_section(&mut out, &ep, &schemas_map);
        // empty_def has no properties so it should be skipped
        assert!(!out.contains("#### `empty_def`"));
    }

    // --- Properties ordering: alphabetical within required/optional ---

    #[test]
    fn properties_alphabetical_within_groups() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["z_req", "a_req"],
            "properties": {
                "z_req": { "type": "string" },
                "a_req": { "type": "string" },
                "z_opt": { "type": "string" },
                "a_opt": { "type": "string" }
            }
        });
        let doc = parse_schema(&schema);
        assert_eq!(doc.properties.len(), 4);
        // Required first, alphabetical
        assert_eq!(doc.properties[0].name, "a_req");
        assert!(doc.properties[0].required);
        assert_eq!(doc.properties[1].name, "z_req");
        assert!(doc.properties[1].required);
        // Optional next, alphabetical
        assert_eq!(doc.properties[2].name, "a_opt");
        assert!(!doc.properties[2].required);
        assert_eq!(doc.properties[3].name, "z_opt");
        assert!(!doc.properties[3].required);
    }

    // --- Large schema handling ---

    #[test]
    fn parse_schema_many_properties() {
        let mut props = serde_json::Map::new();
        for i in 0..50 {
            props.insert(
                format!("field_{:03}", i),
                serde_json::json!({ "type": "string", "description": format!("Field {}", i) }),
            );
        }
        let schema = serde_json::json!({
            "title": "Large Schema",
            "type": "object",
            "properties": props
        });
        let doc = parse_schema(&schema);
        assert_eq!(doc.properties.len(), 50);
        // All should be non-required since no "required" array
        assert!(doc.properties.iter().all(|p| !p.required));
        // Should be alphabetically sorted
        for window in doc.properties.windows(2) {
            assert!(window[0].name <= window[1].name);
        }
    }

    #[test]
    fn parse_schema_many_definitions() {
        let mut defs = serde_json::Map::new();
        for i in 0..20 {
            defs.insert(
                format!("def_{:02}", i),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "val": { "type": "integer" }
                    }
                }),
            );
        }
        let schema = serde_json::json!({ "$defs": defs });
        let doc = parse_schema(&schema);
        assert_eq!(doc.definitions.len(), 20);
        // Should be sorted
        for window in doc.definitions.windows(2) {
            assert!(window[0].0 <= window[1].0);
        }
    }

    // --- Determinism with schemas provided ---

    #[test]
    fn generate_reference_deterministic_with_schemas() {
        let registry = SchemaRegistry::canonical();
        let schema = sample_schema();
        let envelope = sample_envelope();
        let schemas = vec![
            ("wa-robot-state.json".to_string(), schema.clone()),
            ("wa-robot-envelope.json".to_string(), envelope.clone()),
        ];
        let config = DocGenConfig::default();
        let pages1 = generate_reference(&registry, &schemas, &config);
        let pages2 = generate_reference(&registry, &schemas, &config);
        assert_eq!(pages1[0].content, pages2[0].content);
    }

    // --- Endpoint section with schema that has no properties ---

    #[test]
    fn endpoint_section_schema_no_properties_skips_table() {
        let ep = EndpointMeta {
            id: "help".into(),
            title: "Help".into(),
            description: "Help endpoint".into(),
            robot_command: Some("robot help".into()),
            mcp_tool: None,
            schema_file: "help.json".into(),
            stable: true,
            since: "0.1.0".into(),
        };
        let schema = serde_json::json!({
            "title": "Help Response",
            "type": "object"
        });
        let mut schemas_map: BTreeMap<&str, &Value> = BTreeMap::new();
        schemas_map.insert("help.json", &schema);
        let mut out = String::new();
        write_endpoint_section(&mut out, &ep, &schemas_map);
        assert!(out.contains("### Help"));
        assert!(!out.contains("#### Response Fields"));
    }

    // --- Category title exhaustive coverage ---

    #[test]
    fn category_titles_are_distinct() {
        let titles: Vec<&str> = EndpointCategory::all().iter().map(|c| c.title()).collect();
        let mut deduped = titles.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            titles.len(),
            deduped.len(),
            "all category titles should be unique"
        );
    }
}
