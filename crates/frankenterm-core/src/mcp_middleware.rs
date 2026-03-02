//! Middleware wrappers for MCP tool handling.

mod mcp_middleware_framework {
    pub use fastmcp::{
        Content as FrameworkContent, McpContext as FrameworkMcpContext,
        McpError as FrameworkMcpError, McpResult as FrameworkMcpResult, Tool as FrameworkTool,
        ToolHandler as FrameworkToolHandler,
    };
}

use super::{
    MCP_ERR_INVALID_ARGS, McpEnvelope, elapsed_ms, envelope_to_content, record_mcp_audit_sync,
};
use mcp_middleware_framework::{
    FrameworkContent as Content, FrameworkMcpContext as McpContext, FrameworkMcpError as McpError,
    FrameworkMcpResult as McpResult, FrameworkTool as Tool, FrameworkToolHandler as ToolHandler,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum McpOutputFormat {
    #[default]
    Json,
    Toon,
}

pub(super) fn parse_mcp_output_format(raw: &str) -> Option<McpOutputFormat> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "json" => Some(McpOutputFormat::Json),
        "toon" => Some(McpOutputFormat::Toon),
        _ => None,
    }
}

pub(super) fn extract_mcp_output_format(
    arguments: &mut serde_json::Value,
) -> std::result::Result<McpOutputFormat, String> {
    let Some(object) = arguments.as_object_mut() else {
        return Ok(McpOutputFormat::Json);
    };

    let Some(raw_value) = object.remove("format") else {
        return Ok(McpOutputFormat::Json);
    };

    let Some(raw_format) = raw_value.as_str() else {
        return Err("Invalid format: expected string 'json' or 'toon'".to_string());
    };

    parse_mcp_output_format(raw_format)
        .ok_or_else(|| format!("Invalid format '{raw_format}': expected one of ['json', 'toon']"))
}

pub(super) fn augment_tool_schema_with_format(input_schema: &mut serde_json::Value) {
    let Some(schema_obj) = input_schema.as_object_mut() else {
        return;
    };
    if schema_obj.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return;
    }

    let properties = schema_obj
        .entry("properties")
        .or_insert_with(|| serde_json::json!({}));
    let Some(properties_obj) = properties.as_object_mut() else {
        return;
    };

    properties_obj
        .entry("format".to_string())
        .or_insert_with(|| {
            serde_json::json!({
                "type": "string",
                "enum": ["json", "toon"],
                "description": "Optional output format override for this call"
            })
        });
}

pub(super) fn encode_mcp_contents(
    contents: Vec<Content>,
    format: McpOutputFormat,
) -> McpResult<Vec<Content>> {
    match format {
        McpOutputFormat::Json => Ok(contents),
        McpOutputFormat::Toon => contents
            .into_iter()
            .map(|content| match content {
                Content::Text { text } => {
                    let value = serde_json::from_str::<serde_json::Value>(&text).map_err(|e| {
                        McpError::internal_error(format!(
                            "Unable to transcode MCP payload to TOON: {e}"
                        ))
                    })?;
                    Ok(Content::Text {
                        text: toon_rust::encode(value, None),
                    })
                }
                other => Ok(other),
            })
            .collect(),
    }
}

/// Wrapper that allows per-call MCP output format negotiation (`json` or `toon`).
///
/// Each wrapped tool accepts an optional `format` argument in its input schema.
/// The argument is stripped before forwarding to the inner handler so existing
/// tool parameter structs do not need to change.
pub(super) struct FormatAwareToolHandler<T: ToolHandler> {
    inner: T,
}

impl<T: ToolHandler> FormatAwareToolHandler<T> {
    pub(super) fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: ToolHandler> ToolHandler for FormatAwareToolHandler<T> {
    fn definition(&self) -> Tool {
        let mut definition = self.inner.definition();
        augment_tool_schema_with_format(&mut definition.input_schema);
        definition
    }

    fn call(&self, ctx: &McpContext, mut arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let format = match extract_mcp_output_format(&mut arguments) {
            Ok(format) => format,
            Err(message) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    message,
                    Some("Set format to either 'json' or 'toon'.".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let contents = self.inner.call(ctx, arguments)?;
        encode_mcp_contents(contents, format)
    }
}

/// Wrapper that records an audit entry for every tool call.
///
/// Wraps any `ToolHandler` and intercepts `call()` to record:
/// - tool name and redacted argument keys
/// - success/failure outcome
/// - error code (if any)
/// - elapsed time
pub(super) struct AuditedToolHandler<T: ToolHandler> {
    inner: T,
    pub(super) tool_name: String,
    db_path: Arc<PathBuf>,
}

impl<T: ToolHandler> AuditedToolHandler<T> {
    pub(super) fn new(inner: T, tool_name: impl Into<String>, db_path: Arc<PathBuf>) -> Self {
        Self {
            inner,
            tool_name: tool_name.into(),
            db_path,
        }
    }
}

impl<T: ToolHandler> ToolHandler for AuditedToolHandler<T> {
    fn definition(&self) -> Tool {
        self.inner.definition()
    }

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let raw_args = arguments.clone();
        let result = self.inner.call(ctx, arguments);

        // Extract ok/error_code from the envelope in the result.
        let (ok, error_code) = match &result {
            Ok(contents) => {
                let parsed = contents.first().and_then(|c| match c {
                    Content::Text { text } => serde_json::from_str::<serde_json::Value>(text).ok(),
                    _ => None,
                });
                let is_ok = parsed
                    .as_ref()
                    .and_then(|v| v.get("ok")?.as_bool())
                    .unwrap_or(true);
                let code = if !is_ok {
                    parsed.and_then(|v| v.get("error_code")?.as_str().map(String::from))
                } else {
                    None
                };
                (is_ok, code)
            }
            Err(_) => (false, Some("MCP_INTERNAL".to_string())),
        };

        record_mcp_audit_sync(
            &self.db_path,
            &self.tool_name,
            &raw_args,
            ok,
            error_code.as_deref(),
            elapsed_ms(start),
        );

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // McpOutputFormat Tests
    // ========================================================================

    #[test]
    fn output_format_default_is_json() {
        assert_eq!(McpOutputFormat::default(), McpOutputFormat::Json);
    }

    // ========================================================================
    // parse_mcp_output_format Tests
    // ========================================================================

    #[test]
    fn parse_format_json() {
        assert_eq!(
            parse_mcp_output_format("json"),
            Some(McpOutputFormat::Json)
        );
    }

    #[test]
    fn parse_format_toon() {
        assert_eq!(
            parse_mcp_output_format("toon"),
            Some(McpOutputFormat::Toon)
        );
    }

    #[test]
    fn parse_format_case_insensitive() {
        assert_eq!(
            parse_mcp_output_format("JSON"),
            Some(McpOutputFormat::Json)
        );
        assert_eq!(
            parse_mcp_output_format("Toon"),
            Some(McpOutputFormat::Toon)
        );
        assert_eq!(
            parse_mcp_output_format("TOON"),
            Some(McpOutputFormat::Toon)
        );
    }

    #[test]
    fn parse_format_with_whitespace() {
        assert_eq!(
            parse_mcp_output_format("  json  "),
            Some(McpOutputFormat::Json)
        );
        assert_eq!(
            parse_mcp_output_format("\ttoon\n"),
            Some(McpOutputFormat::Toon)
        );
    }

    #[test]
    fn parse_format_invalid() {
        assert_eq!(parse_mcp_output_format("xml"), None);
        assert_eq!(parse_mcp_output_format(""), None);
        assert_eq!(parse_mcp_output_format("yaml"), None);
    }

    // ========================================================================
    // extract_mcp_output_format Tests
    // ========================================================================

    #[test]
    fn extract_format_missing_defaults_to_json() {
        let mut args = serde_json::json!({"pane_id": 1});
        let result = extract_mcp_output_format(&mut args);
        assert_eq!(result.unwrap(), McpOutputFormat::Json);
        // format key should not have been inserted
        assert!(args.get("format").is_none());
    }

    #[test]
    fn extract_format_json_removes_key() {
        let mut args = serde_json::json!({"pane_id": 1, "format": "json"});
        let result = extract_mcp_output_format(&mut args);
        assert_eq!(result.unwrap(), McpOutputFormat::Json);
        // format key should be removed after extraction
        assert!(args.get("format").is_none());
        // Other keys should remain
        assert_eq!(args.get("pane_id").unwrap().as_u64(), Some(1));
    }

    #[test]
    fn extract_format_toon_removes_key() {
        let mut args = serde_json::json!({"format": "toon"});
        let result = extract_mcp_output_format(&mut args);
        assert_eq!(result.unwrap(), McpOutputFormat::Toon);
        assert!(args.get("format").is_none());
    }

    #[test]
    fn extract_format_invalid_string_returns_error() {
        let mut args = serde_json::json!({"format": "xml"});
        let result = extract_mcp_output_format(&mut args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("xml"));
    }

    #[test]
    fn extract_format_non_string_returns_error() {
        let mut args = serde_json::json!({"format": 42});
        let result = extract_mcp_output_format(&mut args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expected string"));
    }

    #[test]
    fn extract_format_non_object_defaults_to_json() {
        let mut args = serde_json::json!("string_value");
        let result = extract_mcp_output_format(&mut args);
        assert_eq!(result.unwrap(), McpOutputFormat::Json);
    }

    #[test]
    fn extract_format_null_defaults_to_json() {
        let mut args = serde_json::json!(null);
        let result = extract_mcp_output_format(&mut args);
        assert_eq!(result.unwrap(), McpOutputFormat::Json);
    }

    // ========================================================================
    // augment_tool_schema_with_format Tests
    // ========================================================================

    #[test]
    fn augment_schema_adds_format_property() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "pane_id": {"type": "integer"}
            }
        });
        augment_tool_schema_with_format(&mut schema);

        let format_prop = schema
            .get("properties")
            .unwrap()
            .get("format")
            .expect("format property should exist");
        assert_eq!(format_prop.get("type").unwrap().as_str(), Some("string"));
        let enum_values = format_prop.get("enum").unwrap().as_array().unwrap();
        assert!(enum_values.contains(&serde_json::json!("json")));
        assert!(enum_values.contains(&serde_json::json!("toon")));
    }

    #[test]
    fn augment_schema_preserves_existing_format() {
        let custom_format = serde_json::json!({"type": "string", "enum": ["custom"]});
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "format": custom_format.clone()
            }
        });
        augment_tool_schema_with_format(&mut schema);

        // Should not overwrite existing "format" property
        let format_prop = schema.get("properties").unwrap().get("format").unwrap();
        assert_eq!(format_prop, &custom_format);
    }

    #[test]
    fn augment_schema_creates_properties_if_missing() {
        let mut schema = serde_json::json!({"type": "object"});
        augment_tool_schema_with_format(&mut schema);

        assert!(schema.get("properties").is_some());
        assert!(schema.get("properties").unwrap().get("format").is_some());
    }

    #[test]
    fn augment_schema_non_object_type_is_noop() {
        let mut schema = serde_json::json!({"type": "array"});
        let before = schema.clone();
        augment_tool_schema_with_format(&mut schema);
        assert_eq!(schema, before);
    }

    #[test]
    fn augment_schema_non_object_value_is_noop() {
        let mut schema = serde_json::json!("not an object");
        let before = schema.clone();
        augment_tool_schema_with_format(&mut schema);
        assert_eq!(schema, before);
    }

    // ========================================================================
    // encode_mcp_contents Tests
    // ========================================================================

    #[test]
    fn encode_json_passthrough() {
        let contents = vec![Content::Text {
            text: r#"{"ok":true}"#.to_string(),
        }];
        let result = encode_mcp_contents(contents.clone(), McpOutputFormat::Json).unwrap();
        assert_eq!(result.len(), 1);
        if let Content::Text { text } = &result[0] {
            assert_eq!(text, r#"{"ok":true}"#);
        }
    }

    #[test]
    fn encode_toon_converts_json() {
        let contents = vec![Content::Text {
            text: r#"{"ok":true,"data":"hello"}"#.to_string(),
        }];
        let result = encode_mcp_contents(contents, McpOutputFormat::Toon).unwrap();
        assert_eq!(result.len(), 1);
        if let Content::Text { text } = &result[0] {
            // TOON output should be different from raw JSON
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn encode_toon_invalid_json_returns_error() {
        let contents = vec![Content::Text {
            text: "not valid json".to_string(),
        }];
        let result = encode_mcp_contents(contents, McpOutputFormat::Toon);
        assert!(result.is_err());
    }

    #[test]
    fn encode_empty_contents() {
        let result = encode_mcp_contents(vec![], McpOutputFormat::Json).unwrap();
        assert!(result.is_empty());

        let result = encode_mcp_contents(vec![], McpOutputFormat::Toon).unwrap();
        assert!(result.is_empty());
    }
}
