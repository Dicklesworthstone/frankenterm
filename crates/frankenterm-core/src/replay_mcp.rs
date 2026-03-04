//! MCP tool surface for replay and decision-diff workflows.
//!
//! Provides typed JSON schemas and dispatch logic for replay MCP tools.
//! All tools follow the `wa.*` naming convention but use the `wa.replay.*`
//! namespace.
//!
//! # Tools
//!
//! | Tool                      | Description |
//! |---------------------------|-------------|
//! | `wa.replay.inspect`       | Inspect a trace file's metadata |
//! | `wa.replay.diff`          | Decision-diff two trace files |
//! | `wa.replay.regression`    | Run regression suite |
//! | `wa.replay.artifact_list` | List registered artifacts |
//! | `wa.replay.artifact_add`  | Register a new artifact |
//! | `wa.replay.artifact_retire` | Retire an artifact |
//!
//! # Schema Format
//!
//! Each tool defines its input schema as a JSON Schema object suitable
//! for MCP `tool_use` integration. Schemas are also available at runtime
//! via [`ReplayToolSchema::input_schema`].

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Tool names
// ---------------------------------------------------------------------------

pub const TOOL_REPLAY_INSPECT: &str = "wa.replay.inspect";
pub const TOOL_REPLAY_DIFF: &str = "wa.replay.diff";
pub const TOOL_REPLAY_REGRESSION: &str = "wa.replay.regression";
pub const TOOL_REPLAY_ARTIFACT_LIST: &str = "wa.replay.artifact_list";
pub const TOOL_REPLAY_ARTIFACT_ADD: &str = "wa.replay.artifact_add";
pub const TOOL_REPLAY_ARTIFACT_RETIRE: &str = "wa.replay.artifact_retire";

/// All replay tool names.
pub const ALL_REPLAY_TOOLS: &[&str] = &[
    TOOL_REPLAY_INSPECT,
    TOOL_REPLAY_DIFF,
    TOOL_REPLAY_REGRESSION,
    TOOL_REPLAY_ARTIFACT_LIST,
    TOOL_REPLAY_ARTIFACT_ADD,
    TOOL_REPLAY_ARTIFACT_RETIRE,
];

// ---------------------------------------------------------------------------
// Tool schema registry
// ---------------------------------------------------------------------------

/// Metadata for a single replay MCP tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayToolSchema {
    /// Tool name (e.g., "wa.replay.inspect").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the tool's input arguments.
    pub input_schema: serde_json::Value,
    /// Tags for discovery.
    pub tags: Vec<String>,
}

/// Return all replay tool schemas.
#[must_use]
pub fn all_tool_schemas() -> Vec<ReplayToolSchema> {
    vec![
        inspect_schema(),
        diff_schema(),
        regression_schema(),
        artifact_list_schema(),
        artifact_add_schema(),
        artifact_retire_schema(),
    ]
}

/// Lookup a schema by tool name.
#[must_use]
pub fn schema_for(name: &str) -> Option<ReplayToolSchema> {
    all_tool_schemas().into_iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// Individual schemas
// ---------------------------------------------------------------------------

/// Schema for `wa.replay.inspect`.
#[must_use]
pub fn inspect_schema() -> ReplayToolSchema {
    ReplayToolSchema {
        name: TOOL_REPLAY_INSPECT.into(),
        description: "Inspect a replay trace file: event count, pane count, rule count, \
                       decision types, integrity status."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "trace": {
                    "type": "string",
                    "description": "Path to the .ftreplay trace file"
                }
            },
            "required": ["trace"],
            "additionalProperties": false
        }),
        tags: vec!["replay".into(), "inspect".into()],
    }
}

/// Schema for `wa.replay.diff`.
#[must_use]
pub fn diff_schema() -> ReplayToolSchema {
    ReplayToolSchema {
        name: TOOL_REPLAY_DIFF.into(),
        description: "Run a decision-diff comparison between baseline and candidate \
                       trace files. Returns divergences, risk scores, and gate result."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "baseline": {
                    "type": "string",
                    "description": "Path to the baseline .ftreplay trace"
                },
                "candidate": {
                    "type": "string",
                    "description": "Path to the candidate .ftreplay trace"
                },
                "tolerance_ms": {
                    "type": "integer",
                    "description": "Time tolerance for shifted detection (ms)",
                    "default": 100,
                    "minimum": 0
                },
                "budget": {
                    "type": "string",
                    "description": "Path to regression budget TOML file (optional)"
                }
            },
            "required": ["baseline", "candidate"],
            "additionalProperties": false
        }),
        tags: vec!["replay".into(), "diff".into(), "regression".into()],
    }
}

/// Schema for `wa.replay.regression`.
#[must_use]
pub fn regression_schema() -> ReplayToolSchema {
    ReplayToolSchema {
        name: TOOL_REPLAY_REGRESSION.into(),
        description: "Run the replay regression suite. Evaluates all registered \
                       artifacts against their baselines and returns pass/fail with evidence."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "suite_dir": {
                    "type": "string",
                    "description": "Directory containing the regression suite",
                    "default": "tests/regression/replay/"
                },
                "budget": {
                    "type": "string",
                    "description": "Path to regression budget TOML (optional)"
                }
            },
            "additionalProperties": false
        }),
        tags: vec!["replay".into(), "regression".into(), "ci".into()],
    }
}

/// Schema for `wa.replay.artifact_list`.
#[must_use]
pub fn artifact_list_schema() -> ReplayToolSchema {
    ReplayToolSchema {
        name: TOOL_REPLAY_ARTIFACT_LIST.into(),
        description: "List registered replay artifacts with optional filtering \
                       by sensitivity tier and status."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "tier": {
                    "type": "string",
                    "description": "Filter by sensitivity tier: T1, T2, T3",
                    "enum": ["T1", "T2", "T3"]
                },
                "status": {
                    "type": "string",
                    "description": "Filter by status: active, retired",
                    "enum": ["active", "retired"]
                }
            },
            "additionalProperties": false
        }),
        tags: vec!["replay".into(), "artifact".into()],
    }
}

/// Schema for `wa.replay.artifact_add`.
#[must_use]
pub fn artifact_add_schema() -> ReplayToolSchema {
    ReplayToolSchema {
        name: TOOL_REPLAY_ARTIFACT_ADD.into(),
        description: "Register a new replay artifact in the manifest. \
                       Validates integrity and computes SHA-256."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the .ftreplay artifact file"
                },
                "label": {
                    "type": "string",
                    "description": "Human-readable label for the artifact",
                    "default": "unlabeled"
                },
                "tier": {
                    "type": "string",
                    "description": "Sensitivity tier: T1 (default), T2, T3",
                    "enum": ["T1", "T2", "T3"],
                    "default": "T1"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        tags: vec!["replay".into(), "artifact".into()],
    }
}

/// Schema for `wa.replay.artifact_retire`.
#[must_use]
pub fn artifact_retire_schema() -> ReplayToolSchema {
    ReplayToolSchema {
        name: TOOL_REPLAY_ARTIFACT_RETIRE.into(),
        description: "Mark a registered artifact as retired. Does not delete \
                       the file; excludes it from the regression suite."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the artifact to retire"
                },
                "reason": {
                    "type": "string",
                    "description": "Reason for retirement"
                }
            },
            "required": ["path", "reason"],
            "additionalProperties": false
        }),
        tags: vec!["replay".into(), "artifact".into()],
    }
}

// ---------------------------------------------------------------------------
// Dispatch result types
// ---------------------------------------------------------------------------

/// Outcome of dispatching a replay MCP tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DispatchResult {
    /// Successful execution.
    Ok {
        /// JSON-serialized result data.
        data: serde_json::Value,
    },
    /// Error with structured error code.
    Error {
        /// Error code from `replay_robot::REPLAY_ERR_*`.
        code: String,
        /// Human-readable message.
        message: String,
        /// Recovery hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
}

impl DispatchResult {
    /// Create a successful result.
    pub fn ok(data: serde_json::Value) -> Self {
        Self::Ok { data }
    }

    /// Create an error result.
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.to_string(),
            message: message.into(),
            hint: None,
        }
    }

    /// Create an error result with a hint.
    pub fn error_with_hint(
        code: &str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self::Error {
            code: code.to_string(),
            message: message.into(),
            hint: Some(hint.into()),
        }
    }

    /// Whether the dispatch succeeded.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    /// Whether the dispatch failed.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

/// Validate that required string argument is present and non-empty.
pub fn validate_required_str(args: &serde_json::Value, field: &str) -> Result<String, String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("missing or empty required field: {field}"))
}

/// Validate an optional string argument.
pub fn validate_optional_str(args: &serde_json::Value, field: &str) -> Option<String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// Validate an optional integer with a default.
pub fn validate_optional_u64(args: &serde_json::Value, field: &str, default: u64) -> u64 {
    args.get(field).and_then(|v| v.as_u64()).unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Schema registry ──────────────────────────────────────────────────

    #[test]
    fn all_schemas_count() {
        assert_eq!(all_tool_schemas().len(), 6);
    }

    #[test]
    fn all_schemas_unique_names() {
        let schemas = all_tool_schemas();
        let mut names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), schemas.len());
    }

    #[test]
    fn all_tools_constant_matches() {
        let schemas = all_tool_schemas();
        let schema_names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        for name in ALL_REPLAY_TOOLS {
            assert!(
                schema_names.contains(name),
                "tool constant {name} has no schema"
            );
        }
    }

    #[test]
    fn schema_for_existing() {
        let s = schema_for(TOOL_REPLAY_INSPECT).unwrap();
        assert_eq!(s.name, TOOL_REPLAY_INSPECT);
        assert!(!s.description.is_empty());
    }

    #[test]
    fn schema_for_missing() {
        assert!(schema_for("wa.replay.nonexistent").is_none());
    }

    // ── Individual schema structure ──────────────────────────────────────

    #[test]
    fn inspect_schema_valid() {
        let s = inspect_schema();
        assert_eq!(s.name, TOOL_REPLAY_INSPECT);
        let required = s.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("trace")));
    }

    #[test]
    fn diff_schema_valid() {
        let s = diff_schema();
        assert_eq!(s.name, TOOL_REPLAY_DIFF);
        let required = s.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("baseline")));
        assert!(required.iter().any(|v| v.as_str() == Some("candidate")));
    }

    #[test]
    fn regression_schema_valid() {
        let s = regression_schema();
        assert_eq!(s.name, TOOL_REPLAY_REGRESSION);
        let props = s.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("suite_dir"));
    }

    #[test]
    fn artifact_list_schema_valid() {
        let s = artifact_list_schema();
        assert_eq!(s.name, TOOL_REPLAY_ARTIFACT_LIST);
        let props = s.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("tier"));
        assert!(props.contains_key("status"));
    }

    #[test]
    fn artifact_add_schema_valid() {
        let s = artifact_add_schema();
        assert_eq!(s.name, TOOL_REPLAY_ARTIFACT_ADD);
        let required = s.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("path")));
    }

    #[test]
    fn artifact_retire_schema_valid() {
        let s = artifact_retire_schema();
        assert_eq!(s.name, TOOL_REPLAY_ARTIFACT_RETIRE);
        let required = s.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("path")));
        assert!(required.iter().any(|v| v.as_str() == Some("reason")));
    }

    // ── Schema serde ─────────────────────────────────────────────────────

    #[test]
    fn schema_serde_roundtrip() {
        for schema in all_tool_schemas() {
            let json = serde_json::to_string(&schema).unwrap();
            let restored: ReplayToolSchema = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, schema);
        }
    }

    #[test]
    fn schema_has_type_object() {
        for schema in all_tool_schemas() {
            let ty = schema.input_schema["type"].as_str().unwrap();
            assert_eq!(
                ty, "object",
                "schema {} should have type=object",
                schema.name
            );
        }
    }

    #[test]
    fn schema_has_properties() {
        for schema in all_tool_schemas() {
            assert!(
                schema.input_schema["properties"].is_object(),
                "schema {} should have properties",
                schema.name
            );
        }
    }

    #[test]
    fn all_schemas_have_tags() {
        for schema in all_tool_schemas() {
            assert!(
                !schema.tags.is_empty(),
                "schema {} should have tags",
                schema.name
            );
            assert!(
                schema.tags.contains(&"replay".to_string()),
                "schema {} should be tagged 'replay'",
                schema.name
            );
        }
    }

    #[test]
    fn all_schemas_no_additional_properties() {
        for schema in all_tool_schemas() {
            let addl = &schema.input_schema["additionalProperties"];
            assert_eq!(
                addl.as_bool(),
                Some(false),
                "schema {} should have additionalProperties: false",
                schema.name
            );
        }
    }

    // ── DispatchResult ───────────────────────────────────────────────────

    #[test]
    fn dispatch_ok() {
        let r = DispatchResult::ok(serde_json::json!({"count": 5}));
        assert!(r.is_ok());
        assert!(!r.is_error());
    }

    #[test]
    fn dispatch_error() {
        let r = DispatchResult::error("replay.not_found", "not found");
        assert!(!r.is_ok());
        assert!(r.is_error());
    }

    #[test]
    fn dispatch_error_with_hint() {
        let r = DispatchResult::error_with_hint("replay.parse_error", "bad json", "check format");
        assert!(r.is_error());
        if let DispatchResult::Error { hint, .. } = &r {
            assert_eq!(hint.as_deref(), Some("check format"));
        }
    }

    #[test]
    fn dispatch_result_serde() {
        let ok = DispatchResult::ok(serde_json::json!({"x": 1}));
        let json = serde_json::to_string(&ok).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, ok);

        let err = DispatchResult::error("replay.error", "msg");
        let json = serde_json::to_string(&err).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, err);
    }

    // ── Argument validation ──────────────────────────────────────────────

    #[test]
    fn validate_required_str_present() {
        let args = serde_json::json!({"trace": "test.ftreplay"});
        let result = validate_required_str(&args, "trace");
        assert_eq!(result.unwrap(), "test.ftreplay");
    }

    #[test]
    fn validate_required_str_missing() {
        let args = serde_json::json!({});
        let result = validate_required_str(&args, "trace");
        assert!(result.is_err());
    }

    #[test]
    fn validate_required_str_empty() {
        let args = serde_json::json!({"trace": ""});
        let result = validate_required_str(&args, "trace");
        assert!(result.is_err());
    }

    #[test]
    fn validate_required_str_whitespace_only_rejected() {
        let args = serde_json::json!({"trace": "   \n\t"});
        let result = validate_required_str(&args, "trace");
        assert!(result.is_err());
    }

    #[test]
    fn validate_optional_str_present() {
        let args = serde_json::json!({"budget": "budget.toml"});
        assert_eq!(
            validate_optional_str(&args, "budget"),
            Some("budget.toml".into())
        );
    }

    #[test]
    fn validate_optional_str_missing() {
        let args = serde_json::json!({});
        assert_eq!(validate_optional_str(&args, "budget"), None);
    }

    #[test]
    fn validate_optional_str_whitespace_returns_none() {
        let args = serde_json::json!({"budget": "   "});
        assert_eq!(validate_optional_str(&args, "budget"), None);
    }

    #[test]
    fn validate_optional_u64_present() {
        let args = serde_json::json!({"tolerance_ms": 200});
        assert_eq!(validate_optional_u64(&args, "tolerance_ms", 100), 200);
    }

    #[test]
    fn validate_optional_u64_default() {
        let args = serde_json::json!({});
        assert_eq!(validate_optional_u64(&args, "tolerance_ms", 100), 100);
    }

    // ── Tool name constants ──────────────────────────────────────────────

    #[test]
    fn tool_names_namespace() {
        for name in ALL_REPLAY_TOOLS {
            assert!(
                name.starts_with("wa.replay."),
                "tool name should start with wa.replay.: {name}"
            );
        }
    }

    #[test]
    fn tool_names_unique() {
        let mut names: Vec<&&str> = ALL_REPLAY_TOOLS.iter().collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), ALL_REPLAY_TOOLS.len());
    }
}
