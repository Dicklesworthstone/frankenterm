#![allow(clippy::float_cmp)]
#![allow(clippy::similar_names)]
#![allow(clippy::overly_complex_bool_expr)]
#![allow(unused_parens)]
//! Machine contracts, SDK generation, NTM-compat shim, and replay tests (ft-3681t.4.4).
//!
//! Publishes durable machine contracts (schemas, specs, examples), provides
//! SDK generation surfaces, adds an NTM compatibility shim for migration
//! acceleration, and enforces behavior via replay-based contract tests.
//!
//! # Architecture
//!
//! ```text
//! MachineContract
//!   ├── EndpointSpec[]            — per-endpoint schema + examples
//!   ├── SdkSurface                — generated client interface definitions
//!   ├── NtmCompatShim             — NTM→ft response translation
//!   └── ReplayContractTest        — replay-based contract enforcement
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::events::UserVarError;
use crate::ipc::{IpcClient, IpcResponse};
use crate::robot_types::{RobotError, RobotResponse};

// =============================================================================
// Endpoint specifications
// =============================================================================

/// HTTP method (for SDK generation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}

impl HttpMethod {
    /// Lowercase label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// Type of a field in the schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FieldType {
    String,
    Integer,
    Float,
    Boolean,
    Array(Box<FieldType>),
    Object(Vec<FieldSpec>),
    Optional(Box<FieldType>),
    /// Free-form JSON value.
    Json,
}

impl FieldType {
    /// Human label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::String => "string".into(),
            Self::Integer => "integer".into(),
            Self::Float => "float".into(),
            Self::Boolean => "boolean".into(),
            Self::Array(inner) => format!("array<{}>", inner.label()),
            Self::Object(_) => "object".into(),
            Self::Optional(inner) => format!("{}?", inner.label()),
            Self::Json => "json".into(),
        }
    }
}

/// Specification of a field in a request or response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldSpec {
    /// Field name.
    pub name: String,
    /// Field type.
    pub field_type: FieldType,
    /// Description.
    pub description: String,
    /// Whether this field is required.
    pub required: bool,
    /// Example value (as JSON string).
    pub example: String,
}

impl FieldSpec {
    /// Create a required field.
    #[must_use]
    pub fn required(
        name: impl Into<String>,
        field_type: FieldType,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            field_type,
            description: description.into(),
            required: true,
            example: String::new(),
        }
    }

    /// Create an optional field.
    #[must_use]
    pub fn optional(
        name: impl Into<String>,
        field_type: FieldType,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            field_type,
            description: description.into(),
            required: false,
            example: String::new(),
        }
    }

    /// Set example value.
    #[must_use]
    pub fn with_example(mut self, example: impl Into<String>) -> Self {
        self.example = example.into();
        self
    }
}

/// Specification for a robot API endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointSpec {
    /// Command name (e.g., "get-text").
    pub command: String,
    /// HTTP method for SDK mapping.
    pub method: HttpMethod,
    /// Human description.
    pub description: String,
    /// Whether this is a mutation (write) operation.
    pub is_mutation: bool,
    /// Request fields.
    pub request_fields: Vec<FieldSpec>,
    /// Response fields.
    pub response_fields: Vec<FieldSpec>,
    /// Error codes this endpoint can return.
    pub error_codes: Vec<ErrorCodeSpec>,
    /// Example request JSON.
    pub example_request: String,
    /// Example response JSON.
    pub example_response: String,
    /// Schema version this spec is valid for.
    pub since_version: String,
    /// Whether NTM compatibility is required.
    pub ntm_compat: bool,
}

impl EndpointSpec {
    /// Create a new endpoint spec.
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        method: HttpMethod,
        description: impl Into<String>,
    ) -> Self {
        Self {
            command: command.into(),
            method,
            description: description.into(),
            is_mutation: matches!(
                method,
                HttpMethod::Post | HttpMethod::Put | HttpMethod::Delete
            ),
            request_fields: Vec::new(),
            response_fields: Vec::new(),
            error_codes: Vec::new(),
            example_request: String::new(),
            example_response: String::new(),
            since_version: "1.0".into(),
            ntm_compat: false,
        }
    }

    /// Mark as requiring NTM compatibility.
    #[must_use]
    pub fn ntm_compatible(mut self) -> Self {
        self.ntm_compat = true;
        self
    }

    /// Add a request field.
    pub fn add_request_field(&mut self, field: FieldSpec) {
        self.request_fields.push(field);
    }

    /// Add a response field.
    pub fn add_response_field(&mut self, field: FieldSpec) {
        self.response_fields.push(field);
    }

    /// Required request fields.
    #[must_use]
    pub fn required_request_fields(&self) -> Vec<&FieldSpec> {
        self.request_fields.iter().filter(|f| f.required).collect()
    }

    /// Required response fields.
    #[must_use]
    pub fn required_response_fields(&self) -> Vec<&FieldSpec> {
        self.response_fields.iter().filter(|f| f.required).collect()
    }
}

/// Error code specification for an endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorCodeSpec {
    /// Error code (e.g., "wezterm.1001").
    pub code: String,
    /// When this error occurs.
    pub condition: String,
    /// Suggested recovery action.
    pub recovery: String,
}

// =============================================================================
// SDK surface generation
// =============================================================================

/// Language target for SDK generation.
///
/// [`SdkLanguage::Python`], [`SdkLanguage::TypeScript`], and
/// [`SdkLanguage::Rust`] are fully-supported finish-line SDK targets: Python
/// and TypeScript wire Robot Mode through bounded default process transports,
/// and Rust wires through [`RustSdkTransport`] end-to-end. The `Go` variant
/// still renders a *template skeleton* whose transport method panics with
/// `transport not wired` and must be implemented by the consumer before use.
/// Use [`SdkLanguage::is_fully_supported`] to gate any code path that requires
/// a real, wired transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SdkLanguage {
    /// Python SDK with a default `ft robot --format json` process transport.
    Python,
    /// TypeScript/JavaScript SDK with a Node-only default process transport.
    TypeScript,
    /// Rust SDK (client crate). Fully-supported finish-line target.
    Rust,
    /// Go SDK template. Transport stub: consumer must implement `call`.
    Go,
}

impl SdkLanguage {
    /// File extension for generated code.
    #[must_use]
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Python => ".py",
            Self::TypeScript => ".ts",
            Self::Rust => ".rs",
            Self::Go => ".go",
        }
    }

    /// Label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Python => "Python",
            Self::TypeScript => "TypeScript",
            Self::Rust => "Rust",
            Self::Go => "Go",
        }
    }

    /// Returns `true` only for SDK targets whose generated client has a
    /// fully-wired transport.
    #[must_use]
    pub fn is_fully_supported(&self) -> bool {
        matches!(self, Self::Python | Self::TypeScript | Self::Rust)
    }
}

/// A generated SDK method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkMethod {
    /// Method name (e.g., "get_text" for Python, "getText" for TS).
    pub method_name: String,
    /// Corresponding robot command.
    pub command: String,
    /// Parameter types.
    pub params: Vec<SdkParam>,
    /// Return type description.
    pub return_type: String,
    /// Whether this method is async.
    pub is_async: bool,
    /// Documentation string.
    pub doc: String,
}

/// A parameter in an SDK method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkParam {
    /// Parameter name.
    pub name: String,
    /// Serialized field name used on the wire.
    pub wire_name: String,
    /// Type in the target language.
    pub param_type: String,
    /// Whether optional.
    pub optional: bool,
    /// Default value (empty if none).
    pub default: String,
}

/// SDK surface for a target language.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkSurface {
    /// Target language.
    pub language: SdkLanguage,
    /// Package/crate/module name.
    pub package_name: String,
    /// Version.
    pub version: String,
    /// Generated methods.
    pub methods: Vec<SdkMethod>,
}

impl SdkSurface {
    /// Create a new SDK surface.
    #[must_use]
    pub fn new(language: SdkLanguage, package_name: impl Into<String>) -> Self {
        Self {
            language,
            package_name: package_name.into(),
            version: "0.1.0".into(),
            methods: Vec::new(),
        }
    }

    /// Generate SDK methods from endpoint specs.
    pub fn generate_from_specs(&mut self, specs: &[EndpointSpec]) {
        for spec in specs {
            let method_name = match self.language {
                SdkLanguage::Python | SdkLanguage::Rust => spec.command.replace('-', "_"),
                SdkLanguage::TypeScript => to_camel_case(&spec.command),
                SdkLanguage::Go => to_pascal_case(&spec.command),
            };

            let params: Vec<SdkParam> = spec
                .request_fields
                .iter()
                .map(|f| SdkParam {
                    name: match self.language {
                        SdkLanguage::Python | SdkLanguage::Rust => f.name.clone(),
                        SdkLanguage::TypeScript => to_camel_case(&f.name),
                        SdkLanguage::Go => to_pascal_case(&f.name),
                    },
                    wire_name: f.name.clone(),
                    param_type: map_type_to_language(&f.field_type, self.language),
                    optional: !f.required,
                    default: String::new(),
                })
                .collect();

            self.methods.push(SdkMethod {
                method_name,
                command: spec.command.clone(),
                params,
                return_type: format!("{}Response", to_pascal_case(&spec.command)),
                is_async: true,
                doc: spec.description.clone(),
            });
        }
    }

    /// Method count.
    #[must_use]
    pub fn method_count(&self) -> usize {
        self.methods.len()
    }

    /// Deterministic artifact filename for the generated client surface.
    #[must_use]
    pub fn artifact_filename(&self) -> String {
        format!(
            "frankenterm_client_{}{}",
            self.language.label().to_ascii_lowercase(),
            self.language.extension()
        )
    }

    /// Render a self-describing SDK client stub for audit and artifact capture.
    #[must_use]
    pub fn render_client_source(&self) -> String {
        match self.language {
            SdkLanguage::Python => render_python_client(self),
            SdkLanguage::TypeScript => render_typescript_client(self),
            SdkLanguage::Rust => render_rust_client(self),
            SdkLanguage::Go => render_go_client(self),
        }
    }
}

/// Convert kebab-case to camelCase.
fn to_camel_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = false;
    for c in s.chars() {
        if c == '-' || c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_uppercase().next().unwrap_or(c));
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

/// Convert kebab-case or snake_case to PascalCase.
fn to_pascal_case(s: &str) -> String {
    let camel = to_camel_case(s);
    let mut chars = camel.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut result = String::new();
    result.push(first.to_ascii_uppercase());
    result.push_str(chars.as_str());
    result
}

/// Map a FieldType to a language-specific type string.
fn map_type_to_language(ft: &FieldType, lang: SdkLanguage) -> String {
    match (ft, lang) {
        (FieldType::String, SdkLanguage::Python) => "str".into(),
        (FieldType::String, SdkLanguage::TypeScript) => "string".into(),
        (FieldType::String, SdkLanguage::Rust) => "String".into(),
        (FieldType::String, SdkLanguage::Go) => "string".into(),
        (FieldType::Integer, SdkLanguage::Python) => "int".into(),
        (FieldType::Integer, SdkLanguage::TypeScript) => "number".into(),
        (FieldType::Integer, SdkLanguage::Rust) => "i64".into(),
        (FieldType::Integer, SdkLanguage::Go) => "int64".into(),
        (FieldType::Float, SdkLanguage::Python) => "float".into(),
        (FieldType::Float, SdkLanguage::TypeScript) => "number".into(),
        (FieldType::Float, SdkLanguage::Rust) => "f64".into(),
        (FieldType::Float, SdkLanguage::Go) => "float64".into(),
        (FieldType::Boolean, SdkLanguage::Python) => "bool".into(),
        (FieldType::Boolean, SdkLanguage::TypeScript) => "boolean".into(),
        (FieldType::Boolean, SdkLanguage::Rust) => "bool".into(),
        (FieldType::Boolean, SdkLanguage::Go) => "bool".into(),
        (FieldType::Array(inner), lang) => {
            let inner_type = map_type_to_language(inner, lang);
            match lang {
                SdkLanguage::Python => format!("list[{inner_type}]"),
                SdkLanguage::TypeScript => format!("{inner_type}[]"),
                SdkLanguage::Rust => format!("Vec<{inner_type}>"),
                SdkLanguage::Go => format!("[]{inner_type}"),
            }
        }
        (FieldType::Optional(inner), lang) => {
            let inner_type = map_type_to_language(inner, lang);
            match lang {
                SdkLanguage::Python => format!("Optional[{inner_type}]"),
                SdkLanguage::TypeScript => format!("{inner_type} | undefined"),
                SdkLanguage::Rust => format!("Option<{inner_type}>"),
                SdkLanguage::Go => format!("*{inner_type}"),
            }
        }
        (FieldType::Object(_), SdkLanguage::Python) => "dict".into(),
        (FieldType::Object(_), SdkLanguage::TypeScript) => "Record<string, unknown>".into(),
        (FieldType::Object(_), SdkLanguage::Rust) => "serde_json::Value".into(),
        (FieldType::Object(_), SdkLanguage::Go) => "map[string]interface{}".into(),
        (FieldType::Json, SdkLanguage::Python) => "Any".into(),
        (FieldType::Json, SdkLanguage::TypeScript) => "unknown".into(),
        (FieldType::Json, SdkLanguage::Rust) => "serde_json::Value".into(),
        (FieldType::Json, SdkLanguage::Go) => "interface{}".into(),
    }
}

/// Canonical Rust transport backend for generated robot SDK clients.
pub struct RustSdkTransport {
    ipc: IpcClient,
}

impl RustSdkTransport {
    const SUPPORTED_COMMANDS: &[&str] = &["get-text", "send-text", "state", "search"];

    /// Build a transport backed by watcher IPC RPC.
    #[must_use]
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            ipc: IpcClient::new(socket_path),
        }
    }

    /// Build a transport with an explicit IPC auth token.
    #[must_use]
    pub fn with_token(socket_path: impl AsRef<Path>, token: impl Into<String>) -> Self {
        Self {
            ipc: IpcClient::with_token(socket_path, token),
        }
    }

    /// Update the IPC auth token (use `None` to clear).
    pub fn set_token(&mut self, token: Option<String>) {
        self.ipc.set_token(token);
    }

    /// Whether the configured watcher IPC socket currently exists.
    #[must_use]
    pub fn socket_exists(&self) -> bool {
        self.ipc.socket_exists()
    }

    /// Supported contract commands for the generated Rust SDK.
    #[must_use]
    pub fn supported_commands() -> &'static [&'static str] {
        Self::SUPPORTED_COMMANDS
    }

    /// Whether this transport supports the supplied contract command.
    #[must_use]
    pub fn supports_command(command: &str) -> bool {
        Self::SUPPORTED_COMMANDS.contains(&command)
    }

    /// Execute a contract command and decode the robot envelope into `T`.
    ///
    /// When an ambient asupersync capability context is installed, prefer the
    /// Cx-aware IPC path so SDK callers inherit transport cancellation without
    /// needing an API break. Falls back to the legacy request path when no
    /// ambient Cx exists.
    ///
    /// # Errors
    /// Returns an explicit transport, payload-shape, or robot-mode error.
    pub async fn call<T: DeserializeOwned>(
        &self,
        command: &str,
        payload: serde_json::Value,
    ) -> Result<T, RustSdkTransportError> {
        let args = build_rust_sdk_ipc_args(command, &payload)?;
        if let Some(cx) = crate::cx::Cx::current() {
            let response = self
                .ipc
                .call_rpc_with_cx(&cx, args, None)
                .await
                .map_err(RustSdkTransportError::Transport)?;
            return decode_rust_sdk_response(response);
        }

        let response = self
            .ipc
            .call_rpc(args, None)
            .await
            .map_err(RustSdkTransportError::Transport)?;
        decode_rust_sdk_response(response)
    }

    /// Execute a contract command and return the untyped JSON payload.
    ///
    /// # Errors
    /// Returns an explicit transport, payload-shape, or robot-mode error.
    pub async fn call_value(
        &self,
        command: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, RustSdkTransportError> {
        self.call(command, payload).await
    }

    /// Cx-first [`Self::call`] (ft-xbnl0.2.3). Routes the IPC
    /// RPC call through [`crate::ipc::IpcClient::call_rpc_with_cx`]
    /// (tick 78) so caller cancellation propagates into the
    /// socket round-trip — the 4 await points inside
    /// `send_request_with_id_with_cx` (connect, write, flush,
    /// read) all honor cx.
    pub async fn call_with_cx<T: DeserializeOwned>(
        &self,
        cx: &crate::cx::Cx,
        command: &str,
        payload: serde_json::Value,
    ) -> Result<T, RustSdkTransportError> {
        let args = build_rust_sdk_ipc_args(command, &payload)?;
        let response = self
            .ipc
            .call_rpc_with_cx(cx, args, None)
            .await
            .map_err(RustSdkTransportError::Transport)?;
        decode_rust_sdk_response(response)
    }

    /// Cx-first [`Self::call_value`] (ft-xbnl0.2.3). Pure
    /// delegate to [`Self::call_with_cx`] with
    /// `T = serde_json::Value`.
    pub async fn call_value_with_cx(
        &self,
        cx: &crate::cx::Cx,
        command: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, RustSdkTransportError> {
        self.call_with_cx(cx, command, payload).await
    }
}

/// Errors surfaced by the Rust SDK IPC transport.
#[derive(Debug)]
pub enum RustSdkTransportError {
    UnsupportedCommand {
        command: String,
    },
    InvalidPayload {
        command: String,
        field: &'static str,
        message: String,
    },
    Transport(UserVarError),
    ResponseEncode(serde_json::Error),
    ResponseDecode(serde_json::Error),
    Robot(RobotError),
}

impl std::fmt::Display for RustSdkTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedCommand { command } => write!(
                f,
                "unsupported robot SDK command '{command}' (supported: {})",
                RustSdkTransport::supported_commands().join(", ")
            ),
            Self::InvalidPayload {
                command,
                field,
                message,
            } => {
                write!(
                    f,
                    "invalid payload for robot SDK command '{command}' field '{field}': {message}"
                )
            }
            Self::Transport(err) => write!(f, "robot SDK transport error: {err}"),
            Self::ResponseEncode(err) => write!(f, "failed to encode IPC response: {err}"),
            Self::ResponseDecode(err) => {
                write!(f, "failed to decode robot response envelope: {err}")
            }
            Self::Robot(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for RustSdkTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(err) => Some(err),
            Self::ResponseEncode(err) | Self::ResponseDecode(err) => Some(err),
            Self::Robot(err) => Some(err),
            Self::UnsupportedCommand { .. } | Self::InvalidPayload { .. } => None,
        }
    }
}

fn decode_rust_sdk_response<T: DeserializeOwned>(
    response: IpcResponse,
) -> Result<T, RustSdkTransportError> {
    let raw = serde_json::to_vec(&response).map_err(RustSdkTransportError::ResponseEncode)?;
    let envelope: RobotResponse<T> =
        serde_json::from_slice(&raw).map_err(RustSdkTransportError::ResponseDecode)?;
    envelope.into_result().map_err(RustSdkTransportError::Robot)
}

fn build_rust_sdk_ipc_args(
    command: &str,
    payload: &serde_json::Value,
) -> Result<Vec<String>, RustSdkTransportError> {
    match command {
        "get-text" => build_get_text_ipc_args(command, payload),
        "send-text" => build_send_text_ipc_args(command, payload),
        "state" => build_state_ipc_args(command, payload),
        "search" => build_search_ipc_args(command, payload),
        _ => Err(RustSdkTransportError::UnsupportedCommand {
            command: command.to_string(),
        }),
    }
}

fn build_get_text_ipc_args(
    command: &str,
    payload: &serde_json::Value,
) -> Result<Vec<String>, RustSdkTransportError> {
    let mut args = vec![
        "get-text".to_string(),
        required_nonnegative_integer(payload, command, "pane_id")?,
    ];

    if let Some(tail_lines) = optional_nonnegative_integer(payload, command, "tail_lines")? {
        args.push("--tail".to_string());
        args.push(tail_lines);
    }
    if optional_bool(payload, command, "escapes")?.unwrap_or(false) {
        args.push("--escapes".to_string());
    }

    Ok(args)
}

fn build_send_text_ipc_args(
    command: &str,
    payload: &serde_json::Value,
) -> Result<Vec<String>, RustSdkTransportError> {
    let mut args = vec![
        "send".to_string(),
        required_nonnegative_integer(payload, command, "pane_id")?,
        required_string(payload, command, "text")?,
    ];

    if optional_bool(payload, command, "dry_run")?.unwrap_or(false) {
        args.push("--dry-run".to_string());
    }
    if let Some(approval_code) = optional_string(payload, command, "approval_code")? {
        args.push("--approval-code".to_string());
        args.push(approval_code);
    }

    let wait_for = optional_string(payload, command, "wait_for")?;
    let wait_for_regex = optional_bool(payload, command, "wait_for_regex")?.unwrap_or(false);
    let timeout_secs = optional_nonnegative_integer(payload, command, "timeout_secs")?;

    if wait_for.is_none() && wait_for_regex {
        return Err(RustSdkTransportError::InvalidPayload {
            command: command.to_string(),
            field: "wait_for_regex",
            message: "wait_for_regex requires wait_for".to_string(),
        });
    }
    if wait_for.is_none() && timeout_secs.is_some() {
        return Err(RustSdkTransportError::InvalidPayload {
            command: command.to_string(),
            field: "timeout_secs",
            message: "timeout_secs requires wait_for".to_string(),
        });
    }

    if let Some(pattern) = wait_for {
        args.push("--wait-for".to_string());
        args.push(pattern);
        if let Some(timeout_secs) = timeout_secs {
            args.push("--timeout-secs".to_string());
            args.push(timeout_secs);
        }
        if wait_for_regex {
            args.push("--wait-for-regex".to_string());
        }
    }

    Ok(args)
}

fn build_state_ipc_args(
    command: &str,
    payload: &serde_json::Value,
) -> Result<Vec<String>, RustSdkTransportError> {
    let mut args = vec!["state".to_string()];
    let include_text = optional_bool(payload, command, "include_text")?.unwrap_or(false);
    let tail = optional_nonnegative_integer(payload, command, "tail")?;
    let escapes = optional_bool(payload, command, "escapes")?.unwrap_or(false);
    let wants_text = include_text || tail.is_some() || escapes;

    if wants_text {
        args.push("--include-text".to_string());
        if let Some(tail) = tail {
            args.push("--tail".to_string());
            args.push(tail);
        }
        if escapes {
            args.push("--escapes".to_string());
        }
    }

    Ok(args)
}

fn build_search_ipc_args(
    command: &str,
    payload: &serde_json::Value,
) -> Result<Vec<String>, RustSdkTransportError> {
    let mut args = vec![
        "search".to_string(),
        required_string(payload, command, "query")?,
    ];

    if let Some(limit) = optional_nonnegative_integer(payload, command, "limit")? {
        args.push("--limit".to_string());
        args.push(limit);
    }
    if let Some(pane) = optional_nonnegative_integer(payload, command, "pane")? {
        args.push("--pane".to_string());
        args.push(pane);
    }
    if let Some(since) = optional_integer(payload, command, "since")? {
        args.push("--since".to_string());
        args.push(since);
    }
    if let Some(until) = optional_integer(payload, command, "until")? {
        args.push("--until".to_string());
        args.push(until);
    }
    if let Some(snippets) = optional_bool(payload, command, "snippets")? {
        args.push(if snippets {
            "--snippets".to_string()
        } else {
            "--snippets=false".to_string()
        });
    }
    if let Some(mode) = optional_string(payload, command, "mode")? {
        match mode.as_str() {
            "lexical" | "semantic" | "hybrid" => {
                args.push("--mode".to_string());
                args.push(mode);
            }
            _ => {
                return Err(RustSdkTransportError::InvalidPayload {
                    command: command.to_string(),
                    field: "mode",
                    message: "expected one of: lexical, semantic, hybrid".to_string(),
                });
            }
        }
    }

    Ok(args)
}

fn payload_object<'a>(
    payload: &'a serde_json::Value,
    command: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>, RustSdkTransportError> {
    payload
        .as_object()
        .ok_or_else(|| RustSdkTransportError::InvalidPayload {
            command: command.to_string(),
            field: "<payload>",
            message: "expected a JSON object".to_string(),
        })
}

fn optional_value<'a>(
    payload: &'a serde_json::Value,
    command: &str,
    field: &'static str,
) -> Result<Option<&'a serde_json::Value>, RustSdkTransportError> {
    Ok(match payload_object(payload, command)?.get(field) {
        Some(serde_json::Value::Null) | None => None,
        Some(value) => Some(value),
    })
}

fn required_string(
    payload: &serde_json::Value,
    command: &str,
    field: &'static str,
) -> Result<String, RustSdkTransportError> {
    optional_string(payload, command, field)?.ok_or_else(|| RustSdkTransportError::InvalidPayload {
        command: command.to_string(),
        field,
        message: "missing required string".to_string(),
    })
}

fn optional_string(
    payload: &serde_json::Value,
    command: &str,
    field: &'static str,
) -> Result<Option<String>, RustSdkTransportError> {
    optional_value(payload, command, field)?
        .map(|value| {
            value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                RustSdkTransportError::InvalidPayload {
                    command: command.to_string(),
                    field,
                    message: "expected a string".to_string(),
                }
            })
        })
        .transpose()
}

fn optional_bool(
    payload: &serde_json::Value,
    command: &str,
    field: &'static str,
) -> Result<Option<bool>, RustSdkTransportError> {
    optional_value(payload, command, field)?
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| RustSdkTransportError::InvalidPayload {
                    command: command.to_string(),
                    field,
                    message: "expected a boolean".to_string(),
                })
        })
        .transpose()
}

fn optional_integer(
    payload: &serde_json::Value,
    command: &str,
    field: &'static str,
) -> Result<Option<String>, RustSdkTransportError> {
    optional_value(payload, command, field)?
        .map(|value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
                .map(|number| number.to_string())
                .ok_or_else(|| RustSdkTransportError::InvalidPayload {
                    command: command.to_string(),
                    field,
                    message: "expected an integer".to_string(),
                })
        })
        .transpose()
}

fn optional_nonnegative_integer(
    payload: &serde_json::Value,
    command: &str,
    field: &'static str,
) -> Result<Option<String>, RustSdkTransportError> {
    optional_integer(payload, command, field)?
        .map(|value| {
            if value.starts_with('-') {
                Err(RustSdkTransportError::InvalidPayload {
                    command: command.to_string(),
                    field,
                    message: "expected a non-negative integer".to_string(),
                })
            } else {
                Ok(value)
            }
        })
        .transpose()
}

fn required_nonnegative_integer(
    payload: &serde_json::Value,
    command: &str,
    field: &'static str,
) -> Result<String, RustSdkTransportError> {
    optional_nonnegative_integer(payload, command, field)?.ok_or_else(|| {
        RustSdkTransportError::InvalidPayload {
            command: command.to_string(),
            field,
            message: "missing required integer".to_string(),
        }
    })
}

fn render_python_client(surface: &SdkSurface) -> String {
    let mut out = String::from(
        "from __future__ import annotations\n\nimport asyncio\nimport json\nimport os\nfrom collections.abc import Awaitable, Callable, Mapping, Sequence\nfrom dataclasses import dataclass\nfrom typing import Any\n\n",
    );

    for return_type in unique_return_types(surface) {
        out.push_str(&format!("{return_type} = dict[str, Any]\n"));
    }

    out.push_str(
        r#"

_SUPPORTED_COMMANDS = ("get-text", "send-text", "state", "search")
_STDERR_LIMIT = 4096


@dataclass(frozen=True)
class FrankentermProcessResult:
    returncode: int
    stdout: bytes
    stderr: bytes


ProcessRunner = Callable[
    [Sequence[str], Mapping[str, str] | None, float | None],
    Awaitable[FrankentermProcessResult],
]


class FrankentermTransportError(RuntimeError):
    pass


class FrankentermUnsupportedCommandError(FrankentermTransportError):
    def __init__(self, command: str) -> None:
        supported = ", ".join(_SUPPORTED_COMMANDS)
        super().__init__(f"unsupported robot SDK command {command!r} (supported: {supported})")
        self.command = command


class FrankentermRobotError(RuntimeError):
    def __init__(
        self,
        message: str,
        code: str | None = None,
        hint: str | None = None,
        envelope: Mapping[str, Any] | None = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.code = code
        self.hint = hint
        self.envelope = dict(envelope) if envelope is not None else None


class FrankentermClient:
    def __init__(
        self,
        ft_binary: str | None = None,
        timeout: float | None = 30.0,
        env: Mapping[str, str] | None = None,
        runner: ProcessRunner | None = None,
    ) -> None:
        if timeout is not None and timeout <= 0:
            raise ValueError("timeout must be positive or None")
        self._ft_binary = ft_binary or os.environ.get("FRANKENTERM_FT_BINARY", "ft")
        self._timeout = timeout
        self._env = dict(env) if env is not None else None
        self._runner = runner or self._run_process

    @classmethod
    def supported_commands(cls) -> tuple[str, ...]:
        return _SUPPORTED_COMMANDS

    @classmethod
    def supports_command(cls, command: str) -> bool:
        return command in _SUPPORTED_COMMANDS

    async def _call(self, command: str, payload: dict[str, Any]) -> Any:
        if command not in _SUPPORTED_COMMANDS:
            raise FrankentermUnsupportedCommandError(command)

        clean_payload = {key: value for key, value in payload.items() if value is not None}
        args = [self._ft_binary, "robot", "--format", "json"]
        args.extend(_command_args(command, clean_payload))
        result = await self._runner(args, self._env, self._timeout)

        if result.returncode != 0:
            raise FrankentermTransportError(
                f"robot command exited {result.returncode}: {_stderr_tail(result.stderr)}"
            )

        envelope = _decode_envelope(result.stdout, command)
        ok = envelope.get("ok")
        if ok is True:
            if "data" not in envelope:
                raise FrankentermTransportError(
                    f"robot command {command!r} returned ok=true without data"
                )
            return envelope["data"]
        if ok is False:
            raise _robot_error(envelope)

        raise FrankentermTransportError(
            f"robot command {command!r} returned malformed envelope: missing boolean ok"
        )

    @staticmethod
    async def _run_process(
        args: Sequence[str],
        env: Mapping[str, str] | None,
        timeout: float | None,
    ) -> FrankentermProcessResult:
        try:
            process = await asyncio.create_subprocess_exec(
                *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                env=_merged_env(env),
            )
        except FileNotFoundError as exc:
            raise FrankentermTransportError(f"ft binary not found: {args[0]}") from exc
        except OSError as exc:
            raise FrankentermTransportError(
                f"failed to start robot command {_format_args(args)}: {exc}"
            ) from exc

        try:
            stdout, stderr = await asyncio.wait_for(process.communicate(), timeout=timeout)
        except TimeoutError as exc:
            process.kill()
            await process.wait()
            raise FrankentermTransportError(
                f"robot command timed out after {timeout} seconds: {_format_args(args)}"
            ) from exc

        return FrankentermProcessResult(
            returncode=process.returncode,
            stdout=stdout,
            stderr=stderr,
        )

"#,
    );

    for method in &surface.methods {
        out.push_str(&format!(
            "\n    async def {}({}) -> {}:\n        \"\"\"{}\"\"\"\n        return await self._call(\"{}\", {})\n",
            method.method_name,
            render_python_params(&method.params),
            method.return_type,
            method.doc,
            method.command,
            render_python_payload(&method.params),
        ));
    }

    out.push_str(
        r#"


def _command_args(command: str, payload: Mapping[str, Any]) -> list[str]:
    if command == "get-text":
        args = ["get-text", str(_required_nonnegative_int(payload, command, "pane_id"))]
        tail_lines = _optional_nonnegative_int(payload, command, "tail_lines")
        if tail_lines is not None:
            args.extend(["--tail", str(tail_lines)])
        if _optional_bool(payload, command, "escapes") is True:
            args.append("--escapes")
        return args

    if command == "send-text":
        args = [
            "send",
            str(_required_nonnegative_int(payload, command, "pane_id")),
            _required_str(payload, command, "text"),
        ]
        if _optional_bool(payload, command, "dry_run") is True:
            args.append("--dry-run")
        approval_code = _optional_str(payload, command, "approval_code")
        if approval_code is not None:
            args.extend(["--approval-code", approval_code])

        wait_for = _optional_str(payload, command, "wait_for")
        wait_for_regex = _optional_bool(payload, command, "wait_for_regex") is True
        timeout_secs = _optional_nonnegative_int(payload, command, "timeout_secs")
        if wait_for is None and wait_for_regex:
            raise FrankentermTransportError("wait_for_regex requires wait_for")
        if wait_for is None and timeout_secs is not None:
            raise FrankentermTransportError("timeout_secs requires wait_for")

        if wait_for is not None:
            args.extend(["--wait-for", wait_for])
            if timeout_secs is not None:
                args.extend(["--timeout-secs", str(timeout_secs)])
            if wait_for_regex:
                args.append("--wait-for-regex")
        return args

    if command == "state":
        args = ["state"]
        include_text = _optional_bool(payload, command, "include_text") is True
        tail = _optional_nonnegative_int(payload, command, "tail")
        escapes = _optional_bool(payload, command, "escapes") is True
        if include_text or tail is not None or escapes:
            args.append("--include-text")
            if tail is not None:
                args.extend(["--tail", str(tail)])
            if escapes:
                args.append("--escapes")
        return args

    if command == "search":
        args = ["search", _required_str(payload, command, "query")]
        limit = _optional_nonnegative_int(payload, command, "limit")
        if limit is not None:
            args.extend(["--limit", str(limit)])
        pane = _optional_nonnegative_int(payload, command, "pane")
        if pane is not None:
            args.extend(["--pane", str(pane)])
        since = _optional_int(payload, command, "since")
        if since is not None:
            args.extend(["--since", str(since)])
        until = _optional_int(payload, command, "until")
        if until is not None:
            args.extend(["--until", str(until)])
        snippets = _optional_bool(payload, command, "snippets")
        if snippets is not None:
            args.append("--snippets" if snippets else "--snippets=false")
        mode = _optional_str(payload, command, "mode")
        if mode is not None:
            if mode not in {"lexical", "semantic", "hybrid"}:
                raise FrankentermTransportError("mode must be one of: lexical, semantic, hybrid")
            args.extend(["--mode", mode])
        return args

    raise FrankentermUnsupportedCommandError(command)


def _required_str(payload: Mapping[str, Any], command: str, field: str) -> str:
    value = _required_value(payload, command, field)
    if not isinstance(value, str):
        raise FrankentermTransportError(
            f"invalid payload for {command!r} field {field!r}: expected string"
        )
    return value


def _optional_str(payload: Mapping[str, Any], command: str, field: str) -> str | None:
    value = payload.get(field)
    if value is None:
        return None
    if not isinstance(value, str):
        raise FrankentermTransportError(
            f"invalid payload for {command!r} field {field!r}: expected string"
        )
    return value


def _optional_bool(payload: Mapping[str, Any], command: str, field: str) -> bool | None:
    value = payload.get(field)
    if value is None:
        return None
    if not isinstance(value, bool):
        raise FrankentermTransportError(
            f"invalid payload for {command!r} field {field!r}: expected boolean"
        )
    return value


def _optional_int(payload: Mapping[str, Any], command: str, field: str) -> int | None:
    value = payload.get(field)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int):
        raise FrankentermTransportError(
            f"invalid payload for {command!r} field {field!r}: expected integer"
        )
    return value


def _required_nonnegative_int(payload: Mapping[str, Any], command: str, field: str) -> int:
    value = _optional_nonnegative_int(payload, command, field)
    if value is None:
        raise FrankentermTransportError(
            f"invalid payload for {command!r} field {field!r}: missing required integer"
        )
    return value


def _optional_nonnegative_int(payload: Mapping[str, Any], command: str, field: str) -> int | None:
    value = _optional_int(payload, command, field)
    if value is None:
        return None
    if value < 0:
        raise FrankentermTransportError(
            f"invalid payload for {command!r} field {field!r}: expected non-negative integer"
        )
    return value


def _required_value(payload: Mapping[str, Any], command: str, field: str) -> Any:
    value = payload.get(field)
    if value is None:
        raise FrankentermTransportError(
            f"invalid payload for {command!r} field {field!r}: missing required value"
        )
    return value


def _decode_envelope(stdout: bytes, command: str) -> Mapping[str, Any]:
    try:
        envelope = json.loads(stdout.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise FrankentermTransportError(
            f"robot command {command!r} returned invalid JSON"
        ) from exc
    if not isinstance(envelope, Mapping):
        raise FrankentermTransportError(
            f"robot command {command!r} returned non-object JSON envelope"
        )
    return envelope


def _robot_error(envelope: Mapping[str, Any]) -> FrankentermRobotError:
    message = envelope.get("error")
    if not isinstance(message, str) or not message:
        message = "unknown robot error"
    code = envelope.get("error_code")
    if code is not None and not isinstance(code, str):
        code = None
    hint = envelope.get("hint")
    if hint is not None and not isinstance(hint, str):
        hint = None
    return FrankentermRobotError(message, code=code, hint=hint, envelope=envelope)


def _stderr_tail(stderr: bytes) -> str:
    text = stderr.decode("utf-8", errors="replace").strip()
    if not text:
        return "stderr was empty"
    return text[-_STDERR_LIMIT:]


def _merged_env(env: Mapping[str, str] | None) -> Mapping[str, str] | None:
    if env is None:
        return None
    merged = os.environ.copy()
    merged.update(env)
    return merged


def _format_args(args: Sequence[str]) -> str:
    return " ".join(str(part) for part in args)
"#,
    );

    out
}

fn render_typescript_client(surface: &SdkSurface) -> String {
    let mut out = String::from(
        r"declare const require: (id: string) => any;
declare const process: { env: Record<string, string | undefined> };

export type JsonPayload = Record<string, unknown>;
export type FrankentermProcessResult = {
  returnCode: number;
  stdout: string;
  stderr: string;
};
export type ProcessRunner = (
  args: readonly string[],
  env: Record<string, string> | undefined,
  timeoutMs: number | undefined,
) => Promise<FrankentermProcessResult>;
export type FrankentermClientOptions = {
  ftBinary?: string;
  timeoutMs?: number;
  env?: Record<string, string>;
  runner?: ProcessRunner;
};

",
    );

    for return_type in unique_return_types(surface) {
        out.push_str(&format!("export type {return_type} = unknown;\n"));
    }

    out.push_str(
        r#"
const SUPPORTED_COMMANDS = ["get-text", "send-text", "state", "search"] as const;
const SUPPORTED_COMMAND_SET = new Set<string>(SUPPORTED_COMMANDS);
const STDERR_LIMIT = 4096;

type ChildProcessLike = {
  stdout?: StreamLike;
  stderr?: StreamLike;
  on(event: "error", handler: (error: unknown) => void): void;
  on(event: "close", handler: (code: unknown) => void): void;
  kill(signal?: string): void;
};

type StreamLike = {
  on(event: "data", handler: (chunk: unknown) => void): void;
};

export class FrankentermTransportError extends Error {
  readonly cause?: unknown;

  constructor(message: string, cause?: unknown) {
    super(message);
    this.name = "FrankentermTransportError";
    this.cause = cause;
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

export class FrankentermUnsupportedCommandError extends FrankentermTransportError {
  readonly command: string;

  constructor(command: string) {
    super(
      `unsupported robot SDK command ${JSON.stringify(command)} (supported: ${SUPPORTED_COMMANDS.join(", ")})`,
    );
    this.name = "FrankentermUnsupportedCommandError";
    this.command = command;
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

export class FrankentermRobotError extends Error {
  readonly code?: string;
  readonly hint?: string;
  readonly details?: unknown;
  readonly elapsedMs?: number;
  readonly envelope: JsonPayload;

  constructor(
    message: string,
    options: {
      code?: string;
      hint?: string;
      details?: unknown;
      elapsedMs?: number;
      envelope: JsonPayload;
    },
  ) {
    super(message);
    this.name = "FrankentermRobotError";
    this.code = options.code;
    this.hint = options.hint;
    this.details = options.details;
    this.elapsedMs = options.elapsedMs;
    this.envelope = options.envelope;
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

export class FrankentermClient {
  private readonly ftBinary: string;
  private readonly timeoutMs: number | undefined;
  private readonly env: Record<string, string> | undefined;
  private readonly runner: ProcessRunner;

  constructor(options: FrankentermClientOptions = {}) {
    if (options.timeoutMs !== undefined && options.timeoutMs <= 0) {
      throw new Error("timeoutMs must be positive or undefined");
    }
    this.ftBinary = options.ftBinary ?? process.env.FRANKENTERM_FT_BINARY ?? "ft";
    this.timeoutMs = options.timeoutMs ?? 30_000;
    this.env = options.env;
    this.runner = options.runner ?? runProcess;
  }

  static supportedCommands(): readonly string[] {
    return SUPPORTED_COMMANDS;
  }

  static supportsCommand(command: string): boolean {
    return SUPPORTED_COMMAND_SET.has(command);
  }

  protected async call(command: string, payload: JsonPayload): Promise<unknown> {
    if (!SUPPORTED_COMMAND_SET.has(command)) {
      throw new FrankentermUnsupportedCommandError(command);
    }

    const cleanPayload = withoutUndefined(payload);
    const args = [
      this.ftBinary,
      "robot",
      "--format",
      "json",
      ...commandArgs(command, cleanPayload),
    ];
    const result = await this.runner(args, this.env, this.timeoutMs);

    if (result.returnCode !== 0) {
      throw new FrankentermTransportError(
        `robot command exited ${result.returnCode}: ${stderrTail(result.stderr)}`,
      );
    }

    const envelope = decodeEnvelope(result.stdout, command);
    const ok = envelope["ok"];
    if (ok === true) {
      if (!Object.prototype.hasOwnProperty.call(envelope, "data")) {
        throw new FrankentermTransportError(
          `robot command ${JSON.stringify(command)} returned ok=true without data`,
        );
      }
      return envelope["data"];
    }
    if (ok === false) {
      throw robotError(envelope);
    }

    throw new FrankentermTransportError(
      `robot command ${JSON.stringify(command)} returned malformed envelope: missing boolean ok`,
    );
  }
"#,
    );

    for method in &surface.methods {
        out.push_str(&format!(
            "\n  async {}({}): Promise<{}> {{\n    return this.call(\"{}\", {}) as Promise<{}>;\n  }}\n",
            method.method_name,
            render_typescript_params(&method.params),
            method.return_type,
            method.command,
            render_typescript_payload(&method.params),
            method.return_type,
        ));
    }

    out.push_str("}\n");
    out.push_str(
        r#"

async function runProcess(
  args: readonly string[],
  env: Record<string, string> | undefined,
  timeoutMs: number | undefined,
): Promise<FrankentermProcessResult> {
  return new Promise((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    let settled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const childProcess = require("node:child_process") as {
      spawn: (
        command: string,
        args: readonly string[],
        options: {
          env?: Record<string, string | undefined>;
          stdio: readonly ["ignore", "pipe", "pipe"];
        },
      ) => ChildProcessLike;
    };

    const finish = (fn: () => void): void => {
      if (settled) {
        return;
      }
      settled = true;
      if (timer !== undefined) {
        clearTimeout(timer);
      }
      fn();
    };

    let child: ChildProcessLike;
    try {
      child = childProcess.spawn(args[0], args.slice(1), {
        env: mergedEnv(env),
        stdio: ["ignore", "pipe", "pipe"] as const,
      });
    } catch (error: unknown) {
      throw new FrankentermTransportError(
        `failed to start robot command ${formatArgs(args)}: ${errorMessage(error)}`,
        error,
      );
    }

    child.stdout?.on("data", (chunk: unknown) => {
      stdout += chunkToString(chunk);
    });
    child.stderr?.on("data", (chunk: unknown) => {
      stderr += chunkToString(chunk);
    });
    child.on("error", (error: unknown) => {
      finish(() => {
        const code = errorCode(error);
        const message =
          code === "ENOENT"
            ? `ft binary not found: ${args[0]}`
            : `failed to start robot command ${formatArgs(args)}: ${errorMessage(error)}`;
        reject(new FrankentermTransportError(message, error));
      });
    });
    child.on("close", (code: unknown) => {
      finish(() => {
        resolve({
          returnCode: typeof code === "number" ? code : -1,
          stdout,
          stderr,
        });
      });
    });

    if (timeoutMs !== undefined) {
      timer = setTimeout(() => {
        finish(() => {
          child.kill();
          reject(
            new FrankentermTransportError(
              `robot command timed out after ${timeoutMs} ms: ${formatArgs(args)}`,
            ),
          );
        });
      }, timeoutMs);
    }
  });
}

function commandArgs(command: string, payload: JsonPayload): string[] {
  if (command === "get-text") {
    const args = ["get-text", String(requiredNonnegativeInteger(payload, command, "pane_id"))];
    const tailLines = optionalNonnegativeInteger(payload, command, "tail_lines");
    if (tailLines !== undefined) {
      args.push("--tail", String(tailLines));
    }
    if (optionalBoolean(payload, command, "escapes") === true) {
      args.push("--escapes");
    }
    return args;
  }

  if (command === "send-text") {
    const args = [
      "send",
      String(requiredNonnegativeInteger(payload, command, "pane_id")),
      requiredString(payload, command, "text"),
    ];
    if (optionalBoolean(payload, command, "dry_run") === true) {
      args.push("--dry-run");
    }
    const approvalCode = optionalString(payload, command, "approval_code");
    if (approvalCode !== undefined) {
      args.push("--approval-code", approvalCode);
    }

    const waitFor = optionalString(payload, command, "wait_for");
    const waitForRegex = optionalBoolean(payload, command, "wait_for_regex") === true;
    const timeoutSecs = optionalNonnegativeInteger(payload, command, "timeout_secs");
    if (waitFor === undefined && waitForRegex) {
      throw new FrankentermTransportError("wait_for_regex requires wait_for");
    }
    if (waitFor === undefined && timeoutSecs !== undefined) {
      throw new FrankentermTransportError("timeout_secs requires wait_for");
    }
    if (waitFor !== undefined) {
      args.push("--wait-for", waitFor);
      if (timeoutSecs !== undefined) {
        args.push("--timeout-secs", String(timeoutSecs));
      }
      if (waitForRegex) {
        args.push("--wait-for-regex");
      }
    }
    return args;
  }

  if (command === "state") {
    const args = ["state"];
    const includeText = optionalBoolean(payload, command, "include_text") === true;
    const tail = optionalNonnegativeInteger(payload, command, "tail");
    const escapes = optionalBoolean(payload, command, "escapes") === true;
    if (includeText || tail !== undefined || escapes) {
      args.push("--include-text");
      if (tail !== undefined) {
        args.push("--tail", String(tail));
      }
      if (escapes) {
        args.push("--escapes");
      }
    }
    return args;
  }

  if (command === "search") {
    const args = ["search", requiredString(payload, command, "query")];
    const limit = optionalNonnegativeInteger(payload, command, "limit");
    if (limit !== undefined) {
      args.push("--limit", String(limit));
    }
    const pane = optionalNonnegativeInteger(payload, command, "pane");
    if (pane !== undefined) {
      args.push("--pane", String(pane));
    }
    const since = optionalInteger(payload, command, "since");
    if (since !== undefined) {
      args.push("--since", String(since));
    }
    const until = optionalInteger(payload, command, "until");
    if (until !== undefined) {
      args.push("--until", String(until));
    }
    const snippets = optionalBoolean(payload, command, "snippets");
    if (snippets !== undefined) {
      args.push(snippets ? "--snippets" : "--snippets=false");
    }
    const mode = optionalString(payload, command, "mode");
    if (mode !== undefined) {
      if (!["lexical", "semantic", "hybrid"].includes(mode)) {
        throw new FrankentermTransportError("mode must be one of: lexical, semantic, hybrid");
      }
      args.push("--mode", mode);
    }
    return args;
  }

  throw new FrankentermUnsupportedCommandError(command);
}

function requiredString(payload: JsonPayload, command: string, field: string): string {
  const value = requiredValue(payload, command, field);
  if (typeof value !== "string") {
    throw invalidPayload(command, field, "expected string");
  }
  return value;
}

function optionalString(payload: JsonPayload, command: string, field: string): string | undefined {
  const value = payload[field];
  if (value == null) {
    return undefined;
  }
  if (typeof value !== "string") {
    throw invalidPayload(command, field, "expected string");
  }
  return value;
}

function optionalBoolean(payload: JsonPayload, command: string, field: string): boolean | undefined {
  const value = payload[field];
  if (value == null) {
    return undefined;
  }
  if (typeof value !== "boolean") {
    throw invalidPayload(command, field, "expected boolean");
  }
  return value;
}

function optionalInteger(payload: JsonPayload, command: string, field: string): number | undefined {
  const value = payload[field];
  if (value == null) {
    return undefined;
  }
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw invalidPayload(command, field, "expected integer");
  }
  return value;
}

function requiredNonnegativeInteger(payload: JsonPayload, command: string, field: string): number {
  const value = optionalNonnegativeInteger(payload, command, field);
  if (value === undefined) {
    throw invalidPayload(command, field, "missing required integer");
  }
  return value;
}

function optionalNonnegativeInteger(
  payload: JsonPayload,
  command: string,
  field: string,
): number | undefined {
  const value = optionalInteger(payload, command, field);
  if (value === undefined) {
    return undefined;
  }
  if (value < 0) {
    throw invalidPayload(command, field, "expected non-negative integer");
  }
  return value;
}

function requiredValue(payload: JsonPayload, command: string, field: string): unknown {
  const value = payload[field];
  if (value == null) {
    throw invalidPayload(command, field, "missing required value");
  }
  return value;
}

function invalidPayload(command: string, field: string, message: string): FrankentermTransportError {
  return new FrankentermTransportError(
    `invalid payload for ${JSON.stringify(command)} field ${JSON.stringify(field)}: ${message}`,
  );
}

function decodeEnvelope(stdout: string, command: string): JsonPayload {
  let decoded: unknown;
  try {
    decoded = JSON.parse(stdout);
  } catch (error: unknown) {
    throw new FrankentermTransportError(
      `robot command ${JSON.stringify(command)} returned invalid JSON`,
      error,
    );
  }
  if (!isRecord(decoded)) {
    throw new FrankentermTransportError(
      `robot command ${JSON.stringify(command)} returned non-object JSON envelope`,
    );
  }
  return decoded;
}

function robotError(envelope: JsonPayload): FrankentermRobotError {
  const rawMessage = envelope["error"] ?? envelope["message"];
  const message = typeof rawMessage === "string" && rawMessage !== "" ? rawMessage : "unknown robot error";
  const code = typeof envelope["error_code"] === "string" ? envelope["error_code"] : undefined;
  const hint = typeof envelope["hint"] === "string" ? envelope["hint"] : undefined;
  const elapsedMs = typeof envelope["elapsed_ms"] === "number" ? envelope["elapsed_ms"] : undefined;
  return new FrankentermRobotError(message, {
    code,
    hint,
    details: envelope["details"],
    elapsedMs,
    envelope,
  });
}

function stderrTail(stderr: string): string {
  const trimmed = stderr.trim();
  if (trimmed === "") {
    return "stderr was empty";
  }
  return trimmed.slice(-STDERR_LIMIT);
}

function mergedEnv(env: Record<string, string> | undefined): Record<string, string | undefined> | undefined {
  if (env === undefined) {
    return undefined;
  }
  return { ...process.env, ...env };
}

function withoutUndefined(payload: JsonPayload): JsonPayload {
  const clean: JsonPayload = {};
  for (const [key, value] of Object.entries(payload)) {
    if (value !== undefined) {
      clean[key] = value;
    }
  }
  return clean;
}

function isRecord(value: unknown): value is JsonPayload {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function chunkToString(chunk: unknown): string {
  if (typeof chunk === "string") {
    return chunk;
  }
  if (chunk && typeof (chunk as { toString?: (encoding?: string) => string }).toString === "function") {
    return (chunk as { toString: (encoding?: string) => string }).toString("utf8");
  }
  return String(chunk);
}

function errorCode(error: unknown): string | undefined {
  if (isRecord(error) && typeof error["code"] === "string") {
    return error["code"];
  }
  return undefined;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

function formatArgs(args: readonly string[]): string {
  return args.join(" ");
}
"#,
    );
    out
}

fn render_rust_client(surface: &SdkSurface) -> String {
    let mut out = String::from(
        "use frankenterm_core::robot_sdk_contracts::{RustSdkTransport, RustSdkTransportError};\nuse serde_json::json;\n\n",
    );

    for return_type in unique_return_types(surface) {
        out.push_str(&format!("pub type {return_type} = serde_json::Value;\n"));
    }

    out.push_str(
        "\npub struct FrankentermClient {\n    transport: RustSdkTransport,\n}\n\nimpl FrankentermClient {\n    pub fn new(socket_path: impl AsRef<std::path::Path>) -> Self {\n        Self {\n            transport: RustSdkTransport::new(socket_path),\n        }\n    }\n\n    pub fn with_token(\n        socket_path: impl AsRef<std::path::Path>,\n        token: impl Into<String>,\n    ) -> Self {\n        Self {\n            transport: RustSdkTransport::with_token(socket_path, token),\n        }\n    }\n\n    pub fn socket_exists(&self) -> bool {\n        self.transport.socket_exists()\n    }\n\n    pub fn supported_commands() -> &'static [&'static str] {\n        RustSdkTransport::supported_commands()\n    }\n\n    pub fn supports_command(command: &str) -> bool {\n        RustSdkTransport::supports_command(command)\n    }\n\n    pub async fn call(\n        &self,\n        command: &str,\n        payload: serde_json::Value,\n    ) -> Result<serde_json::Value, RustSdkTransportError> {\n        self.transport.call_value(command, payload).await\n    }\n",
    );

    for method in &surface.methods {
        out.push_str(&format!(
            "\n    pub async fn {}(&self{}) -> Result<{}, RustSdkTransportError> {{\n        self.call(\"{}\", {}).await\n    }}\n",
            method.method_name,
            render_rust_params(&method.params),
            method.return_type,
            method.command,
            render_rust_payload(&method.params),
        ));
    }

    out.push_str("}\n");
    out
}

fn render_go_client(surface: &SdkSurface) -> String {
    let mut out = String::from("package frankenterm\n\n");

    for return_type in unique_return_types(surface) {
        out.push_str(&format!("type {return_type} = map[string]interface{{}}\n"));
    }

    out.push_str(
        "\ntype FrankentermClient struct{}\n\nfunc (c *FrankentermClient) call(command string, payload map[string]interface{}) (map[string]interface{}, error) {\n\tpanic(\"transport not wired\")\n}\n",
    );

    for method in &surface.methods {
        out.push_str(&format!(
            "\nfunc (c *FrankentermClient) {}({}) ({}, error) {{\n\tresult, err := c.call(\"{}\", {})\n\tif err != nil {{\n\t\treturn nil, err\n\t}}\n\treturn result, nil\n}}\n",
            method.method_name,
            render_go_params(&method.params),
            method.return_type,
            method.command,
            render_go_payload(&method.params),
        ));
    }

    out
}

fn unique_return_types(surface: &SdkSurface) -> Vec<String> {
    let mut seen = BTreeSet::new();
    for method in &surface.methods {
        seen.insert(method.return_type.clone());
    }
    seen.into_iter().collect()
}

fn render_python_params(params: &[SdkParam]) -> String {
    let mut rendered = vec!["self".to_string()];
    for param in params {
        if param.optional {
            rendered.push(format!(
                "{}: {} | None = None",
                param.name, param.param_type
            ));
        } else {
            rendered.push(format!("{}: {}", param.name, param.param_type));
        }
    }
    rendered.join(", ")
}

fn render_typescript_params(params: &[SdkParam]) -> String {
    params
        .iter()
        .map(|param| {
            if param.optional {
                format!("{}?: {}", param.name, param.param_type)
            } else {
                format!("{}: {}", param.name, param.param_type)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_rust_params(params: &[SdkParam]) -> String {
    use std::fmt::Write;
    params.iter().fold(String::new(), |mut acc, param| {
        let _ = write!(acc, ", {}: {}", param.name, param.param_type);
        acc
    })
}

fn render_go_params(params: &[SdkParam]) -> String {
    params
        .iter()
        .map(|param| format!("{} {}", param.name, param.param_type))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_python_payload(params: &[SdkParam]) -> String {
    if params.is_empty() {
        return "{}".to_string();
    }

    let mut out = String::from("{\n");
    for param in params {
        out.push_str(&format!(
            "            \"{}\": {},\n",
            param.wire_name, param.name
        ));
    }
    out.push_str("        }");
    out
}

fn render_typescript_payload(params: &[SdkParam]) -> String {
    if params.is_empty() {
        return "{}".to_string();
    }

    let mut out = String::from("{\n");
    for param in params {
        out.push_str(&format!("      \"{}\": {},\n", param.wire_name, param.name));
    }
    out.push_str("    }");
    out
}

fn render_rust_payload(params: &[SdkParam]) -> String {
    if params.is_empty() {
        return "json!({})".to_string();
    }

    let mut out = String::from("json!({\n");
    for param in params {
        out.push_str(&format!(
            "            \"{}\": {},\n",
            param.wire_name, param.name
        ));
    }
    out.push_str("        })");
    out
}

fn render_go_payload(params: &[SdkParam]) -> String {
    if params.is_empty() {
        return "map[string]interface{}{}".to_string();
    }

    let mut out = String::from("map[string]interface{}{\n");
    for param in params {
        out.push_str(&format!("\t\t\"{}\": {},\n", param.wire_name, param.name));
    }
    out.push_str("\t}");
    out
}

// =============================================================================
// NTM compatibility shim
// =============================================================================

/// NTM field mapping direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MappingDirection {
    /// Map NTM field name to ft field name.
    NtmToFt,
    /// Map ft field name to NTM field name.
    FtToNtm,
}

/// A single field mapping between NTM and ft response formats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldMapping {
    /// NTM field name.
    pub ntm_field: String,
    /// ft field name.
    pub ft_field: String,
    /// Whether the field requires value transformation (not just rename).
    pub requires_transform: bool,
    /// Description of the transformation.
    pub transform_description: String,
}

/// Compatibility classification for a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompatLevel {
    /// Fully compatible — same schema.
    Full,
    /// Compatible with field mappings.
    MappedCompat,
    /// Partially compatible — some fields differ in semantics.
    Partial,
    /// Not compatible — different response structure.
    Incompatible,
    /// No NTM equivalent exists.
    NoEquivalent,
}

impl CompatLevel {
    /// Whether this level allows migration acceleration.
    #[must_use]
    pub fn allows_migration(&self) -> bool {
        matches!(self, Self::Full | Self::MappedCompat | Self::Partial)
    }

    /// Human label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::MappedCompat => "mapped-compat",
            Self::Partial => "partial",
            Self::Incompatible => "incompatible",
            Self::NoEquivalent => "no-equivalent",
        }
    }
}

/// NTM compatibility shim for a single command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmCompatEntry {
    /// ft command name.
    pub ft_command: String,
    /// NTM equivalent command (if any).
    pub ntm_command: String,
    /// Compatibility level.
    pub compat_level: CompatLevel,
    /// Field mappings.
    pub field_mappings: Vec<FieldMapping>,
    /// Fields present in NTM but absent in ft.
    pub ntm_only_fields: Vec<String>,
    /// Fields present in ft but absent in NTM.
    pub ft_only_fields: Vec<String>,
    /// Migration notes.
    pub notes: String,
}

impl NtmCompatEntry {
    /// Create a fully compatible entry.
    #[must_use]
    pub fn full_compat(command: impl Into<String>) -> Self {
        let cmd = command.into();
        Self {
            ft_command: cmd.clone(),
            ntm_command: cmd,
            compat_level: CompatLevel::Full,
            field_mappings: Vec::new(),
            ntm_only_fields: Vec::new(),
            ft_only_fields: Vec::new(),
            notes: String::new(),
        }
    }

    /// Create a no-equivalent entry.
    #[must_use]
    pub fn no_equivalent(ft_command: impl Into<String>) -> Self {
        Self {
            ft_command: ft_command.into(),
            ntm_command: String::new(),
            compat_level: CompatLevel::NoEquivalent,
            field_mappings: Vec::new(),
            ntm_only_fields: Vec::new(),
            ft_only_fields: Vec::new(),
            notes: "No NTM equivalent — ft-native only".into(),
        }
    }
}

/// The complete NTM compatibility shim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmCompatShim {
    /// Compatibility entries keyed by ft command.
    pub entries: BTreeMap<String, NtmCompatEntry>,
    /// Overall migration readiness.
    pub migration_ready: bool,
}

impl NtmCompatShim {
    /// Create a new shim.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            migration_ready: false,
        }
    }

    /// Register a compatibility entry.
    pub fn register(&mut self, entry: NtmCompatEntry) {
        self.entries.insert(entry.ft_command.clone(), entry);
    }

    /// Get compatibility level for a command.
    #[must_use]
    pub fn compat_level(&self, command: &str) -> CompatLevel {
        self.entries
            .get(command)
            .map(|e| e.compat_level)
            .unwrap_or(CompatLevel::NoEquivalent)
    }

    /// Commands that are fully compatible.
    #[must_use]
    pub fn fully_compatible(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, e)| e.compat_level == CompatLevel::Full)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// Commands that need mapping.
    #[must_use]
    pub fn needs_mapping(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, e)| e.compat_level == CompatLevel::MappedCompat)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// Commands that are incompatible or have no NTM equivalent.
    #[must_use]
    pub fn not_migratable(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, e)| !e.compat_level.allows_migration())
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// Migration readiness summary.
    #[must_use]
    pub fn readiness_summary(&self) -> CompatSummary {
        let total = self.entries.len();
        let full = self
            .entries
            .values()
            .filter(|e| e.compat_level == CompatLevel::Full)
            .count();
        let mapped = self
            .entries
            .values()
            .filter(|e| e.compat_level == CompatLevel::MappedCompat)
            .count();
        let partial = self
            .entries
            .values()
            .filter(|e| e.compat_level == CompatLevel::Partial)
            .count();
        let incompatible = self
            .entries
            .values()
            .filter(|e| e.compat_level == CompatLevel::Incompatible)
            .count();
        let no_equiv = self
            .entries
            .values()
            .filter(|e| e.compat_level == CompatLevel::NoEquivalent)
            .count();

        let migratable = full + mapped + partial;
        CompatSummary {
            total,
            full,
            mapped,
            partial,
            incompatible,
            no_equivalent: no_equiv,
            migration_coverage: if total > 0 {
                migratable as f64 / total as f64
            } else {
                0.0
            },
        }
    }

    /// Render a Markdown migration report suitable for artifact capture.
    #[must_use]
    pub fn render_markdown_summary(&self) -> String {
        let mut out = String::from("# NTM Compatibility Summary\n\n");
        let summary = self.readiness_summary();
        out.push_str(&format!(
            "- Total commands: {}\n- Fully compatible: {}\n- Mapped compatibility: {}\n- Partial compatibility: {}\n- Migration coverage: {:.2}%\n\n",
            summary.total,
            summary.full,
            summary.mapped,
            summary.partial,
            summary.migration_coverage * 100.0,
        ));
        out.push_str("| ft command | NTM command | compatibility | notes |\n");
        out.push_str("|------------|-------------|---------------|-------|\n");

        for entry in self.entries.values() {
            let ntm_command = if entry.ntm_command.is_empty() {
                "n/a"
            } else {
                entry.ntm_command.as_str()
            };
            let notes = if entry.notes.is_empty() {
                "none"
            } else {
                entry.notes.as_str()
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                entry.ft_command,
                ntm_command,
                entry.compat_level.label(),
                notes,
            ));
        }

        out
    }
}

impl Default for NtmCompatShim {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary of NTM compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatSummary {
    /// Total commands evaluated.
    pub total: usize,
    /// Fully compatible.
    pub full: usize,
    /// Compatible with mappings.
    pub mapped: usize,
    /// Partially compatible.
    pub partial: usize,
    /// Incompatible.
    pub incompatible: usize,
    /// No NTM equivalent.
    pub no_equivalent: usize,
    /// Migration coverage (0.0–1.0).
    pub migration_coverage: f64,
}

// =============================================================================
// Replay contract tests
// =============================================================================

/// A replay-based contract test definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayContractTest {
    /// Test identifier.
    pub test_id: String,
    /// Command being tested.
    pub command: String,
    /// Description.
    pub description: String,
    /// Input fixture path.
    pub input_fixture: String,
    /// Expected output fixture path.
    pub expected_output: String,
    /// Tolerance for numeric field comparisons.
    pub numeric_tolerance: f64,
    /// Fields to ignore during comparison.
    pub ignore_fields: Vec<String>,
    /// Whether this test is blocking.
    pub blocking: bool,
}

impl ReplayContractTest {
    /// Create a new test.
    #[must_use]
    pub fn new(
        test_id: impl Into<String>,
        command: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            test_id: test_id.into(),
            command: command.into(),
            description: description.into(),
            input_fixture: String::new(),
            expected_output: String::new(),
            numeric_tolerance: 0.01,
            ignore_fields: vec!["elapsed_ms".into(), "now".into()],
            blocking: true,
        }
    }

    /// Set fixture paths.
    #[must_use]
    pub fn with_fixtures(mut self, input: impl Into<String>, expected: impl Into<String>) -> Self {
        self.input_fixture = input.into();
        self.expected_output = expected.into();
        self
    }
}

/// Result of a replay contract test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayTestResult {
    /// Test ID.
    pub test_id: String,
    /// Whether the test passed.
    pub passed: bool,
    /// Diff summary (empty if passed).
    pub diff_summary: String,
    /// Number of field differences.
    pub diff_count: u64,
    /// Duration (ms).
    pub duration_ms: u64,
}

/// Aggregate replay test suite results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayTestSuiteResult {
    /// Suite identifier.
    pub suite_id: String,
    /// Per-test results.
    pub results: Vec<ReplayTestResult>,
    /// Total tests.
    pub total: usize,
    /// Passed.
    pub passed: usize,
    /// Failed.
    pub failed: usize,
    /// Pass rate.
    pub pass_rate: f64,
    /// Whether all blocking tests passed.
    pub blocking_pass: bool,
}

impl ReplayTestSuiteResult {
    /// Compute from results and test definitions.
    #[must_use]
    pub fn from_results(
        suite_id: impl Into<String>,
        results: Vec<ReplayTestResult>,
        tests: &[ReplayContractTest],
    ) -> Self {
        let total = results.len();
        let passed = results.iter().filter(|r| r.passed).count();
        let failed = total - passed;
        let pass_rate = if total > 0 {
            passed as f64 / total as f64
        } else {
            1.0
        };

        let blocking_pass = !results
            .iter()
            .any(|r| !r.passed && tests.iter().any(|t| t.test_id == r.test_id && t.blocking));

        Self {
            suite_id: suite_id.into(),
            results,
            total,
            passed,
            failed,
            pass_rate,
            blocking_pass,
        }
    }
}

/// Deterministic artifact bundle for machine-contract evidence capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractArtifactBundle {
    /// Pretty-printed endpoint catalog JSON.
    pub endpoint_specs_json: String,
    /// Markdown compatibility report.
    pub ntm_compat_markdown: String,
    /// Generated fully-supported client sources keyed by deterministic filename.
    pub sdk_sources: BTreeMap<String, String>,
    /// Pretty-printed replay test manifest JSON.
    pub replay_tests_json: String,
}

impl ContractArtifactBundle {
    /// Number of generated SDK source artifacts.
    #[must_use]
    pub fn sdk_count(&self) -> usize {
        self.sdk_sources.len()
    }
}

// =============================================================================
// Standard factories
// =============================================================================

/// Create standard NTM compat shim for core robot commands.
#[must_use]
pub fn standard_ntm_compat_shim() -> NtmCompatShim {
    let mut shim = NtmCompatShim::new();

    // Fully compatible commands (same schema)
    shim.register(NtmCompatEntry::full_compat("get-text"));
    shim.register(NtmCompatEntry::full_compat("send-text"));
    shim.register(NtmCompatEntry::full_compat("state"));
    shim.register(NtmCompatEntry::full_compat("events"));
    shim.register(NtmCompatEntry::full_compat("workflow-run"));
    shim.register(NtmCompatEntry::full_compat("workflow-list"));
    shim.register(NtmCompatEntry::full_compat("rules-list"));

    // Mapped-compatible (field renames)
    let batch = NtmCompatEntry {
        ft_command: "batch-get-text".into(),
        ntm_command: "batch-get-text".into(),
        compat_level: CompatLevel::MappedCompat,
        field_mappings: vec![FieldMapping {
            ntm_field: "pane_results".into(),
            ft_field: "results".into(),
            requires_transform: false,
            transform_description: "field rename only".into(),
        }],
        ntm_only_fields: Vec::new(),
        ft_only_fields: vec!["escapes_included".into()],
        notes: "ft adds escapes_included field not present in NTM".into(),
    };
    shim.register(batch);

    let search_entry = NtmCompatEntry {
        ft_command: "search".into(),
        ntm_command: "search".into(),
        compat_level: CompatLevel::MappedCompat,
        field_mappings: Vec::new(),
        ntm_only_fields: Vec::new(),
        ft_only_fields: vec!["metrics".into(), "mode".into()],
        notes: "ft adds semantic search metrics and mode field".into(),
    };
    shim.register(search_entry);

    // No NTM equivalent (ft-native only)
    shim.register(NtmCompatEntry::no_equivalent("tx-plan"));
    shim.register(NtmCompatEntry::no_equivalent("tx-run"));
    shim.register(NtmCompatEntry::no_equivalent("tx-show"));
    shim.register(NtmCompatEntry::no_equivalent("mission-state"));
    shim.register(NtmCompatEntry::no_equivalent("mission-decisions"));
    shim.register(NtmCompatEntry::no_equivalent("replay-inspect"));
    shim.register(NtmCompatEntry::no_equivalent("replay-diff"));
    shim.register(NtmCompatEntry::no_equivalent("replay-regression"));
    shim.register(NtmCompatEntry::no_equivalent("search-explain"));
    shim.register(NtmCompatEntry::no_equivalent("search-pipeline-status"));

    shim.migration_ready = true;
    shim
}

/// Standard replay contract tests for core robot workflows.
#[must_use]
pub fn standard_replay_contract_tests() -> Vec<ReplayContractTest> {
    vec![
        ReplayContractTest::new(
            "replay-get-text",
            "get-text",
            "deterministic get-text replay",
        )
        .with_fixtures(
            "fixtures/get-text-input.json",
            "fixtures/get-text-expected.json",
        ),
        ReplayContractTest::new("replay-search", "search", "deterministic search replay")
            .with_fixtures(
                "fixtures/search-input.json",
                "fixtures/search-expected.json",
            ),
        ReplayContractTest::new("replay-events", "events", "deterministic events replay")
            .with_fixtures(
                "fixtures/events-input.json",
                "fixtures/events-expected.json",
            ),
    ]
}

/// Render the standard machine-contract artifacts for export and auditing.
pub fn standard_contract_artifacts() -> Result<ContractArtifactBundle, serde_json::Error> {
    let specs = core_endpoint_specs();
    let shim = standard_ntm_compat_shim();

    let mut sdk_sources = BTreeMap::new();
    for language in [
        SdkLanguage::Python,
        SdkLanguage::TypeScript,
        SdkLanguage::Rust,
        SdkLanguage::Go,
    ] {
        if !language.is_fully_supported() {
            continue;
        }
        let mut sdk = SdkSurface::new(language, "frankenterm-client");
        sdk.generate_from_specs(&specs);
        sdk_sources.insert(sdk.artifact_filename(), sdk.render_client_source());
    }

    Ok(ContractArtifactBundle {
        endpoint_specs_json: serde_json::to_string_pretty(&specs)?,
        ntm_compat_markdown: shim.render_markdown_summary(),
        sdk_sources,
        replay_tests_json: serde_json::to_string_pretty(&standard_replay_contract_tests())?,
    })
}

/// Create standard endpoint specs for core pane operations.
#[must_use]
pub fn core_endpoint_specs() -> Vec<EndpointSpec> {
    let mut specs = Vec::new();

    let mut get_text = EndpointSpec::new("get-text", HttpMethod::Get, "Retrieve pane text content")
        .ntm_compatible();
    get_text.add_request_field(FieldSpec::required(
        "pane_id",
        FieldType::Integer,
        "Target pane ID",
    ));
    get_text.add_request_field(FieldSpec::optional(
        "tail_lines",
        FieldType::Integer,
        "Lines from end",
    ));
    get_text.add_request_field(FieldSpec::optional(
        "escapes",
        FieldType::Boolean,
        "Include ANSI escape sequences",
    ));
    get_text.add_response_field(FieldSpec::required(
        "pane_id",
        FieldType::Integer,
        "Pane ID",
    ));
    get_text.add_response_field(FieldSpec::required(
        "text",
        FieldType::String,
        "Pane content",
    ));
    get_text.add_response_field(FieldSpec::required(
        "tail_lines",
        FieldType::Integer,
        "Lines returned",
    ));
    get_text.add_response_field(FieldSpec::required(
        "escapes_included",
        FieldType::Boolean,
        "Whether ANSI escape sequences were included",
    ));
    get_text.add_response_field(FieldSpec::required(
        "truncated",
        FieldType::Boolean,
        "Whether truncated",
    ));
    get_text.add_response_field(FieldSpec::optional(
        "truncation_info",
        FieldType::Object(Vec::new()),
        "Truncation metadata when output exceeds limits",
    ));
    specs.push(get_text);

    let mut send_text =
        EndpointSpec::new("send-text", HttpMethod::Post, "Send keystrokes to a pane")
            .ntm_compatible();
    send_text.add_request_field(FieldSpec::required(
        "pane_id",
        FieldType::Integer,
        "Target pane",
    ));
    send_text.add_request_field(FieldSpec::required(
        "text",
        FieldType::String,
        "Text to send",
    ));
    send_text.add_request_field(FieldSpec::optional(
        "dry_run",
        FieldType::Boolean,
        "Preview without executing",
    ));
    send_text.add_request_field(FieldSpec::optional(
        "approval_code",
        FieldType::String,
        "Inline approval code for gated sends",
    ));
    send_text.add_request_field(FieldSpec::optional(
        "wait_for",
        FieldType::String,
        "Verification pattern to wait for after sending",
    ));
    send_text.add_request_field(FieldSpec::optional(
        "timeout_secs",
        FieldType::Integer,
        "Wait-for timeout in seconds",
    ));
    send_text.add_request_field(FieldSpec::optional(
        "wait_for_regex",
        FieldType::Boolean,
        "Treat wait_for as a regex",
    ));
    send_text.add_response_field(FieldSpec::required(
        "pane_id",
        FieldType::Integer,
        "Pane ID",
    ));
    send_text.add_response_field(FieldSpec::required(
        "injection",
        FieldType::Json,
        "Injection details",
    ));
    send_text.add_response_field(FieldSpec::optional(
        "wait_for",
        FieldType::Object(Vec::new()),
        "Verification result when wait_for is supplied",
    ));
    send_text.add_response_field(FieldSpec::optional(
        "verification_error",
        FieldType::String,
        "Wait-for verification failure description",
    ));
    specs.push(send_text);

    let mut state =
        EndpointSpec::new("state", HttpMethod::Get, "List pane states").ntm_compatible();
    state.add_request_field(FieldSpec::optional(
        "include_text",
        FieldType::Boolean,
        "Include per-pane text payloads in the response",
    ));
    state.add_request_field(FieldSpec::optional(
        "tail",
        FieldType::Integer,
        "Tail lines to include when include_text is enabled",
    ));
    state.add_request_field(FieldSpec::optional(
        "escapes",
        FieldType::Boolean,
        "Include ANSI escape sequences when include_text is enabled",
    ));
    state.add_response_field(FieldSpec::required(
        "panes",
        FieldType::Array(Box::new(FieldType::Object(Vec::new()))),
        "Pane state list",
    ));
    state.add_response_field(FieldSpec::optional(
        "tail_lines",
        FieldType::Integer,
        "Tail lines included when pane text is attached",
    ));
    state.add_response_field(FieldSpec::optional(
        "escapes_included",
        FieldType::Boolean,
        "Whether ANSI escape sequences were included for pane text",
    ));
    state.add_response_field(FieldSpec::optional(
        "pane_text",
        FieldType::Object(Vec::new()),
        "Per-pane text results when include_text is enabled",
    ));
    specs.push(state);

    let mut search = EndpointSpec::new("search", HttpMethod::Get, "Search pane content");
    search.add_request_field(FieldSpec::required(
        "query",
        FieldType::String,
        "Search query",
    ));
    search.add_request_field(FieldSpec::optional(
        "limit",
        FieldType::Integer,
        "Max results",
    ));
    search.add_request_field(FieldSpec::optional(
        "pane",
        FieldType::Integer,
        "Restrict search to a single pane",
    ));
    search.add_request_field(FieldSpec::optional(
        "since",
        FieldType::Integer,
        "Lower timestamp bound (epoch ms)",
    ));
    search.add_request_field(FieldSpec::optional(
        "until",
        FieldType::Integer,
        "Upper timestamp bound (epoch ms)",
    ));
    search.add_request_field(FieldSpec::optional(
        "snippets",
        FieldType::Boolean,
        "Whether highlighted snippets should be included",
    ));
    search.add_request_field(FieldSpec::optional(
        "mode",
        FieldType::String,
        "Search mode: lexical, semantic, or hybrid",
    ));
    search.add_response_field(FieldSpec::required(
        "query",
        FieldType::String,
        "Original query",
    ));
    search.add_response_field(FieldSpec::required(
        "results",
        FieldType::Array(Box::new(FieldType::Object(Vec::new()))),
        "Search hits",
    ));
    search.add_response_field(FieldSpec::required(
        "total_hits",
        FieldType::Integer,
        "Total matches",
    ));
    search.add_response_field(FieldSpec::required(
        "limit",
        FieldType::Integer,
        "Applied limit",
    ));
    search.add_response_field(FieldSpec::optional(
        "pane_filter",
        FieldType::Integer,
        "Applied pane filter",
    ));
    search.add_response_field(FieldSpec::optional(
        "since_filter",
        FieldType::Integer,
        "Applied lower timestamp filter",
    ));
    search.add_response_field(FieldSpec::optional(
        "until_filter",
        FieldType::Integer,
        "Applied upper timestamp filter",
    ));
    search.add_response_field(FieldSpec::optional(
        "mode",
        FieldType::String,
        "Search mode used for execution",
    ));
    search.add_response_field(FieldSpec::optional(
        "metrics",
        FieldType::Json,
        "Optional search pipeline metrics",
    ));
    specs.push(search);

    specs
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- FieldType ----

    /// ft-xbnl0.2.3 Cx-first: `call_with_cx` must surface
    /// `WatcherNotRunning` on a missing-socket — same as legacy
    /// `call` — proving the IPC cx-first path reaches the
    /// socket check. Only exercises the error-path semantics;
    /// full round-trip testing requires a running watcher
    /// daemon which is out of scope for unit tests.
    #[cfg(unix)]
    #[test]
    fn call_with_cx_surfaces_watcher_not_running() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let bogus = std::env::temp_dir().join(format!(
                    "ft-rusticmaple-tick87-robot-sdk-{}-{}.sock",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                ));
                assert!(!bogus.exists());
                let transport = RustSdkTransport::new(&bogus);
                let cx = crate::cx::for_request();

                let err: RustSdkTransportError = transport
                    .call_value_with_cx(&cx, "get-text", serde_json::json!({"pane_id": 1}))
                    .await
                    .expect_err("cx-first call on missing socket should error");

                match err {
                    RustSdkTransportError::Transport(UserVarError::WatcherNotRunning {
                        ..
                    }) => {}
                    other => panic!(
                        "expected Transport(WatcherNotRunning) on cx-first missing socket, got {other}"
                    ),
                }
            });
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn field_type_labels() {
        assert_eq!(FieldType::String.label(), "string");
        assert_eq!(FieldType::Integer.label(), "integer");
        assert_eq!(
            FieldType::Array(Box::new(FieldType::String)).label(),
            "array<string>"
        );
        assert_eq!(
            FieldType::Optional(Box::new(FieldType::Integer)).label(),
            "integer?"
        );
    }

    // ---- FieldSpec ----

    #[test]
    fn field_spec_constructors() {
        let req = FieldSpec::required("pane_id", FieldType::Integer, "Pane ID");
        assert!(req.required);
        assert_eq!(req.name, "pane_id");

        let opt =
            FieldSpec::optional("limit", FieldType::Integer, "Max results").with_example("100");
        assert!(!opt.required);
        assert_eq!(opt.example, "100");
    }

    // ---- EndpointSpec ----

    #[test]
    fn endpoint_spec_mutation_detection() {
        let get = EndpointSpec::new("get-text", HttpMethod::Get, "read");
        assert!(!get.is_mutation);

        let post = EndpointSpec::new("send-text", HttpMethod::Post, "write");
        assert!(post.is_mutation);
    }

    #[test]
    fn endpoint_required_fields() {
        let mut spec = EndpointSpec::new("test", HttpMethod::Get, "test");
        spec.add_request_field(FieldSpec::required("a", FieldType::String, "required"));
        spec.add_request_field(FieldSpec::optional("b", FieldType::Integer, "optional"));

        assert_eq!(spec.required_request_fields().len(), 1);
        assert_eq!(spec.required_request_fields()[0].name, "a");
    }

    // ---- SdkSurface ----

    #[test]
    fn sdk_generation_from_specs() {
        let specs = core_endpoint_specs();

        let mut py_sdk = SdkSurface::new(SdkLanguage::Python, "frankenterm");
        py_sdk.generate_from_specs(&specs);

        assert_eq!(py_sdk.method_count(), 4);
        assert_eq!(py_sdk.methods[0].method_name, "get_text");
        assert!(py_sdk.methods[0].is_async);

        let mut ts_sdk = SdkSurface::new(SdkLanguage::TypeScript, "frankenterm");
        ts_sdk.generate_from_specs(&specs);

        assert_eq!(ts_sdk.methods[0].method_name, "getText");
        assert_eq!(ts_sdk.methods[0].params[0].wire_name, "pane_id");
    }

    #[test]
    fn sdk_type_mapping() {
        assert_eq!(
            map_type_to_language(&FieldType::String, SdkLanguage::Python),
            "str"
        );
        assert_eq!(
            map_type_to_language(&FieldType::String, SdkLanguage::TypeScript),
            "string"
        );
        assert_eq!(
            map_type_to_language(&FieldType::String, SdkLanguage::Rust),
            "String"
        );
        assert_eq!(
            map_type_to_language(
                &FieldType::Array(Box::new(FieldType::Integer)),
                SdkLanguage::Rust
            ),
            "Vec<i64>"
        );
        assert_eq!(
            map_type_to_language(
                &FieldType::Optional(Box::new(FieldType::Boolean)),
                SdkLanguage::Go
            ),
            "*bool"
        );
    }

    // ---- to_camel_case ----

    #[test]
    fn camel_case_conversion() {
        assert_eq!(to_camel_case("get-text"), "getText");
        assert_eq!(to_camel_case("batch-get-text"), "batchGetText");
        assert_eq!(to_camel_case("search"), "search");
        assert_eq!(
            to_camel_case("search_pipeline_status"),
            "searchPipelineStatus"
        );
    }

    // ---- NtmCompatShim ----

    #[test]
    fn compat_level_migration() {
        assert!(CompatLevel::Full.allows_migration());
        assert!(CompatLevel::MappedCompat.allows_migration());
        assert!(CompatLevel::Partial.allows_migration());
        assert!(!CompatLevel::Incompatible.allows_migration());
        assert!(!CompatLevel::NoEquivalent.allows_migration());
    }

    #[test]
    fn standard_shim_has_entries() {
        let shim = standard_ntm_compat_shim();
        assert!(!shim.entries.is_empty());
        assert!(shim.migration_ready);
    }

    #[test]
    fn standard_shim_fully_compatible() {
        let shim = standard_ntm_compat_shim();
        let full = shim.fully_compatible();
        assert!(full.contains(&"get-text"));
        assert!(full.contains(&"send-text"));
        assert!(full.contains(&"state"));
    }

    #[test]
    fn standard_shim_no_equivalent() {
        let shim = standard_ntm_compat_shim();
        let no_equiv = shim.not_migratable();
        assert!(no_equiv.contains(&"tx-plan"));
        assert!(no_equiv.contains(&"mission-state"));
    }

    #[test]
    fn standard_shim_readiness_summary() {
        let shim = standard_ntm_compat_shim();
        let summary = shim.readiness_summary();
        assert!(summary.total > 0);
        assert!(summary.full > 0);
        assert!(summary.no_equivalent > 0);
        assert!(summary.migration_coverage > 0.0);
        assert!(summary.migration_coverage < 1.0);
    }

    #[test]
    fn standard_shim_markdown_summary() {
        let shim = standard_ntm_compat_shim();
        let markdown = shim.render_markdown_summary();
        assert!(markdown.contains("# NTM Compatibility Summary"));
        assert!(markdown.contains("| ft command | NTM command | compatibility | notes |"));
        assert!(markdown.contains("| get-text | get-text | full | none |"));
    }

    #[test]
    fn shim_compat_lookup() {
        let shim = standard_ntm_compat_shim();
        assert_eq!(shim.compat_level("get-text"), CompatLevel::Full);
        assert_eq!(shim.compat_level("tx-plan"), CompatLevel::NoEquivalent);
        assert_eq!(shim.compat_level("nonexistent"), CompatLevel::NoEquivalent);
    }

    // ---- ReplayContractTest ----

    #[test]
    fn replay_test_builder() {
        let test = ReplayContractTest::new("t1", "get-text", "test get-text")
            .with_fixtures("fixtures/input.json", "fixtures/expected.json");
        assert_eq!(test.test_id, "t1");
        assert_eq!(test.input_fixture, "fixtures/input.json");
        assert!(test.blocking);
        assert!(test.ignore_fields.contains(&"elapsed_ms".to_string()));
    }

    #[test]
    fn replay_suite_result() {
        let tests = vec![
            ReplayContractTest::new("t1", "get-text", "test 1"),
            ReplayContractTest::new("t2", "search", "test 2"),
        ];
        let results = vec![
            ReplayTestResult {
                test_id: "t1".into(),
                passed: true,
                diff_summary: String::new(),
                diff_count: 0,
                duration_ms: 10,
            },
            ReplayTestResult {
                test_id: "t2".into(),
                passed: false,
                diff_summary: "field 'total_hits' differs".into(),
                diff_count: 1,
                duration_ms: 15,
            },
        ];

        let suite = ReplayTestSuiteResult::from_results("suite-1", results, &tests);
        assert_eq!(suite.total, 2);
        assert_eq!(suite.passed, 1);
        assert_eq!(suite.failed, 1);
        assert_eq!(suite.pass_rate, 0.5);
        assert!(!suite.blocking_pass); // t2 is blocking and failed
    }

    // ---- Serde ----

    #[test]
    fn endpoint_spec_serde_roundtrip() {
        let specs = core_endpoint_specs();
        let json = serde_json::to_string(&specs).unwrap();
        let specs2: Vec<EndpointSpec> = serde_json::from_str(&json).unwrap();
        assert_eq!(specs2.len(), specs.len());
    }

    #[test]
    fn ntm_shim_serde_roundtrip() {
        let shim = standard_ntm_compat_shim();
        let json = serde_json::to_string(&shim).unwrap();
        let shim2: NtmCompatShim = serde_json::from_str(&json).unwrap();
        assert_eq!(shim2.entries.len(), shim.entries.len());
    }

    #[test]
    fn sdk_surface_serde_roundtrip() {
        let mut sdk = SdkSurface::new(SdkLanguage::Python, "frankenterm");
        sdk.generate_from_specs(&core_endpoint_specs());
        let json = serde_json::to_string(&sdk).unwrap();
        let sdk2: SdkSurface = serde_json::from_str(&json).unwrap();
        assert_eq!(sdk2.method_count(), sdk.method_count());
    }

    #[test]
    fn contract_artifact_bundle_renders_deterministic_exports() {
        let bundle = standard_contract_artifacts().unwrap();
        assert_eq!(bundle.sdk_count(), 3);
        assert!(
            bundle
                .endpoint_specs_json
                .contains("\"command\": \"get-text\"")
        );
        assert!(bundle.ntm_compat_markdown.contains("Migration coverage"));
        assert!(bundle.replay_tests_json.contains("replay-get-text"));
        assert!(
            bundle
                .sdk_sources
                .keys()
                .all(|filename| filename.starts_with("frankenterm_client_"))
        );
        assert_eq!(
            bundle
                .sdk_sources
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![
                "frankenterm_client_python.py",
                "frankenterm_client_rust.rs",
                "frankenterm_client_typescript.ts"
            ]
        );
        assert!(
            bundle
                .sdk_sources
                .values()
                .all(|source| !source.contains("transport not wired")),
            "production contract artifact bundle must not ship template-only SDK transports"
        );
    }

    #[test]
    fn contract_artifact_bundle_rust_sdk_source_includes_wire_keys() {
        let bundle = standard_contract_artifacts().unwrap();
        let rust = bundle
            .sdk_sources
            .get("frankenterm_client_rust.rs")
            .unwrap();

        assert!(rust.contains("\"pane_id\": pane_id"));
    }

    #[test]
    fn contract_artifact_bundle_python_sdk_source_includes_process_transport() {
        let bundle = standard_contract_artifacts().unwrap();
        let python = bundle
            .sdk_sources
            .get("frankenterm_client_python.py")
            .unwrap();

        assert!(python.contains("asyncio.create_subprocess_exec"));
        assert!(python.contains("[self._ft_binary, \"robot\", \"--format\", \"json\"]"));
        assert!(python.contains("FrankentermRobotError"));
        assert!(python.contains("FrankentermTransportError"));
        assert!(python.contains("_SUPPORTED_COMMANDS"));
        assert!(python.contains("wait_for_regex requires wait_for"));
        assert!(!python.contains("transport not wired"));
        assert!(!python.contains("NotImplementedError"));
    }

    #[test]
    fn contract_artifact_bundle_typescript_sdk_source_includes_process_transport() {
        let bundle = standard_contract_artifacts().unwrap();
        let typescript = bundle
            .sdk_sources
            .get("frankenterm_client_typescript.ts")
            .unwrap();

        assert!(typescript.contains("node:child_process"));
        assert!(typescript.contains("FRANKENTERM_FT_BINARY"));
        assert!(typescript.contains("FrankentermRobotError"));
        assert!(typescript.contains("FrankentermTransportError"));
        assert!(typescript.contains("FrankentermUnsupportedCommandError"));
        assert!(typescript.contains("SUPPORTED_COMMANDS"));
        assert!(typescript.contains("wait_for_regex requires wait_for"));
        assert!(typescript.contains("mode must be one of: lexical, semantic, hybrid"));
        assert!(!typescript.contains("transport not wired"));
    }

    #[test]
    fn rust_sdk_transport_supported_commands_are_explicit() {
        assert_eq!(
            RustSdkTransport::supported_commands(),
            &["get-text", "send-text", "state", "search"]
        );
        assert!(RustSdkTransport::supports_command("send-text"));
        assert!(!RustSdkTransport::supports_command("events"));
    }

    #[test]
    fn rust_sdk_transport_maps_send_text_to_robot_send_args() {
        let args = build_rust_sdk_ipc_args(
            "send-text",
            &serde_json::json!({
                "pane_id": 7,
                "text": "echo hello",
                "dry_run": true,
                "approval_code": "abc123",
                "wait_for": "Done",
                "timeout_secs": 45,
                "wait_for_regex": true
            }),
        )
        .unwrap();

        assert_eq!(
            args,
            vec![
                "send",
                "7",
                "echo hello",
                "--dry-run",
                "--approval-code",
                "abc123",
                "--wait-for",
                "Done",
                "--timeout-secs",
                "45",
                "--wait-for-regex",
            ]
        );
    }

    #[test]
    fn rust_sdk_transport_rejects_unsupported_command() {
        let err = build_rust_sdk_ipc_args("events", &serde_json::json!({})).unwrap_err();
        assert!(matches!(
            err,
            RustSdkTransportError::UnsupportedCommand { command } if command == "events"
        ));
    }

    #[test]
    fn rust_sdk_transport_rejects_wait_for_flags_without_wait_for_pattern() {
        let err = build_rust_sdk_ipc_args(
            "send-text",
            &serde_json::json!({
                "pane_id": 7,
                "text": "echo hello",
                "wait_for_regex": true
            }),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            RustSdkTransportError::InvalidPayload { field, .. } if field == "wait_for_regex"
        ));
    }

    #[test]
    fn rust_sdk_transport_decodes_robot_errors_from_ipc_response() {
        let err = decode_rust_sdk_response::<serde_json::Value>(IpcResponse::error_with_code(
            "robot.policy_denied",
            "blocked by policy",
            Some("use an approval code".to_string()),
        ))
        .unwrap_err();

        match err {
            RustSdkTransportError::Robot(robot_err) => {
                assert_eq!(robot_err.code.as_deref(), Some("robot.policy_denied"));
                assert_eq!(robot_err.message, "blocked by policy");
                assert_eq!(robot_err.hint.as_deref(), Some("use an approval code"));
            }
            other => panic!("expected robot error, got {other:?}"),
        }
    }

    #[test]
    fn rust_sdk_render_uses_real_transport_backend() {
        let mut sdk = SdkSurface::new(SdkLanguage::Rust, "frankenterm");
        sdk.generate_from_specs(&core_endpoint_specs());
        let source = sdk.render_client_source();

        assert!(source.contains("RustSdkTransport"));
        assert!(source.contains("supported_commands"));
        assert!(!source.contains("transport not wired"));
        assert!(!source.contains("unimplemented!("));
    }

    #[test]
    fn python_sdk_render_uses_real_transport_backend() {
        let mut sdk = SdkSurface::new(SdkLanguage::Python, "frankenterm");
        sdk.generate_from_specs(&core_endpoint_specs());
        let source = sdk.render_client_source();

        assert!(source.contains("asyncio.create_subprocess_exec"));
        assert!(source.contains("FRANKENTERM_FT_BINARY"));
        assert!(source.contains("FrankentermProcessResult"));
        assert!(source.contains("FrankentermUnsupportedCommandError"));
        assert!(source.contains("FrankentermRobotError"));
        assert!(source.contains("FrankentermTransportError"));
        assert!(source.contains("\"send\","));
        assert!(source.contains("--snippets=false"));
        assert!(source.contains("mode must be one of: lexical, semantic, hybrid"));
        assert!(!source.contains("transport not wired"));
        assert!(!source.contains("NotImplementedError"));
    }

    #[test]
    fn typescript_sdk_render_uses_real_transport_backend() {
        let mut sdk = SdkSurface::new(SdkLanguage::TypeScript, "frankenterm");
        sdk.generate_from_specs(&core_endpoint_specs());
        let source = sdk.render_client_source();

        assert!(source.contains("node:child_process"));
        assert!(source.contains("ProcessRunner"));
        assert!(source.contains("FrankentermClientOptions"));
        assert!(source.contains("FrankentermUnsupportedCommandError"));
        assert!(source.contains("FrankentermRobotError"));
        assert!(source.contains("\"send\","));
        assert!(source.contains("--snippets=false"));
        assert!(source.contains("mode must be one of: lexical, semantic, hybrid"));
        assert!(!source.contains("transport not wired"));
    }

    #[cfg(unix)]
    #[test]
    fn python_sdk_generated_transport_fixture_behaviors() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};

        if Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 missing; skipping generated Python SDK behavior fixture");
            return;
        }

        let dir = tempfile::tempdir().expect("create Python SDK fixture tempdir");
        let sleep_binary = dir.path().join("ft-sleep");
        let missing_binary = dir.path().join("missing-ft");
        assert!(!missing_binary.exists());
        std::fs::write(
            &sleep_binary,
            "#!/usr/bin/env python3\nimport time\ntime.sleep(1)\n",
        )
        .expect("write timeout fixture binary");
        let mut permissions = std::fs::metadata(&sleep_binary)
            .expect("stat timeout fixture binary")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&sleep_binary, permissions).expect("chmod timeout fixture binary");

        let mut sdk = SdkSurface::new(SdkLanguage::Python, "frankenterm");
        sdk.generate_from_specs(&core_endpoint_specs());
        let mut script = sdk.render_client_source();
        script.push_str(
            r#"

import asyncio
import json
import os


class FixtureRunner:
    def __init__(self, result):
        self.result = result
        self.calls = []

    async def __call__(self, args, env, timeout):
        self.calls.append((list(args), env, timeout))
        return self.result


async def main():
    success_runner = FixtureRunner(
        FrankentermProcessResult(
            0,
            json.dumps(
                {
                    "ok": True,
                    "data": {"pane_id": 7, "text": "hello", "tail_lines": 12},
                    "elapsed_ms": 1,
                    "version": "test",
                    "now": 1,
                    "schema_version": 1,
                }
            ).encode("utf-8"),
            b"",
        )
    )
    client = FrankentermClient(
        ft_binary="/bin/ft-test",
        timeout=12.0,
        env={"FT_TEST": "1"},
        runner=success_runner,
    )
    data = await client.get_text(7, tail_lines=12, escapes=True)
    assert data["text"] == "hello"
    assert success_runner.calls[0] == (
        [
            "/bin/ft-test",
            "robot",
            "--format",
            "json",
            "get-text",
            "7",
            "--tail",
            "12",
            "--escapes",
        ],
        {"FT_TEST": "1"},
        12.0,
    )

    robot_error_runner = FixtureRunner(
        FrankentermProcessResult(
            0,
            json.dumps(
                {
                    "ok": False,
                    "error": "blocked by policy",
                    "error_code": "robot.policy_denied",
                    "hint": "request approval",
                    "elapsed_ms": 1,
                    "version": "test",
                    "now": 1,
                    "schema_version": 1,
                }
            ).encode("utf-8"),
            b"",
        )
    )
    try:
        await FrankentermClient(runner=robot_error_runner).search("panic")
    except FrankentermRobotError as exc:
        assert exc.code == "robot.policy_denied"
        assert exc.hint == "request approval"
    else:
        raise AssertionError("expected robot error")

    try:
        await client.get_text("not-an-int")
    except FrankentermTransportError as exc:
        assert "pane_id" in str(exc)
    else:
        raise AssertionError("expected invalid payload transport error")

    try:
        await client._call("events", {})
    except FrankentermUnsupportedCommandError as exc:
        assert exc.command == "events"
    else:
        raise AssertionError("expected unsupported command error")

    try:
        await FrankentermClient(ft_binary=os.environ["FT_MISSING_BINARY"]).state()
    except FrankentermTransportError as exc:
        assert "ft binary not found" in str(exc)
    else:
        raise AssertionError("expected missing binary transport error")

    try:
        await FrankentermClient(ft_binary=os.environ["FT_SLEEP_BINARY"], timeout=0.01).state()
    except FrankentermTransportError as exc:
        assert "timed out" in str(exc)
    else:
        raise AssertionError("expected timeout transport error")


asyncio.run(main())
"#,
        );

        let mut child = Command::new("python3")
            .arg("-")
            .env("FT_MISSING_BINARY", &missing_binary)
            .env("FT_SLEEP_BINARY", &sleep_binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn generated Python SDK fixture");
        child
            .stdin
            .as_mut()
            .expect("open Python stdin")
            .write_all(script.as_bytes())
            .expect("write generated Python SDK fixture");
        let output = child.wait_with_output().expect("wait for Python fixture");

        assert!(
            output.status.success(),
            "generated Python SDK fixture failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn typescript_sdk_generated_transport_fixture_behaviors() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("node missing; skipping generated TypeScript SDK behavior fixture");
            return;
        }
        if Command::new("tsc").arg("--version").output().is_err() {
            eprintln!("tsc missing; skipping generated TypeScript SDK behavior fixture");
            return;
        }

        let dir = tempfile::tempdir().expect("create TypeScript SDK fixture tempdir");
        let sleep_binary = dir.path().join("ft-sleep");
        let missing_binary = dir.path().join("missing-ft");
        let source_path = dir.path().join("frankenterm_client_typescript.ts");
        let out_dir = dir.path().join("js");
        assert!(!missing_binary.exists());
        std::fs::create_dir(&out_dir).expect("create TypeScript SDK fixture out dir");
        std::fs::write(&sleep_binary, "#!/bin/sh\nsleep 1\n")
            .expect("write timeout fixture binary");
        let mut permissions = std::fs::metadata(&sleep_binary)
            .expect("stat timeout fixture binary")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&sleep_binary, permissions).expect("chmod timeout fixture binary");

        let mut sdk = SdkSurface::new(SdkLanguage::TypeScript, "frankenterm");
        sdk.generate_from_specs(&core_endpoint_specs());
        let mut script = sdk.render_client_source();
        script.push_str(
            r#"

type CallRecord = {
  args: readonly string[];
  env: Record<string, string> | undefined;
  timeoutMs: number | undefined;
};

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

function assertDeepEqual(actual: unknown, expected: unknown, message: string): void {
  const actualJson = JSON.stringify(actual);
  const expectedJson = JSON.stringify(expected);
  if (actualJson !== expectedJson) {
    throw new Error(`${message}: expected ${expectedJson}, got ${actualJson}`);
  }
}

async function expectError(
  label: string,
  action: () => Promise<unknown>,
  expectedCtor: new (...args: any[]) => Error,
  expectedText: string,
): Promise<Error> {
  try {
    await action();
  } catch (error: unknown) {
    assert(error instanceof expectedCtor, `${label}: wrong error class ${String(error)}`);
    assert(error.message.includes(expectedText), `${label}: wrong message ${error.message}`);
    return error;
  }
  throw new Error(`${label}: expected error`);
}

class ExposedClient extends FrankentermClient {
  callForTest(command: string, payload: JsonPayload): Promise<unknown> {
    return this.call(command, payload);
  }
}

async function main(): Promise<void> {
  const successCalls: CallRecord[] = [];
  const successRunner: ProcessRunner = async (args, env, timeoutMs) => {
    successCalls.push({ args: [...args], env, timeoutMs });
    return {
      returnCode: 0,
      stdout: JSON.stringify({
        ok: true,
        data: { pane_id: 7, text: "hello", tail_lines: 12 },
        elapsed_ms: 1,
        version: "test",
        now: 1,
        schema_version: 1,
      }),
      stderr: "warning: ignored on success",
    };
  };
  const client = new FrankentermClient({
    ftBinary: "/bin/ft-test",
    timeoutMs: 12000,
    env: { FT_TEST: "1" },
    runner: successRunner,
  });
  const data = (await client.getText(7, 12, true)) as { text: string };
  assert(data.text === "hello", "success envelope should return decoded data");
  assertDeepEqual(
    successCalls[0],
    {
      args: [
        "/bin/ft-test",
        "robot",
        "--format",
        "json",
        "get-text",
        "7",
        "--tail",
        "12",
        "--escapes",
      ],
      env: { FT_TEST: "1" },
      timeoutMs: 12000,
    },
    "getText should map to robot get-text args",
  );

  const robotErrorRunner: ProcessRunner = async () => ({
    returnCode: 0,
    stdout: JSON.stringify({
      ok: false,
      error: "blocked by policy",
      error_code: "robot.policy_denied",
      hint: "request approval",
      details: { rule: "fixture" },
      elapsed_ms: 2,
    }),
    stderr: "",
  });
  const robotError = await expectError(
    "robot error",
    () => new FrankentermClient({ runner: robotErrorRunner }).search("panic"),
    FrankentermRobotError,
    "blocked by policy",
  ) as FrankentermRobotError;
  assert(robotError.code === "robot.policy_denied", "robot error code should be preserved");
  assert(robotError.hint === "request approval", "robot error hint should be preserved");
  assert(robotError.elapsedMs === 2, "robot error elapsed_ms should be preserved");

  await expectError(
    "invalid payload",
    () => (client as any).getText("not-an-int"),
    FrankentermTransportError,
    "pane_id",
  );
  assert(successCalls.length === 1, "invalid payload should not invoke process runner");

  let unsupportedCalls = 0;
  const unsupportedRunner: ProcessRunner = async () => {
    unsupportedCalls += 1;
    return { returnCode: 0, stdout: "{}", stderr: "" };
  };
  const unsupported = await expectError(
    "unsupported command",
    () => new ExposedClient({ runner: unsupportedRunner }).callForTest("events", {}),
    FrankentermUnsupportedCommandError,
    "unsupported robot SDK command",
  ) as FrankentermUnsupportedCommandError;
  assert(unsupported.command === "events", "unsupported command should be preserved");
  assert(unsupportedCalls === 0, "unsupported command should not invoke process runner");

  await expectError(
    "invalid JSON",
    () => new FrankentermClient({
      runner: async () => ({ returnCode: 0, stdout: "not-json", stderr: "" }),
    }).state(),
    FrankentermTransportError,
    "invalid JSON",
  );

  await expectError(
    "nonzero exit",
    () => new FrankentermClient({
      runner: async () => ({ returnCode: 2, stdout: "", stderr: "boom" }),
    }).state(),
    FrankentermTransportError,
    "exited 2: boom",
  );

  const missingBinary = process.env.FT_MISSING_BINARY;
  assert(typeof missingBinary === "string", "FT_MISSING_BINARY must be set");
  await expectError(
    "missing binary",
    () => new FrankentermClient({ ftBinary: missingBinary, timeoutMs: 100 }).state(),
    FrankentermTransportError,
    "ft binary not found",
  );

  const sleepBinary = process.env.FT_SLEEP_BINARY;
  assert(typeof sleepBinary === "string", "FT_SLEEP_BINARY must be set");
  await expectError(
    "timeout",
    () => new FrankentermClient({ ftBinary: sleepBinary, timeoutMs: 10 }).state(),
    FrankentermTransportError,
    "timed out",
  );
}

main();
"#,
        );
        std::fs::write(&source_path, script).expect("write generated TypeScript SDK fixture");

        let tsc_output = Command::new("tsc")
            .arg("--target")
            .arg("ES2020")
            .arg("--module")
            .arg("commonjs")
            .arg("--strict")
            .arg("--skipLibCheck")
            .arg("--outDir")
            .arg(&out_dir)
            .arg(&source_path)
            .output()
            .expect("spawn tsc for generated TypeScript SDK fixture");
        assert!(
            tsc_output.status.success(),
            "generated TypeScript SDK fixture failed to compile\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&tsc_output.stdout),
            String::from_utf8_lossy(&tsc_output.stderr)
        );

        let js_path = out_dir.join("frankenterm_client_typescript.js");
        let output = Command::new("node")
            .arg(&js_path)
            .env("FT_MISSING_BINARY", &missing_binary)
            .env("FT_SLEEP_BINARY", &sleep_binary)
            .output()
            .expect("spawn generated TypeScript SDK fixture");
        assert!(
            output.status.success(),
            "generated TypeScript SDK fixture failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn ft_xbnl0_3_6_python_rust_and_typescript_sdk_targets_are_finish_line_supported() {
        // Truth-sweep guard for ft-xbnl0.3.6 plus ft-gzgfc.3: Python,
        // TypeScript, and Rust now ship real generated transports. Go stays
        // template-only until its dedicated promotion bead wires and tests it.
        assert!(SdkLanguage::Python.is_fully_supported());
        assert!(SdkLanguage::TypeScript.is_fully_supported());
        assert!(SdkLanguage::Rust.is_fully_supported());
        assert!(!SdkLanguage::Go.is_fully_supported());

        for lang in [
            SdkLanguage::Python,
            SdkLanguage::TypeScript,
            SdkLanguage::Rust,
        ] {
            let mut sdk = SdkSurface::new(lang, "frankenterm");
            sdk.generate_from_specs(&core_endpoint_specs());
            let source = sdk.render_client_source();
            assert!(
                !source.contains("transport not wired"),
                "{} SDK is marked supported and must not emit a template stub",
                lang.label()
            );
        }

        let mut go_sdk = SdkSurface::new(SdkLanguage::Go, "frankenterm");
        go_sdk.generate_from_specs(&core_endpoint_specs());
        let go_source = go_sdk.render_client_source();
        assert!(
            go_source.contains("transport not wired"),
            "Go SDK template must keep an explicit unsupported transport marker until promoted"
        );
    }

    // ---- E2E ----

    #[test]
    fn e2e_sdk_generation_and_compat_validation() {
        // Generate specs
        let specs = core_endpoint_specs();
        assert!(specs.len() >= 4);

        // Generate SDKs for all languages
        let languages = [
            SdkLanguage::Python,
            SdkLanguage::TypeScript,
            SdkLanguage::Rust,
            SdkLanguage::Go,
        ];

        for lang in languages {
            let mut sdk = SdkSurface::new(lang, "frankenterm-client");
            sdk.generate_from_specs(&specs);
            assert_eq!(sdk.method_count(), specs.len());

            // Verify all methods have correct names
            for method in &sdk.methods {
                assert!(!method.method_name.is_empty());
                assert!(!method.command.is_empty());
                assert!(method.is_async);
            }
        }

        // Validate NTM compatibility
        let shim = standard_ntm_compat_shim();
        let summary = shim.readiness_summary();

        // Core commands should be migratable
        for spec in &specs {
            if spec.ntm_compat {
                let level = shim.compat_level(&spec.command);
                assert!(
                    level.allows_migration(),
                    "NTM-compat command {} is not migratable: {:?}",
                    spec.command,
                    level
                );
            }
        }

        // Verify readiness
        assert!(summary.full >= 7); // at least 7 fully compatible
        assert!(summary.migration_coverage > 0.3); // >30% coverage
    }

    #[test]
    fn e2e_replay_contract_suite() {
        // Define replay tests for core commands
        let tests = vec![
            ReplayContractTest::new(
                "replay-get-text",
                "get-text",
                "get-text deterministic replay",
            )
            .with_fixtures(
                "fixtures/get-text-input.json",
                "fixtures/get-text-expected.json",
            ),
            ReplayContractTest::new("replay-search", "search", "search deterministic replay")
                .with_fixtures(
                    "fixtures/search-input.json",
                    "fixtures/search-expected.json",
                ),
            ReplayContractTest::new("replay-events", "events", "events deterministic replay")
                .with_fixtures(
                    "fixtures/events-input.json",
                    "fixtures/events-expected.json",
                ),
        ];

        // Simulate all passing
        let results: Vec<ReplayTestResult> = tests
            .iter()
            .map(|t| ReplayTestResult {
                test_id: t.test_id.clone(),
                passed: true,
                diff_summary: String::new(),
                diff_count: 0,
                duration_ms: 50,
            })
            .collect();

        let suite = ReplayTestSuiteResult::from_results("replay-contracts", results, &tests);
        assert_eq!(suite.pass_rate, 1.0);
        assert!(suite.blocking_pass);
        assert_eq!(suite.total, 3);
    }

    // ========================================================================
    // HttpMethod
    // ========================================================================

    #[test]
    fn http_method_labels() {
        assert_eq!(HttpMethod::Get.label(), "GET");
        assert_eq!(HttpMethod::Post.label(), "POST");
        assert_eq!(HttpMethod::Put.label(), "PUT");
        assert_eq!(HttpMethod::Delete.label(), "DELETE");
    }

    #[test]
    fn http_method_serde_roundtrip() {
        for method in [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Delete,
        ] {
            let json = serde_json::to_string(&method).unwrap();
            let back: HttpMethod = serde_json::from_str(&json).unwrap();
            assert_eq!(method, back);
        }
    }

    // ========================================================================
    // SdkLanguage
    // ========================================================================

    #[test]
    fn sdk_language_extensions() {
        assert_eq!(SdkLanguage::Python.extension(), ".py");
        assert_eq!(SdkLanguage::TypeScript.extension(), ".ts");
        assert_eq!(SdkLanguage::Rust.extension(), ".rs");
        assert_eq!(SdkLanguage::Go.extension(), ".go");
    }

    #[test]
    fn sdk_language_labels() {
        assert_eq!(SdkLanguage::Python.label(), "Python");
        assert_eq!(SdkLanguage::TypeScript.label(), "TypeScript");
        assert_eq!(SdkLanguage::Rust.label(), "Rust");
        assert_eq!(SdkLanguage::Go.label(), "Go");
    }

    #[test]
    fn sdk_language_serde_roundtrip() {
        for lang in [
            SdkLanguage::Python,
            SdkLanguage::TypeScript,
            SdkLanguage::Rust,
            SdkLanguage::Go,
        ] {
            let json = serde_json::to_string(&lang).unwrap();
            let back: SdkLanguage = serde_json::from_str(&json).unwrap();
            assert_eq!(lang, back);
        }
    }

    // ========================================================================
    // CompatLevel
    // ========================================================================

    #[test]
    fn compat_level_serde_roundtrip() {
        for level in [
            CompatLevel::Full,
            CompatLevel::MappedCompat,
            CompatLevel::Partial,
            CompatLevel::Incompatible,
            CompatLevel::NoEquivalent,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let back: CompatLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
    }

    // ========================================================================
    // MappingDirection
    // ========================================================================

    #[test]
    fn mapping_direction_serde_roundtrip() {
        for dir in [MappingDirection::NtmToFt, MappingDirection::FtToNtm] {
            let json = serde_json::to_string(&dir).unwrap();
            let back: MappingDirection = serde_json::from_str(&json).unwrap();
            assert_eq!(dir, back);
        }
    }

    // ========================================================================
    // ReplayTestResult
    // ========================================================================

    #[test]
    fn replay_test_result_serde_roundtrip() {
        let result = ReplayTestResult {
            test_id: "replay-1".to_string(),
            passed: false,
            diff_summary: "field $.data.count: expected 5, got 3".to_string(),
            diff_count: 1,
            duration_ms: 42,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ReplayTestResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.test_id, "replay-1");
        assert!(!back.passed);
        assert_eq!(back.diff_count, 1);
        assert_eq!(back.duration_ms, 42);
    }

    // ========================================================================
    // ReplayTestSuiteResult with failures
    // ========================================================================

    #[test]
    fn replay_suite_with_failures_reports_correct_pass_rate() {
        let tests = vec![
            ReplayContractTest::new("t1", "cmd1", "test 1"),
            ReplayContractTest::new("t2", "cmd2", "test 2"),
            ReplayContractTest::new("t3", "cmd3", "test 3"),
        ];

        let results = vec![
            ReplayTestResult {
                test_id: "t1".to_string(),
                passed: true,
                diff_summary: String::new(),
                diff_count: 0,
                duration_ms: 10,
            },
            ReplayTestResult {
                test_id: "t2".to_string(),
                passed: false,
                diff_summary: "mismatch".to_string(),
                diff_count: 2,
                duration_ms: 20,
            },
            ReplayTestResult {
                test_id: "t3".to_string(),
                passed: true,
                diff_summary: String::new(),
                diff_count: 0,
                duration_ms: 15,
            },
        ];

        let suite = ReplayTestSuiteResult::from_results("suite-mixed", results, &tests);
        assert_eq!(suite.total, 3);
        assert_eq!(suite.passed, 2);
        assert_eq!(suite.failed, 1);
        // 2/3 ≈ 0.6667
        assert!((suite.pass_rate - 2.0 / 3.0).abs() < 0.01);
        let failed_ids: Vec<&str> = suite
            .results
            .iter()
            .filter(|r| !r.passed)
            .map(|r| r.test_id.as_str())
            .collect();
        assert_eq!(failed_ids, vec!["t2"]);
    }

    // ========================================================================
    // ErrorCodeSpec
    // ========================================================================

    #[test]
    fn error_code_spec_serde_roundtrip() {
        let spec = ErrorCodeSpec {
            code: "wezterm.1001".to_string(),
            condition: "mux server unreachable".to_string(),
            recovery: "restart wezterm".to_string(),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: ErrorCodeSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.code, "wezterm.1001");
        assert_eq!(back.condition, "mux server unreachable");
        assert_eq!(back.recovery, "restart wezterm");
    }
}
