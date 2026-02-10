//! Wrapper for the `caut` CLI (usage/refresh JSON parsing with safety).
//!
//! This module treats caut as the source of truth for account usage data.
//! It provides a small, typed API with:
//! - hard timeouts
//! - output size limits
//! - JSON parsing with redacted error previews

use crate::error::Remediation;
use crate::policy::Redactor;
use crate::suggestions::Platform;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Supported caut services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CautService {
    OpenAI,
}

impl CautService {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
        }
    }
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
#[derive(thiserror::Error, Debug)]
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
            .command("Retry usage", "caut usage --service openai --format json")
            .alternative("Increase the timeout for caut commands."),
            Self::NonZeroExit { .. } => Remediation::new(
                "caut exited with an error. Check caut logs or rerun with verbose output.",
            )
            .command("Retry usage", "caut usage --service openai --format json")
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
        self.run_and_parse("usage", service).await
    }

    /// Refresh usage data via `caut refresh`.
    pub async fn refresh(&self, service: CautService) -> Result<CautRefresh, CautError> {
        self.run_and_parse("refresh", service).await
    }

    fn build_args(subcommand: &str, service: CautService) -> Vec<String> {
        vec![
            subcommand.to_string(),
            "--service".to_string(),
            service.as_str().to_string(),
            "--format".to_string(),
            "json".to_string(),
        ]
    }

    async fn run_and_parse<T: DeserializeOwned>(
        &self,
        subcommand: &str,
        service: CautService,
    ) -> Result<T, CautError> {
        let args = Self::build_args(subcommand, service);
        let output = self.run(&args).await?;
        parse_json(&output, self.max_error_bytes)
    }

    async fn run(&self, args: &[String]) -> Result<String, CautError> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(args);

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
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let stderr_preview = redact_and_truncate(&stderr, self.max_error_bytes);
            return Err(CautError::NonZeroExit {
                status,
                stderr: stderr_preview,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let bytes = stdout.len();
        if bytes > self.max_output_bytes {
            return Err(CautError::OutputTooLarge {
                bytes,
                max_bytes: self.max_output_bytes,
            });
        }

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

fn redact_and_truncate(input: &str, max_len: usize) -> String {
    let redactor = Redactor::new();
    let redacted = redactor.redact(input);
    if redacted.len() <= max_len {
        return redacted;
    }
    let mut truncated = redacted.chars().take(max_len).collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_args_includes_service_and_format() {
        let args = CautClient::build_args("usage", CautService::OpenAI);
        assert_eq!(
            args,
            ["usage", "--service", "openai", "--format", "json"]
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
        );
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
        assert_eq!(format!("{}", CautService::OpenAI), "openai");
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
}
