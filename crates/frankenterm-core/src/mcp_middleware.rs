//! Middleware wrappers for MCP tool handling.

use super::{
    MCP_ERR_INTERNAL, MCP_ERR_INVALID_ARGS, McpEnvelope, elapsed_ms, envelope_to_content,
    record_mcp_audit_sync_with_policy,
};
use crate::mcp_framework::{
    FrameworkContent as Content, FrameworkMcpContext as McpContext, FrameworkMcpError as McpError,
    FrameworkMcpResult as McpResult, FrameworkResource as Resource,
    FrameworkResourceContent as ResourceContent, FrameworkResourceHandler as ResourceHandler,
    FrameworkResourceTemplate as ResourceTemplate, FrameworkResponseDeliveryCoordinator,
    FrameworkTool as Tool, FrameworkToolHandler as ToolHandler,
};
use crate::redactor::Redactor;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// A server-owned output policy shared by native tools, resources and templates.
/// Construction validates directly supplied configs as well as loaded configs;
/// no process-global state can leak one workspace's policy into another server.
#[derive(Clone)]
pub(super) struct McpOutputPolicy {
    redactor: Arc<Redactor>,
    redaction_config: Arc<crate::config::RedactionConfig>,
}

impl McpOutputPolicy {
    pub(super) fn new(config: &crate::config::RedactionConfig) -> crate::Result<Self> {
        Ok(Self {
            redactor: Arc::new(config.build_redactor()?),
            redaction_config: Arc::new(config.clone()),
        })
    }

    pub(super) fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    pub(super) fn shared_redaction_config(&self) -> Arc<crate::config::RedactionConfig> {
        Arc::clone(&self.redaction_config)
    }

    pub(super) fn audited_tool<T: ToolHandler>(
        &self,
        inner: T,
        tool_name: impl Into<String>,
        db_path: Arc<PathBuf>,
    ) -> AuditedToolHandler<T> {
        AuditedToolHandler {
            inner,
            tool_name: tool_name.into(),
            db_path,
            output: self.clone(),
        }
    }

    pub(super) fn tool<T: ToolHandler>(&self, inner: T) -> FormatAwareToolHandler<T> {
        FormatAwareToolHandler::new(inner, Arc::clone(&self.redactor), false)
    }

    #[cfg(any(feature = "mcp-client", test))]
    pub(super) fn proxy_tool<T: ToolHandler>(&self, inner: T) -> FormatAwareToolHandler<T> {
        // Remote MCP tools may return plain text instead of FT's JSON envelope.
        FormatAwareToolHandler::new(inner, Arc::clone(&self.redactor), true)
    }

    pub(super) fn tool_with_response_delivery<T: ToolHandler>(
        &self,
        inner: T,
        response_delivery: Arc<FrameworkResponseDeliveryCoordinator>,
    ) -> FormatAwareToolHandler<T> {
        let mut handler = self.tool(inner);
        handler.response_delivery = Some(response_delivery);
        handler
    }

    pub(super) fn resource<T: ResourceHandler>(&self, inner: T) -> RedactedResourceHandler<T> {
        RedactedResourceHandler {
            inner,
            redactor: Arc::clone(&self.redactor),
        }
    }
}

const MCP_OUTPUT_REDACTION_MAX_BYTES: usize =
    crate::tuning_config::IpcTuning::MAX_MAX_MESSAGE_SIZE;
const MCP_OUTPUT_REDACTION_MAX_DEPTH: usize = 64;
const MCP_OUTPUT_REDACTION_MAX_NODES: usize = 100_000;
const MCP_OUTPUT_REDACTION_ERROR: &str = "MCP output could not be safely redacted";

fn output_redaction_error() -> McpError {
    McpError::internal_error(MCP_OUTPUT_REDACTION_ERROR)
}

fn output_refusal_contents(error: McpError, elapsed: u64) -> McpResult<Vec<Content>> {
    if error.code == McpError::request_cancelled().code {
        return Err(error);
    }
    envelope_to_content(McpEnvelope::<()>::error(
        MCP_ERR_INTERNAL,
        MCP_OUTPUT_REDACTION_ERROR,
        None,
        elapsed,
    ))
}

fn output_checkpoint(ctx: &McpContext) -> McpResult<()> {
    ctx.cx()
        .checkpoint()
        .map_err(|_| McpError::request_cancelled())
}

/// Count actual serialized bytes, including escaping and all content blocks,
/// without allocating another response-sized buffer.
fn check_output_size<T: serde::Serialize + ?Sized>(
    ctx: &McpContext,
    value: &T,
) -> McpResult<()> {
    struct BoundedWriter(usize);

    impl std::io::Write for BoundedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other(MCP_OUTPUT_REDACTION_ERROR))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    output_checkpoint(ctx)?;
    serde_json::to_writer(BoundedWriter(MCP_OUTPUT_REDACTION_MAX_BYTES), value)
        .map_err(|_| output_redaction_error())?;
    output_checkpoint(ctx)
}

fn redact_output_text(text: &str, redactor: &Redactor) -> McpResult<String> {
    if text.len() > MCP_OUTPUT_REDACTION_MAX_BYTES {
        return Err(output_redaction_error());
    }
    let normalized = crate::output::normalize_terminal_text_for_redaction(text);
    let normalized_redacted = redactor.redact(&normalized);
    let redacted = if normalized_redacted == normalized {
        // Honor wa.get_text escapes=true for safe text. If normalization
        // exposes a split secret, only the normalized redacted view is safe.
        redactor.redact(text)
    } else {
        normalized_redacted
    };
    if redacted.len() > MCP_OUTPUT_REDACTION_MAX_BYTES {
        return Err(output_redaction_error());
    }
    Ok(redacted)
}

fn output_field_is_identity(key: &str) -> bool {
    matches!(
        key,
        "id"
            | "uri"
            | "status"
            | "state"
            | "schema"
            | "$schema"
            | "schemaVersion"
            | "protocolVersion"
            | "code"
            | "version"
            | "format"
            | "kind"
            | "type"
            | "mode"
            | "severity"
            | "outcome"
            | "receipt"
            | "cursor"
            | "epoch"
            | "scope"
            | "cursor_epoch"
            | "cursor_scope"
            | "dedupe_key"
            | "record_key"
            | "final_cursor_epoch"
            | "final_cursor_scope"
            | "claim_delivery"
    ) || [
        "_id", "_ids", "_uuid", "_uuids", "_status", "_state", "_version", "_type",
        "_code", "_mode", "_cursor", "_cursor_epoch", "_cursor_scope", "_token",
    ]
    .iter()
    .any(|suffix| key.ends_with(*suffix))
}

fn redact_output_json(
    ctx: &McpContext,
    value: &mut serde_json::Value,
    redactor: &Redactor,
    depth: usize,
    remaining_nodes: &mut usize,
    identity: bool,
    changed: &mut bool,
) -> McpResult<()> {
    if depth >= MCP_OUTPUT_REDACTION_MAX_DEPTH || *remaining_nodes == 0 {
        return Err(output_redaction_error());
    }
    *remaining_nodes -= 1;
    if *remaining_nodes % 256 == 0 {
        output_checkpoint(ctx)?;
    }
    match value {
        serde_json::Value::String(text) => {
            let redacted = redact_output_text(text, redactor)?;
            // IDs, cursors, enum discriminants and schema versions must retain
            // their authority. Refuse a matching payload instead of inventing
            // a different machine identifier or emitting an invalid schema.
            if redacted != *text {
                if identity {
                    return Err(output_redaction_error());
                }
                *text = redacted;
                *changed = true;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_output_json(
                    ctx,
                    item,
                    redactor,
                    depth + 1,
                    remaining_nodes,
                    identity,
                    changed,
                )?;
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                // Never rename object keys: replacements can collide and can
                // silently destroy extension data or required schema fields.
                if redact_output_text(key, redactor)? != *key {
                    return Err(output_redaction_error());
                }
                redact_output_json(
                    ctx,
                    item,
                    redactor,
                    depth + 1,
                    remaining_nodes,
                    identity || output_field_is_identity(key),
                    changed,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn redact_output_json_text(
    ctx: &McpContext,
    text: &str,
    redactor: &Redactor,
    remaining_nodes: &mut usize,
    allow_plain_text: bool,
) -> McpResult<String> {
    output_checkpoint(ctx)?;
    if text.len() > MCP_OUTPUT_REDACTION_MAX_BYTES {
        return Err(output_redaction_error());
    }
    let mut value = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => value,
        Err(_) if allow_plain_text => {
            // A malformed/deep JSON document must not evade structural checks
            // by falling back to a raw-text pass over escaped JSON strings.
            let first = text.trim_start().chars().next();
            if matches!(first, Some('{' | '[' | '"')) {
                return Err(output_redaction_error());
            }
            return redact_output_text(text, redactor);
        }
        Err(_) => return Err(output_redaction_error()),
    };
    let mut changed = false;
    redact_output_json(ctx, &mut value, redactor, 0, remaining_nodes, false, &mut changed)?;
    if !changed && !value.is_object() && !value.is_array() {
        // Scalar JSON can retain its original spelling and whitespace. For
        // objects/arrays always serialize the inspected tree: serde discards
        // duplicate keys, whose overwritten raw values may contain secrets.
        output_checkpoint(ctx)?;
        return Ok(text.to_string());
    }
    check_output_size(ctx, &value)?;
    serde_json::to_string(&value).map_err(|_| output_redaction_error())
}

/// Error data can be constructed directly by a handler, without serde's parse
/// depth limit. Dispose of rejected values iteratively instead of recursively.
fn discard_output_json(value: serde_json::Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Array(mut items) => pending.append(&mut items),
            serde_json::Value::Object(items) => {
                pending.extend(items.into_iter().map(|(_, value)| value));
            }
            _ => {}
        }
    }
}

fn redact_output_error(ctx: &McpContext, mut error: McpError, redactor: &Redactor) -> McpError {
    if output_checkpoint(ctx).is_err() {
        if let Some(data) = error.data.take() {
            discard_output_json(data);
        }
        return McpError::request_cancelled();
    }
    let message = redact_output_text(&error.message, redactor);
    if let Some(mut data) = error.data.take() {
        let mut remaining_nodes = MCP_OUTPUT_REDACTION_MAX_NODES;
        let mut changed = false;
        let safe = redact_output_json(
            ctx, &mut data, redactor, 0, &mut remaining_nodes, false, &mut changed,
        )
        .and_then(|()| check_output_size(ctx, &data));
        if safe.is_err() || message.is_err() {
            discard_output_json(data);
            if output_checkpoint(ctx).is_err() {
                return McpError::request_cancelled();
            }
            error.message = MCP_OUTPUT_REDACTION_ERROR.to_string();
            return error;
        }
        error.data = Some(data);
    }
    error.message = message.unwrap_or_else(|_| MCP_OUTPUT_REDACTION_ERROR.to_string());
    if check_output_size(ctx, &(&error.message, &error.data)).is_err() {
        if let Some(data) = error.data.take() {
            discard_output_json(data);
        }
        if output_checkpoint(ctx).is_err() {
            return McpError::request_cancelled();
        }
        error.message = MCP_OUTPUT_REDACTION_ERROR.to_string();
    }
    error
}

fn redact_resource_content(
    ctx: &McpContext,
    resource: &mut ResourceContent,
    redactor: &Redactor,
    remaining_nodes: &mut usize,
) -> McpResult<()> {
    if redact_output_text(&resource.uri, redactor)? != resource.uri {
        return Err(output_redaction_error());
    }
    if let Some(mime_type) = &resource.mime_type
        && redact_output_text(mime_type, redactor)? != *mime_type
    {
        return Err(output_redaction_error());
    }
    if let Some(text) = &mut resource.text {
        let is_json = resource.mime_type.as_deref().is_some_and(|mime_type| {
            let media_type = mime_type.split(';').next().unwrap_or_default().trim();
            media_type.eq_ignore_ascii_case("application/json") || media_type.ends_with("+json")
        });
        *text = redact_output_json_text(ctx, text, redactor, remaining_nodes, !is_json)?;
    }
    Ok(())
}

fn redact_tool_contents(
    ctx: &McpContext,
    mut contents: Vec<Content>,
    redactor: &Redactor,
    allow_plain_text: bool,
) -> McpResult<Vec<Content>> {
    check_output_size(ctx, &contents)?;
    let mut remaining_nodes = MCP_OUTPUT_REDACTION_MAX_NODES;
    for content in &mut contents {
        output_checkpoint(ctx)?;
        match content {
            Content::Text { text } => {
                *text = redact_output_json_text(
                    ctx,
                    text,
                    redactor,
                    &mut remaining_nodes,
                    allow_plain_text,
                )?;
            }
            Content::Resource { resource } => {
                redact_resource_content(ctx, resource, redactor, &mut remaining_nodes)?;
            }
            _ => {}
        }
    }
    check_output_size(ctx, &contents)?;
    Ok(contents)
}

fn redact_resource_contents(
    ctx: &McpContext,
    mut contents: Vec<ResourceContent>,
    redactor: &Redactor,
) -> McpResult<Vec<ResourceContent>> {
    check_output_size(ctx, &contents)?;
    let mut remaining_nodes = MCP_OUTPUT_REDACTION_MAX_NODES;
    for resource in &mut contents {
        output_checkpoint(ctx)?;
        redact_resource_content(ctx, resource, redactor, &mut remaining_nodes)?;
    }
    check_output_size(ctx, &contents)?;
    Ok(contents)
}

pub(super) struct RedactedResourceHandler<T: ResourceHandler> {
    inner: T,
    redactor: Arc<Redactor>,
}

impl<T: ResourceHandler> ResourceHandler for RedactedResourceHandler<T> {
    fn definition(&self) -> Resource {
        self.inner.definition()
    }

    fn template(&self) -> Option<ResourceTemplate> {
        self.inner.template()
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let contents = self
            .inner
            .read(ctx)
            .map_err(|error| redact_output_error(ctx, error, &self.redactor))?;
        redact_resource_contents(ctx, contents, &self.redactor)
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let contents = self
            .inner
            .read_with_uri(ctx, uri, params)
            .map_err(|error| redact_output_error(ctx, error, &self.redactor))?;
        redact_resource_contents(ctx, contents, &self.redactor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum McpOutputFormat {
    #[default]
    Json,
    Toon,
}

pub(super) const MCP_OUTPUT_FORMAT_MAX_BYTES: usize = 16;

pub(super) fn parse_mcp_output_format(raw: &str) -> Option<McpOutputFormat> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("json") {
        Some(McpOutputFormat::Json)
    } else if trimmed.eq_ignore_ascii_case("toon") {
        Some(McpOutputFormat::Toon)
    } else {
        None
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

    let raw_len = raw_format.len();
    if raw_len > MCP_OUTPUT_FORMAT_MAX_BYTES {
        return Err(format!(
            "Invalid format: value is {raw_len} bytes; max allowed is {MCP_OUTPUT_FORMAT_MAX_BYTES}"
        ));
    }

    parse_mcp_output_format(raw_format)
        .ok_or_else(|| "Invalid format: expected one of ['json', 'toon']".to_string())
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
                "maxLength": MCP_OUTPUT_FORMAT_MAX_BYTES,
                "description": "Optional output format override for this call"
            })
        });
}

#[cfg(test)]
pub(super) fn encode_mcp_contents(
    contents: Vec<Content>,
    format: McpOutputFormat,
) -> McpResult<Vec<Content>> {
    encode_mcp_contents_with_delivery_status(contents, format).0
}

/// Encode the final requested representation and report whether it faithfully
/// represents the tool payload. TOON transcode failures are returned as the
/// established FT-MCP error envelope, so the boolean is the signal that a
/// prepared delivery must be released rather than armed.
fn encode_mcp_contents_with_delivery_status(
    contents: Vec<Content>,
    format: McpOutputFormat,
) -> (McpResult<Vec<Content>>, bool) {
    match format {
        McpOutputFormat::Json => (Ok(contents), true),
        McpOutputFormat::Toon => {
            let mut encoded = Vec::with_capacity(contents.len());
            for content in contents {
                match content {
                    Content::Text { text } => {
                        let value = match serde_json::from_str::<serde_json::Value>(&text) {
                            Ok(value) => value,
                            Err(err) => {
                                return (
                                    envelope_to_content(McpEnvelope::<()>::error(
                                        MCP_ERR_INTERNAL,
                                        "Unable to transcode MCP payload to TOON",
                                        Some(format!("Tool output was not valid JSON: {err}")),
                                        0,
                                    )),
                                    false,
                                );
                            }
                        };
                        encoded.push(Content::Text {
                            text: toon_rust::encode(value, None),
                        });
                    }
                    other => encoded.push(other),
                }
            }
            (Ok(encoded), true)
        }
    }
}

/// Wrapper that allows per-call MCP output format negotiation (`json` or `toon`).
///
/// Each wrapped tool accepts an optional `format` argument in its input schema.
/// The argument is stripped before forwarding to the inner handler so existing
/// tool parameter structs do not need to change.
pub(super) struct FormatAwareToolHandler<T: ToolHandler> {
    inner: T,
    redactor: Arc<Redactor>,
    allow_plain_text: bool,
    response_delivery: Option<Arc<FrameworkResponseDeliveryCoordinator>>,
}

impl<T: ToolHandler> FormatAwareToolHandler<T> {
    fn new(inner: T, redactor: Arc<Redactor>, allow_plain_text: bool) -> Self {
        Self {
            inner,
            redactor,
            allow_plain_text,
            response_delivery: None,
        }
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
        let contents = match self.inner.call(ctx, arguments) {
            Ok(contents) => contents,
            Err(err) => {
                if let Some(response_delivery) = &self.response_delivery {
                    response_delivery.fail_prepared();
                }
                return Err(redact_output_error(ctx, err, &self.redactor));
            }
        };
        // Redact decoded JSON before format conversion so escaped strings,
        // extension fields and error envelopes obey this server's policy in
        // both JSON and TOON. A refusal must release any prepared event lease.
        let contents = match redact_tool_contents(
            ctx,
            contents,
            &self.redactor,
            self.allow_plain_text,
        ) {
            Ok(contents) => contents,
            Err(error) => {
                if let Some(response_delivery) = &self.response_delivery {
                    response_delivery.fail_prepared();
                }
                return output_refusal_contents(error, elapsed_ms(start));
            }
        };
        let (encoded, faithful) = encode_mcp_contents_with_delivery_status(contents, format);
        match encoded {
            Ok(contents) => {
                // Formatting can expand output or exhaust the caller's budget.
                // Settle those checks before arming transport-bound authority.
                if let Err(error) = check_output_size(ctx, &contents) {
                    if let Some(response_delivery) = &self.response_delivery {
                        response_delivery.fail_prepared();
                    }
                    return output_refusal_contents(error, elapsed_ms(start));
                }
                if let Some(response_delivery) = &self.response_delivery {
                    if faithful {
                        let _ = response_delivery.activate_prepared();
                    } else {
                        response_delivery.fail_prepared();
                    }
                }
                Ok(contents)
            }
            Err(err) => {
                if let Some(response_delivery) = &self.response_delivery {
                    response_delivery.fail_prepared();
                }
                Err(redact_output_error(ctx, err, &self.redactor))
            }
        }
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
    output: McpOutputPolicy,
}

#[cfg(test)]
impl<T: ToolHandler> AuditedToolHandler<T> {
    pub(super) fn new(inner: T, tool_name: impl Into<String>, db_path: Arc<PathBuf>) -> Self {
        McpOutputPolicy::new(&crate::config::RedactionConfig::default())
            .expect("default MCP audit redaction")
            .audited_tool(inner, tool_name, db_path)
    }
}

impl<T: ToolHandler> ToolHandler for AuditedToolHandler<T> {
    fn definition(&self) -> Tool {
        self.inner.definition()
    }

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let capacity_timer = crate::runtime_telemetry::SwarmCapacityStageTimer::start(
            crate::runtime_telemetry::SwarmCapacityStage::RobotMcp,
            0,
        );
        let start = Instant::now();
        let raw_args = arguments.clone();

        // ft-ymn10: pre-flight Cx checkpoint. The wrapper is the SOLE
        // audit point for tool calls; if `inner.call` is invoked on an
        // already-cancelled or budget-exhausted Cx the caller's deadline
        // is silently violated and the inner tool runs to completion
        // anyway. This slice catches the pre-expired case and writes an
        // audit row reflecting the violation. Mid-call hangs require
        // ToolHandler trait surgery (option B in ft-ymn10) tracked
        // separately; this is the bounded option-A++ that closes the
        // pre-expired-budget hole.
        if let Err(cx_err) = ctx.cx().checkpoint() {
            let error_code = "deadline_exceeded".to_string();
            record_mcp_audit_sync_with_policy(
                &self.output,
                &self.db_path,
                &self.tool_name,
                &raw_args,
                false,
                Some(&error_code),
                elapsed_ms(start),
                None,
            );
            let bail: McpResult<Vec<Content>> = Err(McpError::internal_error(format!(
                "Cx pre-flight checkpoint failed before tool {} dispatch: {cx_err}",
                self.tool_name
            )));
            capacity_timer.finish_result(&bail);
            return bail;
        }

        let result = self.inner.call(ctx, arguments);

        let (ok, error_code) = classify_audited_tool_result(&self.tool_name, &result);

        // br-ft-2fjx0: pass None to preserve the backward-compat
        // 10s retry deadline default. The caller-budget plumbing
        // path (use ctx.cx.budget().deadline() to derive the
        // remaining request window and pass Some(d) here) is
        // deferred to a follow-up commit so this fix stays
        // bounded — the parameter ships now so future callers
        // have a place to thread their budget through.
        record_mcp_audit_sync_with_policy(
            &self.output,
            &self.db_path,
            &self.tool_name,
            &raw_args,
            ok,
            error_code.as_deref(),
            elapsed_ms(start),
            None,
        );

        capacity_timer.finish_result(&result);
        result
    }
}

/// Classify a tool-call result for audit-row construction
/// (br-ft-uesdi).
///
/// Returns `(ok, error_code)` where `ok=true` means the tool
/// succeeded and `error_code` describes the failure mode otherwise.
///
/// Pre-fix the implementation defaulted to `ok=true` when the
/// envelope was unparseable — silently promoting "we don't know"
/// to "success" in the audit log. Post-fix every unparseable
/// shape lands as `ok=false, error_code=Some("envelope_unparsable")`
/// so operators can distinguish:
///
/// - genuine `{"ok":true}` → (true, None)
/// - genuine `{"ok":false, "error_code":"X"}` → (false, Some("X"))
/// - Err arm from inner.call → (false, Some("MCP_INTERNAL"))
/// - empty Vec / non-Text first content / non-JSON Text /
///   JSON missing `ok` field / non-bool `ok` →
///   (false, Some("envelope_unparsable"))
fn classify_tool_result(result: &McpResult<Vec<Content>>) -> (bool, Option<String>) {
    let contents = match result {
        Ok(c) => c,
        Err(_) => return (false, Some("MCP_INTERNAL".to_string())),
    };

    let parsed: Option<serde_json::Value> = contents.first().and_then(|c| match c {
        Content::Text { text } => serde_json::from_str::<serde_json::Value>(text).ok(),
        _ => None,
    });

    match parsed
        .as_ref()
        .and_then(|v| v.get("ok").and_then(|o| o.as_bool()))
    {
        Some(true) => (true, None),
        Some(false) => {
            let code = parsed.and_then(|v| {
                v.get("error_code")
                    .and_then(|c| c.as_str())
                    .map(String::from)
            });
            (false, code)
        }
        None => {
            // Envelope did not carry a parseable `ok: bool` field.
            // Fail-closed with a distinct error_code so an
            // operator can tell "tool reported failure"
            // (Some(false) above) from "tool emitted an
            // unparseable envelope" (this arm). The latter is a
            // tool-contract violation worth investigating.
            (false, Some("envelope_unparsable".to_string()))
        }
    }
}

fn classify_audited_tool_result(
    tool_name: &str,
    result: &McpResult<Vec<Content>>,
) -> (bool, Option<String>) {
    if tool_name.starts_with("remote/") {
        return match result {
            Ok(contents) => {
                let parsed: Option<serde_json::Value> = contents.first().and_then(|c| match c {
                    Content::Text { text } => serde_json::from_str::<serde_json::Value>(text).ok(),
                    _ => None,
                });
                match parsed
                    .as_ref()
                    .and_then(|v| v.get("ok").and_then(|o| o.as_bool()))
                {
                    Some(false) => {
                        let code = parsed.and_then(|v| {
                            v.get("error_code")
                                .and_then(|c| c.as_str())
                                .map(String::from)
                        });
                        (false, code)
                    }
                    _ => (true, None),
                }
            }
            Err(_) => (false, Some("MCP_INTERNAL".to_string())),
        };
    }

    classify_tool_result(result)
}

#[cfg(test)]
mod tests {
    use super::{
        AuditedToolHandler, MCP_OUTPUT_FORMAT_MAX_BYTES, MCP_OUTPUT_REDACTION_ERROR,
        MCP_OUTPUT_REDACTION_MAX_BYTES, MCP_OUTPUT_REDACTION_MAX_DEPTH,
        MCP_OUTPUT_REDACTION_MAX_NODES, McpOutputFormat, McpOutputPolicy,
        augment_tool_schema_with_format, check_output_size, classify_audited_tool_result,
        classify_tool_result, encode_mcp_contents, extract_mcp_output_format,
        parse_mcp_output_format,
    };
    use crate::mcp_framework::{
        FrameworkContent as Content, FrameworkMcpContext as McpContext,
        FrameworkMcpError as McpError, FrameworkMcpResult as McpResult,
        FrameworkResource as Resource, FrameworkResourceContent as ResourceContent,
        FrameworkResourceHandler as ResourceHandler, FrameworkResourceTemplate as ResourceTemplate,
        FrameworkResponseDeliveryCoordinator, FrameworkResponseDeliveryOutcome,
        FrameworkTool as Tool, FrameworkToolHandler as ToolHandler,
    };
    use proptest::prelude::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

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
        assert_eq!(parse_mcp_output_format("json"), Some(McpOutputFormat::Json));
    }

    #[test]
    fn parse_format_toon() {
        assert_eq!(parse_mcp_output_format("toon"), Some(McpOutputFormat::Toon));
    }

    #[test]
    fn parse_format_case_insensitive() {
        assert_eq!(parse_mcp_output_format("JSON"), Some(McpOutputFormat::Json));
        assert_eq!(parse_mcp_output_format("Toon"), Some(McpOutputFormat::Toon));
        assert_eq!(parse_mcp_output_format("TOON"), Some(McpOutputFormat::Toon));
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
        let err = result.unwrap_err();
        assert!(err.contains("expected one of"));
        assert!(!err.contains("xml"));
    }

    #[test]
    fn extract_format_oversized_string_returns_bounded_redacted_error() {
        let secret = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        let oversized = format!("{secret}{}", "x".repeat(MCP_OUTPUT_FORMAT_MAX_BYTES + 1));
        let mut args = serde_json::json!({"format": oversized});

        let err = extract_mcp_output_format(&mut args).unwrap_err();

        assert!(err.contains("max allowed"));
        assert!(!err.contains(secret));
        assert!(args.get("format").is_none());
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
        assert_eq!(
            format_prop.get("maxLength").unwrap().as_u64(),
            Some(MCP_OUTPUT_FORMAT_MAX_BYTES as u64)
        );
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
            assert_ne!(text, "");
        }
    }

    #[test]
    fn encode_toon_invalid_json_returns_error_envelope() {
        let contents = vec![Content::Text {
            text: "not valid json sk-ant-api03-middleware-secret".to_string(),
        }];
        let result = encode_mcp_contents(contents, McpOutputFormat::Toon)
            .expect("malformed tool output should return an FT-MCP envelope");

        assert_eq!(result.len(), 1);
        let text = match &result[0] {
            Content::Text { text } => text,
            other => panic!("expected text envelope, got {other:?}"),
        };
        let envelope: serde_json::Value =
            serde_json::from_str(text).expect("transcode failure envelope is JSON");
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], crate::mcp_error::MCP_ERR_INTERNAL);
        assert_eq!(envelope["error"], "Unable to transcode MCP payload to TOON");
        assert!(
            !envelope.to_string().contains("sk-ant-api03-"),
            "malformed MCP tool output leaked through transcode failure envelope"
        );
    }

    #[test]
    fn encode_empty_contents() {
        let result = encode_mcp_contents(vec![], McpOutputFormat::Json).unwrap();
        assert!(result.is_empty());

        let result = encode_mcp_contents(vec![], McpOutputFormat::Toon).unwrap();
        assert!(result.is_empty());
    }

    const CUSTOM_SECRET: &str = "orchid-private-value";

    fn configured_output_policy(secret: &str, placeholder: &str) -> McpOutputPolicy {
        McpOutputPolicy::new(&crate::config::RedactionConfig {
            patterns: vec![regex::escape(secret)],
            placeholder: placeholder.to_string(),
            ..Default::default()
        })
        .unwrap()
    }

    fn output_context() -> McpContext {
        McpContext::new(crate::mcp_framework::FrameworkCx::for_testing(), 1)
    }

    fn output_text(contents: &[Content]) -> &str {
        let [Content::Text { text }] = contents else {
            panic!("expected one text response, got {contents:?}");
        };
        text
    }

    fn output_test_tool() -> Tool {
        Tool {
            name: "test.configured_output".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: None,
        }
    }

    type OutputCompletion = Arc<Mutex<Option<FrameworkResponseDeliveryOutcome>>>;

    struct OutputFixtureTool {
        contents: Vec<Content>,
        delivery: Option<(Arc<FrameworkResponseDeliveryCoordinator>, OutputCompletion)>,
    }

    impl OutputFixtureTool {
        fn json(value: serde_json::Value) -> Self {
            Self::text(value.to_string())
        }

        fn text(text: String) -> Self {
            Self {
                contents: vec![Content::Text { text }],
                delivery: None,
            }
        }

        fn with_delivery(
            mut self,
            coordinator: Arc<FrameworkResponseDeliveryCoordinator>,
            completion: OutputCompletion,
        ) -> Self {
            self.delivery = Some((coordinator, completion));
            self
        }
    }

    impl ToolHandler for OutputFixtureTool {
        fn definition(&self) -> Tool {
            output_test_tool()
        }

        fn call(&self, _ctx: &McpContext, _arguments: serde_json::Value) -> McpResult<Vec<Content>> {
            if let Some((coordinator, completion)) = &self.delivery {
                let completion = Arc::clone(completion);
                let action = Box::new(move |outcome| {
                    *completion.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(outcome);
                });
                if let Err(action) = coordinator.try_prepare(action) {
                    action(FrameworkResponseDeliveryOutcome::Failed);
                    panic!("test delivery was already prepared");
                }
            }
            Ok(self.contents.clone())
        }
    }

    struct OutputErrorTool(serde_json::Value);

    impl ToolHandler for OutputErrorTool {
        fn definition(&self) -> Tool {
            output_test_tool()
        }

        fn call(&self, _ctx: &McpContext, _arguments: serde_json::Value) -> McpResult<Vec<Content>> {
            let mut error = McpError::internal_error("backend orchid-\x1b[31mprivate-value\x1b[0m");
            error.data = Some(self.0.clone());
            Err(error)
        }
    }

    struct OutputFixtureResource {
        text: String,
        mime_type: String,
    }

    impl ResourceHandler for OutputFixtureResource {
        fn definition(&self) -> Resource {
            Resource {
                uri: "wa://test/output".to_string(),
                name: "test output".to_string(),
                description: None,
                mime_type: Some(self.mime_type.clone()),
                icon: None,
                version: None,
                tags: Vec::new(),
            }
        }

        fn template(&self) -> Option<ResourceTemplate> {
            Some(ResourceTemplate {
                uri_template: "wa://test/output/{item}".to_string(),
                name: "test output template".to_string(),
                description: None,
                mime_type: Some(self.mime_type.clone()),
                icon: None,
                version: None,
                tags: Vec::new(),
            })
        }

        fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            self.read_with_uri(ctx, "wa://test/output", &HashMap::new())
        }

        fn read_with_uri(
            &self,
            _ctx: &McpContext,
            uri: &str,
            params: &HashMap<String, String>,
        ) -> McpResult<Vec<ResourceContent>> {
            Ok(vec![ResourceContent {
                uri: uri.to_string(),
                mime_type: Some(self.mime_type.clone()),
                text: Some(params.get("message").unwrap_or(&self.text).clone()),
                blob: None,
            }])
        }
    }

    #[test]
    fn configured_mcp_output_redacts_custom_secrets_before_json_and_toon() {
        let builtin = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        assert_eq!(crate::redactor::Redactor::new().redact(CUSTOM_SECRET), CUSTOM_SECRET);
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let payload = serde_json::json!({
            "ok": true,
            "data": {
                "content": "orchid-\x1b[31mprivate-value\x1b[0m",
                "query": CUSTOM_SECRET,
                "extra": [{"snippet": format!("before {CUSTOM_SECRET} after")}],
                "builtin": builtin,
                "pane_id": 42,
                "workflow_id": "workflow-safe"
            }
        });
        let expected = serde_json::json!({
            "ok": true,
            "data": {
                "content": "$0", "query": "$0",
                "extra": [{"snippet": "before $0 after"}],
                "builtin": "$0", "pane_id": 42, "workflow_id": "workflow-safe"
            }
        });
        for format in ["json", "toon"] {
            let contents = output
                .tool(OutputFixtureTool::json(payload.clone()))
                .call(&output_context(), serde_json::json!({"format": format}))
                .unwrap();
            let text = output_text(&contents);
            assert!(!text.contains(CUSTOM_SECRET));
            assert!(!text.contains(builtin));
            if format == "json" {
                assert_eq!(serde_json::from_str::<serde_json::Value>(text).unwrap(), expected);
            } else {
                assert_eq!(text, toon_rust::encode(expected.clone(), None));
            }
        }
    }

    #[test]
    fn configured_mcp_output_is_isolated_between_server_policies() {
        let first = configured_output_policy(CUSTOM_SECRET, "[FIRST]");
        let second = configured_output_policy("violet-private-value", "[SECOND]");
        let source = format!("{CUSTOM_SECRET} violet-private-value");
        for (policy, expected) in [
            (&first, "[FIRST] violet-private-value"),
            (&second, "orchid-private-value [SECOND]"),
            (&first, "[FIRST] violet-private-value"),
        ] {
            let contents = policy
                .tool(OutputFixtureTool::json(serde_json::json!({"data": source})))
                .call(&output_context(), serde_json::json!({}))
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(output_text(&contents)).unwrap();
            assert_eq!(value["data"], expected);
            let resources = policy
                .resource(OutputFixtureResource {
                    text: source.clone(),
                    mime_type: "text/plain".to_string(),
                })
                .read(&output_context())
                .unwrap();
            assert_eq!(resources[0].text.as_deref(), Some(expected));
        }
    }

    #[test]
    fn configured_mcp_output_protects_actual_tool_and_resource_errors() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let contents = output
            .tool(super::super::mcp_tools::WaRulesListTool)
            .call(&output_context(), serde_json::json!({"agent_type": CUSTOM_SECRET}))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(output_text(&contents)).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error_code"], crate::mcp_error::MCP_ERR_INVALID_ARGS);
        assert!(value["error"].as_str().unwrap().contains("$0"));
        assert!(!output_text(&contents).contains(CUSTOM_SECRET));

        let resources = output
            .resource(super::super::mcp_resources::WaRulesByAgentTemplateResource)
            .read_with_uri(
                &output_context(),
                "wa://rules/safe",
                &HashMap::from([("agent_type".to_string(), CUSTOM_SECRET.to_string())]),
            )
            .unwrap();
        let text = resources[0].text.as_deref().unwrap();
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().unwrap().contains("$0"));
        assert!(!text.contains(CUSTOM_SECRET));
        assert_eq!(resources[0].uri, "wa://rules/safe");
    }

    #[test]
    fn configured_mcp_output_protects_framework_error_data() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let error = output
            .tool(OutputErrorTool(serde_json::json!({
                "details": [CUSTOM_SECRET], "pane_id": 42
            })))
            .call(&output_context(), serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, McpError::internal_error("").code);
        assert_eq!(error.message, "backend $0");
        assert_eq!(error.data, Some(serde_json::json!({"details": ["$0"], "pane_id": 42})));

        let mut deep = serde_json::json!(CUSTOM_SECRET);
        for _ in 0..MCP_OUTPUT_REDACTION_MAX_DEPTH + 1 {
            deep = serde_json::Value::Array(vec![deep]);
        }
        for data in [serde_json::json!({(CUSTOM_SECRET): "value"}), deep] {
            let error = output
                .tool(OutputErrorTool(data))
                .call(&output_context(), serde_json::json!({}))
                .unwrap_err();
            assert_eq!(error.message, MCP_OUTPUT_REDACTION_ERROR);
            assert!(error.data.is_none());
        }
    }

    #[test]
    fn configured_mcp_output_covers_resource_reads_and_templates() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let handler = output.resource(OutputFixtureResource {
            text: serde_json::json!({"data": {"text": CUSTOM_SECRET}}).to_string(),
            mime_type: "application/json; charset=utf-8".to_string(),
        });
        assert_eq!(handler.definition().uri, "wa://test/output");
        assert_eq!(handler.template().unwrap().uri_template, "wa://test/output/{item}");
        let expected = serde_json::json!({"data": {"text": "$0"}});
        let direct = handler.read(&output_context()).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(direct[0].text.as_deref().unwrap()).unwrap(),
            expected
        );
        let params = HashMap::from([(
            "message".to_string(),
            serde_json::json!({"data": {"text": "orchid-\x1b[31mprivate-value"}}).to_string(),
        )]);
        let templated = handler
            .read_with_uri(&output_context(), "wa://test/output/1", &params)
            .unwrap();
        assert_eq!(templated[0].uri, "wa://test/output/1");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(templated[0].text.as_deref().unwrap()).unwrap(),
            expected
        );
        let error = handler
            .read_with_uri(&output_context(), &format!("wa://test/{CUSTOM_SECRET}"), &params)
            .unwrap_err();
        assert_eq!(error.message, MCP_OUTPUT_REDACTION_ERROR);
    }

    #[test]
    fn configured_mcp_output_refuses_secret_keys_and_rewritten_protocol_identity() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let mut cases = vec![serde_json::json!({(CUSTOM_SECRET): "safe", "$0": "other"})];
        for key in [
            "workflow_id", "pane_uuids", "status", "schema_version", "schemaVersion",
            "protocolVersion", "code", "cursor", "epoch", "scope", "cursor_epoch",
            "cursor_scope", "dedupe_key", "record_key", "next_cursor_epoch",
            "next_cursor_scope", "final_cursor_epoch", "final_cursor_scope", "claim_delivery",
        ] {
            cases.push(serde_json::json!({(key): CUSTOM_SECRET}));
        }
        for data in cases {
            let contents = output
                .tool(OutputFixtureTool::json(serde_json::json!({"ok": true, "data": data})))
                .call(&output_context(), serde_json::json!({}))
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(output_text(&contents)).unwrap();
            assert_eq!(value["ok"], false);
            assert_eq!(value["error"], MCP_OUTPUT_REDACTION_ERROR);
            assert!(!output_text(&contents).contains(CUSTOM_SECRET));
        }
    }

    #[test]
    fn configured_mcp_output_preserves_safe_explicit_escape_views() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let safe = "\x1b[31mordinary text\x1b[0m";
        let contents = output
            .tool(OutputFixtureTool::json(serde_json::json!({"data": {"text": safe}})))
            .call(&output_context(), serde_json::json!({}))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(output_text(&contents)).unwrap();
        assert_eq!(value["data"]["text"], safe);
    }

    #[test]
    fn configured_mcp_output_refuses_uninspected_deep_or_wide_json() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let mut deep = serde_json::json!(CUSTOM_SECRET);
        for _ in 0..MCP_OUTPUT_REDACTION_MAX_DEPTH {
            deep = serde_json::Value::Array(vec![deep]);
        }
        let mut wide = vec![serde_json::Value::Null; MCP_OUTPUT_REDACTION_MAX_NODES];
        wide.push(serde_json::json!(CUSTOM_SECRET));
        for data in [deep, serde_json::Value::Array(wide)] {
            let contents = output
                .tool(OutputFixtureTool::json(serde_json::json!({"ok": true, "data": data})))
                .call(&output_context(), serde_json::json!({}))
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(output_text(&contents)).unwrap();
            assert_eq!(value["ok"], false);
            assert_eq!(value["error"], MCP_OUTPUT_REDACTION_ERROR);
            assert!(!output_text(&contents).contains(CUSTOM_SECRET));
        }
    }

    #[test]
    fn configured_mcp_output_counts_escaped_serialized_bytes() {
        // The decoded string fits; JSON escaping exceeds the response budget.
        let text = "\0".repeat(MCP_OUTPUT_REDACTION_MAX_BYTES / 6 + 1);
        assert!(text.len() < MCP_OUTPUT_REDACTION_MAX_BYTES);
        assert_eq!(
            check_output_size(&output_context(), &text).unwrap_err().message,
            MCP_OUTPUT_REDACTION_ERROR
        );
    }

    #[test]
    fn configured_mcp_output_preserves_proxy_plain_text_and_scrubs_embedded_resources() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let contents = output
            .proxy_tool(OutputFixtureTool {
                contents: vec![
                    Content::Text { text: "orchid-\x1b[31mprivate-value".to_string() },
                    Content::Resource {
                        resource: ResourceContent {
                            uri: "wa://remote/safe".to_string(),
                            mime_type: Some("application/json".to_string()),
                            text: Some(serde_json::json!({"text": CUSTOM_SECRET}).to_string()),
                            blob: None,
                        },
                    },
                ],
                delivery: None,
            })
            .call(&output_context(), serde_json::json!({}))
            .unwrap();
        assert!(matches!(&contents[0], Content::Text { text } if text == "$0"));
        let Content::Resource { resource } = &contents[1] else {
            panic!("embedded resource was not preserved");
        };
        assert_eq!(resource.uri, "wa://remote/safe");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(resource.text.as_deref().unwrap()).unwrap(),
            serde_json::json!({"text": "$0"})
        );

        let malformed = format!(r#"{{"text":"{CUSTOM_SECRET}""#);
        let refused = output
            .proxy_tool(OutputFixtureTool::text(malformed))
            .call(&output_context(), serde_json::json!({}))
            .unwrap();
        assert!(!output_text(&refused).contains(CUSTOM_SECRET));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(output_text(&refused)).unwrap()["ok"],
            false
        );
    }

    #[test]
    fn configured_mcp_output_preserves_unchanged_proxy_json_scalars_exactly() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        for text in [
            "1.00",
            " 42\n",
            "\"ordinary text\"",
        ] {
            let contents = output
                .proxy_tool(OutputFixtureTool::text(text.to_string()))
                .call(&output_context(), serde_json::json!({"format": "json"}))
                .unwrap();
            assert_eq!(output_text(&contents), text);
        }
    }

    #[test]
    fn configured_mcp_output_never_returns_uninspected_duplicate_values() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let cases = [
            (
                r#"{"ok":true,"data":{"text":"orchid-private-value","text":"safe"}}"#,
                serde_json::json!({"ok": true, "data": {"text": "safe"}}),
            ),
            (
                r#"{"ok":true,"data":{"items":[{"text":"orchid-\u001b[31mprivate-value\u001b[0m","text":"safe"}]}}"#,
                serde_json::json!({"ok": true, "data": {"items": [{"text": "safe"}]}}),
            ),
            (
                r#"{"ok":true,"data":{"text":"safe","text":"orchid-\u001b[31mprivate-value\u001b[0m"}}"#,
                serde_json::json!({"ok": true, "data": {"text": "$0"}}),
            ),
        ];
        for (payload, expected) in cases {
            for proxy in [false, true] {
                let inner = OutputFixtureTool::text(payload.to_string());
                let handler = if proxy { output.proxy_tool(inner) } else { output.tool(inner) };
                let contents = handler.call(&output_context(), serde_json::json!({})).unwrap();
                let text = output_text(&contents);
                assert!(!text.contains(CUSTOM_SECRET));
                assert!(!text.contains("private-value"));
                assert_eq!(serde_json::from_str::<serde_json::Value>(text).unwrap(), expected);
            }
        }
    }

    #[test]
    fn configured_mcp_output_duplicate_identity_settles_only_inspected_payload() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        for (payload, refused) in [
            (
                r#"{"ok":true,"data":{"final_cursor_epoch":"safe","final_cursor_epoch":"orchid-\u001b[31mprivate-value"}}"#,
                true,
            ),
            (
                r#"{"ok":true,"data":{"final_cursor_epoch":"orchid-private-value","final_cursor_epoch":"safe"}}"#,
                false,
            ),
        ] {
            let coordinator = Arc::new(FrameworkResponseDeliveryCoordinator::default());
            let completion = Arc::new(Mutex::new(None));
            let contents = output
                .tool_with_response_delivery(
                    OutputFixtureTool::text(payload.to_string())
                        .with_delivery(Arc::clone(&coordinator), Arc::clone(&completion)),
                    Arc::clone(&coordinator),
                )
                .call(&output_context(), serde_json::json!({}))
                .unwrap();
            assert!(!output_text(&contents).contains("private-value"));
            let value: serde_json::Value = serde_json::from_str(output_text(&contents)).unwrap();
            if refused {
                assert_eq!(value["ok"], false);
                assert_eq!(*completion.lock().unwrap(), Some(FrameworkResponseDeliveryOutcome::Failed));
            } else {
                assert_eq!(value, serde_json::json!({
                    "ok": true, "data": {"final_cursor_epoch": "safe"}
                }));
                assert_eq!(*completion.lock().unwrap(), None);
            }
            assert!(!coordinator.activate_prepared());
            coordinator.fail_all();
            assert_eq!(*completion.lock().unwrap(), Some(FrameworkResponseDeliveryOutcome::Failed));
        }
    }

    #[test]
    fn configured_mcp_output_arms_delivery_only_after_redaction_and_formatting() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        for format in ["json", "toon"] {
            let coordinator = Arc::new(FrameworkResponseDeliveryCoordinator::default());
            let completion = Arc::new(Mutex::new(None));
            let payload = serde_json::json!({
                "ok": true,
                "data": {
                    "type": "await_event",
                    "final_cursor": 42,
                    "final_cursor_epoch": "epoch-safe",
                    "final_cursor_scope": "scope-safe",
                    "claim_delivery": "pending_transport",
                    "events": [{"id": 42, "matched_text": CUSTOM_SECRET}]
                }
            });
            let mut expected = payload.clone();
            expected["data"]["events"][0]["matched_text"] = serde_json::json!("$0");
            let contents = output
                .tool_with_response_delivery(
                    OutputFixtureTool::json(payload).with_delivery(
                        Arc::clone(&coordinator), Arc::clone(&completion),
                    ),
                    Arc::clone(&coordinator),
                )
                .call(&output_context(), serde_json::json!({"format": format}))
                .unwrap();
            if format == "json" {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(output_text(&contents)).unwrap(),
                    expected
                );
            } else {
                assert_eq!(output_text(&contents), toon_rust::encode(expected, None));
            }
            assert_eq!(*completion.lock().unwrap(), None);
            assert!(!coordinator.activate_prepared(), "delivery must already be armed");
            coordinator.fail_all();
            assert_eq!(*completion.lock().unwrap(), Some(FrameworkResponseDeliveryOutcome::Failed));
        }
    }

    #[test]
    fn configured_mcp_output_refusal_releases_prepared_delivery() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        for key in [
            "final_cursor_epoch", "cursor", "cursor_epoch", "cursor_scope",
            "epoch", "scope", "dedupe_key", "record_key",
        ] {
            let coordinator = Arc::new(FrameworkResponseDeliveryCoordinator::default());
            let completion = Arc::new(Mutex::new(None));
            let contents = output
                .tool_with_response_delivery(
                    OutputFixtureTool::json(serde_json::json!({
                        "ok": true, "data": {(key): CUSTOM_SECRET}
                    }))
                    .with_delivery(Arc::clone(&coordinator), Arc::clone(&completion)),
                    Arc::clone(&coordinator),
                )
                .call(&output_context(), serde_json::json!({}))
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(output_text(&contents)).unwrap();
            assert_eq!(value["ok"], false);
            assert_eq!(value["error"], MCP_OUTPUT_REDACTION_ERROR);
            assert!(!output_text(&contents).contains(CUSTOM_SECRET));
            assert_eq!(
                *completion.lock().unwrap(),
                Some(FrameworkResponseDeliveryOutcome::Failed)
            );
            assert!(!coordinator.activate_prepared(), "{key}: refused output must release its lease");
        }
    }

    #[test]
    fn configured_mcp_output_cancellation_releases_prepared_delivery() {
        let output = configured_output_policy(CUSTOM_SECRET, "$0");
        let coordinator = Arc::new(FrameworkResponseDeliveryCoordinator::default());
        let completion = Arc::new(Mutex::new(None));
        let handler = output.tool_with_response_delivery(
            OutputFixtureTool::json(serde_json::json!({"ok": true, "text": CUSTOM_SECRET}))
                .with_delivery(Arc::clone(&coordinator), Arc::clone(&completion)),
            Arc::clone(&coordinator),
        );
        let cx = crate::mcp_framework::FrameworkCx::for_testing_with_budget(
            crate::mcp_framework::FrameworkBudget::with_deadline_at_ns(1),
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        let error = handler
            .call(&McpContext::new(cx, 1), serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, McpError::request_cancelled().code);
        assert_eq!(*completion.lock().unwrap(), Some(FrameworkResponseDeliveryOutcome::Failed));
        assert!(!coordinator.activate_prepared());
    }

    struct PreparedMalformedPayloadTool {
        coordinator: Arc<FrameworkResponseDeliveryCoordinator>,
        completion: Arc<Mutex<Option<FrameworkResponseDeliveryOutcome>>>,
    }

    impl ToolHandler for PreparedMalformedPayloadTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "test.prepared_malformed_payload".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(
            &self,
            _ctx: &McpContext,
            _arguments: serde_json::Value,
        ) -> McpResult<Vec<Content>> {
            let completion = Arc::clone(&self.completion);
            let action = Box::new(move |outcome| {
                *completion
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
            });
            if let Err(action) = self.coordinator.try_prepare(action) {
                action(FrameworkResponseDeliveryOutcome::Failed);
                panic!("test coordinator unexpectedly contained another delivery");
            }
            Ok(vec![Content::Text {
                text: "not valid JSON for the requested TOON representation".to_string(),
            }])
        }
    }

    #[test]
    fn format_aware_toon_transcode_failure_releases_prepared_delivery() {
        let coordinator = Arc::new(FrameworkResponseDeliveryCoordinator::default());
        let completion = Arc::new(Mutex::new(None));
        let output = McpOutputPolicy::new(&crate::config::RedactionConfig::default()).unwrap();
        let handler = output.tool_with_response_delivery(
            PreparedMalformedPayloadTool {
                coordinator: Arc::clone(&coordinator),
                completion: Arc::clone(&completion),
            },
            Arc::clone(&coordinator),
        );

        let contents = handler
            .call(
                &McpContext::new(crate::mcp_framework::FrameworkCx::for_testing(), 1),
                serde_json::json!({"format": "toon"}),
            )
            .expect("TOON transcode failure should return the established error envelope");
        let Content::Text { text } = &contents[0] else {
            panic!("expected text error envelope");
        };
        let envelope: serde_json::Value =
            serde_json::from_str(text).expect("transcode error envelope should be JSON");
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], crate::mcp_error::MCP_ERR_INTERNAL);
        assert_eq!(
            *completion
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(FrameworkResponseDeliveryOutcome::Failed)
        );
        assert!(
            !coordinator.activate_prepared(),
            "failed final serialization must not leave an armable delivery"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn prop_parse_mcp_output_format_normalizes_case_and_whitespace(
            use_toon in any::<bool>(),
            leading_ws in "[ \t\n]{0,4}",
            trailing_ws in "[ \t\n]{0,4}",
            upper in any::<bool>(),
        ) {
            let base = if use_toon { "toon" } else { "json" };
            let token = if upper {
                base.to_ascii_uppercase()
            } else {
                let mut chars = base.chars();
                let first = chars.next().unwrap().to_ascii_uppercase();
                format!("{first}{}", chars.as_str())
            };
            let raw = format!("{leading_ws}{token}{trailing_ws}");
            let expected = if use_toon {
                McpOutputFormat::Toon
            } else {
                McpOutputFormat::Json
            };

            prop_assert_eq!(parse_mcp_output_format(&raw), Some(expected));
        }

        #[test]
        fn prop_extract_mcp_output_format_removes_only_format_key(
            use_toon in any::<bool>(),
            pane_id in any::<u64>(),
            dry_run in any::<bool>(),
        ) {
            let mut args = serde_json::json!({
                "pane_id": pane_id,
                "dry_run": dry_run,
                "format": if use_toon { "toon" } else { "json" },
            });

            let extracted = extract_mcp_output_format(&mut args).expect("format should parse");
            let expected = if use_toon {
                McpOutputFormat::Toon
            } else {
                McpOutputFormat::Json
            };

            prop_assert_eq!(extracted, expected);
            prop_assert!(args.get("format").is_none());
            prop_assert_eq!(args["pane_id"].as_u64(), Some(pane_id));
            prop_assert_eq!(args["dry_run"].as_bool(), Some(dry_run));
        }

        #[test]
        fn prop_augment_tool_schema_preserves_existing_properties(
            existing_key in "[A-Za-z_][A-Za-z0-9_]{0,12}",
        ) {
            prop_assume!(existing_key != "format");
            let mut schema = serde_json::json!({
                "type": "object",
                "properties": {
                    existing_key.clone(): {"type": "integer"}
                }
            });

            augment_tool_schema_with_format(&mut schema);

            let properties = schema["properties"].as_object().expect("properties object");
            prop_assert!(properties.contains_key(&existing_key));
            prop_assert!(properties.contains_key("format"));
            prop_assert_eq!(properties[&existing_key]["type"].as_str(), Some("integer"));
            prop_assert_eq!(properties["format"]["type"].as_str(), Some("string"));
        }
    }

    // ========================================================================
    // br-ft-uesdi: classify_tool_result fail-closed semantics.
    //
    // Pre-fix the AuditedToolHandler defaulted to ok=true on any
    // envelope-parse failure. Post-fix every unparseable shape
    // lands as (false, Some("envelope_unparsable")). These tests
    // pin the eight distinct shapes spelled out in the bead's
    // acceptance section.
    // ========================================================================

    fn ok_text(value: serde_json::Value) -> Vec<Content> {
        vec![Content::Text {
            text: value.to_string(),
        }]
    }

    #[test]
    fn classify_genuine_ok_true_envelope() {
        let result = Ok(ok_text(serde_json::json!({"ok": true, "data": {"v": 1}})));
        assert_eq!(classify_tool_result(&result), (true, None));
    }

    #[test]
    fn classify_genuine_ok_false_envelope_with_error_code() {
        let result = Ok(ok_text(serde_json::json!({
            "ok": false,
            "error_code": "robot.policy_denied",
        })));
        assert_eq!(
            classify_tool_result(&result),
            (false, Some("robot.policy_denied".to_string()))
        );
    }

    #[test]
    fn classify_genuine_ok_false_envelope_without_error_code() {
        let result = Ok(ok_text(serde_json::json!({"ok": false})));
        assert_eq!(classify_tool_result(&result), (false, None));
    }

    #[test]
    fn classify_empty_contents_fails_closed_with_unparsable_code() {
        // br-ft-uesdi acceptance: empty Vec must NOT default to
        // ok=true. Pre-fix this case lands ok=true silently.
        let result: McpResult<Vec<Content>> = Ok(Vec::new());
        assert_eq!(
            classify_tool_result(&result),
            (false, Some("envelope_unparsable".to_string()))
        );
    }

    #[test]
    fn classify_invalid_json_text_fails_closed_with_unparsable_code() {
        let result = Ok(vec![Content::Text {
            text: "not-json-at-all {{".to_string(),
        }]);
        assert_eq!(
            classify_tool_result(&result),
            (false, Some("envelope_unparsable".to_string()))
        );
    }

    #[test]
    fn classify_json_without_ok_field_fails_closed_with_unparsable_code() {
        // Tool returned valid JSON but no `"ok"` key. Pre-fix this
        // landed as ok=true via unwrap_or(true).
        let result = Ok(ok_text(serde_json::json!({"data": "no ok field here"})));
        assert_eq!(
            classify_tool_result(&result),
            (false, Some("envelope_unparsable".to_string()))
        );
    }

    #[test]
    fn classify_json_with_non_bool_ok_field_fails_closed() {
        // `"ok"` is not a bool — pre-fix landed as ok=true.
        let result = Ok(ok_text(serde_json::json!({"ok": "yes"})));
        assert_eq!(
            classify_tool_result(&result),
            (false, Some("envelope_unparsable".to_string()))
        );
    }

    #[test]
    fn classify_first_content_not_text_fails_closed() {
        // Image content (or any non-Text variant) — pre-fix
        // landed as ok=true.
        let result = Ok(vec![Content::Image {
            data: "iVBORw0KGgo=".to_string(),
            mime_type: "image/png".to_string(),
        }]);
        assert_eq!(
            classify_tool_result(&result),
            (false, Some("envelope_unparsable".to_string()))
        );
    }

    #[test]
    fn classify_ok_true_envelope_ignores_extraneous_error_code() {
        // ok=true means success, regardless of any stray
        // error_code field. Defensive: ensures the post-fix
        // refactor preserved the original ok=true short-circuit.
        let result = Ok(ok_text(serde_json::json!({
            "ok": true,
            "error_code": "ignored_when_ok"
        })));
        assert_eq!(classify_tool_result(&result), (true, None));
    }

    #[test]
    fn classify_audited_local_tool_still_fails_closed_on_raw_text() {
        let result = Ok(vec![Content::Text {
            text: "plain remote-style content".to_string(),
        }]);
        assert_eq!(
            classify_audited_tool_result("wa.search", &result),
            (false, Some("envelope_unparsable".to_string()))
        );
    }

    #[test]
    fn classify_audited_remote_proxy_tool_allows_raw_success_content() {
        let result = Ok(vec![Content::Text {
            text: "plain remote-style content".to_string(),
        }]);
        assert_eq!(
            classify_audited_tool_result("remote/mock/echo", &result),
            (true, None)
        );
    }

    #[test]
    fn classify_audited_remote_proxy_tool_decodes_ft_error_envelope() {
        let result = Ok(ok_text(serde_json::json!({
            "ok": false,
            "error_code": "robot.policy_denied",
            "error": "denied"
        })));
        assert_eq!(
            classify_audited_tool_result("remote/mock/search", &result),
            (false, Some("robot.policy_denied".to_string()))
        );
    }

    // ========================================================================
    // ft-ymn10: AuditedToolHandler pre-flight Cx checkpoint
    // ========================================================================

    /// Mock ToolHandler whose `call` flips a flag (and returns success);
    /// the test asserts the flag was NOT set when the wrapper bails on a
    /// pre-expired Cx.
    struct InnerCallObserver {
        called: Arc<AtomicBool>,
    }

    impl ToolHandler for InnerCallObserver {
        fn definition(&self) -> Tool {
            Tool {
                name: "ft_ymn10_observer".to_string(),
                description: Some("Test observer for Cx pre-flight handling".to_string()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: Some(crate::VERSION.to_string()),
                tags: vec!["test".to_string()],
                annotations: None,
            }
        }

        fn call(
            &self,
            _ctx: &McpContext,
            _arguments: serde_json::Value,
        ) -> McpResult<Vec<Content>> {
            self.called.store(true, Ordering::SeqCst);
            Ok(vec![Content::Text {
                text: serde_json::json!({"ok": true}).to_string(),
            }])
        }
    }

    /// ft-ymn10 acceptance: when the caller's Cx is already cancelled or
    /// budget-exhausted, the AuditedToolHandler MUST bail before invoking
    /// the inner tool AND return Err. Pre-fix the wrapper would invoke
    /// inner.call regardless and silently violate the caller's budget.
    #[test]
    fn audited_tool_handler_bails_pre_flight_on_expired_cx_ft_ymn10() {
        let called = Arc::new(AtomicBool::new(false));
        let inner = InnerCallObserver {
            called: Arc::clone(&called),
        };

        // Use a tmpdir for the audit DB path — the wrapper writes a row
        // on the bail path. We don't read it back here (audit-row schema
        // verification is upstream of this slice); the assertion is that
        // the wrapper short-circuits inner.call.
        let tmp = std::env::temp_dir().join(format!(
            "ft_ymn10_audit_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let wrapper = AuditedToolHandler::new(inner, "ft_ymn10_observer", Arc::new(tmp));

        // Construct a Cx with a near-Time::ZERO absolute deadline so checkpoint()
        // fails immediately. The asupersync-side test helper exposes this.
        let cx = crate::mcp_framework::FrameworkCx::for_testing_with_budget(
            crate::mcp_framework::FrameworkBudget::with_deadline_at_ns(1),
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        let ctx = McpContext::new(cx, 1);

        let result = wrapper.call(&ctx, serde_json::json!({}));

        assert!(
            result.is_err(),
            "wrapper must return Err when Cx pre-flight fails (got {:?})",
            result
        );
        assert!(
            !called.load(Ordering::SeqCst),
            "inner.call must NOT be invoked when Cx pre-flight fails"
        );
    }
}
