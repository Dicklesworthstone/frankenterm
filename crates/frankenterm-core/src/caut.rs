//! Wrapper for the `caut` CLI (usage/refresh JSON parsing with safety).
//!
//! This module treats caut as the source of truth for account usage data.
//! It provides a small, typed API with:
//! - hard timeouts
//! - output size limits
//! - JSON parsing with redacted error previews

use crate::agent_provider::AgentProvider;
use crate::error::Remediation;
use crate::policy::Redactor;
use crate::runtime_compat::timeout;
use crate::suggestions::Platform;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;

/// Supported caut services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CautService {
    OpenAI,
    Anthropic,
    Google,
}

impl CautService {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
        }
    }

    /// caut provider argument corresponding to this service.
    #[must_use]
    pub fn provider_arg(self) -> &'static str {
        match self {
            Self::OpenAI => "codex",
            Self::Anthropic => "claude",
            Self::Google => "gemini",
        }
    }

    /// Parse user-provided service input.
    #[must_use]
    pub fn from_cli_input(input: &str) -> Option<Self> {
        if is_openai_slug(input) {
            return Some(Self::OpenAI);
        }
        if is_anthropic_slug(input) {
            return Some(Self::Anthropic);
        }
        if is_google_slug(input) {
            return Some(Self::Google);
        }
        None
    }

    /// Supported service values for CLI/UI hints.
    #[must_use]
    pub fn supported_cli_inputs() -> &'static [&'static str] {
        &["openai", "codex", "anthropic", "claude", "google", "gemini"]
    }

    /// Map a canonical agent provider to the corresponding caut service.
    #[must_use]
    pub fn from_provider(provider: &AgentProvider) -> Option<Self> {
        match provider {
            AgentProvider::Codex => Some(Self::OpenAI),
            AgentProvider::Claude => Some(Self::Anthropic),
            AgentProvider::Gemini => Some(Self::Google),
            AgentProvider::Unknown(slug) if is_openai_slug(slug) => Some(Self::OpenAI),
            AgentProvider::Unknown(slug) if is_anthropic_slug(slug) => Some(Self::Anthropic),
            AgentProvider::Unknown(slug) if is_google_slug(slug) => Some(Self::Google),
            _ => None,
        }
    }

    /// Canonical provider hint for this service.
    #[must_use]
    pub fn provider_hint(self) -> AgentProvider {
        match self {
            Self::OpenAI => AgentProvider::Codex,
            Self::Anthropic => AgentProvider::Claude,
            Self::Google => AgentProvider::Gemini,
        }
    }
}

fn is_openai_slug(slug: &str) -> bool {
    matches!(
        slug.trim().to_ascii_lowercase().as_str(),
        "openai" | "codex" | "chatgpt" | "chat-gpt" | "chat_gpt" | "gpt" | "gpt4" | "gpt-4"
    )
}

fn is_anthropic_slug(slug: &str) -> bool {
    matches!(
        slug.trim().to_ascii_lowercase().as_str(),
        "anthropic" | "claude" | "claude-code" | "claude_code"
    )
}

fn is_google_slug(slug: &str) -> bool {
    matches!(
        slug.trim().to_ascii_lowercase().as_str(),
        "google" | "google-ai" | "google_ai" | "gemini" | "gemini-cli" | "gemini_cli"
    )
}

impl std::fmt::Display for CautService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Parsed output for `caut usage`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CautUsage {
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub accounts: Vec<CautAccountUsage>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Parsed output for `caut refresh`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CautRefresh {
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub refreshed_at: Option<String>,
    #[serde(default)]
    pub accounts: Vec<CautAccountUsage>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Account usage details (best-effort parsing).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CautAccountUsage {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, alias = "percentRemaining")]
    pub percent_remaining: Option<f64>,
    #[serde(default, alias = "limitHours")]
    pub limit_hours: Option<u64>,
    #[serde(default, alias = "resetAt")]
    pub reset_at: Option<String>,
    #[serde(default, alias = "tokensUsed")]
    pub tokens_used: Option<u64>,
    #[serde(default, alias = "tokensRemaining")]
    pub tokens_remaining: Option<u64>,
    #[serde(default, alias = "tokensLimit")]
    pub tokens_limit: Option<u64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Errors produced by the caut wrapper.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CautError {
    #[error("caut is not installed or not found on PATH")]
    NotInstalled,
    #[error("caut timed out after {timeout_secs}s")]
    Timeout { timeout_secs: u64 },
    #[error("caut failed with exit code {status}: {stderr}")]
    NonZeroExit { status: i32, stderr: String },
    #[error("caut output exceeded {max_bytes} bytes")]
    OutputTooLarge { bytes: usize, max_bytes: usize },
    #[error("caut returned invalid JSON: {message}")]
    InvalidJson { message: String, preview: String },
    #[error("caut I/O error: {message}")]
    Io { message: String },
}

impl CautError {
    /// Optional remediation guidance for this error.
    #[must_use]
    pub fn remediation(&self) -> Remediation {
        match self {
            Self::NotInstalled => {
                let mut remediation =
                    Remediation::new("Install caut and ensure it is available on PATH.")
                        .command("Verify install", "caut --version");
                if let Some(cmd) = Platform::detect().install_command("caut") {
                    remediation = remediation.command("Install caut", cmd);
                }
                remediation.alternative("If caut is installed elsewhere, add it to PATH.")
            }
            Self::Timeout { timeout_secs } => Remediation::new(format!(
                "caut did not respond within {timeout_secs}s. Retry or check system load."
            ))
            .command("Retry usage", "caut usage --provider codex --format json")
            .alternative("Increase the timeout for caut commands."),
            Self::NonZeroExit { .. } => Remediation::new(
                "caut exited with an error. Check caut logs or rerun with verbose output.",
            )
            .command("Retry usage", "caut usage --provider codex --format json")
            .alternative("Ensure caut is authenticated for the target service."),
            Self::OutputTooLarge { .. } => Remediation::new(
                "caut output was too large. Reduce output size or tighten the account set.",
            )
            .alternative("Limit caut to a smaller account pool."),
            Self::InvalidJson { .. } => Remediation::new(
                "caut returned malformed JSON. Upgrade caut or verify output format.",
            )
            .command("Check caut version", "caut --version")
            .alternative("Report the issue with a redacted output sample."),
            Self::Io { .. } => {
                Remediation::new("I/O error while running caut. Check permissions and retry.")
                    .alternative("Verify caut binary permissions.")
            }
        }
    }
}

/// Thin wrapper around the caut CLI.
#[derive(Debug, Clone)]
pub struct CautClient {
    binary: String,
    timeout: Duration,
    max_output_bytes: usize,
    max_error_bytes: usize,
}

impl Default for CautClient {
    fn default() -> Self {
        Self {
            binary: "caut".to_string(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 256 * 1024,
            max_error_bytes: 8 * 1024,
        }
    }
}

impl CautClient {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    #[must_use]
    pub fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout = Duration::from_secs(timeout_secs);
        self
    }

    #[must_use]
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    #[must_use]
    pub fn with_max_error_bytes(mut self, max_error_bytes: usize) -> Self {
        self.max_error_bytes = max_error_bytes;
        self
    }

    /// Fetch usage data via `caut usage`.
    pub async fn usage(&self, service: CautService) -> Result<CautUsage, CautError> {
        let args = Self::build_args("usage", service);
        let output = self.run(&args).await?;
        parse_usage_json(&output, service, self.max_error_bytes)
    }

    /// Refresh usage data via `caut usage`.
    ///
    /// Newer caut versions removed the `refresh` subcommand. A provider-scoped
    /// `usage` call performs the refresh/read in one step and returns the latest
    /// account snapshot.
    pub async fn refresh(&self, service: CautService) -> Result<CautRefresh, CautError> {
        let args = Self::build_args("usage", service);
        let output = self.run(&args).await?;
        parse_refresh_json(&output, service, self.max_error_bytes)
    }

    fn build_args(subcommand: &str, service: CautService) -> Vec<String> {
        vec![
            subcommand.to_string(),
            "--provider".to_string(),
            service.provider_arg().to_string(),
            "--format".to_string(),
            "json".to_string(),
        ]
    }

    async fn run(&self, args: &[String]) -> Result<String, CautError> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(args);
        cmd.kill_on_drop(true);

        let output = match timeout(self.timeout, cmd.output()).await {
            Ok(result) => result.map_err(|err| categorize_io_error(&err))?,
            Err(_) => {
                return Err(CautError::Timeout {
                    timeout_secs: self.timeout.as_secs(),
                });
            }
        };

        if !output.status.success() {
            let status = output.status.code().unwrap_or(-1);
            let stderr_bytes = if output.stderr.len() > self.max_error_bytes {
                &output.stderr[..self.max_error_bytes]
            } else {
                &output.stderr
            };
            let stderr = String::from_utf8_lossy(stderr_bytes).to_string();
            let stderr_preview = redact_and_truncate(&stderr, self.max_error_bytes);
            return Err(CautError::NonZeroExit {
                status,
                stderr: stderr_preview,
            });
        }

        // Check raw byte length BEFORE allocating string to prevent OOM on huge outputs
        let raw_bytes = output.stdout.len();
        if raw_bytes > self.max_output_bytes {
            return Err(CautError::OutputTooLarge {
                bytes: raw_bytes,
                max_bytes: self.max_output_bytes,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        Ok(stdout)
    }
}

fn categorize_io_error(err: &std::io::Error) -> CautError {
    match err.kind() {
        std::io::ErrorKind::NotFound => CautError::NotInstalled,
        _ => CautError::Io {
            message: err.to_string(),
        },
    }
}

fn parse_json<T: DeserializeOwned>(input: &str, max_preview: usize) -> Result<T, CautError> {
    serde_json::from_str(input).map_err(|err| CautError::InvalidJson {
        message: err.to_string(),
        preview: redact_and_truncate(input, max_preview),
    })
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CautV1Envelope {
    #[serde(rename = "schemaVersion", default)]
    schema_version: Option<String>,
    #[serde(rename = "generatedAt", default)]
    generated_at: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    data: Vec<CautV1UsageEntry>,
    #[serde(default)]
    errors: Vec<String>,
    #[serde(default)]
    meta: HashMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CautV1UsageEntry {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    status: Option<Value>,
    #[serde(default)]
    usage: Option<CautV1UsagePayload>,
    #[serde(rename = "authWarning", default)]
    auth_warning: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CautV1UsagePayload {
    #[serde(default)]
    primary: Option<Value>,
    #[serde(default)]
    secondary: Option<Value>,
    #[serde(rename = "updatedAt", default)]
    updated_at: Option<String>,
    #[serde(default)]
    identity: Option<CautV1Identity>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct CautV1Identity {
    #[serde(rename = "accountEmail", default)]
    account_email: Option<String>,
    #[serde(rename = "accountOrganization", default)]
    account_organization: Option<String>,
    #[serde(rename = "loginMethod", default)]
    login_method: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

/// Returns true if the JSON input looks like a caut.v1 envelope
/// (contains a `schemaVersion` field). This discriminator must be
/// checked before attempting a direct parse into CautUsage/CautRefresh,
/// because those types have `#[serde(flatten)]` which absorbs any
/// unknown fields and always succeeds — causing the v1 fallback path
/// to never execute.
fn is_caut_v1_envelope(input: &str) -> bool {
    input.contains("\"schemaVersion\"")
}

fn parse_usage_json(
    input: &str,
    service: CautService,
    max_preview: usize,
) -> Result<CautUsage, CautError> {
    if !is_caut_v1_envelope(input) {
        if let Ok(mut usage) = parse_json::<CautUsage>(input, max_preview) {
            if usage.service.is_none() {
                usage.service = Some(service.as_str().to_string());
            }
            return Ok(usage);
        }
    }

    let envelope: CautV1Envelope = parse_json(input, max_preview)?;
    Ok(caut_v1_to_usage(envelope, service))
}

fn parse_refresh_json(
    input: &str,
    service: CautService,
    max_preview: usize,
) -> Result<CautRefresh, CautError> {
    if !is_caut_v1_envelope(input) {
        if let Ok(mut refresh) = parse_json::<CautRefresh>(input, max_preview) {
            if refresh.service.is_none() {
                refresh.service = Some(service.as_str().to_string());
            }
            return Ok(refresh);
        }
    }

    let envelope: CautV1Envelope = parse_json(input, max_preview)?;
    Ok(caut_v1_to_refresh(envelope, service))
}

fn caut_v1_to_usage(envelope: CautV1Envelope, service: CautService) -> CautUsage {
    let accounts = envelope.data.iter().map(caut_v1_entry_to_account).collect();
    CautUsage {
        service: Some(service.as_str().to_string()),
        generated_at: envelope.generated_at.clone(),
        accounts,
        extra: caut_v1_extra(envelope),
    }
}

fn caut_v1_to_refresh(envelope: CautV1Envelope, service: CautService) -> CautRefresh {
    let refreshed_at = envelope.generated_at.clone();
    let accounts = envelope.data.iter().map(caut_v1_entry_to_account).collect();
    CautRefresh {
        service: Some(service.as_str().to_string()),
        refreshed_at,
        accounts,
        extra: caut_v1_extra(envelope),
    }
}

fn caut_v1_extra(mut envelope: CautV1Envelope) -> HashMap<String, Value> {
    let mut extra = std::mem::take(&mut envelope.meta);
    if let Some(schema_version) = envelope.schema_version.take() {
        extra.insert("schemaVersion".to_string(), Value::String(schema_version));
    }
    if let Some(command) = envelope.command.take() {
        extra.insert("command".to_string(), Value::String(command));
    }
    if !envelope.errors.is_empty() {
        extra.insert(
            "errors".to_string(),
            Value::Array(envelope.errors.into_iter().map(Value::String).collect()),
        );
    }
    extra
}

fn caut_v1_entry_to_account(entry: &CautV1UsageEntry) -> CautAccountUsage {
    let usage = entry.usage.as_ref();
    let identity = usage.and_then(|value| value.identity.as_ref());

    let mut extra = entry.extra.clone();
    if let Some(provider) = &entry.provider {
        extra.insert("provider".to_string(), Value::String(provider.clone()));
    }
    if let Some(source) = &entry.source {
        extra.insert("source".to_string(), Value::String(source.clone()));
    }
    if let Some(status) = &entry.status {
        extra.insert("status".to_string(), status.clone());
    }
    if let Some(auth_warning) = &entry.auth_warning {
        extra.insert(
            "auth_warning".to_string(),
            Value::String(auth_warning.clone()),
        );
    }
    if let Some(usage) = usage {
        if let Some(primary) = &usage.primary {
            extra.insert("primary".to_string(), primary.clone());
        }
        if let Some(secondary) = &usage.secondary {
            extra.insert("secondary".to_string(), secondary.clone());
        }
        if let Some(updated_at) = &usage.updated_at {
            extra.insert("updated_at".to_string(), Value::String(updated_at.clone()));
        }
        if !usage.extra.is_empty() {
            let mut usage_extra = serde_json::Map::new();
            for (key, value) in &usage.extra {
                usage_extra.insert(key.clone(), value.clone());
            }
            extra.insert("usage_extra".to_string(), Value::Object(usage_extra));
        }
    }
    if let Some(identity) = identity {
        if let Ok(identity_value) = serde_json::to_value(identity) {
            extra.insert("identity".to_string(), identity_value);
        }
    }

    CautAccountUsage {
        id: entry
            .account
            .clone()
            .or_else(|| identity.and_then(|value| value.account_email.clone())),
        name: identity
            .and_then(|value| value.account_organization.clone())
            .or_else(|| entry.account.clone()),
        percent_remaining: usage.and_then(|value| {
            extract_usage_f64(
                value,
                &[
                    "percentRemaining",
                    "percent_remaining",
                    "remainingPercent",
                    "quotaPercentRemaining",
                ],
            )
        }),
        limit_hours: usage.and_then(|value| {
            extract_usage_u64(value, &["limitHours", "limit_hours", "quotaHours", "limit"])
        }),
        reset_at: usage.and_then(|value| {
            extract_usage_string(
                value,
                &["resetAt", "reset_at", "reset", "resetsAt", "nextResetAt"],
            )
        }),
        tokens_used: usage.and_then(|value| {
            extract_usage_u64(
                value,
                &["tokensUsed", "tokens_used", "usedTokens", "usageTokens"],
            )
        }),
        tokens_remaining: usage.and_then(|value| {
            extract_usage_u64(
                value,
                &[
                    "tokensRemaining",
                    "tokens_remaining",
                    "remainingTokens",
                    "availableTokens",
                ],
            )
        }),
        tokens_limit: usage.and_then(|value| {
            extract_usage_u64(
                value,
                &["tokensLimit", "tokens_limit", "tokenLimit", "quotaLimit"],
            )
        }),
        extra,
    }
}

fn extract_usage_f64(usage: &CautV1UsagePayload, keys: &[&str]) -> Option<f64> {
    extract_usage_value(usage, keys).and_then(value_as_f64)
}

fn extract_usage_u64(usage: &CautV1UsagePayload, keys: &[&str]) -> Option<u64> {
    extract_usage_value(usage, keys).and_then(value_as_u64)
}

fn extract_usage_string(usage: &CautV1UsagePayload, keys: &[&str]) -> Option<String> {
    extract_usage_value(usage, keys).and_then(value_as_string)
}

fn extract_usage_value<'a>(usage: &'a CautV1UsagePayload, keys: &[&str]) -> Option<&'a Value> {
    extract_object_value(usage.primary.as_ref(), keys)
        .or_else(|| extract_object_value(usage.secondary.as_ref(), keys))
}

fn extract_object_value<'a>(candidate: Option<&'a Value>, keys: &[&str]) -> Option<&'a Value> {
    let object = candidate.and_then(Value::as_object)?;
    for key in keys {
        let value = object.get(*key)?;
        if !value.is_null() {
            return Some(value);
        }
    }
    None
}

fn value_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|n| n as f64))
        .or_else(|| value.as_u64().map(|n| n as f64))
        .or_else(|| value.as_str()?.parse::<f64>().ok())
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| value.as_str()?.parse::<u64>().ok())
}

fn value_as_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .or_else(|| value.as_i64().map(|n| n.to_string()))
        .or_else(|| value.as_f64().map(|n| n.to_string()))
}

fn redact_and_truncate(input: &str, max_len: usize) -> String {
    let redactor = Redactor::new();
    let redacted = redactor.redact(input);
    if redacted.len() <= max_len {
        return redacted;
    }

    // `max_len` is byte-oriented (CLI/output budgets). Truncate on a UTF-8
    // boundary so we never exceed that byte budget while keeping valid UTF-8.
    let mut end = max_len.min(redacted.len());
    while end > 0 && !redacted.is_char_boundary(end) {
        end -= 1;
    }

    let mut truncated = redacted[..end].to_string();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn build_args_includes_service_and_format() {
        let args = CautClient::build_args("usage", CautService::OpenAI);
        assert_eq!(
            args,
            ["usage", "--provider", "codex", "--format", "json"]
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_usage_supports_caut_v1_schema() {
        let payload = json!({
            "schemaVersion": "caut.v1",
            "generatedAt": "2026-02-27T21:00:00Z",
            "command": "usage",
            "data": [
                {
                    "provider": "codex",
                    "account": "acc-primary",
                    "usage": {
                        "primary": {
                            "percentRemaining": 82.5,
                            "tokensUsed": 1750,
                            "tokensRemaining": 8250,
                            "tokensLimit": 10000,
                            "resetAt": "2026-03-01T00:00:00Z"
                        },
                        "updatedAt": "2026-02-27T20:59:59Z",
                        "identity": {
                            "accountEmail": "test@example.com",
                            "accountOrganization": "Personal"
                        }
                    }
                }
            ],
            "errors": [],
            "meta": { "runtime": "cli" }
        });

        let parsed = parse_usage_json(&payload.to_string(), CautService::OpenAI, 4096)
            .expect("caut.v1 usage payload should parse");
        assert_eq!(parsed.service.as_deref(), Some("openai"));
        assert_eq!(parsed.generated_at.as_deref(), Some("2026-02-27T21:00:00Z"));
        assert_eq!(parsed.accounts.len(), 1);
        let account = &parsed.accounts[0];
        assert_eq!(account.id.as_deref(), Some("acc-primary"));
        assert_eq!(account.name.as_deref(), Some("Personal"));
        assert_eq!(account.percent_remaining, Some(82.5));
        assert_eq!(account.tokens_used, Some(1750));
        assert_eq!(account.tokens_remaining, Some(8250));
        assert_eq!(account.tokens_limit, Some(10000));
        assert_eq!(account.reset_at.as_deref(), Some("2026-03-01T00:00:00Z"));
    }

    #[test]
    fn parse_refresh_supports_caut_v1_schema() {
        let payload = json!({
            "schemaVersion": "caut.v1",
            "generatedAt": "2026-02-27T21:05:00Z",
            "command": "usage",
            "data": [
                {
                    "provider": "codex",
                    "account": "acc-1",
                    "usage": { "primary": { "percentRemaining": 50.0 } }
                }
            ],
            "errors": []
        });

        let parsed = parse_refresh_json(&payload.to_string(), CautService::OpenAI, 4096)
            .expect("caut.v1 refresh payload should parse");
        assert_eq!(parsed.service.as_deref(), Some("openai"));
        assert_eq!(parsed.refreshed_at.as_deref(), Some("2026-02-27T21:05:00Z"));
        assert_eq!(parsed.accounts.len(), 1);
        assert_eq!(parsed.accounts[0].id.as_deref(), Some("acc-1"));
        assert_eq!(parsed.accounts[0].percent_remaining, Some(50.0));
    }

    #[test]
    fn parse_usage_accepts_unknown_fields() {
        let payload = json!({
            "service": "openai",
            "generated_at": "2026-01-25T00:00:00Z",
            "accounts": [
                {
                    "name": "alpha",
                    "percent_remaining": 12.5,
                    "limit_hours": 24,
                    "reset_at": "2026-01-26T00:00:00Z",
                    "tokens_used": 1234
                }
            ],
            "extra_field": "ignored"
        });

        let parsed: CautUsage = parse_json(&payload.to_string(), 200).expect("usage should parse");
        assert_eq!(parsed.service.as_deref(), Some("openai"));
        assert_eq!(parsed.accounts.len(), 1);
        assert!(parsed.extra.contains_key("extra_field"));
    }

    #[test]
    fn parse_refresh_accepts_partial_payloads() {
        let payload = json!({
            "service": "openai",
            "accounts": []
        });

        let parsed: CautRefresh =
            parse_json(&payload.to_string(), 200).expect("refresh should parse");
        assert_eq!(parsed.service.as_deref(), Some("openai"));
        assert!(parsed.accounts.is_empty());
    }

    #[test]
    fn invalid_json_returns_preview() {
        let err = parse_json::<CautUsage>("{not_json}", 20).expect_err("should error");
        match err {
            CautError::InvalidJson { preview, .. } => {
                assert!(preview.contains("not_json"));
            }
            other => panic!("Unexpected error: {other:?}"),
        }
    }

    #[test]
    fn redact_and_truncate_masks_secrets() {
        let secret = "sk-abc123456789012345678901234567890123456789012345678901";
        let text = format!("token={secret}");
        let redacted = redact_and_truncate(&text, 200);
        assert!(!redacted.contains("sk-"));
        assert!(redacted.contains("[REDACTED]"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(40))]

        #[test]
        fn redact_and_truncate_never_exceeds_limit_plus_ellipsis(
            input in "\\PC*",
            max_len in 0usize..256usize,
        ) {
            let redacted = redact_and_truncate(&input, max_len);
            prop_assert!(
                redacted.len() <= max_len + 3,
                "redacted length {} should be <= limit+ellipsis {}",
                redacted.len(),
                max_len + 3
            );
        }

        #[test]
        fn redact_and_truncate_masks_sk_prefix(
            suffix in "[A-Za-z0-9]{24,64}",
            max_len in 16usize..256usize,
        ) {
            let input = format!("prefix sk-{suffix} suffix");
            let redacted = redact_and_truncate(&input, max_len);
            prop_assert!(
                !redacted.contains("sk-"),
                "redacted output should not expose OpenAI-style key prefixes: {redacted}"
            );
        }

        #[test]
        fn parse_json_invalid_preview_respects_bound(
            input in "\\PC{1,200}",
            max_preview in 0usize..96usize,
        ) {
            prop_assume!(serde_json::from_str::<serde_json::Value>(&input).is_err());

            let err = parse_json::<CautUsage>(&input, max_preview).expect_err("invalid json expected");
            match err {
                CautError::InvalidJson { preview, .. } => {
                    prop_assert!(
                        preview.len() <= max_preview + 3,
                        "preview length {} should be <= {}",
                        preview.len(),
                        max_preview + 3
                    );
                }
                other => prop_assert!(false, "expected InvalidJson, got {:?}", other),
            }
        }
    }

    // =========================================================================
    // Fixture-based parsing tests (wa-nu4.1.5.3)
    // =========================================================================

    #[test]
    fn parse_usage_multiple_accounts_different_quotas() {
        let payload = json!({
            "service": "openai",
            "generated_at": "2026-01-28T12:00:00Z",
            "accounts": [
                {
                    "id": "acc-1",
                    "name": "Primary",
                    "percent_remaining": 85.0,
                    "tokens_used": 1500,
                    "tokens_remaining": 8500,
                    "tokens_limit": 10000,
                    "reset_at": "2026-02-01T00:00:00Z"
                },
                {
                    "id": "acc-2",
                    "name": "Backup",
                    "percent_remaining": 20.0,
                    "tokens_used": 8000,
                    "tokens_remaining": 2000,
                    "tokens_limit": 10000
                },
                {
                    "id": "acc-3",
                    "name": "Depleted",
                    "percent_remaining": 0.0,
                    "tokens_used": 10000,
                    "tokens_remaining": 0,
                    "tokens_limit": 10000
                }
            ]
        });

        let parsed: CautUsage = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.accounts.len(), 3);
        assert!((parsed.accounts[0].percent_remaining.unwrap() - 85.0).abs() < 0.001);
        assert!((parsed.accounts[1].percent_remaining.unwrap() - 20.0).abs() < 0.001);
        assert!((parsed.accounts[2].percent_remaining.unwrap()).abs() < 0.001);
    }

    #[test]
    fn parse_usage_camel_case_aliases() {
        let payload = json!({
            "service": "openai",
            "accounts": [
                {
                    "id": "acc-1",
                    "name": "CamelCase",
                    "percentRemaining": 42.5,
                    "limitHours": 24,
                    "resetAt": "2026-02-01T00:00:00Z",
                    "tokensUsed": 5750,
                    "tokensRemaining": 4250,
                    "tokensLimit": 10000
                }
            ]
        });

        let parsed: CautUsage = parse_json(&payload.to_string(), 4096).expect("should parse");
        let acct = &parsed.accounts[0];
        assert!((acct.percent_remaining.unwrap() - 42.5).abs() < 0.001);
        assert_eq!(acct.limit_hours, Some(24));
        assert_eq!(acct.reset_at.as_deref(), Some("2026-02-01T00:00:00Z"));
        assert_eq!(acct.tokens_used, Some(5750));
        assert_eq!(acct.tokens_remaining, Some(4250));
        assert_eq!(acct.tokens_limit, Some(10000));
    }

    #[test]
    fn parse_usage_missing_optional_fields() {
        // Account with only required fields — all Optional fields absent
        let payload = json!({
            "service": "openai",
            "accounts": [
                {}
            ]
        });

        let parsed: CautUsage = parse_json(&payload.to_string(), 4096).expect("should parse");
        let acct = &parsed.accounts[0];
        assert!(acct.id.is_none());
        assert!(acct.name.is_none());
        assert!(acct.percent_remaining.is_none());
        assert!(acct.limit_hours.is_none());
        assert!(acct.reset_at.is_none());
        assert!(acct.tokens_used.is_none());
        assert!(acct.tokens_remaining.is_none());
        assert!(acct.tokens_limit.is_none());
    }

    #[test]
    fn parse_usage_null_fields() {
        let payload = json!({
            "service": "openai",
            "accounts": [
                {
                    "id": null,
                    "name": null,
                    "percent_remaining": null,
                    "tokens_used": null,
                    "tokens_remaining": null,
                    "tokens_limit": null,
                    "reset_at": null
                }
            ]
        });

        let parsed: CautUsage = parse_json(&payload.to_string(), 4096).expect("should parse");
        let acct = &parsed.accounts[0];
        assert!(acct.id.is_none());
        assert!(acct.name.is_none());
        assert!(acct.percent_remaining.is_none());
        assert!(acct.tokens_used.is_none());
    }

    #[test]
    fn parse_usage_empty_accounts_array() {
        let payload = json!({
            "service": "openai",
            "generated_at": "2026-01-28T12:00:00Z",
            "accounts": []
        });

        let parsed: CautUsage = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert!(parsed.accounts.is_empty());
        assert_eq!(parsed.service.as_deref(), Some("openai"));
    }

    #[test]
    fn parse_usage_extra_account_fields_captured() {
        let payload = json!({
            "service": "openai",
            "accounts": [
                {
                    "id": "acc-1",
                    "name": "Test",
                    "percent_remaining": 50.0,
                    "custom_field": "hello",
                    "nested_data": { "deep": true }
                }
            ]
        });

        let parsed: CautUsage = parse_json(&payload.to_string(), 4096).expect("should parse");
        let acct = &parsed.accounts[0];
        assert!(acct.extra.contains_key("custom_field"));
        assert!(acct.extra.contains_key("nested_data"));
    }

    #[test]
    fn parse_refresh_with_multiple_accounts() {
        let payload = json!({
            "service": "openai",
            "refreshed_at": "2026-01-28T12:00:00Z",
            "accounts": [
                { "id": "acc-1", "name": "Alpha", "percent_remaining": 90.0 },
                { "id": "acc-2", "name": "Beta", "percent_remaining": 10.0 }
            ]
        });

        let parsed: CautRefresh = parse_json(&payload.to_string(), 4096).expect("should parse");
        assert_eq!(parsed.accounts.len(), 2);
        assert_eq!(parsed.refreshed_at.as_deref(), Some("2026-01-28T12:00:00Z"));
    }

    #[test]
    fn parse_minimal_valid_json_object() {
        // Bare minimum: empty object with defaults
        let parsed: CautUsage = parse_json("{}", 4096).expect("should parse");
        assert!(parsed.service.is_none());
        assert!(parsed.accounts.is_empty());
    }

    #[test]
    fn parse_error_preview_truncated_at_limit() {
        let long_input = "x".repeat(500);
        let err = parse_json::<CautUsage>(&long_input, 50).expect_err("should error");
        match err {
            CautError::InvalidJson { preview, .. } => {
                // Preview should be truncated + "..."
                assert!(preview.len() <= 54); // 50 chars + "..."
                assert!(preview.ends_with("..."));
            }
            other => panic!("Expected InvalidJson, got: {other:?}"),
        }
    }

    #[test]
    fn parse_deterministic_across_calls() {
        let payload = json!({
            "service": "openai",
            "accounts": [
                { "id": "a", "percent_remaining": 50.0, "tokens_used": 100 },
                { "id": "b", "percent_remaining": 30.0, "tokens_used": 200 }
            ]
        });
        let input = payload.to_string();

        let p1: CautUsage = parse_json(&input, 4096).expect("first parse");
        let p2: CautUsage = parse_json(&input, 4096).expect("second parse");

        assert_eq!(p1.accounts.len(), p2.accounts.len());
        for (a, b) in p1.accounts.iter().zip(p2.accounts.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.percent_remaining, b.percent_remaining);
            assert_eq!(a.tokens_used, b.tokens_used);
        }
    }

    #[test]
    fn caut_service_display() {
        assert_eq!(CautService::OpenAI.as_str(), "openai");
        assert_eq!(CautService::Anthropic.as_str(), "anthropic");
        assert_eq!(CautService::Google.as_str(), "google");
        assert_eq!(format!("{}", CautService::OpenAI), "openai");
        assert_eq!(format!("{}", CautService::Anthropic), "anthropic");
        assert_eq!(format!("{}", CautService::Google), "google");
    }

    #[test]
    fn caut_service_from_provider_bridge() {
        assert_eq!(
            CautService::from_provider(&AgentProvider::Codex),
            Some(CautService::OpenAI)
        );
        assert_eq!(
            CautService::from_provider(&AgentProvider::Claude),
            Some(CautService::Anthropic)
        );
        assert_eq!(
            CautService::from_provider(&AgentProvider::Gemini),
            Some(CautService::Google)
        );
        assert_eq!(
            CautService::from_provider(&AgentProvider::Unknown("openai".to_string())),
            Some(CautService::OpenAI)
        );
        assert_eq!(
            CautService::from_provider(&AgentProvider::Unknown("chat-gpt".to_string())),
            Some(CautService::OpenAI)
        );
        assert_eq!(
            CautService::from_provider(&AgentProvider::Unknown("anthropic".to_string())),
            Some(CautService::Anthropic)
        );
        assert_eq!(
            CautService::from_provider(&AgentProvider::Unknown("claude-code".to_string())),
            Some(CautService::Anthropic)
        );
        assert_eq!(
            CautService::from_provider(&AgentProvider::Unknown("google".to_string())),
            Some(CautService::Google)
        );
        assert_eq!(
            CautService::from_provider(&AgentProvider::Unknown("gemini-cli".to_string())),
            Some(CautService::Google)
        );
        assert_eq!(
            CautService::from_provider(&AgentProvider::Unknown("copilot".to_string())),
            None
        );
    }

    #[test]
    fn caut_service_from_cli_input_aliases() {
        assert_eq!(
            CautService::from_cli_input("openai"),
            Some(CautService::OpenAI)
        );
        assert_eq!(
            CautService::from_cli_input("codex"),
            Some(CautService::OpenAI)
        );
        assert_eq!(
            CautService::from_cli_input("chat-gpt"),
            Some(CautService::OpenAI)
        );
        assert_eq!(
            CautService::from_cli_input("anthropic"),
            Some(CautService::Anthropic)
        );
        assert_eq!(
            CautService::from_cli_input("claude"),
            Some(CautService::Anthropic)
        );
        assert_eq!(
            CautService::from_cli_input("google"),
            Some(CautService::Google)
        );
        assert_eq!(
            CautService::from_cli_input("gemini"),
            Some(CautService::Google)
        );
        assert_eq!(CautService::from_cli_input("unknown"), None);
    }

    #[test]
    fn caut_service_provider_hint_bridge() {
        assert_eq!(CautService::OpenAI.provider_hint(), AgentProvider::Codex);
        assert_eq!(
            CautService::Anthropic.provider_hint(),
            AgentProvider::Claude
        );
        assert_eq!(CautService::Google.provider_hint(), AgentProvider::Gemini);
    }

    #[test]
    fn caut_service_provider_args_and_supported_inputs() {
        assert_eq!(CautService::OpenAI.provider_arg(), "codex");
        assert_eq!(CautService::Anthropic.provider_arg(), "claude");
        assert_eq!(CautService::Google.provider_arg(), "gemini");
        let supported = CautService::supported_cli_inputs();
        assert!(supported.contains(&"openai"));
        assert!(supported.contains(&"anthropic"));
        assert!(supported.contains(&"google"));
    }

    #[test]
    fn caut_error_remediation_not_installed() {
        let err = CautError::NotInstalled;
        let rem = err.remediation();
        assert!(!rem.summary.is_empty());
        assert!(!rem.commands.is_empty());
    }

    #[test]
    fn caut_error_remediation_timeout() {
        let err = CautError::Timeout { timeout_secs: 10 };
        let rem = err.remediation();
        assert!(rem.summary.contains("10s"));
    }

    #[test]
    fn caut_error_remediation_non_zero_exit() {
        let err = CautError::NonZeroExit {
            status: 1,
            stderr: "auth failed".to_string(),
        };
        let rem = err.remediation();
        assert!(!rem.summary.is_empty());
    }

    #[test]
    fn caut_error_remediation_output_too_large() {
        let err = CautError::OutputTooLarge {
            bytes: 500_000,
            max_bytes: 256_000,
        };
        let rem = err.remediation();
        assert!(!rem.summary.is_empty());
    }

    #[test]
    fn caut_error_remediation_invalid_json() {
        let err = CautError::InvalidJson {
            message: "expected value".to_string(),
            preview: "{bad".to_string(),
        };
        let rem = err.remediation();
        assert!(!rem.summary.is_empty());
    }

    #[test]
    fn caut_error_remediation_io() {
        let err = CautError::Io {
            message: "permission denied".to_string(),
        };
        let rem = err.remediation();
        assert!(!rem.summary.is_empty());
    }

    #[test]
    fn categorize_not_found_as_not_installed() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let caut_err = categorize_io_error(&io_err);
        assert!(matches!(caut_err, CautError::NotInstalled));
    }

    #[test]
    fn categorize_other_io_as_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let caut_err = categorize_io_error(&io_err);
        match caut_err {
            CautError::Io { message } => assert!(message.contains("denied")),
            other => panic!("Expected Io, got: {other:?}"),
        }
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ───────────────────────────────────

    // ── CautError Display ──

    #[test]
    fn caut_error_display_not_installed() {
        let err = CautError::NotInstalled;
        assert_eq!(
            err.to_string(),
            "caut is not installed or not found on PATH"
        );
    }

    #[test]
    fn caut_error_display_timeout() {
        let err = CautError::Timeout { timeout_secs: 30 };
        assert_eq!(err.to_string(), "caut timed out after 30s");
    }

    #[test]
    fn caut_error_display_non_zero_exit() {
        let err = CautError::NonZeroExit {
            status: 127,
            stderr: "command not found".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("127"));
        assert!(msg.contains("command not found"));
    }

    #[test]
    fn caut_error_display_output_too_large() {
        let err = CautError::OutputTooLarge {
            bytes: 500_000,
            max_bytes: 262_144,
        };
        let msg = err.to_string();
        // Display only prints max_bytes, not bytes
        assert!(msg.contains("262144"));
    }

    #[test]
    fn caut_error_display_invalid_json() {
        let err = CautError::InvalidJson {
            message: "expected value at line 1".to_string(),
            preview: "{bad".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("invalid JSON"));
        assert!(msg.contains("expected value at line 1"));
    }

    #[test]
    fn caut_error_display_io() {
        let err = CautError::Io {
            message: "broken pipe".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("broken pipe"));
    }

    // ── CautError serde roundtrip ──

    #[test]
    fn caut_error_serde_roundtrip() {
        let errors = vec![
            CautError::NotInstalled,
            CautError::Timeout { timeout_secs: 5 },
            CautError::NonZeroExit {
                status: 1,
                stderr: "fail".to_string(),
            },
            CautError::OutputTooLarge {
                bytes: 100,
                max_bytes: 50,
            },
            CautError::InvalidJson {
                message: "bad".to_string(),
                preview: "...".to_string(),
            },
            CautError::Io {
                message: "oops".to_string(),
            },
        ];
        for err in &errors {
            let json = serde_json::to_string(err).unwrap();
            let decoded: CautError = serde_json::from_str(&json).unwrap();
            assert_eq!(err, &decoded);
        }
    }

    // ── CautError Clone/PartialEq/Eq ──

    #[test]
    fn caut_error_clone_and_eq() {
        let err1 = CautError::NotInstalled;
        let err2 = err1.clone();
        assert_eq!(err1, err2);

        let err3 = CautError::Timeout { timeout_secs: 10 };
        let err4 = CautError::Timeout { timeout_secs: 10 };
        assert_eq!(err3, err4);

        assert_ne!(err1, err3);
    }

    #[test]
    fn caut_error_debug() {
        let err = CautError::NotInstalled;
        let debug = format!("{:?}", err);
        assert!(debug.contains("NotInstalled"));
    }

    // ── CautService traits ──

    #[test]
    fn caut_service_debug() {
        let debug = format!("{:?}", CautService::OpenAI);
        assert_eq!(debug, "OpenAI");
    }

    #[test]
    fn caut_service_clone_copy_eq() {
        let s1 = CautService::OpenAI;
        let s2 = s1; // Copy
        let s3 = s1;
        assert_eq!(s1, s2);
        assert_eq!(s1, s3);
    }

    // ── CautClient defaults and builders ──

    #[test]
    fn caut_client_default_values() {
        let client = CautClient::default();
        assert_eq!(client.binary, "caut");
        assert_eq!(client.timeout, Duration::from_secs(10));
        assert_eq!(client.max_output_bytes, 256 * 1024);
        assert_eq!(client.max_error_bytes, 8 * 1024);
    }

    #[test]
    fn caut_client_new_equals_default() {
        let c1 = CautClient::new();
        let c2 = CautClient::default();
        assert_eq!(c1.binary, c2.binary);
        assert_eq!(c1.timeout, c2.timeout);
        assert_eq!(c1.max_output_bytes, c2.max_output_bytes);
        assert_eq!(c1.max_error_bytes, c2.max_error_bytes);
    }

    #[test]
    fn caut_client_with_binary() {
        let client = CautClient::new().with_binary("/usr/local/bin/caut");
        assert_eq!(client.binary, "/usr/local/bin/caut");
    }

    #[test]
    fn caut_client_with_timeout_secs() {
        let client = CautClient::new().with_timeout_secs(30);
        assert_eq!(client.timeout, Duration::from_secs(30));
    }

    #[test]
    fn caut_client_with_max_output_bytes() {
        let client = CautClient::new().with_max_output_bytes(1_000_000);
        assert_eq!(client.max_output_bytes, 1_000_000);
    }

    #[test]
    fn caut_client_with_max_error_bytes() {
        let client = CautClient::new().with_max_error_bytes(16_384);
        assert_eq!(client.max_error_bytes, 16_384);
    }

    #[test]
    fn caut_client_builder_chaining() {
        let client = CautClient::new()
            .with_binary("my-caut")
            .with_timeout_secs(60)
            .with_max_output_bytes(512_000)
            .with_max_error_bytes(4_096);
        assert_eq!(client.binary, "my-caut");
        assert_eq!(client.timeout, Duration::from_secs(60));
        assert_eq!(client.max_output_bytes, 512_000);
        assert_eq!(client.max_error_bytes, 4_096);
    }

    #[test]
    fn caut_client_debug() {
        let client = CautClient::new();
        let debug = format!("{:?}", client);
        assert!(debug.contains("CautClient"));
        assert!(debug.contains("caut"));
    }

    #[test]
    fn caut_client_clone() {
        let client = CautClient::new()
            .with_binary("custom")
            .with_timeout_secs(20);
        let cloned = client.clone();
        assert_eq!(cloned.binary, "custom");
        assert_eq!(cloned.timeout, Duration::from_secs(20));
    }

    // ── CautUsage/CautRefresh/CautAccountUsage defaults ──

    #[test]
    fn caut_usage_default() {
        let usage = CautUsage::default();
        assert!(usage.service.is_none());
        assert!(usage.generated_at.is_none());
        assert!(usage.accounts.is_empty());
        assert!(usage.extra.is_empty());
    }

    #[test]
    fn caut_refresh_default() {
        let refresh = CautRefresh::default();
        assert!(refresh.service.is_none());
        assert!(refresh.refreshed_at.is_none());
        assert!(refresh.accounts.is_empty());
        assert!(refresh.extra.is_empty());
    }

    #[test]
    fn caut_account_usage_default() {
        let acct = CautAccountUsage::default();
        assert!(acct.id.is_none());
        assert!(acct.name.is_none());
        assert!(acct.percent_remaining.is_none());
        assert!(acct.limit_hours.is_none());
        assert!(acct.reset_at.is_none());
        assert!(acct.tokens_used.is_none());
        assert!(acct.tokens_remaining.is_none());
        assert!(acct.tokens_limit.is_none());
        assert!(acct.extra.is_empty());
    }

    #[test]
    fn caut_usage_debug_and_clone() {
        let usage = CautUsage {
            service: Some("openai".to_string()),
            generated_at: Some("now".to_string()),
            accounts: vec![],
            extra: HashMap::new(),
        };
        let debug = format!("{:?}", usage);
        assert!(debug.contains("CautUsage"));
        assert!(debug.contains("openai"));

        let cloned = usage.clone();
        assert_eq!(cloned.service, Some("openai".to_string()));
    }

    #[test]
    fn caut_refresh_debug_and_clone() {
        let refresh = CautRefresh {
            service: Some("openai".to_string()),
            refreshed_at: Some("2026-01-01".to_string()),
            accounts: vec![],
            extra: HashMap::new(),
        };
        let debug = format!("{:?}", refresh);
        assert!(debug.contains("CautRefresh"));

        let cloned = refresh.clone();
        assert_eq!(cloned.refreshed_at, Some("2026-01-01".to_string()));
    }

    #[test]
    fn caut_account_usage_debug_and_clone() {
        let acct = CautAccountUsage {
            id: Some("acc-1".to_string()),
            name: Some("Test".to_string()),
            percent_remaining: Some(42.5),
            ..Default::default()
        };
        let debug = format!("{:?}", acct);
        assert!(debug.contains("CautAccountUsage"));
        assert!(debug.contains("acc-1"));

        let cloned = acct.clone();
        assert_eq!(cloned.id, Some("acc-1".to_string()));
        assert_eq!(cloned.percent_remaining, Some(42.5));
    }

    // ── redact_and_truncate edge cases ──

    #[test]
    fn redact_and_truncate_short_input_unchanged() {
        let result = redact_and_truncate("hello world", 100);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn redact_and_truncate_zero_max_len() {
        let result = redact_and_truncate("some text", 0);
        assert_eq!(result, "...");
    }

    #[test]
    fn redact_and_truncate_empty_input() {
        let result = redact_and_truncate("", 100);
        assert_eq!(result, "");
    }

    #[test]
    fn redact_and_truncate_exact_limit() {
        let input = "abcde"; // 5 bytes
        let result = redact_and_truncate(input, 5);
        assert_eq!(result, "abcde"); // no truncation needed
    }

    #[test]
    fn redact_and_truncate_one_over_limit() {
        let input = "abcdef"; // 6 bytes
        let result = redact_and_truncate(input, 5);
        assert_eq!(result, "abcde...");
    }
}
