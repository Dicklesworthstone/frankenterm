//! Secret redaction for read, export, and audit surfaces.

use regex::Regex;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Redaction marker used in place of detected secrets.
pub const REDACTED_MARKER: &str = "[REDACTED]";

/// Bytes retained between streaming redaction chunks.
///
/// The catalog contains fixed-shape API tokens plus multiline armoured blocks;
/// this cap bounds pathological unterminated-prefix buffering. Ordinary chunks
/// with no secret-looking suffix are emitted immediately.
pub const DEFAULT_STREAMING_REDACTOR_TAIL_BYTES: usize = 64 * 1024;

/// Maximum bytes the [`StreamingRedactor.pending`] buffer may hold before
/// forced emission kicks in. [ft-4socw]
///
/// Pre-fix \`pending\` had no upper bound: an adversarial or buggy producer
/// streaming repeated occurrences of any [`STREAMING_SECRET_ANCHORS`] entry
/// (e.g. `"rk_AAAAAAAA"` × 1 GiB) would drive memory growth without limit
/// because every anchor in the rolling tail-window pushes the safe-emit
/// boundary backwards. Setting an absolute cap ensures the buffer drains
/// even when no clean boundary is found.
///
/// 8 MiB is generous for legitimate streams (a single chunked terminal
/// log line rarely exceeds a few KiB) but quickly halts runaway growth.
pub const DEFAULT_STREAMING_REDACTOR_MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

/// Smallest pending-buffer cap that can make overflow emission progress.
///
/// The overflow path emits `max_pending_bytes / 2`; caps below 2 would make
/// forced emission drain zero bytes while still incrementing overflow telemetry.
pub const MIN_STREAMING_REDACTOR_MAX_PENDING_BYTES: usize = 2;

/// br-ft-4socw: cumulative count of [`StreamingRedactor::redact_chunk`]
/// calls that triggered forced emission because `pending.len()` exceeded
/// `max_pending_bytes`.
///
/// Non-zero values signal a runaway producer — the streaming redactor
/// drained content with the redactor regex applied (catching complete
/// patterns) but cannot guarantee that partial-secret prefixes
/// straddling the cut boundary weren't emitted unredacted. Operators
/// should investigate the producer when this counter has a steady
/// non-zero rate; one-off bumps from large legitimate streams are
/// expected.
///
/// Same observability defect family as ft-luav8 / ft-skec1 / ft-tpdl5
/// / ft-wzk10 — make silent state loss visible.
static STREAMING_REDACTOR_PENDING_OVERFLOW_COUNT: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of forced emissions from the streaming redactor's
/// pending buffer overflow path. See
/// [`STREAMING_REDACTOR_PENDING_OVERFLOW_COUNT`] for the contract.
#[must_use]
pub fn streaming_redactor_pending_overflow_count() -> u64 {
    STREAMING_REDACTOR_PENDING_OVERFLOW_COUNT.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so tests that exercise overflow
/// can assert post-increment values without state leakage.
#[cfg(test)]
pub fn reset_streaming_redactor_pending_overflow_count_for_test() {
    STREAMING_REDACTOR_PENDING_OVERFLOW_COUNT.store(0, Ordering::Relaxed);
}

/// Internal helper: bump the counter when forced emission fires.
fn record_streaming_redactor_pending_overflow() {
    STREAMING_REDACTOR_PENDING_OVERFLOW_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Literal anchors that can begin a catalog match.
///
/// Streaming redaction does not need to retain an unconditional overlap for
/// every chunk. It only needs to keep suffixes that already contain one of
/// these anchors, or suffix fragments that could become one when the next chunk
/// arrives.
const STREAMING_SECRET_ANCHORS: &[&str] = &[
    "sk-",
    "sk-ant-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xai-",
    "gsk_",
    "AIza",
    "ya29.",
    "hf_",
    "r8_",
    "esecret_",
    "pplx-",
    "cohere",
    "mistral",
    "together",
    "fireworks",
    "deepinfra",
    "nvidia",
    "databricks",
    "azure_openai",
    "azure-openai",
    "AKIA",
    "aws_secret_access_key",
    "Authorization",
    "authorization",
    "Bearer ",
    "bearer ",
    "api_key",
    "api-key",
    "apikey",
    "token",
    "password",
    "secret",
    "device_code",
    "device-code",
    "user_code",
    "user-code",
    "access_token",
    "code=",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "sk_live_",
    "sk_test_",
    "pk_live_",
    "pk_test_",
    "rk_live_",
    "rk_test_",
    "whsec_",
    "postgres://",
    "postgresql://",
    "mysql://",
    "mongodb://",
    "redis://",
    "-----BEGIN ",
    "eyJ",
    "glpat-",
    "SG.",
    "AC",
    "DD_API_KEY",
];

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
    LazyLock::new(|| Regex::new(r"sk-ant-[a-zA-Z0-9_-]{40,}").expect("Anthropic key regex"));

/// GitHub classic tokens: ghp_, gho_, ghu_, ghs_, ghr_.
static GITHUB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"gh[pousr]_[a-zA-Z0-9]{36,}").expect("GitHub token regex"));

/// GitHub fine-grained PATs: github_pat_<82+ chars>.
/// Distinct format from classic ghp_ tokens — different length and
/// charset (includes underscores in the body).
static GITHUB_FINE_GRAINED_PAT: LazyLock<Regex> = LazyLock::new(|| {
    // Real fine-grained PATs are 93 chars total; tighten body
    // threshold from 40 to 50 to reduce false-positive risk while
    // still catching every real PAT (br-ft-2xkrc).
    Regex::new(r"github_pat_[A-Za-z0-9_]{50,}").expect("GitHub fine-grained PAT regex")
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

/// Stripe API keys: sk_live_, sk_test_, pk_live_, pk_test_,
/// rk_live_, rk_test_, plus whsec_ webhook signing secrets.
///
/// br-ft-76zp6: pre-fix this only covered `[ps]k_` — restricted
/// keys (`rk_*`, full API credentials with scoped permissions per
/// Stripe docs) and webhook signing secrets (`whsec_*`, the HMAC
/// anchor for payload-integrity verification) were both passing
/// through the redactor unscrubbed. Restricted keys carry the same
/// sensitivity grade as `sk_*` for the scopes they own; an exposed
/// `whsec_*` lets an attacker forge webhook events to the
/// merchant's endpoint.
///
/// Anchored at a word boundary on the leading side so embedded
/// substrings — e.g., `something_whsec_aaa...` or `xpk_live_...`
/// — don't match. Real Stripe credentials always start at a
/// token boundary in configs/logs/env files; the `\b` prevents
/// the false-positive class flagged in the fresh-eyes audit.
static STRIPE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:[psr]k_(?:live|test)|whsec)_[a-zA-Z0-9]{20,}").expect("Stripe key regex")
});

/// Database connection strings with passwords.
static DATABASE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:postgres|mysql|mongodb|redis)(?:ql)?://[^:]+:([^@\s]+)@")
        .expect("Database URL regex")
});

/// SSH / OpenSSL PEM private-key blocks (br-ft-2xkrc).
///
/// Catches every PEM-formatted private-key variant produced by
/// OpenSSH + OpenSSL:
/// - `-----BEGIN RSA PRIVATE KEY-----` (PKCS#1, RSA)
/// - `-----BEGIN DSA PRIVATE KEY-----` (PKCS#1, DSA)
/// - `-----BEGIN EC PRIVATE KEY-----` (PKCS#1, ECDSA)
/// - `-----BEGIN OPENSSH PRIVATE KEY-----` (OpenSSH new-format —
///   what real ed25519 / modern keys ship as)
/// - `-----BEGIN ED25519 PRIVATE KEY-----` (non-standard, some
///   tools emit this)
/// - `-----BEGIN PRIVATE KEY-----` (PKCS#8, unencrypted — NO algo
///   prefix between `BEGIN ` and `PRIVATE`, hence the `*`
///   quantifier on the prefix class instead of `+`)
/// - `-----BEGIN ENCRYPTED PRIVATE KEY-----` (PKCS#8, encrypted —
///   leaked passphrase or weak passphrase + leaked blob =
///   compromised key, so we still scrub)
///
/// The algo prefix `[A-Z0-9 ]*` covers digit-bearing algo names
/// (ED25519) AND the no-prefix PKCS#8 variant. The body uses
/// `[\s\S]+?` so adjacent PEM blocks do not collapse into one
/// match (reluctant quantifier stops at the first `-----END ...
/// PRIVATE KEY-----` it sees). Pre-fix coverage was zero — a
/// developer pasting `cat ~/.ssh/id_rsa` into a pane flowed the
/// entire key block through the cold-tier pipeline / audit chain
/// / search index unredacted.
static SSH_PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]+?-----END [A-Z0-9 ]*PRIVATE KEY-----")
        .expect("SSH private key regex")
});

/// PGP / OpenPGP armoured blocks (br-ft-2xkrc).
///
/// Catches the four armoured-block flavours OpenPGP / GPG emits:
/// - `BEGIN PGP PRIVATE KEY BLOCK` ... `END PGP PRIVATE KEY BLOCK`
///   (the most sensitive — exfiltrated key allows decryption +
///   signature forgery)
/// - `BEGIN PGP PUBLIC KEY BLOCK` ... `END PGP PUBLIC KEY BLOCK`
///   (less sensitive but still worth scrubbing — public keys
///   identify their owner; in some operator workflows the
///   key-ID itself is sensitive)
/// - `BEGIN PGP MESSAGE` ... `END PGP MESSAGE` (encrypted body)
/// - `BEGIN PGP SIGNED MESSAGE` ... `END PGP SIGNATURE`
///   (signed plaintext — note the asymmetric BEGIN/END markers:
///   "SIGNED MESSAGE" opens, "SIGNATURE" closes the trailing
///   signature block)
///
/// The two trailing-marker arms are necessary because PGP signed
/// messages do NOT close with a matching "END PGP SIGNED MESSAGE"
/// — the signature block has its own END.
static PGP_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"-----BEGIN PGP (?:PRIVATE KEY BLOCK|PUBLIC KEY BLOCK|MESSAGE|SIGNED MESSAGE)-----[\s\S]+?-----END PGP (?:PRIVATE KEY BLOCK|PUBLIC KEY BLOCK|MESSAGE|SIGNATURE)-----",
    )
    .expect("PGP block regex")
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
    Regex::new(r"eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+").expect("JWT token regex")
});

/// GitLab personal access tokens: `glpat-<20+ chars>`.
static GITLAB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"glpat-[A-Za-z0-9_-]{20,}").expect("GitLab token regex"));

/// Twilio account SIDs: `AC` + 32 hex chars (case-insensitive).
/// SIDs are not strictly secret but pair with auth tokens; redact for
/// audit-chain hygiene.
static TWILIO_ACCOUNT_SID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"AC[a-fA-F0-9]{32}").expect("Twilio account SID regex"));

/// SendGrid API keys: `SG.<22 chars>.<43 chars>`. Distinctive 3-part
/// format with `SG.` prefix.
static SENDGRID_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"SG\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{40,}").expect("SendGrid API key regex")
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
    // br-ft-2xkrc: SSH/PEM private-key blocks. Runs BEFORE the
    // generic patterns so the multi-line BEGIN/END envelope claims
    // a distinctive name; the generic patterns might otherwise bite
    // base64-like body lines individually with `generic_secret`,
    // leaving the surrounding BEGIN/END markers in plaintext.
    SecretPattern {
        name: "ssh_private_key",
        regex: &SSH_PRIVATE_KEY,
    },
    // br-ft-2xkrc: PGP / OpenPGP armoured blocks (private keys +
    // public keys + encrypted messages + signed messages). Same
    // ordering rationale as ssh_private_key — the multi-line
    // armoured envelope is a distinctive shape that earns its own
    // pattern name.
    SecretPattern {
        name: "pgp_block",
        regex: &PGP_BLOCK,
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
    ///
    /// br-ft-hkif4: walks `SECRET_PATTERNS` in priority order
    /// (the same ordering `redact()` honors at line ~616) and
    /// drops lower-priority overlaps. Without this dedup, a
    /// single Anthropic `sk-ant-api03-...` token would match
    /// both `anthropic_key` (specific, priority 0) and
    /// `openai_key` (broad, priority 1) — operators would see
    /// `matches = 2` in `RedactionResult.evidence` even though
    /// `redact()` produces one marker. The dedup keeps the
    /// higher-priority span (the more specific provider regex
    /// runs first by ordering of `SECRET_PATTERNS`).
    ///
    /// Overlap definition: two spans `[s1, e1)` and `[s2, e2)`
    /// overlap iff `s1 < e2 AND e1 > s2`. Half-open intervals.
    #[must_use]
    pub fn detect(&self, text: &str) -> Vec<(&'static str, usize, usize)> {
        let mut detections: Vec<(&'static str, usize, usize)> = Vec::new();

        // Iterate in priority order; the earliest pattern in
        // SECRET_PATTERNS has the highest priority. By processing
        // priorities highest-first and skipping overlaps with
        // already-kept spans, the higher-priority match wins.
        for pattern in SECRET_PATTERNS {
            for mat in pattern.regex.find_iter(text) {
                let (start, end) = (mat.start(), mat.end());
                let overlaps = detections.iter().any(|&(_, s, e)| start < e && end > s);
                if !overlaps {
                    detections.push((pattern.name, start, end));
                }
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
        // br-ft-r24qu: pre_len is the LOSSY-DECODED UTF-8 byte
        // length, NOT bytes.len(). For invalid UTF-8 input,
        // lossy expands every invalid sequence into U+FFFD
        // (3 bytes). The resulting `bytes_replaced` is in
        // lossy-decoded units — see BytesRedactionEvidence::bytes_replaced
        // for the operator-interpretation contract.
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

/// Stateful chunk-boundary redactor for streaming persistence paths.
///
/// `Redactor::redact_bytes_with_evidence` scans each byte buffer in isolation.
/// That is fine for read/export surfaces, but cold-tier scrollback receives
/// arbitrary chunks; credentials can straddle two adjacent chunks. This wrapper
/// keeps a bounded tail between calls and only emits bytes that cannot be part
/// of a future cross-boundary match.
#[derive(Debug, Clone)]
pub struct StreamingRedactor {
    redactor: Redactor,
    pending: String,
    tail_bytes: usize,
    /// br-ft-4socw: absolute cap on the pending buffer. Above this,
    /// [`StreamingRedactor::redact_chunk`] forces emission of the
    /// oldest portion to prevent unbounded growth under adversarial
    /// anchor-prefix streams. See
    /// [`DEFAULT_STREAMING_REDACTOR_MAX_PENDING_BYTES`].
    max_pending_bytes: usize,
}

impl StreamingRedactor {
    /// Create a streaming redactor with default markers and overlap window.
    #[must_use]
    pub fn new() -> Self {
        Self::with_redactor(Redactor::new())
    }

    /// Create a streaming redactor that uses an existing redactor config.
    #[must_use]
    pub fn with_redactor(redactor: Redactor) -> Self {
        Self {
            redactor,
            pending: String::new(),
            tail_bytes: DEFAULT_STREAMING_REDACTOR_TAIL_BYTES,
            max_pending_bytes: DEFAULT_STREAMING_REDACTOR_MAX_PENDING_BYTES,
        }
    }

    /// Override the retained tail window. Intended for focused tests.
    #[must_use]
    pub fn with_tail_bytes(mut self, tail_bytes: usize) -> Self {
        self.tail_bytes = tail_bytes;
        self
    }

    /// Override the pending-buffer cap. [ft-4socw]
    ///
    /// When `pending.len()` exceeds this value during
    /// [`Self::redact_chunk`], the streaming redactor forcibly emits
    /// the oldest portion (with the regex redactor applied) to halt
    /// unbounded growth. Each forced emission bumps
    /// [`streaming_redactor_pending_overflow_count`].
    ///
    /// Values below [`MIN_STREAMING_REDACTOR_MAX_PENDING_BYTES`] are clamped
    /// upward so the overflow path always drains at least one byte. When the
    /// cap is lower than `tail_bytes`, the retained-tail scan uses the cap as
    /// its effective tail limit. Intended primarily for focused tests;
    /// production callers should rely on the default constant.
    #[must_use]
    pub fn with_max_pending_bytes(mut self, max_pending_bytes: usize) -> Self {
        self.max_pending_bytes = max_pending_bytes.max(MIN_STREAMING_REDACTOR_MAX_PENDING_BYTES);
        self
    }

    /// Redact one chunk, returning the safely-emittable prefix.
    ///
    /// Call [`Self::finish`] after the last chunk to flush the retained tail.
    ///
    /// # Overflow handling [ft-4socw]
    ///
    /// If the pending buffer grows past `max_pending_bytes` after appending
    /// this chunk, the oldest half is force-emitted with the regex redactor
    /// applied and [`streaming_redactor_pending_overflow_count`] is bumped.
    /// The forced emission catches complete secret patterns but may leak
    /// partial-secret prefixes that straddle the cut boundary. The trade-off
    /// is bounded leakage versus unbounded heap growth (OOM); operators are
    /// expected to monitor the counter and investigate runaway producers.
    #[must_use]
    pub fn redact_chunk(&mut self, bytes: &[u8]) -> RedactionResult {
        let lossy = String::from_utf8_lossy(bytes);
        self.pending.push_str(&lossy);

        // br-ft-4socw: forced emission on overflow. Drains the oldest
        // half of pending so subsequent boundary-safe scanning works
        // on the remaining tail. Merges into the same RedactionResult
        // as the normal-path emission so callers see a single result
        // per chunk regardless of overflow.
        let mut overflow_result: Option<RedactionResult> = None;
        if self.pending.len() > self.max_pending_bytes {
            record_streaming_redactor_pending_overflow();
            let half = self.max_pending_bytes / 2;
            let force_boundary = floor_char_boundary(&self.pending, half);
            overflow_result = Some(self.emit_prefix(force_boundary));
        }

        let boundary = self.stable_emit_boundary();
        let normal = self.emit_prefix(boundary);
        match overflow_result {
            Some(forced) => merge_redaction_results(forced, normal),
            None => normal,
        }
    }

    /// Flush and redact all pending bytes at end-of-stream.
    #[must_use]
    pub fn finish(&mut self) -> RedactionResult {
        let pending = std::mem::take(&mut self.pending);
        self.redact_text_with_evidence(&pending)
    }

    /// Bytes currently retained to protect the next chunk boundary.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    fn stable_emit_boundary(&self) -> usize {
        let mut boundary = self.pending.len();
        let detections = self.redactor.detect(&self.pending);

        loop {
            let mut next_boundary = self.earliest_secret_like_suffix_start(boundary, &detections);
            for (_, start, end) in &detections {
                if *start < next_boundary && next_boundary < *end {
                    next_boundary = next_boundary.min(*start);
                }
                if *end == self.pending.len() {
                    next_boundary = next_boundary.min(*start);
                }
            }

            if next_boundary == boundary {
                return boundary;
            }
            boundary = next_boundary;
        }
    }

    fn earliest_secret_like_suffix_start(
        &self,
        current_boundary: usize,
        detections: &[(&'static str, usize, usize)],
    ) -> usize {
        if current_boundary == 0 || self.pending.is_empty() {
            return current_boundary;
        }

        let scan_start = floor_char_boundary(
            &self.pending,
            current_boundary.saturating_sub(self.effective_tail_bytes()),
        );
        let suffix = &self.pending[scan_start..current_boundary];
        let mut earliest = current_boundary;

        for anchor in STREAMING_SECRET_ANCHORS {
            if let Some(offset) = suffix.rfind(anchor) {
                let candidate = scan_start + offset;
                let covered_by_complete_detection = detections.iter().any(|(_, start, end)| {
                    *start == candidate && candidate < *end && *end < current_boundary
                });
                if !covered_by_complete_detection {
                    earliest = earliest.min(candidate);
                }
            }

            for prefix_len in 1..anchor.len() {
                let prefix = &anchor[..prefix_len];
                if suffix.ends_with(prefix) {
                    earliest = earliest.min(current_boundary - prefix.len());
                }
            }
        }

        floor_char_boundary(&self.pending, earliest)
    }

    fn effective_tail_bytes(&self) -> usize {
        self.tail_bytes.min(self.max_pending_bytes)
    }

    fn emit_prefix(&mut self, boundary: usize) -> RedactionResult {
        if boundary == 0 {
            return RedactionResult {
                bytes: Vec::new(),
                evidence: BytesRedactionEvidence::default(),
            };
        }

        let suffix = self.pending.split_off(boundary);
        let prefix = std::mem::replace(&mut self.pending, suffix);
        self.redact_text_with_evidence(&prefix)
    }

    fn redact_text_with_evidence(&self, text: &str) -> RedactionResult {
        let detections = self.redactor.detect(text);
        let matches = detections.len() as u32;
        // br-ft-r24qu: text here is the LOSSY-DECODED pending
        // buffer (from redact_chunk's `from_utf8_lossy(bytes) →
        // self.pending.push_str(...)` pipeline). pre_len is in
        // lossy-decoded UTF-8 byte units, NOT original-input
        // bytes. The FFFD-substitution inflation propagates from
        // the chunk boundary through to bytes_replaced. See
        // BytesRedactionEvidence::bytes_replaced for the
        // operator-interpretation contract.
        let pre_len = text.len();
        let redacted = self.redactor.redact(text);
        let post_len = redacted.len();
        RedactionResult {
            bytes: redacted.into_bytes(),
            evidence: BytesRedactionEvidence {
                matches,
                bytes_replaced: pre_len.saturating_sub(post_len) as u32,
            },
        }
    }
}

impl Default for StreamingRedactor {
    fn default() -> Self {
        Self::new()
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// br-ft-4socw: combine a forced-emission result with the
/// subsequent normal-path emission so callers see a single
/// [`RedactionResult`] per `redact_chunk` invocation regardless of
/// whether the overflow path fired.
fn merge_redaction_results(first: RedactionResult, mut second: RedactionResult) -> RedactionResult {
    let mut bytes = first.bytes;
    bytes.append(&mut second.bytes);
    RedactionResult {
        bytes,
        evidence: BytesRedactionEvidence {
            matches: first
                .evidence
                .matches
                .saturating_add(second.evidence.matches),
            bytes_replaced: first
                .evidence
                .bytes_replaced
                .saturating_add(second.evidence.bytes_replaced),
        },
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
    ///
    /// br-ft-r24qu: this metric is computed in **lossy-decoded
    /// UTF-8 byte units**, NOT original-input byte units. The
    /// substrate runs `String::from_utf8_lossy(bytes)` first, so
    /// invalid UTF-8 sequences (e.g. `[0xff, 0xfe]`) get
    /// substituted with the Unicode replacement character
    /// `U+FFFD` (3 bytes in UTF-8). For mixed-encoding or binary
    /// input, `pre_len = lossy.len()` is INFLATED proportional
    /// to the number of FFFD substitutions; the resulting
    /// `bytes_replaced` overstates redaction work by the same
    /// margin.
    ///
    /// **Pure-UTF-8 input** (the common terminal-output case):
    /// `lossy.len() == bytes.len()`, so `bytes_replaced` equals
    /// the exact original-byte savings. Operators ingesting
    /// well-formed UTF-8 streams can read this number at face
    /// value.
    ///
    /// **Mixed/binary input** (rare; occurs when the pane emits
    /// raw bytes mixed with UTF-8 text): operators must scale by
    /// the FFFD-substitution rate to recover original-byte
    /// semantics. The substrate doesn't track that rate today;
    /// the bead's option-B follow-up adds an `original_bytes:
    /// u32` field for direct exposure.
    ///
    /// **Why this is unfixed in the runtime**: Option B (track
    /// `original_bytes` separately) requires a `BytesRedactionEvidence`
    /// schema bump + cold-tier integration update;
    /// option C (compare against `bytes.len()` instead of
    /// `lossy.len()`) changes the semantics for binary inputs in
    /// a direction-correct but value-different way. Both are
    /// larger than this docstring fix; deferred per the bead's
    /// recommendation. See `Redactor::redact_bytes_with_evidence`
    /// at redactor.rs:660 + `StreamingRedactor::redact_text_with_evidence`
    /// at ~868 for the call sites.
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
    use proptest::prelude::*;

    struct HyphenatedKeyStreamingCase {
        key: &'static str,
        value: &'static str,
    }

    const HYPHENATED_KEY_STREAMING_CASES: &[HyphenatedKeyStreamingCase] = &[
        HyphenatedKeyStreamingCase {
            key: "api-key",
            value: "abcdEFGH/QRST+UVWX=YZ1234567890",
        },
        HyphenatedKeyStreamingCase {
            key: "device-code",
            value: "DEV-ABC123",
        },
        HyphenatedKeyStreamingCase {
            key: "user-code",
            value: "USER-XYZ789",
        },
    ];

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
        assert!(
            !std::str::from_utf8(&result.bytes)
                .unwrap()
                .contains("glpat-")
        );
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
        assert!(
            !std::str::from_utf8(&result.bytes)
                .unwrap()
                .contains("glpat-")
        );
    }

    #[test]
    fn redact_bytes_with_evidence_evidence_made_changes_predicate() {
        let zero = BytesRedactionEvidence {
            matches: 0,
            bytes_replaced: 0,
        };
        let some = BytesRedactionEvidence {
            matches: 3,
            bytes_replaced: 100,
        };
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

    #[test]
    fn streaming_redactor_catches_secret_split_at_every_offset() {
        let key = "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRs";
        let input = format!("before:{key}:after");
        let expected = Redactor::new().redact(&input).into_bytes();

        for split in 1..input.len() {
            let mut streaming = StreamingRedactor::new();
            let mut out = Vec::new();
            out.extend(streaming.redact_chunk(&input.as_bytes()[..split]).bytes);
            out.extend(streaming.redact_chunk(&input.as_bytes()[split..]).bytes);
            let finish = streaming.finish();
            assert!(finish.evidence.made_changes(), "split={split}");
            out.extend(finish.bytes);

            assert_eq!(out, expected, "split={split}");
            let rendered = String::from_utf8(out).unwrap();
            assert!(!rendered.contains("sk-ant-api03-"), "split={split}");
        }
    }

    proptest! {
        #[test]
        fn streaming_redactor_detects_hyphenated_keyed_prefixes(
            case_idx in 0usize..HYPHENATED_KEY_STREAMING_CASES.len(),
            raw_split in 1usize..256,
        ) {
            let case = &HYPHENATED_KEY_STREAMING_CASES[case_idx];
            let input = format!("before {}={} after", case.key, case.value);
            let split = 1 + raw_split % (input.len() - 1);
            let expected = Redactor::new().redact(&input).into_bytes();

            let mut streaming = StreamingRedactor::new();
            let mut out = Vec::new();
            out.extend(streaming.redact_chunk(&input.as_bytes()[..split]).bytes);
            out.extend(streaming.redact_chunk(&input.as_bytes()[split..]).bytes);
            out.extend(streaming.finish().bytes);

            prop_assert_eq!(&out, &expected, "case={} split={}", case.key, split);
            let rendered = String::from_utf8(out).expect("redactor output must stay UTF-8");
            prop_assert!(
                !rendered.contains(case.value),
                "case={} split={} leaked value in {:?}",
                case.key,
                split,
                rendered,
            );
        }
    }

    // br-ft-hkif4: detect() overlap dedup tests
    #[test]
    fn detect_anthropic_token_counts_once_not_twice() {
        // The Anthropic token shape `sk-ant-api03-...` matches
        // both the specific `anthropic_key` regex AND the broader
        // `openai_key` regex (whose body charset accepts `ant-`).
        // Pre-fix detect() would push BOTH matches; post-fix the
        // priority-ordered dedup keeps only `anthropic_key`.
        let token = "sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let r = Redactor::new();
        let detections = r.detect(token);
        assert_eq!(
            detections.len(),
            1,
            "single Anthropic token must produce exactly one detection; got {detections:?}"
        );
        assert_eq!(detections[0].0, "anthropic_key");
    }

    #[test]
    fn detect_redact_evidence_match_count_consistency() {
        // The bead's tamper-evidence concern: evidence.matches
        // should equal the number of distinct secrets in the
        // input, not the cumulative cross-pattern hit count.
        let token = "sk-ant-api03-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let result = Redactor::new().redact_bytes_with_evidence(token.as_bytes());
        assert_eq!(
            result.evidence.matches, 1,
            "evidence.matches must equal distinct secret count, got {}",
            result.evidence.matches
        );
    }

    #[test]
    fn detect_disjoint_secrets_count_independently() {
        // Two distinct, non-overlapping secrets MUST count as 2.
        // Pinning that the dedup doesn't over-collapse.
        let input = "first sk-ant-api03-cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc and second SG.aaaaaaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb done";
        let detections = Redactor::new().detect(input);
        assert_eq!(
            detections.len(),
            2,
            "disjoint secrets must count independently; got {detections:?}"
        );
        // Anthropic comes first by start offset.
        assert_eq!(detections[0].0, "anthropic_key");
        assert_eq!(detections[1].0, "sendgrid_key");
    }

    proptest! {
        /// br-ft-hkif4: priority-preservation property.
        ///
        /// Construct an input with one Anthropic token in the
        /// middle of arbitrary noise. The number of detections
        /// must always be exactly 1 (the single token) AND the
        /// detected pattern name must be `anthropic_key`
        /// (the higher-priority pattern), regardless of where
        /// the token sits or what surrounds it.
        #[test]
        fn detect_anthropic_token_in_arbitrary_noise_counts_once(
            prefix in "[a-z ]{0,40}",
            suffix in "[a-z ]{0,40}",
            // 80 hex-ish chars after `sk-ant-api03-`
            tail in "[a-zA-Z0-9_-]{80,90}",
        ) {
            let input = format!("{prefix}sk-ant-api03-{tail}{suffix}");
            let detections = Redactor::new().detect(&input);
            prop_assert_eq!(
                detections.len(),
                1,
                "single Anthropic token must produce exactly one detection; got {:?}",
                detections
            );
            prop_assert_eq!(
                detections[0].0,
                "anthropic_key",
                "higher-priority pattern must win the overlap; got {:?}",
                detections
            );
        }
    }

    #[test]
    fn streaming_redactor_emits_prefix_and_keeps_bounded_tail() {
        let mut streaming = StreamingRedactor::new().with_tail_bytes(8);
        let first = streaming.redact_chunk(b"plain text with no secret");
        assert!(!first.bytes.is_empty());
        assert!(streaming.pending_bytes() <= 8);

        let mut out = first.bytes;
        out.extend(streaming.finish().bytes);
        assert_eq!(out, b"plain text with no secret");
    }

    // ─── br-ft-4socw: pending overflow guard + observability ─────────
    //
    // Pre-fix StreamingRedactor.pending grew without bound under
    // adversarial anchor-prefix streams. A producer that streams
    // repeated `"rk_..."` (or any STREAMING_SECRET_ANCHORS entry)
    // pushed the safe-emit boundary backwards on every chunk so
    // pending accumulated indefinitely.
    //
    // Post-fix: max_pending_bytes cap forces emission of the oldest
    // half when pending exceeds it. Bumps the
    // streaming_redactor_pending_overflow_count counter so operators
    // can detect runaway producers.
    //
    // Counter is process-wide; tests serialize via a Mutex guard.

    fn streaming_overflow_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn streaming_redactor_pending_overflow_counter_zero_for_normal_traffic() {
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        let mut streaming = StreamingRedactor::new();
        // Plain text well under the 8 MiB default cap.
        for i in 0..100 {
            let chunk = format!("plain log line {i}\n");
            let _ = streaming.redact_chunk(chunk.as_bytes());
        }
        let _ = streaming.finish();

        assert_eq!(
            super::streaming_redactor_pending_overflow_count(),
            0,
            "br-ft-4socw: normal traffic must NOT trigger forced emission"
        );
    }

    #[test]
    fn streaming_redactor_pending_overflow_counter_increments_under_runaway_anchor_stream() {
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        // Drop the cap to a small value so we can exercise the path
        // without allocating real-world MiB. tail_bytes >= cap would
        // be inconsistent; keep tail_bytes well below the cap.
        const TEST_CAP: usize = 1024;
        let mut streaming = StreamingRedactor::new()
            .with_tail_bytes(64)
            .with_max_pending_bytes(TEST_CAP);

        // Adversarial pattern: "rk_AAAAAAAA" repeated. Each chunk
        // contains anchor occurrences that previously kept the
        // boundary stuck at the earliest anchor position, growing
        // pending without bound.
        let runaway = "rk_AAAAAAAA".repeat(64); // 11 bytes × 64 = 704 bytes per chunk.
        for _ in 0..50 {
            let _ = streaming.redact_chunk(runaway.as_bytes());
            assert!(
                streaming.pending_bytes() <= TEST_CAP * 2,
                "br-ft-4socw: pending must stay bounded; got {} > {}",
                streaming.pending_bytes(),
                TEST_CAP * 2
            );
        }

        let count = super::streaming_redactor_pending_overflow_count();
        assert!(
            count > 0,
            "br-ft-4socw: runaway anchor stream must trigger forced \
             emission at least once; got count={count}"
        );

        // Drain pending so subsequent tests start fresh.
        let _ = streaming.finish();
    }

    #[test]
    fn streaming_redactor_degenerate_pending_caps_still_force_progress() {
        let _guard = streaming_overflow_test_lock();

        for requested_cap in [0, 1, 2] {
            super::reset_streaming_redactor_pending_overflow_count_for_test();

            let mut streaming = StreamingRedactor::new()
                .with_tail_bytes(64)
                .with_max_pending_bytes(requested_cap);

            let result = streaming.redact_chunk(b"rk_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");

            assert_eq!(
                super::streaming_redactor_pending_overflow_count(),
                1,
                "br-ft-r4nwe: cap={requested_cap} must still trip overflow telemetry"
            );
            assert!(
                !result.bytes.is_empty()
                    || streaming.pending_bytes() <= super::MIN_STREAMING_REDACTOR_MAX_PENDING_BYTES,
                "br-ft-r4nwe: cap={requested_cap} must force progress instead of counting a zero-byte drain"
            );
            assert!(
                streaming.pending_bytes() <= super::MIN_STREAMING_REDACTOR_MAX_PENDING_BYTES,
                "br-ft-r4nwe: cap={requested_cap} left pending above normalized minimum: {}",
                streaming.pending_bytes()
            );
        }
    }

    #[test]
    fn streaming_redactor_pending_overflow_does_not_drop_secrets_within_emitted_window() {
        // br-ft-4socw: forced emission applies the redactor regex
        // BEFORE emitting. Complete secret patterns within the
        // forced-emit window must still be redacted; the trade-off
        // documented in the bead is that PARTIAL prefixes straddling
        // the cut boundary may leak (acceptable for a hardening
        // counter; OOM is the worse alternative).
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        const TEST_CAP: usize = 2048;
        let mut streaming = StreamingRedactor::new()
            .with_tail_bytes(64)
            .with_max_pending_bytes(TEST_CAP);

        // Force overflow first.
        let filler = "x".repeat(2000);
        let r1 = streaming.redact_chunk(filler.as_bytes());
        // A clearly-complete secret embedded inside the next chunk
        // that pushes us past the cap. The complete pattern should
        // be redacted regardless of overflow path.
        let chunk_with_secret = format!(
            "more filler {} sk-1234567890abcdef1234567890abcdef and tail {}",
            "y".repeat(800),
            "z".repeat(800)
        );
        let r2 = streaming.redact_chunk(chunk_with_secret.as_bytes());
        let r3 = streaming.finish();

        let mut all = r1.bytes;
        all.extend(r2.bytes);
        all.extend(r3.bytes);
        let rendered = String::from_utf8_lossy(&all);

        // The complete sk-... pattern must NOT survive in cleartext.
        assert!(
            !rendered.contains("sk-1234567890abcdef1234567890abcdef"),
            "br-ft-4socw: complete secret patterns within the \
             forced-emit window must still be redacted"
        );
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
        let input = "SENDGRID_API_KEY=SG.AbCdEfGhIjKlMnOpQrStUv.WxYz0123456789abcdefghijklmnopqrstuvwxyzABCD";
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

    // ========================================================================
    // br-ft-76zp6: Stripe key coverage — sk_/pk_/rk_/whsec_ formats.
    // ========================================================================

    #[test]
    fn redact_stripe_sk_live_secret_key() {
        // Regression: pre-fix behaviour for `sk_live_*` must not change.
        let r = redactor_with_named_markers();
        let raw = "sk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
        let out = r.redact(&format!("STRIPE_KEY={raw}"));
        assert!(!out.contains("sk_live_a"), "stripe sk_live leaked: {out:?}");
        assert!(out.contains("[REDACTED:stripe_key]"), "{out:?}");
    }

    #[test]
    fn redact_stripe_pk_test_publishable_key() {
        let r = redactor_with_named_markers();
        let raw = "pk_test_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
        let out = r.redact(&format!("publishable: {raw}"));
        assert!(!out.contains("pk_test_a"), "stripe pk_test leaked: {out:?}");
        assert!(out.contains("[REDACTED:stripe_key]"), "{out:?}");
    }

    #[test]
    fn redact_stripe_rk_live_restricted_key() {
        // br-ft-76zp6: pre-fix the `[ps]k_` class missed the `r`
        // arm; restricted keys (full API credentials with scoped
        // permissions per Stripe docs) leaked verbatim into logs.
        let r = redactor_with_named_markers();
        let raw = "rk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
        let out = r.redact(&format!("STRIPE_RESTRICTED_KEY={raw}"));
        assert!(
            !out.contains("rk_live_a"),
            "ft-76zp6: stripe restricted key leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:stripe_key]"), "{out:?}");
    }

    #[test]
    fn redact_stripe_rk_test_restricted_key() {
        let r = redactor_with_named_markers();
        let raw = "rk_test_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
        let out = r.redact(&format!("test_key: {raw}"));
        assert!(
            !out.contains("rk_test_a"),
            "ft-76zp6: stripe rk_test leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:stripe_key]"), "{out:?}");
    }

    #[test]
    fn redact_stripe_whsec_webhook_signing_secret() {
        // br-ft-76zp6: whsec_* is the HMAC anchor for webhook
        // payload-integrity verification. An exposed webhook
        // secret lets an attacker forge events to the merchant's
        // endpoint (e.g., fake `payment_intent.succeeded`).
        let r = redactor_with_named_markers();
        let raw = "whsec_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
        let out = r.redact(&format!("STRIPE_WEBHOOK_SECRET={raw}"));
        assert!(
            !out.contains("whsec_a"),
            "ft-76zp6: stripe webhook secret leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:stripe_key]"), "{out:?}");
    }

    #[test]
    fn redact_stripe_does_not_match_embedded_substrings() {
        // Anchoring on `\b` keeps the redactor from grabbing strings
        // that merely *contain* `whsec_<long alpha-num>` or
        // `pk_live_<...>` inside a longer word. Pre-fix narrative
        // text or identifier-shaped tokens like
        // `something_whsec_aBcDe...` would have been redacted as if
        // they were a real Stripe credential.
        let r = redactor_with_named_markers();
        let probes = [
            // Underscore-prefixed false positives.
            "something_whsec_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
            "task_pk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
            "wf_rk_test_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
            // Letter-prefixed false positives.
            "xwhsec_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
            "ypk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
        ];
        for probe in probes {
            let out = r.redact(probe);
            assert_eq!(
                out, probe,
                "embedded substring `{probe}` must not redact: got {out:?}"
            );
            assert!(
                !out.contains("[REDACTED:stripe_key]"),
                "embedded substring `{probe}` must not trigger stripe redaction: {out:?}"
            );
        }
    }

    #[test]
    fn redact_stripe_still_matches_at_real_token_boundaries() {
        // The flip side: every token-boundary form a real config
        // would emit must still redact. This pins the new `\b`
        // anchor against accidentally narrowing the legitimate
        // match surface.
        let r = redactor_with_named_markers();
        let suffix = "aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
        let cases = [
            // Start-of-string.
            format!("whsec_{suffix}"),
            format!("sk_live_{suffix}"),
            // After whitespace.
            format!("Stripe webhook secret: whsec_{suffix}"),
            // After equals (env-style).
            format!("STRIPE_API_KEY=sk_live_{suffix}"),
            // After punctuation.
            format!("\"key\":\"rk_live_{suffix}\""),
            // After comma in a list.
            format!("keys=[whsec_{suffix},pk_test_{suffix}]"),
        ];
        for raw in &cases {
            let out = r.redact(raw);
            assert!(
                out.contains("[REDACTED:stripe_key]"),
                "token-boundary form `{raw}` must redact: got {out:?}"
            );
            assert!(
                !out.contains("aBcDeFgHi"),
                "token-boundary form `{raw}` leaked the secret body: {out:?}"
            );
        }
    }

    #[test]
    fn redact_stripe_contains_secrets_predicate_covers_new_formats() {
        // The `contains_secrets` fast path must surface the new
        // formats — callers that gate on it (e.g., audit-write
        // pre-checks) would otherwise route the credential into
        // an unredacted persistence path.
        let r = Redactor::new();
        for raw in [
            "sk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
            "rk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
            "rk_test_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
            "whsec_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
        ] {
            assert!(
                r.contains_secrets(raw),
                "ft-76zp6: contains_secrets missed `{raw}`"
            );
        }
    }

    #[test]
    fn detect_reports_stripe_key_for_each_format() {
        let r = Redactor::new();
        let blob = concat!(
            "sk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890 ",
            "rk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890 ",
            "rk_test_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890 ",
            "whsec_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890 ",
        );
        let detected = r.detect(blob);
        let stripe_count = detected
            .iter()
            .filter(|(name, _, _)| *name == "stripe_key")
            .count();
        assert_eq!(
            stripe_count, 4,
            "ft-76zp6: detect() should report stripe_key once per format; got {detected:?}"
        );
    }

    // ========================================================================
    // br-ft-2xkrc: SSH/PEM private-key block coverage.
    // ========================================================================

    /// Build a PEM-shaped block with a synthetic body. Body length
    /// is intentionally short — the regex's body match is reluctant
    /// `[\s\S]+?` so any non-empty body between BEGIN/END suffices.
    fn pem_block(label: &str) -> String {
        format!(
            "-----BEGIN {label} PRIVATE KEY-----\n\
             MIIEpAIBAAKCAQEAaBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890\n\
             EXAMPLE_BODY_NOT_A_REAL_KEY_aBcDeFgHiJkLmNoPqRsTuVwX\n\
             -----END {label} PRIVATE KEY-----"
        )
    }

    #[test]
    fn redact_ssh_rsa_private_key_block() {
        let r = redactor_with_named_markers();
        let block = pem_block("RSA");
        let out = r.redact(&format!("paste:\n{block}\n(end)"));
        assert!(
            !out.contains("EXAMPLE_BODY"),
            "ft-2xkrc: RSA private-key body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:ssh_private_key]"), "{out:?}");
    }

    #[test]
    fn redact_ssh_ec_private_key_block() {
        let r = redactor_with_named_markers();
        let block = pem_block("EC");
        let out = r.redact(&block);
        assert!(
            !out.contains("EXAMPLE_BODY"),
            "ft-2xkrc: EC key body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:ssh_private_key]"), "{out:?}");
    }

    #[test]
    fn redact_ssh_dsa_private_key_block() {
        let r = redactor_with_named_markers();
        let block = pem_block("DSA");
        let out = r.redact(&block);
        assert!(
            !out.contains("EXAMPLE_BODY"),
            "ft-2xkrc: DSA key body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:ssh_private_key]"), "{out:?}");
    }

    #[test]
    fn redact_openssh_new_format_private_key_block() {
        let r = redactor_with_named_markers();
        let block = pem_block("OPENSSH");
        let out = r.redact(&block);
        assert!(
            !out.contains("EXAMPLE_BODY"),
            "ft-2xkrc: OpenSSH key body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:ssh_private_key]"), "{out:?}");
    }

    #[test]
    fn redact_pkcs8_unencrypted_private_key_block() {
        let r = redactor_with_named_markers();
        // PKCS#8 unencrypted: `BEGIN PRIVATE KEY` (no algorithm prefix).
        let block = "-----BEGIN PRIVATE KEY-----\n\
                     MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDBODY_NOT_REAL\n\
                     -----END PRIVATE KEY-----";
        let out = r.redact(block);
        assert!(
            !out.contains("BODY_NOT_REAL"),
            "ft-2xkrc: PKCS#8 key body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:ssh_private_key]"), "{out:?}");
    }

    #[test]
    fn redact_pkcs8_encrypted_private_key_block() {
        let r = redactor_with_named_markers();
        // Encrypted PKCS#8 still scrubs — leaked passphrase or weak
        // passphrase + leaked blob = compromised key.
        let block = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
                     MIIE6TAbBgkqhkiG9w0BBQMwDgQI_BODY_NOT_REAL_ENC\n\
                     -----END ENCRYPTED PRIVATE KEY-----";
        let out = r.redact(block);
        assert!(
            !out.contains("BODY_NOT_REAL_ENC"),
            "ft-2xkrc: encrypted PKCS#8 key body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:ssh_private_key]"), "{out:?}");
    }

    #[test]
    fn redact_two_adjacent_pem_blocks_redact_independently() {
        // Reluctant body quantifier (`[\s\S]+?`) prevents two adjacent
        // PEM blocks from collapsing into one match. Each block must
        // produce its own [REDACTED] marker.
        let r = redactor_with_named_markers();
        let two = format!("first:\n{}\nsecond:\n{}", pem_block("RSA"), pem_block("EC"));
        let out = r.redact(&two);
        let count = out.matches("[REDACTED:ssh_private_key]").count();
        assert_eq!(
            count, 2,
            "ft-2xkrc: expected 2 independent redactions; got {count} in {out:?}"
        );
    }

    #[test]
    fn ssh_private_key_contains_secrets_predicate() {
        let r = Redactor::new();
        for label in ["RSA", "DSA", "EC", "OPENSSH"] {
            let block = pem_block(label);
            assert!(
                r.contains_secrets(&block),
                "ft-2xkrc: contains_secrets missed `{label}` PEM block"
            );
        }
        let pkcs8 = "-----BEGIN PRIVATE KEY-----\nbody\n-----END PRIVATE KEY-----";
        assert!(r.contains_secrets(pkcs8));
        let enc =
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nbody\n-----END ENCRYPTED PRIVATE KEY-----";
        assert!(r.contains_secrets(enc));
    }

    #[test]
    fn redact_ed25519_private_key_block() {
        // br-ft-2xkrc: ED25519 has a digit-bearing algo prefix; the
        // pre-fix `[A-Z ]+` regex would have missed it. Post-fix
        // `[A-Z0-9 ]+` covers it.
        let r = redactor_with_named_markers();
        let block = "-----BEGIN ED25519 PRIVATE KEY-----\n\
                     EXAMPLE_BODY_NOT_REAL_aBcDeFgHiJkLmNoPqRsTuVwXyZ\n\
                     -----END ED25519 PRIVATE KEY-----";
        let out = r.redact(block);
        assert!(
            !out.contains("EXAMPLE_BODY"),
            "ft-2xkrc: ED25519 key body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:ssh_private_key]"), "{out:?}");
    }

    // ========================================================================
    // br-ft-2xkrc: PGP / OpenPGP armoured-block coverage.
    // ========================================================================

    #[test]
    fn redact_pgp_private_key_block() {
        let r = redactor_with_named_markers();
        let block = "-----BEGIN PGP PRIVATE KEY BLOCK-----\n\
                     \n\
                     lQOYBEXAMPLE_PGP_PRIV_BODY_aBcDeFgHiJkLmNoPqRsTuVwX\n\
                     -----END PGP PRIVATE KEY BLOCK-----";
        let out = r.redact(block);
        assert!(
            !out.contains("EXAMPLE_PGP_PRIV_BODY"),
            "ft-2xkrc: PGP private body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:pgp_block]"), "{out:?}");
    }

    #[test]
    fn redact_pgp_public_key_block() {
        // Public keys are scrubbed too: in some operator workflows
        // the key-ID itself is sensitive (links the agent identity),
        // and the catalog errs on the side of redacting any PGP
        // armoured shape rather than guessing intent.
        let r = redactor_with_named_markers();
        let block = "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\
                     \n\
                     mQENBEXAMPLE_PGP_PUB_BODY_aBcDeFgHiJkLmNoPqRsTuVwX\n\
                     -----END PGP PUBLIC KEY BLOCK-----";
        let out = r.redact(block);
        assert!(
            !out.contains("EXAMPLE_PGP_PUB_BODY"),
            "ft-2xkrc: PGP public body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:pgp_block]"), "{out:?}");
    }

    #[test]
    fn redact_pgp_encrypted_message() {
        let r = redactor_with_named_markers();
        let block = "-----BEGIN PGP MESSAGE-----\n\
                     \n\
                     hQEMA0EXAMPLE_PGP_ENC_BODY_aBcDeFgHiJkLmNoPqRsTuVwX\n\
                     -----END PGP MESSAGE-----";
        let out = r.redact(block);
        assert!(
            !out.contains("EXAMPLE_PGP_ENC_BODY"),
            "ft-2xkrc: PGP encrypted body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:pgp_block]"), "{out:?}");
    }

    #[test]
    fn redact_pgp_signed_message_with_asymmetric_end_marker() {
        // PGP signed messages are special: BEGIN PGP SIGNED MESSAGE
        // opens, but END PGP SIGNATURE closes (the trailing
        // signature block has its own END). The catalog regex
        // accepts either END marker; a brittle "matching BEGIN/END"
        // pattern would have missed this entirely.
        let r = redactor_with_named_markers();
        let block = "-----BEGIN PGP SIGNED MESSAGE-----\n\
                     Hash: SHA256\n\
                     \n\
                     plaintext payload\n\
                     -----BEGIN PGP SIGNATURE-----\n\
                     \n\
                     iQEzBAEBCAAdFEXAMPLE_PGP_SIG_BODY_aBcDeFgHiJkLmNoPq\n\
                     -----END PGP SIGNATURE-----";
        let out = r.redact(block);
        assert!(
            !out.contains("EXAMPLE_PGP_SIG_BODY"),
            "ft-2xkrc: PGP signature body leaked: {out:?}"
        );
        assert!(out.contains("[REDACTED:pgp_block]"), "{out:?}");
    }

    #[test]
    fn pgp_block_contains_secrets_predicate() {
        let r = Redactor::new();
        let cases = [
            "-----BEGIN PGP PRIVATE KEY BLOCK-----\nbody\n-----END PGP PRIVATE KEY BLOCK-----",
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\nbody\n-----END PGP PUBLIC KEY BLOCK-----",
            "-----BEGIN PGP MESSAGE-----\nbody\n-----END PGP MESSAGE-----",
            "-----BEGIN PGP SIGNED MESSAGE-----\ntext\n-----BEGIN PGP SIGNATURE-----\nsig\n-----END PGP SIGNATURE-----",
        ];
        for case in cases {
            assert!(
                r.contains_secrets(case),
                "ft-2xkrc: contains_secrets missed PGP block: {case:?}"
            );
        }
    }
}
