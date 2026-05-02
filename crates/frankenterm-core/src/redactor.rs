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

/// OpenAI API keys: sk-..., sk-proj-..., sk-svcacct-... (and admin variants).
/// Covers DeepSeek + Together-style keys that re-use the `sk-` prefix.
static OPENAI_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"sk-(?:proj-|svcacct-|admin-)?[a-zA-Z0-9_-]{20,}").expect("OpenAI key regex")
});

/// Anthropic API keys: sk-ant-..., sk-ant-api03-..., sk-ant-admin01-...
static ANTHROPIC_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sk-ant-[a-zA-Z0-9_-]{20,}").expect("Anthropic key regex"));

/// GitHub classic tokens: ghp_, gho_, ghu_, ghs_, ghr_.
static GITHUB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"gh[pousr]_[a-zA-Z0-9]{36,}").expect("GitHub token regex"));

/// GitHub fine-grained PATs: github_pat_<82+ chars>.
/// Distinct format from classic ghp_ tokens — different length and
/// charset (includes underscores in the body).
static GITHUB_FINE_GRAINED_PAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"github_pat_[A-Za-z0-9_]{40,}").expect("GitHub fine-grained PAT regex")
});

/// xAI API keys: xai-<80+ alphanumeric>.
static XAI_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"xai-[A-Za-z0-9]{40,}").expect("xAI key regex"));

/// Groq API keys: gsk_<40+ alphanumeric>.
static GROQ_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"gsk_[A-Za-z0-9]{40,}").expect("Groq key regex"));

/// Google API keys (incl. Vertex AI / Gemini / Cloud): AIza<35 chars>.
/// Exact length of 39 total chars; charset includes `_-`.
static GOOGLE_API_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"AIza[A-Za-z0-9_-]{35}").expect("Google API key regex"));

/// Google OAuth 2.0 access tokens: ya29.<base64-ish body>.
/// Used by Vertex AI service-account flows + gcloud auth tokens.
static GOOGLE_OAUTH_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"ya29\.[A-Za-z0-9_-]{20,}").expect("Google OAuth token regex"));

/// Hugging Face tokens: hf_<30+ alphanumeric>.
static HUGGINGFACE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"hf_[A-Za-z0-9]{30,}").expect("Hugging Face token regex"));

/// Replicate API tokens: r8_<30+ alphanumeric>.
static REPLICATE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"r8_[A-Za-z0-9]{30,}").expect("Replicate token regex"));

/// Anyscale API keys: esecret_<30+ alphanumeric>.
static ANYSCALE_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"esecret_[A-Za-z0-9]{30,}").expect("Anyscale key regex"));

/// Perplexity API keys: pplx-<40+ alphanumeric>.
static PERPLEXITY_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"pplx-[A-Za-z0-9]{40,}").expect("Perplexity key regex"));

/// Contextual provider keys for Cohere / Mistral / Together / Fireworks /
/// DeepInfra / Anthropic-Vertex which don't carry a distinct unbreakable
/// prefix in the value itself. Anchored on the variable-name side: the
/// secret only redacts when the surrounding key name names the provider.
/// Catches common config-file shapes like `cohere_api_key=...` or
/// `MISTRAL_API_KEY: "..."`.
static AI_PROVIDER_KEYED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:cohere|mistral|together(?:_ai)?|fireworks|deepinfra|nvidia[_-]?api|databricks[_-]?token|azure[_-]?openai)[_-]?(?:api[_-]?)?(?:key|token|secret)\s*[=:]\s*['"]?([a-zA-Z0-9_/+=.-]{16,})['"]?"#
    )
    .expect("AI provider keyed value regex")
});

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

/// JWT tokens — `<base64-header>.<base64-payload>.<base64-signature>`.
/// Real JWTs always start with `eyJ` (the base64 of `{"`). The most
/// common OAuth / cloud-API secret format in modern logs; this
/// pattern catches BARE jwts (not preceded by `Bearer`), which the
/// `BEARER_TOKEN` regex above wouldn't match.
///
/// br-ft-8nd26: filed coverage gap — JWTs in logs (e.g., a debug
/// log line `Got token: eyJ...`) previously leaked unredacted.
static JWT_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
        .expect("JWT token regex")
});

/// GitLab personal access tokens: `glpat-<20+ chars>`.
static GITLAB_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"glpat-[A-Za-z0-9_-]{20,}").expect("GitLab token regex")
});

/// Twilio account SIDs: `AC` + 32 hex chars (case-insensitive).
/// SIDs are not strictly secret but pair with auth tokens; redact for
/// audit-chain hygiene.
static TWILIO_ACCOUNT_SID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"AC[a-fA-F0-9]{32}").expect("Twilio account SID regex")
});

/// SendGrid API keys: `SG.<22 chars>.<43 chars>`. Distinctive 3-part
/// format with `SG.` prefix.
static SENDGRID_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"SG\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{40,}")
        .expect("SendGrid API key regex")
});

/// Datadog API keys are 32 hex chars; the convention is to set them
/// via `DD_API_KEY=` or `DATADOG_API_KEY=`. Keyed-name match to avoid
/// false positives on bare 32-hex strings.
static DATADOG_API_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:DD|DATADOG)_API_KEY\s*[=:]\s*['"]?([a-fA-F0-9]{32})['"]?"#)
        .expect("Datadog API key regex")
});

/// All secret patterns in priority order.
///
/// Order matters: more specific provider regexes must run before the
/// broader sk- alternation. `openai_key`'s `sk-(?:proj-|svcacct-|admin-)?`
/// alternation does NOT exclude `sk-ant-` (the body charset accepts
/// `ant-` via `[a-zA-Z0-9_-]{20,}`), so `anthropic_key` is sequenced
/// first to claim Anthropic-shaped tokens with the right marker.
static SECRET_PATTERNS: &[SecretPattern] = &[
    SecretPattern {
        name: "anthropic_key",
        regex: &ANTHROPIC_KEY,
    },
    SecretPattern {
        name: "openai_key",
        regex: &OPENAI_KEY,
    },
    SecretPattern {
        name: "github_token",
        regex: &GITHUB_TOKEN,
    },
    SecretPattern {
        name: "github_fine_grained_pat",
        regex: &GITHUB_FINE_GRAINED_PAT,
    },
    SecretPattern {
        name: "gitlab_token",
        regex: &GITLAB_TOKEN,
    },
    SecretPattern {
        name: "xai_key",
        regex: &XAI_KEY,
    },
    SecretPattern {
        name: "groq_key",
        regex: &GROQ_KEY,
    },
    SecretPattern {
        name: "google_api_key",
        regex: &GOOGLE_API_KEY,
    },
    SecretPattern {
        name: "google_oauth_token",
        regex: &GOOGLE_OAUTH_TOKEN,
    },
    SecretPattern {
        name: "huggingface_token",
        regex: &HUGGINGFACE_TOKEN,
    },
    SecretPattern {
        name: "replicate_token",
        regex: &REPLICATE_TOKEN,
    },
    SecretPattern {
        name: "anyscale_key",
        regex: &ANYSCALE_KEY,
    },
    SecretPattern {
        name: "perplexity_key",
        regex: &PERPLEXITY_KEY,
    },
    SecretPattern {
        name: "ai_provider_keyed_value",
        regex: &AI_PROVIDER_KEYED_VALUE,
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
        name: "twilio_account_sid",
        regex: &TWILIO_ACCOUNT_SID,
    },
    SecretPattern {
        name: "sendgrid_key",
        regex: &SENDGRID_KEY,
    },
    SecretPattern {
        name: "datadog_api_key",
        regex: &DATADOG_API_KEY,
    },
    SecretPattern {
        name: "database_url",
        regex: &DATABASE_URL,
    },
    // JWT runs BEFORE the generic patterns so it claims the
    // distinctive `eyJ.eyJ.<sig>` shape with a clear pattern name
    // for telemetry; otherwise generic_token would catch many
    // JWTs but with the less-specific `generic_token` label.
    SecretPattern {
        name: "jwt_token",
        regex: &JWT_TOKEN,
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

    /// Cold-tier integration adapter (br-ft-95vfk slice 1).
    ///
    /// Takes raw chunk bytes (typically UTF-8 terminal output but
    /// may contain arbitrary bytes from misbehaving processes),
    /// runs the redactor, and returns the post-redact bytes plus
    /// evidence the integration plumbs into
    /// `ColdTierPipelineHealth::record_write`'s `redactor_applied`
    /// flag and `bytes_replaced` telemetry.
    ///
    /// Non-UTF-8 input is handled via lossy decode
    /// (`String::from_utf8_lossy`): invalid bytes become `U+FFFD`
    /// in the scanned text. The privacy invariant holds — even
    /// if some bytes are mangled in the lossy decode, the
    /// redactor still scans the salvageable text for secrets.
    /// The returned bytes are the lossy-decoded then redacted
    /// then re-encoded UTF-8.
    ///
    /// `bytes_replaced` is the difference in length between the
    /// pre-redact lossy-decoded text and the post-redact text.
    /// Negative direction (substrate signals length grew because
    /// the marker is longer than the secret) is captured as `0`
    /// since we use `saturating_sub`. Operator interprets as
    /// "bytes that the redactor sanitised away."
    #[must_use]
    pub fn redact_bytes_with_evidence(&self, bytes: &[u8]) -> RedactionResult {
        let lossy = String::from_utf8_lossy(bytes);
        let detections = self.detect(&lossy);
        let matches = detections.len() as u32;
        let pre_len = lossy.len();
        let redacted = self.redact(&lossy);
        let post_len = redacted.len();
        // Positive when redactor shortened (typical: secret →
        // [REDACTED]). Negative direction saturates at 0.
        let bytes_replaced = pre_len.saturating_sub(post_len) as u32;
        RedactionResult {
            bytes: redacted.into_bytes(),
            evidence: BytesRedactionEvidence {
                matches,
                bytes_replaced,
            },
        }
    }
}

/// Evidence the redactor returns to the cold-tier integration
/// per the bead's privacy invariant. Mirrors the
/// `RedactionEvidence` shape in
/// `scrollback_cold_tier_pipeline.rs` so the integration can
/// pass either through `ChunkBytes::redact_with_evidence`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BytesRedactionEvidence {
    /// Number of redactor-rule matches the substrate found
    /// in the input.
    pub matches: u32,
    /// Bytes the redactor replaced (pre-redact length minus
    /// post-redact length, saturating at 0). Operator-readable
    /// signal of "how much the redactor sanitised away."
    pub bytes_replaced: u32,
}

impl BytesRedactionEvidence {
    /// Whether the redactor scanned the input. Always `true`
    /// once evidence is produced — the cold-tier
    /// privacy-invariant contract: integration plumbs this
    /// flag into `ColdTierPipelineHealth::record_write` so the
    /// audit trail proves a real scan happened.
    #[must_use]
    pub const fn redactor_applied(&self) -> bool {
        true
    }

    /// Whether the redactor matched + replaced anything.
    /// Distinct from `redactor_applied` (substrate scanned but
    /// found no secrets ⇒ applied=true, made_changes=false).
    #[must_use]
    pub const fn made_changes(&self) -> bool {
        self.matches > 0
    }
}

/// Aggregate return of `Redactor::redact_bytes_with_evidence`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionResult {
    /// Post-redact bytes. UTF-8-encoded (the lossy decode means
    /// invalid input bytes are now U+FFFD in the output).
    pub bytes: Vec<u8>,
    pub evidence: BytesRedactionEvidence,
}

impl RedactionResult {
    /// Convenience destructure for callers that want the
    /// `(Vec<u8>, BytesRedactionEvidence)` tuple matching
    /// `ChunkBytes::redact_with_evidence`'s closure signature.
    #[must_use]
    pub fn into_pair(self) -> (Vec<u8>, BytesRedactionEvidence) {
        (self.bytes, self.evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // Cold-tier integration adapter (br-ft-95vfk slice 1)
    // ----------------------------------------------------------------

    #[test]
    fn redact_bytes_with_evidence_clean_input_no_match() {
        let r = Redactor::new();
        let result = r.redact_bytes_with_evidence(b"benign log line, nothing secret");
        assert!(result.evidence.redactor_applied());
        assert!(!result.evidence.made_changes());
        assert_eq!(result.evidence.matches, 0);
        assert_eq!(result.evidence.bytes_replaced, 0);
        assert_eq!(result.bytes, b"benign log line, nothing secret");
    }

    #[test]
    fn redact_bytes_with_evidence_single_match_records_count() {
        let r = Redactor::new();
        let input = b"GITLAB_TOKEN=glpat-xxxxxxxxxxxxxxxxxxxx";
        let result = r.redact_bytes_with_evidence(input);
        assert!(result.evidence.redactor_applied());
        assert!(result.evidence.made_changes());
        assert!(result.evidence.matches >= 1);
        assert!(!std::str::from_utf8(&result.bytes).unwrap().contains("glpat-"));
        // Marker is shorter than the secret → bytes_replaced > 0.
        assert!(result.evidence.bytes_replaced > 0);
    }

    #[test]
    fn redact_bytes_with_evidence_into_pair_destructures() {
        let r = Redactor::new();
        let input = b"GITLAB_TOKEN=glpat-yyyyyyyyyyyyyyyyyyyy";
        let (bytes, evidence) = r.redact_bytes_with_evidence(input).into_pair();
        assert!(evidence.redactor_applied());
        assert!(evidence.made_changes());
        assert!(!std::str::from_utf8(&bytes).unwrap().contains("glpat-"));
    }

    #[test]
    fn redact_bytes_with_evidence_handles_invalid_utf8_lossy() {
        let r = Redactor::new();
        // Invalid UTF-8 surrounding a secret. The redactor still
        // scans the salvageable text via lossy-decode.
        let mut input: Vec<u8> = vec![0xFF, 0xFE]; // invalid
        input.extend_from_slice(b" GITLAB_TOKEN=glpat-zzzzzzzzzzzzzzzzzzzz ");
        input.push(0xFD); // invalid trailing
        let result = r.redact_bytes_with_evidence(&input);
        assert!(result.evidence.redactor_applied());
        assert!(result.evidence.made_changes());
        // Output is valid UTF-8 (lossy decode replaced invalid
        // bytes with U+FFFD).
        assert!(std::str::from_utf8(&result.bytes).is_ok());
        assert!(!std::str::from_utf8(&result.bytes).unwrap().contains("glpat-"));
    }

    #[test]
    fn redact_bytes_with_evidence_evidence_made_changes_predicate() {
        let zero = BytesRedactionEvidence { matches: 0, bytes_replaced: 0 };
        let some = BytesRedactionEvidence { matches: 3, bytes_replaced: 100 };
        assert!(zero.redactor_applied());
        assert!(!zero.made_changes());
        assert!(some.redactor_applied());
        assert!(some.made_changes());
    }

    #[test]
    fn redact_bytes_with_evidence_empty_input() {
        let r = Redactor::new();
        let result = r.redact_bytes_with_evidence(b"");
        assert!(result.evidence.redactor_applied());
        assert!(!result.evidence.made_changes());
        assert!(result.bytes.is_empty());
    }

    /// br-ft-8nd26: bare JWT in a log line (not preceded by
    /// `Bearer`) previously leaked because BEARER_TOKEN required
    /// the prefix. New JWT_TOKEN pattern catches the bare form.
    #[test]
    fn redact_bare_jwt_token_not_preceded_by_bearer() {
        let r = Redactor::new();
        let input = "Got token: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = r.redact(input);
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"));
        assert!(!out.contains("SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"));
    }

    #[test]
    fn redact_jwt_in_authorization_header() {
        let r = Redactor::new();
        let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0ZXN0In0.abc123_test";
        let out = r.redact(input);
        // Either Bearer or JWT pattern catches it; both should
        // redact.
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn redact_gitlab_personal_access_token() {
        let r = Redactor::new();
        let input = "GITLAB_TOKEN=glpat-xxxxxxxxxxxxxxxxxxxx";
        let out = r.redact(input);
        assert!(!out.contains("glpat-xxxxxxxxxxxxxxxxxxxx"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_twilio_account_sid() {
        let r = Redactor::new();
        let input = "twilio_sid=ACdeadbeef0123456789abcdef0123456789";
        let out = r.redact(input);
        assert!(!out.contains("ACdeadbeef0123456789abcdef0123456789"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_sendgrid_api_key() {
        let r = Redactor::new();
        let input =
            "SENDGRID_API_KEY=SG.AbCdEfGhIjKlMnOpQrStUv.WxYz0123456789abcdefghijklmnopqrstuvwxyzABCD";
        let out = r.redact(input);
        assert!(!out.contains("SG.AbCdEfGhIjKlMnOpQrStUv"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_datadog_api_key() {
        let r = Redactor::new();
        let input = "DD_API_KEY=deadbeef0123456789abcdef01234567";
        let out = r.redact(input);
        assert!(!out.contains("deadbeef0123456789abcdef01234567"));
        assert!(out.contains("[REDACTED]"));
    }

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

    // ── ft-3xek9: newer AI provider tokens ────────────────────────────────
    //
    // Fixture coverage for prefixed keys that the pre-ft-3xek9 blocklist
    // missed: xAI, Groq, Google (Vertex/Gemini), GitHub fine-grained PATs,
    // Hugging Face, Replicate, Anyscale, Perplexity, plus contextual
    // matching for Cohere/Mistral/Together/Fireworks/DeepInfra/Nvidia
    // API/Databricks/Azure-OpenAI keys that don't carry a distinct prefix.
    //
    // Each fixture uses a synthetic non-functional token in the documented
    // format; the assertion is that the secret bytes are gone from the
    // output, not that the marker is positioned exactly.

    fn redactor_with_named_markers() -> Redactor {
        Redactor::with_debug_markers()
    }

    #[test]
    fn redact_xai_api_key() {
        let r = redactor_with_named_markers();
        let raw = "xai-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRsT01234567890";
        let out = r.redact(&format!("XAI_API_KEY={raw}"));
        assert!(!out.contains(raw), "ft-3xek9: xAI key leaked: {out:?}");
        assert!(out.contains("[REDACTED:xai_key]"), "{out:?}");
    }

    #[test]
    fn redact_groq_api_key() {
        let r = redactor_with_named_markers();
        let raw = "gsk_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgH";
        let out = r.redact(&format!("groq config: {raw}"));
        assert!(!out.contains(raw), "ft-3xek9: Groq key leaked: {out:?}");
        assert!(out.contains("[REDACTED:groq_key]"), "{out:?}");
    }

    #[test]
    fn redact_google_vertex_api_key() {
        let r = redactor_with_named_markers();
        // Google API keys are exactly 39 chars: AIza + 35 chars [A-Za-z0-9_-].
        let raw = "AIzaSyB1234567890_abcdefghijklmnopqrstuv";
        let out = r.redact(&format!("--api-key={raw}"));
        assert!(
            !out.contains("AIzaSy"),
            "ft-3xek9: Google key leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:google_api_key]"), "{out:?}");
    }

    #[test]
    fn redact_google_oauth_token() {
        let r = redactor_with_named_markers();
        let raw = "ya29.a0AfH6SMBxyz_1234567890abcdefghijklmnopqrstuv";
        let out = r.redact(&format!("Bearer auth header: {raw}"));
        assert!(
            !out.contains("ya29.a0"),
            "ft-3xek9: Google OAuth token leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:google_oauth_token]"), "{out:?}");
    }

    #[test]
    fn redact_github_fine_grained_pat() {
        let r = redactor_with_named_markers();
        // Fine-grained PATs: github_pat_<22 chars><59 chars> in practice;
        // we accept >=40 body chars to be permissive.
        let raw = "github_pat_11ABCDEFG0aBcDeFg_HiJkLmNoPqRsTuVwXyZ1234567890ABCDE";
        let out = r.redact(&format!("token: {raw}"));
        assert!(
            !out.contains("github_pat_11"),
            "ft-3xek9: fine-grained PAT leaked: {out:?}"
        );
        assert!(
            out.contains("[REDACTED:github_fine_grained_pat]"),
            "{out:?}"
        );
    }

    #[test]
    fn redact_huggingface_token() {
        let r = redactor_with_named_markers();
        let raw = "hf_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
        let out = r.redact(&format!("HF_TOKEN={raw}"));
        assert!(!out.contains("hf_a"), "ft-3xek9: HF token leaked: {out:?}");
        assert!(out.contains("[REDACTED:huggingface_token]"), "{out:?}");
    }

    #[test]
    fn redact_replicate_token() {
        let r = redactor_with_named_markers();
        let raw = "r8_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aB";
        let out = r.redact(&format!("REPLICATE_API_TOKEN={raw}"));
        assert!(
            !out.contains("r8_a"),
            "ft-3xek9: Replicate token leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:replicate_token]"), "{out:?}");
    }

    #[test]
    fn redact_anyscale_key() {
        let r = redactor_with_named_markers();
        let raw = "esecret_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aB";
        let out = r.redact(&format!("ANYSCALE_API_KEY={raw}"));
        assert!(
            !out.contains("esecret_a"),
            "ft-3xek9: Anyscale key leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:anyscale_key]"), "{out:?}");
    }

    #[test]
    fn redact_perplexity_key() {
        let r = redactor_with_named_markers();
        let raw = "pplx-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgH";
        let out = r.redact(&format!("PPLX={raw}"));
        assert!(
            !out.contains("pplx-a"),
            "ft-3xek9: Perplexity key leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:perplexity_key]"), "{out:?}");
    }

    #[test]
    fn redact_openai_service_account_key() {
        let r = redactor_with_named_markers();
        // sk-svcacct-... is OpenAI's service-account variant. Already
        // matched by the openai_key regex via the (?:proj-|svcacct-|admin-)?
        // alternation — fixture pins that property.
        let raw = "sk-svcacct-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
        let out = r.redact(raw);
        assert!(
            !out.contains("sk-svcacct-a"),
            "ft-3xek9: OAI service-account key leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:openai_key]"), "{out:?}");
    }

    #[test]
    fn redact_anthropic_api03_admin_variants() {
        let r = redactor_with_named_markers();
        // Modern Anthropic key formats include `api03-` and `admin01-`
        // segments; existing sk-ant-[a-zA-Z0-9_-]{20,} regex catches both.
        let api03 = "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890_abcdef";
        let admin01 = "sk-ant-admin01-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890_abc";

        let out = r.redact(&format!("k1={api03} k2={admin01}"));
        assert!(
            !out.contains("api03-a"),
            "ft-3xek9: Anthropic api03 leaked: {out:?}"
        );
        assert!(
            !out.contains("admin01-a"),
            "ft-3xek9: Anthropic admin01 leaked: {out:?}"
        );
        assert!(
            out.matches("[REDACTED:anthropic_key]").count() >= 2,
            "{out:?}"
        );
    }

    #[test]
    fn redact_cohere_keyed_value() {
        let r = redactor_with_named_markers();
        // Cohere keys lack a distinct value prefix; redaction is keyed on
        // the variable name `cohere_api_key`.
        let raw = "abcdefABCDEF1234567890ghijklmnopqrstuvwx";
        let out = r.redact(&format!("cohere_api_key={raw}"));
        assert!(!out.contains(raw), "ft-3xek9: Cohere key leaked: {out:?}");
        assert!(
            out.contains("[REDACTED:ai_provider_keyed_value]"),
            "{out:?}"
        );
    }

    #[test]
    fn redact_mistral_keyed_value() {
        let r = redactor_with_named_markers();
        let raw = "abcdefABCDEF1234567890ghijklmnopqrstuvwx";
        let out = r.redact(&format!(r#"MISTRAL_API_KEY: "{raw}""#));
        assert!(!out.contains(raw), "ft-3xek9: Mistral key leaked: {out:?}");
        assert!(
            out.contains("[REDACTED:ai_provider_keyed_value]"),
            "{out:?}"
        );
    }

    #[test]
    fn redact_together_ai_keyed_value() {
        let r = redactor_with_named_markers();
        // Together AI uses 64-hex tokens but no distinct prefix; covered
        // contextually via `together_api_key` / `together_ai_key`.
        let raw = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let out = r.redact(&format!("together_ai_key={raw}"));
        assert!(
            !out.contains(raw),
            "ft-3xek9: Together AI key leaked: {out:?}"
        );
        assert!(
            out.contains("[REDACTED:ai_provider_keyed_value]"),
            "{out:?}"
        );
    }

    #[test]
    fn redact_fireworks_keyed_value() {
        let r = redactor_with_named_markers();
        let raw = "abcdefABCDEF1234567890ghijklmnopqrstuvwx";
        let out = r.redact(&format!("FIREWORKS_API_KEY={raw}"));
        assert!(
            !out.contains(raw),
            "ft-3xek9: Fireworks key leaked: {out:?}"
        );
        assert!(
            out.contains("[REDACTED:ai_provider_keyed_value]"),
            "{out:?}"
        );
    }

    #[test]
    fn redact_azure_openai_keyed_value() {
        let r = redactor_with_named_markers();
        let raw = "abcdefABCDEF1234567890ghijklmnopqrstuvwx";
        let out = r.redact(&format!("AZURE_OPENAI_API_KEY={raw}"));
        assert!(
            !out.contains(raw),
            "ft-3xek9: Azure OpenAI key leaked: {out:?}"
        );
        assert!(
            out.contains("[REDACTED:ai_provider_keyed_value]"),
            "{out:?}"
        );
    }

    #[test]
    fn no_false_positive_on_common_prose() {
        // ft-3xek9: confirm the new patterns don't fire on benign words
        // sharing prefixes with provider keys (e.g., "xai" appearing as
        // a user nickname, "hf_" inside a commit message). Each new
        // regex requires a body of >=30..40 chars after the prefix, so
        // short occurrences should not redact.
        let r = Redactor::new();
        let prose = "tagged xai-short grok hf_x note: pplx-x lorem r8_x";
        let out = r.redact(prose);
        assert_eq!(out, prose, "ft-3xek9: prose must not redact: {out:?}");
    }

    #[test]
    fn detect_reports_each_new_provider_pattern_by_name() {
        // ft-3xek9: detect() reports each new pattern's name when its
        // signature appears.
        let r = Redactor::new();
        let blob = concat!(
            "xai-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRsT01234567890 ",
            "gsk_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgH ",
            "AIzaSyB1234567890_abcdefghijklmnopqrstuv ",
            "github_pat_11ABCDEFG0aBcDeFg_HiJkLmNoPqRsTuVwXyZ1234567890ABCDE ",
            "hf_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890 ",
            "r8_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aB ",
            "esecret_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aB ",
            "pplx-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgH ",
            "ya29.a0AfH6SMBxyz_1234567890abcdefghijklmnopqrstuv ",
        );

        let names: std::collections::HashSet<&'static str> = r
            .detect(blob)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();

        for required in [
            "xai_key",
            "groq_key",
            "google_api_key",
            "github_fine_grained_pat",
            "huggingface_token",
            "replicate_token",
            "anyscale_key",
            "perplexity_key",
            "google_oauth_token",
        ] {
            assert!(
                names.contains(required),
                "ft-3xek9: detect() missed {required}; got {names:?}"
            );
        }
    }
}
