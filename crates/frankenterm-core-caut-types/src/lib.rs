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

#[cfg(test)]
mod tests {
    //! ft-cui2w: pin the leaf-crate boundary post ft-2z15d extraction.
    //! These tests live in the leaf so a future change to `as_str` /
    //! slug parsing / serde aliases / error messages trips a failure
    //! at the canonical home for the types, independent of how
    //! `frankenterm-core::caut` re-exports them.

    use super::*;

    // ─── CautService ──────────────────────────────────────────────────────

    #[test]
    fn service_as_str_round_trip_for_every_variant() {
        for (svc, expected) in [
            (CautService::OpenAI, "openai"),
            (CautService::Anthropic, "anthropic"),
            (CautService::Google, "google"),
        ] {
            assert_eq!(svc.as_str(), expected);
            assert_eq!(svc.to_string(), expected);
        }
    }

    #[test]
    fn service_provider_arg_matches_caut_cli_provider_names() {
        // These strings are passed straight to the caut binary as
        // `--provider <arg>`; flipping any of them would silently
        // re-route every account selection to the wrong service.
        assert_eq!(CautService::OpenAI.provider_arg(), "codex");
        assert_eq!(CautService::Anthropic.provider_arg(), "claude");
        assert_eq!(CautService::Google.provider_arg(), "gemini");
    }

    #[test]
    fn service_from_cli_input_recognizes_every_documented_slug() {
        // supported_cli_inputs() advertises the canonical surface; every
        // entry must round-trip through from_cli_input.
        for slug in CautService::supported_cli_inputs() {
            assert!(
                CautService::from_cli_input(slug).is_some(),
                "documented slug {slug:?} did not parse"
            );
        }
        assert_eq!(
            CautService::from_cli_input("openai"),
            Some(CautService::OpenAI)
        );
        assert_eq!(
            CautService::from_cli_input("codex"),
            Some(CautService::OpenAI)
        );
        assert_eq!(
            CautService::from_cli_input("claude"),
            Some(CautService::Anthropic)
        );
        assert_eq!(
            CautService::from_cli_input("anthropic"),
            Some(CautService::Anthropic)
        );
        assert_eq!(
            CautService::from_cli_input("gemini"),
            Some(CautService::Google)
        );
        assert_eq!(
            CautService::from_cli_input("google"),
            Some(CautService::Google)
        );
    }

    #[test]
    fn service_from_cli_input_rejects_unknown_slugs() {
        for bogus in ["", "perplexity", "mistral", "gpt-100", "  "] {
            assert_eq!(CautService::from_cli_input(bogus), None, "{bogus:?}");
        }
    }

    #[test]
    fn slug_helpers_are_case_insensitive_and_trim_whitespace() {
        // Pin both the tolerance (whitespace + case) and the boundary
        // (mixed slugs and the empty-string case must NOT match).
        assert!(is_openai_slug("  GPT-4 "));
        assert!(is_openai_slug("ChatGPT"));
        assert!(is_anthropic_slug("CLAUDE"));
        assert!(is_anthropic_slug(" claude_code "));
        assert!(is_google_slug("Gemini-CLI"));
        assert!(!is_openai_slug("claude"));
        assert!(!is_anthropic_slug("openai"));
        assert!(!is_google_slug("openai"));
        assert!(!is_openai_slug(""));
    }

    // ─── CautError ────────────────────────────────────────────────────────

    #[test]
    fn caut_error_display_matches_thiserror_format_strings() {
        // The Display strings are external surface — they show up in
        // recorder logs, MCP responses, and dashboards. Pin every variant
        // including the structured-field ones so a #[error("...")] edit
        // can't silently rotate operator-facing text.
        assert_eq!(
            CautError::NotInstalled.to_string(),
            "caut is not installed or not found on PATH"
        );
        assert_eq!(
            CautError::Timeout { timeout_secs: 12 }.to_string(),
            "caut timed out after 12s"
        );
        assert_eq!(
            CautError::NonZeroExit {
                status: 2,
                stderr: "boom".to_string()
            }
            .to_string(),
            "caut failed with exit code 2: boom"
        );
        assert_eq!(
            CautError::OutputTooLarge {
                bytes: 0,
                max_bytes: 1024
            }
            .to_string(),
            "caut output exceeded 1024 bytes"
        );
        assert_eq!(
            CautError::InvalidJson {
                message: "expected `{`".to_string(),
                preview: "??".to_string()
            }
            .to_string(),
            "caut returned invalid JSON: expected `{`"
        );
        assert_eq!(
            CautError::Io {
                message: "broken pipe".to_string()
            }
            .to_string(),
            "caut I/O error: broken pipe"
        );
    }

    #[test]
    fn caut_error_serde_round_trip_preserves_payload() {
        let cases = vec![
            CautError::NotInstalled,
            CautError::Timeout { timeout_secs: 30 },
            CautError::NonZeroExit {
                status: 7,
                stderr: "auth".to_string(),
            },
            CautError::OutputTooLarge {
                bytes: 9999,
                max_bytes: 4096,
            },
            CautError::InvalidJson {
                message: "trailing comma".to_string(),
                preview: "{ \"a\": 1, }".to_string(),
            },
            CautError::Io {
                message: "EAGAIN".to_string(),
            },
        ];
        for err in cases {
            let json = serde_json::to_string(&err).unwrap();
            let rt: CautError = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, err, "roundtrip drift for {err:?}");
        }
    }

    // ─── CautUsage / CautRefresh / CautAccountUsage ───────────────────────

    #[test]
    fn account_usage_deserializes_caut_camel_case_aliases() {
        // The caut binary emits camelCase JSON; the snake_case Rust field
        // names exist for our internal use, and serde aliases bridge the
        // two. Drop one alias and account selection silently regresses to
        // 0% remaining for every account.
        let json = r#"{
            "id": "acct_42",
            "name": "primary",
            "percentRemaining": 73.5,
            "limitHours": 24,
            "resetAt": "2026-04-26T20:00:00Z",
            "tokensUsed": 1000,
            "tokensRemaining": 9000,
            "tokensLimit": 10000
        }"#;
        let acct: CautAccountUsage = serde_json::from_str(json).unwrap();
        assert_eq!(acct.id.as_deref(), Some("acct_42"));
        assert_eq!(acct.name.as_deref(), Some("primary"));
        assert_eq!(acct.percent_remaining, Some(73.5));
        assert_eq!(acct.limit_hours, Some(24));
        assert_eq!(acct.reset_at.as_deref(), Some("2026-04-26T20:00:00Z"));
        assert_eq!(acct.tokens_used, Some(1000));
        assert_eq!(acct.tokens_remaining, Some(9000));
        assert_eq!(acct.tokens_limit, Some(10000));
        assert!(acct.extra.is_empty());
    }

    #[test]
    fn account_usage_unknown_fields_land_in_extra() {
        let json = r#"{
            "id": "acct_x",
            "vendorSpecific": {"region": "us-east-1"},
            "experimental_flag": true
        }"#;
        let acct: CautAccountUsage = serde_json::from_str(json).unwrap();
        assert_eq!(acct.id.as_deref(), Some("acct_x"));
        assert!(acct.extra.contains_key("vendorSpecific"));
        assert_eq!(
            acct.extra.get("experimental_flag"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn caut_usage_round_trip_preserves_accounts_and_extra() {
        let usage = CautUsage {
            service: Some("openai".to_string()),
            generated_at: Some("2026-04-26T20:00:00Z".to_string()),
            accounts: vec![CautAccountUsage {
                id: Some("a".to_string()),
                percent_remaining: Some(12.0),
                ..Default::default()
            }],
            extra: {
                let mut m = HashMap::new();
                m.insert("vendor".to_string(), serde_json::json!("openai-internal"));
                m
            },
        };
        let json = serde_json::to_string(&usage).unwrap();
        let rt: CautUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.service, usage.service);
        assert_eq!(rt.generated_at, usage.generated_at);
        assert_eq!(rt.accounts.len(), 1);
        assert_eq!(rt.accounts[0].id, Some("a".to_string()));
        assert_eq!(rt.accounts[0].percent_remaining, Some(12.0));
        assert_eq!(
            rt.extra.get("vendor"),
            Some(&serde_json::json!("openai-internal"))
        );
    }

    #[test]
    fn caut_refresh_round_trip_preserves_accounts_and_extra() {
        let refresh = CautRefresh {
            service: Some("anthropic".to_string()),
            refreshed_at: Some("2026-04-26T20:01:00Z".to_string()),
            accounts: vec![CautAccountUsage::default()],
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&refresh).unwrap();
        let rt: CautRefresh = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.service, refresh.service);
        assert_eq!(rt.refreshed_at, refresh.refreshed_at);
        assert_eq!(rt.accounts.len(), 1);
    }

    #[test]
    fn caut_usage_default_is_empty() {
        let u = CautUsage::default();
        assert!(u.service.is_none());
        assert!(u.generated_at.is_none());
        assert!(u.accounts.is_empty());
        assert!(u.extra.is_empty());
    }
}
