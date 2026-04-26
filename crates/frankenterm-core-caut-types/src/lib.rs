//! `frankenterm-core-caut-types` — caut wrapper type definitions leaf crate
//! (ft-2z15d / ft-y0loj.5.D.2).
//!
//! Tier-1 leaf crate carved out from `frankenterm-core::caut`. Holds the
//! plain-data types that the caut wrapper exchanges with callers:
//!
//!   - [`CautService`] enum + slug parsing helpers + `Display` impl
//!   - [`CautUsage`] / [`CautRefresh`] / [`CautAccountUsage`] structs
//!   - [`CautError`] enum (without the in-core `remediation()` impl,
//!     which stays next to `Remediation` in `frankenterm-core`)
//!
//! `CautClient` and the JSON parsing / `CautV1` envelope conversion
//! pipeline stay in `frankenterm-core::caut` because they pull on
//! `Command`, `Redactor`, and the in-core IO surface.
//!
//! Re-exported from `frankenterm-core::caut::*` so existing
//! `crate::caut::CautError`, `crate::caut::CautService`, etc. paths
//! resolve unchanged. The new crate is a one-way path-dep of
//! `frankenterm-core` — no back-edge.
//!
//! Companion of `frankenterm-core-cass-types` (ft-8cg6y); together they
//! unblock the mcp/connector tier-2 extraction (ft-y0loj.5).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// =============================================================================
// CautService
// =============================================================================

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
}

#[must_use]
pub fn is_openai_slug(slug: &str) -> bool {
    matches!(
        slug.trim().to_ascii_lowercase().as_str(),
        "openai" | "codex" | "chatgpt" | "chat-gpt" | "chat_gpt" | "gpt" | "gpt4" | "gpt-4"
    )
}

#[must_use]
pub fn is_anthropic_slug(slug: &str) -> bool {
    matches!(
        slug.trim().to_ascii_lowercase().as_str(),
        "anthropic" | "claude" | "claude-code" | "claude_code"
    )
}

#[must_use]
pub fn is_google_slug(slug: &str) -> bool {
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

// =============================================================================
// Usage / refresh payloads
// =============================================================================

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

// =============================================================================
// CautError
// =============================================================================

/// Errors produced by the caut wrapper.
///
/// The in-core `frankenterm-core::caut` module supplies an `impl CautError`
/// extension that returns a `Remediation` for each variant; that lives in
/// core because `Remediation` itself depends on the in-core suggestions /
/// platform layer.
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
