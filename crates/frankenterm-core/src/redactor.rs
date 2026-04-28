//! Secret redaction for read, export, and audit surfaces.

use regex::Regex;
use std::sync::LazyLock;

/// Redaction marker used in place of detected secrets.
pub const REDACTED_MARKER: &str = "[REDACTED]";

/// Pattern definition for secret detection.
struct SecretPattern {
    /// Human-readable name for the pattern.
    name: &'static str,
    /// Compiled regex pattern.
    regex: &'static LazyLock<Regex>,
}

/// OpenAI API keys: sk-... (48+ chars) or sk-proj-...
static OPENAI_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sk-(?:proj-)?[a-zA-Z0-9_-]{20,}").expect("OpenAI key regex"));

/// Anthropic API keys: sk-ant-...
static ANTHROPIC_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sk-ant-[a-zA-Z0-9_-]{20,}").expect("Anthropic key regex"));

/// GitHub tokens: ghp_, gho_, ghu_, ghs_, ghr_.
static GITHUB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"gh[pousr]_[a-zA-Z0-9]{36,}").expect("GitHub token regex"));

/// AWS Access Key IDs: AKIA...
static AWS_ACCESS_KEY_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"AKIA[0-9A-Z]{16}").expect("AWS access key regex"));

/// AWS Secret Access Keys (typically 40 chars base64-like, often after aws_secret_access_key=).
static AWS_SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)aws_secret_access_key\\s*[=:]\\s*['\"]?([a-zA-Z0-9/+=]{40})['\"]?")
        .expect("AWS secret key regex")
});

/// Generic Bearer tokens in Authorization headers.
static BEARER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:authorization["']?\s*[:=]\s*["']?bearer\s+|bearer\s+)[a-zA-Z0-9._-]{20,}"#)
        .expect("Bearer token regex")
});

// ft-5o6u5: generic key/token/secret value charsets must accept base64
// padding/alphabet (`/`, `+`, `=`) in addition to alnum/underscore/dash.
// Many OAuth client_secret and base64-encoded values contain those bytes;
// without them in the charset the regex stops at the first `/` or `+` and
// the trailing secret bytes leak unredacted through robot/MCP/audit
// surfaces. The charset still excludes whitespace and quote characters so
// the match terminates at the value boundary.

/// Generic API keys with common prefixes.
static GENERIC_API_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:api[_-]?key|apikey)\s*[=:]\s*['"]?([a-zA-Z0-9_/+=-]{16,})['"]?"#)
        .expect("Generic API key regex")
});

/// Generic token assignments.
static GENERIC_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|[^a-z])token\s*[=:]\s*['"]?([a-zA-Z0-9._/+=-]{16,})['"]?"#)
        .expect("Generic token regex")
});

/// Generic password assignments (password=..., password: ...).
static GENERIC_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)password\s*[=:]\s*(?:'[^']{4,}'|"[^"]{4,}"|[^\s'"]{4,})"#)
        .expect("Generic password regex")
});

/// Generic secret assignments.
static GENERIC_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|[^a-z])secret\s*[=:]\s*['"]?([a-zA-Z0-9_/+=-]{8,})['"]?"#)
        .expect("Generic secret regex")
});

/// Device codes (OAuth device flow) - typically 8+ alphanumeric chars displayed to user.
static DEVICE_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:device[_-]?code|user[_-]?code)\s*[=:]\s*['"]?([A-Za-z0-9-]{6,})['"]?"#)
        .expect("Device code regex")
});

/// OAuth URLs with tokens/codes in query params.
static OAUTH_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s]*[?&](?:access_token|code|token)=[^\s&;'""]+"#)
        .expect("OAuth URL regex")
});

/// Slack tokens: xoxb-, xoxp-, xoxa-, xoxr-.
static SLACK_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"xox[bpar]-[a-zA-Z0-9-]{10,}").expect("Slack token regex"));

/// Stripe API keys: sk_live_, sk_test_, pk_live_, pk_test_.
static STRIPE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[ps]k_(?:live|test)_[a-zA-Z0-9]{20,}").expect("Stripe key regex")
});

/// Database connection strings with passwords.
static DATABASE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:postgres|mysql|mongodb|redis)(?:ql)?://[^:]+:([^@\s]+)@")
        .expect("Database URL regex")
});

/// All secret patterns in priority order.
static SECRET_PATTERNS: &[SecretPattern] = &[
    SecretPattern {
        name: "openai_key",
        regex: &OPENAI_KEY,
    },
    SecretPattern {
        name: "anthropic_key",
        regex: &ANTHROPIC_KEY,
    },
    SecretPattern {
        name: "github_token",
        regex: &GITHUB_TOKEN,
    },
    SecretPattern {
        name: "aws_access_key_id",
        regex: &AWS_ACCESS_KEY_ID,
    },
    SecretPattern {
        name: "aws_secret_key",
        regex: &AWS_SECRET_KEY,
    },
    SecretPattern {
        name: "bearer_token",
        regex: &BEARER_TOKEN,
    },
    SecretPattern {
        name: "slack_token",
        regex: &SLACK_TOKEN,
    },
    SecretPattern {
        name: "stripe_key",
        regex: &STRIPE_KEY,
    },
    SecretPattern {
        name: "database_url",
        regex: &DATABASE_URL,
    },
    SecretPattern {
        name: "device_code",
        regex: &DEVICE_CODE,
    },
    SecretPattern {
        name: "oauth_url",
        regex: &OAUTH_URL,
    },
    SecretPattern {
        name: "generic_api_key",
        regex: &GENERIC_API_KEY,
    },
    SecretPattern {
        name: "generic_token",
        regex: &GENERIC_TOKEN,
    },
    SecretPattern {
        name: "generic_password",
        regex: &GENERIC_PASSWORD,
    },
    SecretPattern {
        name: "generic_secret",
        regex: &GENERIC_SECRET,
    },
];

/// Secret redactor for removing sensitive information from text.
///
/// This redactor uses a conservative set of regex patterns to identify and
/// replace secrets with `[REDACTED]` markers. It is designed to err on the
/// side of caution: it is better to redact something that is not a secret than
/// to leak an actual secret.
///
/// # Logging Conventions
///
/// When using the redactor, follow these conventions:
/// - **Never log raw device codes** - Always redact before logging
/// - **Never log OAuth URLs with embedded params** - Tokens in query strings
/// - **Always redact before audit/export** - Use `Redactor::redact()` on all output
///
/// # Example
///
/// ```
/// use frankenterm_core::redactor::Redactor;
///
/// let redactor = Redactor::new();
/// let input = "My API key is sk-abc123456789012345678901234567890123456789012345678901";
/// let output = redactor.redact(input);
/// assert!(output.contains("[REDACTED]"));
/// assert!(!output.contains("sk-abc"));
/// ```
#[derive(Debug, Default, Clone)]
pub struct Redactor {
    /// Whether to include pattern names in redaction markers (for debugging).
    include_pattern_names: bool,
}

impl Redactor {
    /// Create a new redactor with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            include_pattern_names: false,
        }
    }

    /// Create a redactor that includes pattern names in redaction markers.
    #[must_use]
    pub fn with_debug_markers() -> Self {
        Self {
            include_pattern_names: true,
        }
    }

    /// Redact all detected secrets from the input text.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let mut result = text.to_string();

        for pattern in SECRET_PATTERNS {
            let replacement = if self.include_pattern_names {
                format!("[REDACTED:{}]", pattern.name)
            } else {
                REDACTED_MARKER.to_string()
            };

            result = pattern.regex.replace_all(&result, &replacement).to_string();
        }

        result
    }

    /// Check if text contains any detected secrets.
    #[must_use]
    pub fn contains_secrets(&self, text: &str) -> bool {
        SECRET_PATTERNS
            .iter()
            .any(|pattern| pattern.regex.is_match(text))
    }

    /// Detect all secrets in text and return their locations.
    #[must_use]
    pub fn detect(&self, text: &str) -> Vec<(&'static str, usize, usize)> {
        let mut detections = Vec::new();

        for pattern in SECRET_PATTERNS {
            for mat in pattern.regex.find_iter(text) {
                detections.push((pattern.name, mat.start(), mat.end()));
            }
        }

        detections.sort_by_key(|(_, start, _)| *start);
        detections
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ft-5o6u5: generic key/token/secret value patterns previously consumed
    /// only `[a-zA-Z0-9_-]`, so OAuth/base64 secrets containing `/`, `+`, or
    /// `=` were partially redacted with the trailing bytes left visible. The
    /// fix extends the charset to include `/+=`. Regression covers all three
    /// generic patterns with bytes that would have leaked pre-fix.
    #[test]
    fn redact_strips_full_base64_value_for_generic_secret() {
        let r = Redactor::new();
        let input = "client_secret=abcdEFGHijklMNOP/QRST+UVWX=YZ1234567890";
        let out = r.redact(input);
        assert!(
            !out.contains("QRST+UVWX=YZ1234567890"),
            "ft-5o6u5: base64 secret suffix must be redacted; got {out:?}"
        );
        assert!(
            !out.contains("/QRST"),
            "ft-5o6u5: redaction must not stop at the `/` separator; got {out:?}"
        );
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_strips_full_base64_value_for_generic_api_key() {
        let r = Redactor::new();
        let input = "api_key=abcdEFGH/QRST+UVWX=YZ1234567890";
        let out = r.redact(input);
        assert!(
            !out.contains("/QRST+UVWX=YZ1234567890"),
            "ft-5o6u5: api_key base64 suffix must be redacted; got {out:?}"
        );
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_strips_full_base64_value_for_generic_token() {
        let r = Redactor::new();
        // `token=` form (BEARER_TOKEN regex requires a `bearer ` prefix
        // and Authorization header context, so this exercises the
        // GENERIC_TOKEN path specifically).
        let input = "auth_token=abcdEFGHijklMNOP/QRST+UVWX=YZ1234567890";
        let out = r.redact(input);
        assert!(
            !out.contains("/QRST+UVWX=YZ1234567890"),
            "ft-5o6u5: token base64 suffix must be redacted; got {out:?}"
        );
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_terminates_secret_at_whitespace_boundary() {
        // The widened charset `[a-zA-Z0-9_/+=-]` must still terminate the
        // capture at whitespace so non-secret tail content is preserved.
        let r = Redactor::new();
        let input = "client_secret=abcd/EFGH+IJKL=MNOP1234 next_field=plaintext";
        let out = r.redact(input);
        assert!(
            out.contains("next_field=plaintext"),
            "ft-5o6u5: tail content past whitespace must survive; got {out:?}"
        );
        assert!(
            !out.contains("/EFGH+IJKL=MNOP1234"),
            "ft-5o6u5: full base64 value must be redacted; got {out:?}"
        );
    }

    #[test]
    fn detect_reports_full_span_for_base64_generic_secret() {
        let r = Redactor::new();
        let input = "secret=ABC/DEF+GHI=JKLMNOP1";
        let detections = r.detect(input);
        assert!(
            detections
                .iter()
                .any(|(name, _, _)| *name == "generic_secret"),
            "ft-5o6u5: detect() must flag generic_secret on base64 value; got {detections:?}"
        );
    }
}
