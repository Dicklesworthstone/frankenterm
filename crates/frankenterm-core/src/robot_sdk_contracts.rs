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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SdkLanguage {
    /// Python SDK.
    Python,
    /// TypeScript/JavaScript SDK.
    TypeScript,
    /// Rust SDK (client crate).
    Rust,
    /// Go SDK.
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
    for (i, c) in s.chars().enumerate() {
        if c == '-' || c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_uppercase().next().unwrap_or(c));
            capitalize_next = false;
        } else if i == 0 {
            result.push(c);
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

fn render_python_client(surface: &SdkSurface) -> String {
    let mut out = String::from("from __future__ import annotations\n\nfrom typing import Any\n\n");

    for return_type in unique_return_types(surface) {
        out.push_str(&format!("{return_type} = dict[str, Any]\n"));
    }

    out.push_str(
        "\n\nclass FrankentermClient:\n    async def _call(self, command: str, payload: dict[str, Any]) -> Any:\n        raise NotImplementedError(\"transport not wired\")\n",
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

    out
}

fn render_typescript_client(surface: &SdkSurface) -> String {
    let mut out = String::from("export type JsonPayload = Record<string, unknown>;\n\n");

    for return_type in unique_return_types(surface) {
        out.push_str(&format!("export type {return_type} = unknown;\n"));
    }

    out.push_str(
        "\nexport class FrankentermClient {\n  protected async call(command: string, payload: JsonPayload): Promise<unknown> {\n    throw new Error(`transport not wired for ${command}`);\n  }\n",
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
    out
}

fn render_rust_client(surface: &SdkSurface) -> String {
    let mut out = String::from("use serde_json::json;\n\n");

    for return_type in unique_return_types(surface) {
        out.push_str(&format!("pub type {return_type} = serde_json::Value;\n"));
    }

    out.push_str(
        "\npub struct FrankentermClient;\n\nimpl FrankentermClient {\n    async fn call(&self, _command: &str, _payload: serde_json::Value) -> serde_json::Value {\n        unimplemented!(\"transport not wired\")\n    }\n",
    );

    for method in &surface.methods {
        out.push_str(&format!(
            "\n    pub async fn {}(&self{}) -> {} {{\n        self.call(\"{}\", {}).await\n    }}\n",
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
    let mut seen = BTreeMap::new();
    for method in &surface.methods {
        seen.insert(method.return_type.clone(), ());
    }
    seen.into_keys().collect()
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
    params
        .iter()
        .map(|param| format!(", {}: {}", param.name, param.param_type))
        .collect::<String>()
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
    /// Generated client stubs keyed by deterministic filename.
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
        "truncated",
        FieldType::Boolean,
        "Whether truncated",
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
    specs.push(send_text);

    let mut state =
        EndpointSpec::new("state", HttpMethod::Get, "List pane states").ntm_compatible();
    state.add_response_field(FieldSpec::required(
        "panes",
        FieldType::Array(Box::new(FieldType::Object(Vec::new()))),
        "Pane state list",
    ));
    state.add_response_field(FieldSpec::required(
        "tail_lines",
        FieldType::Integer,
        "Tail lines",
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
        assert_eq!(bundle.sdk_count(), 4);
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
    }

    #[test]
    fn contract_artifact_bundle_sdk_sources_include_wire_keys() {
        let bundle = standard_contract_artifacts().unwrap();
        let python = bundle
            .sdk_sources
            .get("frankenterm_client_python.py")
            .unwrap();
        let typescript = bundle
            .sdk_sources
            .get("frankenterm_client_typescript.ts")
            .unwrap();
        let rust = bundle
            .sdk_sources
            .get("frankenterm_client_rust.rs")
            .unwrap();

        assert!(python.contains("\"pane_id\": pane_id"));
        assert!(typescript.contains("\"pane_id\": paneId"));
        assert!(rust.contains("\"pane_id\": pane_id"));
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
