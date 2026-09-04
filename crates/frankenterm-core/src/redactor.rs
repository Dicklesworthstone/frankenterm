//! Secret redaction for read, export, and audit surfaces.

use aho_corasick::AhoCorasick;
use regex::{Regex, RegexSet};
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
/// The overflow path targets `max_pending_bytes / 2`; caps below 2 make
/// progress depend on a zero-byte target. The forced boundary may advance to
/// the first UTF-8 scalar when flooring would otherwise drain nothing.
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
    // Collapsed key-name: AI_PROVIDER_KEYED_VALUE accepts `azure[_-]?openai`
    // with no separator (`azureopenai_key=`, `azureopenaikey=`), but the
    // separated anchors above never match it, so a value split across a chunk
    // boundary leaked. Anchor scan is case-insensitive. (ft-b1p6x)
    "azureopenai",
    "AKIA",
    "aws_secret_access_key",
    "Authorization",
    "authorization",
    "Bearer ",
    "bearer ",
    "https://",
    "http://",
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
    // Collapsed key-names: DEVICE_CODE accepts `device[_-]?code` /
    // `user[_-]?code` with no separator (`devicecode=`, `usercode=`), which the
    // separated anchors above never match — same streaming leak class. (ft-b1p6x)
    "devicecode",
    "usercode",
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
    // br-ft-zbnz4: `datadog_api_key` matches `(?:DD|DATADOG)_API_KEY`, so the
    // long key-name form must also anchor streaming retention. Without it a
    // `DATADOG_API_KEY=<32 hex>` assignment split across a chunk boundary
    // produced no anchor hit, the prefix was emitted early, and the split hex
    // value leaked unredacted. Short `DD_API_KEY` form was already covered.
    "DATADOG_API_KEY",
];

/// Longest [`STREAMING_SECRET_ANCHORS`] entry, computed at compile time so the
/// tail-window floor below tracks the anchor set automatically.
const fn longest_streaming_anchor_len() -> usize {
    let mut max = 0usize;
    let mut i = 0;
    while i < STREAMING_SECRET_ANCHORS.len() {
        let len = STREAMING_SECRET_ANCHORS[i].len();
        if len > max {
            max = len;
        }
        i += 1;
    }
    max
}

/// Minimum open-anchor scan window the streaming redactor must keep.
///
/// A keyed secret (`key=value`) is only retained across a chunk boundary while
/// the scan window in [`StreamingRedactor::earliest_secret_like_suffix_start`]
/// still reaches back to the `key=` anchor. Once a partial value pushes the
/// anchor out of that window the prefix is emitted early and the value leaks.
/// The window must therefore always cover the longest anchor plus the largest
/// partial value that cannot yet form a standalone detection — `GENERIC_TOKEN`
/// requires ≥16 chars, so a 15-char value prefix is the worst-case undetected
/// remainder. Flooring the effective tail to this value makes the test-only
/// [`StreamingRedactor::with_tail_bytes`] knob safe to tune below it without
/// re-opening that cross-boundary leak.
const STREAMING_ANCHOR_TAIL_FLOOR: usize = longest_streaming_anchor_len() + 16;

/// Lookup table of bytes that begin some [`STREAMING_SECRET_ANCHORS`] entry,
/// in either ASCII case (ft-aznq6).
///
/// The partial-anchor retention rule fires on a single trailing byte whenever
/// that byte is an anchor's first character, which is by far the common case.
/// A table turns that test into one load instead of a rescan of the whole tail
/// window, which is what made the backwards walk quadratic.
const fn anchor_initial_byte_table() -> [bool; 256] {
    let mut table = [false; 256];
    let mut i = 0;
    while i < STREAMING_SECRET_ANCHORS.len() {
        let bytes = STREAMING_SECRET_ANCHORS[i].as_bytes();
        if !bytes.is_empty() {
            table[bytes[0].to_ascii_lowercase() as usize] = true;
            table[bytes[0].to_ascii_uppercase() as usize] = true;
        }
        i += 1;
    }
    table
}

static ANCHOR_INITIAL_BYTES: [bool; 256] = anchor_initial_byte_table();

/// Case-insensitive multi-pattern automaton over [`STREAMING_SECRET_ANCHORS`]
/// (ft-aznq6).
///
/// The anchor-occurrence rule needs every occurrence in the pending buffer, and
/// it needs them repeatedly as the emit boundary walks left. Finding them with
/// one `rfind` per anchor per boundary re-scanned the whole tail window ~78
/// times per step; one automaton pass collects them all in a single scan.
static STREAMING_ANCHOR_AUTOMATON: LazyLock<AhoCorasick> = LazyLock::new(|| {
    AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(STREAMING_SECRET_ANCHORS)
        .expect("streaming anchor automaton")
});

/// Anchors that open a pattern with no bounded span (ft-5lz32).
///
/// Every other catalog pattern becomes undetectable within a bounded amount of
/// text — a fixed-shape token dies at the first byte outside its charset, and a
/// keyed `key=value` match is already complete once the value reaches its
/// minimum length. The armoured blocks are different: `SSH_PRIVATE_KEY` and the
/// PGP patterns only match once the closing `-----END …-----` line arrives, so
/// an armoured block is undetectable for as long as it is unterminated. Those
/// occurrences are exempt from the retention floor; everything else is not.
const UNBOUNDED_SPAN_ANCHORS: &[&str] = &["-----BEGIN "];

fn anchor_span_is_unbounded(anchor: &str) -> bool {
    UNBOUNDED_SPAN_ANCHORS.contains(&anchor)
}

/// A [`STREAMING_SECRET_ANCHORS`] occurrence found in the pending buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AnchorOccurrence {
    /// Byte offset of the first anchor byte within `pending`.
    start: usize,
    /// Byte length of the matched anchor.
    len: usize,
    /// Whether the pattern this anchor opens can span unbounded text
    /// (see [`UNBOUNDED_SPAN_ANCHORS`]).
    unbounded_span: bool,
}

/// Every [`STREAMING_SECRET_ANCHORS`] occurrence in `pending`, ascending by
/// start (ft-aznq6).
///
/// Overlapping matches are all reported: `sk-` and `sk-ant-` both start at the
/// same byte, and a missed occurrence would let the emit boundary advance past
/// a partial secret.
fn streaming_anchor_occurrences(pending: &str) -> Vec<AnchorOccurrence> {
    let mut occurrences: Vec<AnchorOccurrence> = STREAMING_ANCHOR_AUTOMATON
        .find_overlapping_iter(pending)
        .map(|found| AnchorOccurrence {
            start: found.start(),
            len: found.end() - found.start(),
            unbounded_span: anchor_span_is_unbounded(
                STREAMING_SECRET_ANCHORS[found.pattern().as_usize()],
            ),
        })
        .collect();
    occurrences.sort_unstable();
    occurrences
}

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
        // `databricks` (not `databricks[_-]?token`): the shared trailing
        // `(?:key|token|secret)` suffix must terminate the provider name, exactly
        // as it does for `cohere`/`mistral`. The old `databricks[_-]?token`
        // alternative consumed the literal `databricks_token`, leaving the
        // mandatory suffix to face the `=`/`:` separator and fail -- so the real
        // `databricks_token=`, `DATABRICKS_TOKEN=`, and `databricks_api_key=`
        // forms were never matched and the ai_provider_keyed coverage was dead
        // for this provider (ft-sydcu).
        r#"(?i)(?:cohere|mistral|together(?:_ai)?|fireworks|deepinfra|nvidia[_-]?api|databricks|azure[_-]?openai)[_-]?(?:api[_-]?)?(?:key|token|secret)\s*[=:]\s*['"]?([a-zA-Z0-9_/+=.-]{16,})['"]?"#
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
///
/// The value charset carries base64's `/`, `+`, and `=` for the same reason
/// the generic key/token/secret patterns below do (see the ft-5o6u5 note):
/// without them the match stops at the first `/` or `+` and the remainder of
/// the credential is emitted in cleartext. `Bearer AbC+dEf…` previously did
/// not match at all (only 3 chars precede the `+`, so `{20,}` could never be
/// satisfied), leaking the whole token.
static BEARER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:authorization["']?\s*[:=]\s*["']?bearer\s+|bearer\s+)[a-zA-Z0-9._/+=-]{20,}"#,
    )
    .expect("Bearer token regex")
});

/// HTTP Basic credentials in Authorization headers.
///
/// `Authorization` was already a streaming anchor, but the only header regex
/// was [`BEARER_TOKEN`], which requires the literal `bearer`. A routine
/// `curl -v` transcript containing
/// `Authorization: Basic dXNlcjpzdXBlcnNlY3JldA==` therefore flowed through
/// completely unredacted — and base64-decodes straight back to `user:password`.
///
/// The `authorization` prefix is mandatory (unlike the bare-`bearer` arm
/// above) because `basic` is an ordinary English word: an optional-prefix
/// form would redact prose like `basic troubleshooting`.
static HTTP_BASIC_AUTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)authorization["']?\s*[:=]\s*["']?basic\s+[A-Za-z0-9+/]{8,}={0,2}"#)
        .expect("HTTP basic auth regex")
});

// ft-5o6u5: generic key/token/secret value charsets must accept base64
// padding/alphabet (`/`, `+`, `=`) in addition to alnum/underscore/dash.
// Many OAuth client_secret and base64-encoded values contain those bytes;
// without them in the charset the regex stops at the first `/` or `+` and
// the trailing secret bytes leak unredacted through robot/MCP/audit
// surfaces. The charset still excludes whitespace and quote characters so
// the match terminates at the value boundary.

// Two shape fixes apply to all four generic keyed patterns below.
//
// 1. A closing quote may sit between the key name and the delimiter. Every
//    one of these regexes went straight from the keyword to `\s*[=:]`, so a
//    JSON body — `{"api_key": "…"}`, `{"token": "…"}` — never matched, while
//    the bare `api_key=…` form always did. JSON is the dominant shape in a
//    pane (curl output, API responses, config dumps), so this was the widest
//    of the gaps. The value side already tolerated `['"]?`; the key side now
//    does too.
//
// 2. The keyword guard on `token`/`secret` has two arms. The first,
//    `(?:^|[^A-Za-z])`, keeps the keyword from matching inside a longer
//    all-lowercase word. (Under a global `(?i)` the original `[^a-z]` already
//    meant `[^A-Za-z]`, because regex-syntax case-folds a class before
//    negating it; the class is now written out so the intent survives the
//    flag change.) That guard on its own rejected every camelCase key name:
//    in `clientSecret` or `accessToken` the character before the keyword is a
//    letter, so nothing matched and the credential was emitted byte-identical
//    — snake_case always worked, which is why the gap stayed invisible in the
//    corpus. Matching a `[a-z0-9]` → uppercase-initial keyword transition
//    covers the camelCase form without loosening the lowercase-word guard.

/// Generic API keys with common prefixes.
static GENERIC_API_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:api[_-]?key|apikey)['"]?\s*[=:]\s*['"]?([a-zA-Z0-9_/+=-]{16,})['"]?"#)
        .expect("Generic API key regex")
});

/// Generic token assignments.
static GENERIC_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:(?:^|[^A-Za-z])(?i:token)|[a-z0-9](?:Token|TOKEN))['"]?\s*[=:]\s*['"]?([a-zA-Z0-9._/+=-]{16,})['"]?"#,
    )
    .expect("Generic token regex")
});

/// Generic password assignments (password=..., password: ...).
static GENERIC_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)password['"]?\s*[=:]\s*(?:'[^']{4,}'|"[^"]{4,}"|[^\s'"]{4,})"#)
        .expect("Generic password regex")
});

/// Generic secret assignments.
static GENERIC_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:(?:^|[^A-Za-z])(?i:secret)|[a-z0-9](?:Secret|SECRET))['"]?\s*[=:]\s*['"]?([a-zA-Z0-9_/+=-]{8,})['"]?"#,
    )
    .expect("Generic secret regex")
});

/// Device codes (OAuth device flow) - typically 8+ alphanumeric chars displayed to user.
static DEVICE_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:device[_-]?code|user[_-]?code)\s*[=:]\s*['"]?([A-Za-z0-9-]{6,})['"]?"#)
        .expect("Device code regex")
});

/// OAuth URLs with tokens/codes in query params OR the URL fragment.
/// The `#` delimiter is required because the OAuth *implicit* flow returns the
/// token in the fragment (`https://app/cb#access_token=…`), not a query param;
/// the old `[?&]`-only class let that form leak unredacted.
static OAUTH_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s]*[?&#](?:access_token|code|token)=[^\s&;'""]+"#)
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
///
/// Anchored at a leading word boundary, for the same false-positive reason
/// [`STRIPE_KEY`] carries one: unanchored, a bare uppercase-hex digest that
/// happens to contain `AC` in its first 30 positions had 34 of its 64
/// characters overwritten, corrupting persisted history.
///
/// No *trailing* `\b`: a real SID is exactly `AC` + 32 hex, but operators
/// paste over-long hex runs, and requiring an exact-32 boundary made those
/// match nothing at all. The keyed-value leak that motivated looking here is
/// fixed by sequencing instead — see the ordering note on this entry in
/// [`SECRET_PATTERNS`].
static TWILIO_ACCOUNT_SID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bAC[a-fA-F0-9]{32}").expect("Twilio account SID regex"));

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
        name: "http_basic_auth",
        regex: &HTTP_BASIC_AUTH,
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
    // Runs AFTER the generic keyed patterns, unlike every other provider
    // regex. `SECRET_PATTERNS` is applied as a sequence of `replace_all`
    // passes, and this one matches a bare `AC`+hex run with no key name to
    // anchor it. Sequenced early, `api_key=AC<48 hex>` had its first 34
    // characters rewritten to `[REDACTED]`; `generic_api_key` then saw a value
    // beginning with `[`, which is outside its value charset, so it did not
    // match and the trailing 16 characters of the real key leaked. Running
    // last lets the keyed patterns claim a keyed value whole, while a bare SID
    // — which no generic pattern matches — is still caught here.
    SecretPattern {
        name: "twilio_account_sid",
        regex: &TWILIO_ACCOUNT_SID,
    },
];

/// Combined match-set over all [`SECRET_PATTERNS`], used as a single-scan
/// fast-path gate in [`Redactor::redact`]. Built from the SAME pattern sources
/// (`Regex::as_str`) so it is exactly equivalent to OR-ing each pattern's
/// `is_match` — but in one pass instead of 32. When nothing matches, the
/// per-pattern replacement loop is a guaranteed no-op, so `redact` skips it.
static SECRET_PATTERN_SET: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new(SECRET_PATTERNS.iter().map(|pattern| pattern.regex.as_str()))
        .expect("each SECRET_PATTERNS regex is individually valid; their union is too")
});

/// Names of every live secret pattern in priority order.
///
/// The coverage matrix uses this as the catalog source of truth
/// so newly added redactor patterns cannot silently miss the
/// per-release recall report.
pub fn secret_pattern_names() -> impl Iterator<Item = &'static str> {
    SECRET_PATTERNS.iter().map(|pattern| pattern.name)
}

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
        self.redact_observed(text, |_, _, _| {})
    }

    /// Retain source-byte provenance from the actual sequential replacement
    /// passes. `detect` scans the original text and is not an equivalent
    /// oracle when an earlier replacement changes a later pattern's input.
    pub(crate) fn redact_with_replacement_spans(&self, text: &str) -> RedactionTrace {
        let mut provenance = RedactionProvenance::new(text.len());
        let redacted = self.redact_observed(text, |pattern, input, replacement_len| {
            provenance.record_pass(pattern, input, replacement_len);
        });
        RedactionTrace {
            redacted,
            replacements: provenance.replacements,
            replacement_count: provenance.replacement_count,
        }
    }

    fn redact_observed(
        &self,
        text: &str,
        mut observe: impl FnMut(&SecretPattern, &str, usize),
    ) -> String {
        // FND-002 / MT8: per-frame self-time (no-op unless `hot-path-metrics`).
        let _hpt = crate::hot_path_metrics::HotPathTimer::start("redactor.redact");

        // Fast path: one combined RegexSet scan. When no secret pattern matches
        // (the overwhelming common case for captured output), the per-pattern
        // loop below is a guaranteed no-op — every `replace_all` returns its
        // input unchanged — so we skip all 32 full-content scans and the
        // up-to-33 full-content String allocations and return a single owned
        // copy. Output-identical: with zero matches the loop result is `text`.
        if !SECRET_PATTERN_SET.is_match(text) {
            return text.to_string();
        }

        let mut result = text.to_string();

        for pattern in SECRET_PATTERNS {
            let replacement = if self.include_pattern_names {
                format!("[REDACTED:{}]", pattern.name)
            } else {
                REDACTED_MARKER.to_string()
            };

            // The observer sees precisely the input and regex used by this
            // replace_all, including markers emitted by preceding passes.
            observe(pattern, &result, replacement.len());
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

    /// Cold-tier integration adapter (br-ft-95vfk slice 1 / ft-wjjkp.3).
    ///
    /// Takes raw chunk bytes (typically UTF-8 terminal output but
    /// may contain arbitrary bytes from misbehaving processes),
    /// runs the redactor, and returns the post-redact bytes plus
    /// evidence the integration plumbs into
    /// `ColdTierPipelineHealth::record_write`'s
    /// `redactor_applied` flag and byte-accounting telemetry.
    ///
    /// Non-UTF-8 input is handled via lossy decode
    /// (`String::from_utf8_lossy`): invalid bytes become `U+FFFD`
    /// in the scanned text. The privacy invariant holds — even
    /// if some bytes are mangled in the lossy decode, the
    /// redactor still scans the salvageable text for secrets.
    /// The returned bytes are the lossy-decoded then redacted
    /// then re-encoded UTF-8.
    ///
    /// Evidence counts original input bytes, lossy-decode expansion,
    /// emitted redacted bytes, and the exact original source bytes
    /// covered by replacement spans. Evidence stores counts only:
    /// no snippets, offsets, hashes, or raw bytes leave this helper.
    #[must_use]
    pub fn redact_bytes_with_evidence(&self, bytes: &[u8]) -> RedactionResult {
        let decoded = PendingDecodedText::from_lossy_decoded(bytes);
        redact_decoded_text_with_evidence(self, &decoded)
    }
}

/// Redacted output and the original source intervals removed by production
/// replacements. Each source byte is recorded only when removed; markers have
/// no original source bytes, even when a later pass replaces a marker.
pub(crate) struct RedactionTrace {
    pub(crate) redacted: String,
    pub(crate) replacements: Vec<(&'static str, usize, usize)>,
    pub(crate) replacement_count: usize,
}

#[derive(Clone, Copy)]
struct RetainedSourceSpan {
    output_start: usize,
    output_end: usize,
    source_start: usize,
}

struct RedactionProvenance {
    retained: Vec<RetainedSourceSpan>,
    source_cursor: usize,
    replacements: Vec<(&'static str, usize, usize)>,
    replacement_count: usize,
}

impl RedactionProvenance {
    fn new(input_len: usize) -> Self {
        Self {
            retained: vec![RetainedSourceSpan {
                output_start: 0,
                output_end: input_len,
                source_start: 0,
            }],
            source_cursor: 0,
            replacements: Vec::new(),
            replacement_count: 0,
        }
    }

    fn record_pass(&mut self, pattern: &SecretPattern, input: &str, replacement_len: usize) {
        let mut matches = pattern.regex.find_iter(input).peekable();
        if matches.peek().is_none() {
            return;
        }
        self.source_cursor = 0;
        let mut retained = Vec::new();
        let mut input_cursor = 0;
        let mut output_cursor = 0;
        for matched in matches {
            self.visit_source_range(
                input_cursor..matched.start(),
                Some(output_cursor),
                pattern.name,
                &mut retained,
            );
            output_cursor += matched.start() - input_cursor;
            self.visit_source_range(matched.range(), None, pattern.name, &mut retained);
            self.replacement_count = self.replacement_count.saturating_add(1);
            output_cursor += replacement_len;
            input_cursor = matched.end();
        }
        self.visit_source_range(
            input_cursor..input.len(),
            Some(output_cursor),
            pattern.name,
            &mut retained,
        );
        self.retained = retained;
    }

    fn visit_source_range(
        &mut self,
        range: std::ops::Range<usize>,
        output_start: Option<usize>,
        pattern_name: &'static str,
        retained: &mut Vec<RetainedSourceSpan>,
    ) {
        if range.is_empty() {
            return;
        }
        while let Some(span) = self.retained.get(self.source_cursor).copied() {
            if span.output_end <= range.start {
                self.source_cursor += 1;
                continue;
            }
            if span.output_start >= range.end {
                break;
            }
            let start = span.output_start.max(range.start);
            let end = span.output_end.min(range.end);
            let source_start = span.source_start + start - span.output_start;
            if let Some(output_start) = output_start {
                let output_start = output_start + start - range.start;
                retained.push(RetainedSourceSpan {
                    output_start,
                    output_end: output_start + end - start,
                    source_start,
                });
            } else {
                self.replacements
                    .push((pattern_name, source_start, source_start + end - start));
            }
            if span.output_end > range.end {
                break;
            }
            self.source_cursor += 1;
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PendingDecodedText {
    text: String,
    spans: Vec<DecodedSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DecodedSpan {
    text_start: usize,
    text_end: usize,
    original_bytes: u64,
    lossy: bool,
    lossy_replacement_count: u32,
}

impl PendingDecodedText {
    fn from_lossy_decoded(bytes: &[u8]) -> Self {
        let mut decoded = Self::default();
        decoded.push_lossy_decoded(bytes);
        decoded
    }

    fn push_lossy_decoded(&mut self, bytes: &[u8]) {
        let decoded = decode_lossy_with_spans(bytes);
        self.append(decoded);
    }

    fn append(&mut self, decoded: Self) {
        let offset = self.text.len();
        self.text.push_str(&decoded.text);
        self.spans
            .extend(decoded.spans.into_iter().map(|span| DecodedSpan {
                text_start: span.text_start + offset,
                text_end: span.text_end + offset,
                ..span
            }));
    }

    fn push_span_text(
        &mut self,
        text: &str,
        original_bytes: u64,
        lossy: bool,
        lossy_replacement_count: u32,
    ) {
        if text.is_empty() {
            return;
        }

        let text_start = self.text.len();
        self.text.push_str(text);
        self.spans.push(DecodedSpan {
            text_start,
            text_end: self.text.len(),
            original_bytes,
            lossy,
            lossy_replacement_count,
        });
    }

    fn len(&self) -> usize {
        self.text.len()
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn as_str(&self) -> &str {
        &self.text
    }

    fn take_prefix(&mut self, boundary: usize) -> Self {
        let boundary = floor_char_boundary(&self.text, boundary);
        let suffix_text = self.text.split_off(boundary);
        let prefix_text = std::mem::replace(&mut self.text, suffix_text);

        let mut prefix_spans = Vec::new();
        let mut suffix_spans = Vec::new();
        for span in self.spans.drain(..) {
            if span.text_end <= boundary {
                prefix_spans.push(span);
            } else if span.text_start >= boundary {
                suffix_spans.push(span.rebased_after(boundary));
            } else {
                let (prefix, suffix) = span.split_at(boundary);
                if let Some(prefix) = prefix {
                    prefix_spans.push(prefix);
                }
                if let Some(suffix) = suffix {
                    suffix_spans.push(suffix.rebased_after(boundary));
                }
            }
        }

        self.spans = suffix_spans;
        Self {
            text: prefix_text,
            spans: prefix_spans,
        }
    }

    fn original_input_bytes(&self) -> u64 {
        self.spans
            .iter()
            .map(|span| span.original_bytes)
            .fold(0, u64::saturating_add)
    }

    fn lossy_input_bytes(&self) -> u64 {
        self.spans
            .iter()
            .filter(|span| span.lossy)
            .map(|span| span.original_bytes)
            .fold(0, u64::saturating_add)
    }

    fn lossy_replacement_count(&self) -> u32 {
        self.spans
            .iter()
            .map(|span| span.lossy_replacement_count)
            .fold(0, u32::saturating_add)
    }

    fn original_bytes_for_text_range(&self, start: usize, end: usize) -> u64 {
        self.spans
            .iter()
            .map(|span| span.original_bytes_for_text_range(start, end))
            .fold(0, u64::saturating_add)
    }
}

impl DecodedSpan {
    fn rebased_after(self, boundary: usize) -> Self {
        Self {
            text_start: self.text_start - boundary,
            text_end: self.text_end - boundary,
            ..self
        }
    }

    fn split_at(self, boundary: usize) -> (Option<Self>, Option<Self>) {
        debug_assert!(self.text_start < boundary && boundary < self.text_end);
        if self.lossy {
            return (Some(self), None);
        }

        let prefix_text_bytes = boundary - self.text_start;
        let suffix_text_bytes = self.text_end - boundary;
        let prefix = (prefix_text_bytes > 0).then_some(Self {
            text_start: self.text_start,
            text_end: boundary,
            original_bytes: prefix_text_bytes as u64,
            lossy: false,
            lossy_replacement_count: 0,
        });
        let suffix = (suffix_text_bytes > 0).then_some(Self {
            text_start: boundary,
            text_end: self.text_end,
            original_bytes: suffix_text_bytes as u64,
            lossy: false,
            lossy_replacement_count: 0,
        });
        (prefix, suffix)
    }

    fn original_bytes_for_text_range(self, start: usize, end: usize) -> u64 {
        let overlap_start = self.text_start.max(start);
        let overlap_end = self.text_end.min(end);
        if overlap_start >= overlap_end {
            return 0;
        }
        if self.lossy {
            self.original_bytes
        } else {
            (overlap_end - overlap_start) as u64
        }
    }
}

fn decode_lossy_with_spans(mut remaining: &[u8]) -> PendingDecodedText {
    let mut decoded = PendingDecodedText::default();

    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                decoded.push_span_text(valid, remaining.len() as u64, false, 0);
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let valid = std::str::from_utf8(&remaining[..valid_up_to])
                        .expect("valid_up_to prefix is valid UTF-8");
                    decoded.push_span_text(valid, valid_up_to as u64, false, 0);
                    remaining = &remaining[valid_up_to..];
                    continue;
                }

                let invalid_len = error.error_len().unwrap_or(remaining.len()).max(1);
                decoded.push_span_text("\u{fffd}", invalid_len as u64, true, 1);
                remaining = &remaining[invalid_len..];
            }
        }
    }

    decoded
}

fn redact_decoded_text_with_evidence(
    redactor: &Redactor,
    decoded: &PendingDecodedText,
) -> RedactionResult {
    let trace = redactor.redact_with_replacement_spans(decoded.as_str());
    let replacement_count = usize_to_u32_saturating(trace.replacement_count);
    // Provenance records a source byte only when it is removed. A later
    // replacement of a marker cannot count that original byte a second time.
    let secret_input_bytes_replaced = trace
        .replacements
        .iter()
        .map(|(_, start, end)| decoded.original_bytes_for_text_range(*start, *end))
        .fold(0, u64::saturating_add);
    let redacted = trace.redacted;
    let redacted_output_bytes = redacted.len() as u64;

    RedactionResult {
        bytes: redacted.into_bytes(),
        evidence: BytesRedactionEvidence {
            replacement_count,
            original_input_bytes: decoded.original_input_bytes(),
            decoded_input_text_bytes: decoded.len() as u64,
            redacted_output_bytes,
            secret_input_bytes_replaced,
            lossy_input_bytes: decoded.lossy_input_bytes(),
            lossy_replacement_count: decoded.lossy_replacement_count(),
        },
    }
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
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
    pending: PendingDecodedText,
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
            pending: PendingDecodedText::default(),
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
    ///
    /// # Chunk boundaries
    ///
    /// Each chunk is lossy-decoded independently
    /// (`String::from_utf8_lossy`) before being appended to the pending
    /// buffer, so a chunk boundary that falls in the *middle* of a valid
    /// multibyte UTF-8 scalar degrades that scalar to `U+FFFD` (one
    /// replacement char per orphaned byte) rather than carrying the
    /// trailing bytes into the next chunk. The output stays valid UTF-8
    /// in all cases, but callers that need byte-exact passthrough of
    /// multibyte content (rather than redaction-grade fidelity) should
    /// align chunk boundaries to scalar boundaries. Secret detection is
    /// unaffected: every catalog pattern is ASCII-prefixed, so a scalar
    /// mangled at a chunk seam never masks a credential.
    #[must_use]
    pub fn redact_chunk(&mut self, bytes: &[u8]) -> RedactionResult {
        self.pending.push_lossy_decoded(bytes);

        // br-ft-4socw: forced emission on overflow. Drains the oldest
        // half of pending so subsequent boundary-safe scanning works
        // on the remaining tail. Merges into the same RedactionResult
        // as the normal-path emission so callers see a single result
        // per chunk regardless of overflow.
        let mut overflow_result: Option<RedactionResult> = None;
        while self.pending.len() > self.max_pending_bytes {
            record_streaming_redactor_pending_overflow();
            let half = self.max_pending_bytes / 2;
            let force_boundary = overflow_emit_boundary(self.pending.as_str(), half);
            let forced = self.emit_prefix(force_boundary);
            overflow_result = Some(match overflow_result {
                Some(prev) => merge_redaction_results(prev, forced),
                None => forced,
            });
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
        redact_decoded_text_with_evidence(&self.redactor, &pending)
    }

    /// Bytes currently retained to protect the next chunk boundary.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    /// Text currently retained to protect the next chunk boundary.
    ///
    /// Test-only introspection: the segment-persistence path no longer reads the
    /// pending tail (ft-e8hd7 replaced its `StreamingRedactor` use with a raw
    /// lookback), so this accessor now exists solely for `StreamingRedactor`
    /// unit tests.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn pending_text(&self) -> &str {
        self.pending.as_str()
    }

    fn stable_emit_boundary(&self) -> usize {
        let mut boundary = self.pending.len();
        let detections = self.redactor.detect(self.pending.as_str());
        // ft-aznq6: one automaton pass, reused by every fixed-point iteration.
        let anchor_occurrences = streaming_anchor_occurrences(self.pending.as_str());
        // ft-5lz32: the tail window is supposed to bound how much text is held
        // back for the next chunk, but the anchor rule re-centres its window on
        // each boundary it proposes, so a stream whose anchors are closer
        // together than the window chains all the way to byte 0. `AC`, `token`
        // and `secret` are anchors and occur in ordinary English, so ordinary
        // output emitted nothing at all and `pending` grew to
        // `max_pending_bytes` — reaching the forced-overflow path, whose own
        // contract cannot guarantee that partial secrets weren't emitted
        // unredacted. Anchor-derived retention is therefore floored here.
        //
        // Emitting at the floor is safe because the emitted prefix is itself
        // redacted: splitting a secret only leaks when the pattern needs more
        // context than the retained window to become detectable at all, and
        // every catalog pattern except the armoured blocks is detectable within
        // far less than `effective_tail_bytes`. Armoured blocks are exempt (see
        // `UNBOUNDED_SPAN_ANCHORS`), and complete detections are applied below
        // the floor unconditionally.
        let retention_floor = boundary.saturating_sub(self.effective_tail_bytes());

        loop {
            // The floor is applied inside each retention rule rather than to
            // their combined result, so the unbounded-span exemption and the
            // complete-detection rules below can still reach past it.
            let mut next_boundary = self.earliest_secret_like_suffix_start(
                boundary,
                &detections,
                &anchor_occurrences,
                retention_floor,
            );
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
        anchor_occurrences: &[AnchorOccurrence],
        retention_floor: usize,
    ) -> usize {
        if current_boundary == 0 || self.pending.is_empty() {
            return current_boundary;
        }

        let mut earliest = current_boundary;

        // br-ft-zbnz4: anchors are matched case-insensitively. The keyed secret
        // patterns are `(?i)` (e.g. `API_KEY=`, `TOKEN=`, `MISTRAL_API_KEY=`,
        // `AWS_SECRET_ACCESS_KEY=`), so an UPPERCASE env-var-style key name split
        // across a chunk boundary must still trigger tail retention; a
        // case-sensitive scan missed those and leaked the split value. The
        // automaton behind `anchor_occurrences` is built with
        // `ascii_case_insensitive(true)`.
        //
        // ft-aznq6: an anchor occurrence inside the scan window pulls the
        // boundary back to its start, and the rule then applies again at the
        // new boundary — so this is a chain, and the caller's fixed-point loop
        // used to re-derive one link of it per iteration by re-scanning the
        // whole tail window once per anchor (~4.6 MB per link). Short anchors
        // like `AC` occur in ordinary English, so a 200 KiB run of plain build
        // output took minutes. Walking the precomputed occurrence list instead
        // collapses the whole chain in one descending pass.
        //
        // The collapse is exact: if the rule fires at boundary `b` and moves to
        // occurrence `p`, then every position `q` in `(p, b)` still has `p`
        // inside its own window (`p >= b - tail >= q - tail`) and still sees it
        // as uncovered (coverage requires a detection ending before the
        // boundary, and lowering the boundary only shrinks that set), so no
        // fixed point of the rule can be skipped by the jump.
        earliest = earliest.min(self.anchor_occurrence_chain_start(
            current_boundary,
            detections,
            anchor_occurrences,
            retention_floor,
        ));

        // ft-aznq6: retain the whole trailing run of partial-anchor /
        // separator bytes in a single backward pass.
        //
        // The two rules folded into `retainable_trailing_run_start` (a
        // truncated anchor prefix at the end of `pending`, and a trailing `_`
        // or `-`) each move the boundary back by only a few bytes, and the
        // caller re-applies them at every boundary its fixed-point loop
        // proposes. Evaluating them one boundary at a time therefore walked
        // backwards a byte at a time through any trailing run of
        // anchor-initial characters, re-lowercasing the whole tail window and
        // re-running ~78 `rfind` scans of it per byte of progress: ~4.6 MB
        // scanned per byte. `print('a' * 1_000_000)` never returned, and
        // because the boundary can reach 0, `pending` never drained and grew
        // to `max_pending_bytes`, degrading a cost bug into the
        // forced-overflow path's weaker redaction guarantee.
        //
        // Collapsing the run in one pass is not an approximation. If
        // `pending[b-p..b]` equals an anchor's `p`-byte prefix then for every
        // `0 < q < p` the boundary `b-q` ends with that anchor's `(p-q)`-byte
        // prefix, so the same rule fires there and lands no later than `b-p`.
        // Every position the per-byte walk would have visited therefore also
        // retains, and no jump can skip over a non-retaining position — the
        // walk's fixed point is exactly the last position where neither rule
        // fires, which is what this computes.
        earliest = earliest.min(retainable_trailing_run_start(
            self.pending.as_str(),
            current_boundary,
            self.effective_tail_bytes(),
            retention_floor,
        ));

        floor_char_boundary(self.pending.as_str(), earliest)
    }

    /// Walk the anchor-occurrence retention rule to its fixed point (ft-aznq6).
    ///
    /// Starting at `current_boundary`, repeatedly move to the start of the
    /// latest anchor occurrence that is fully inside the scan window and is not
    /// already covered by a complete detection. Returns the first boundary whose
    /// window holds no such occurrence.
    fn anchor_occurrence_chain_start(
        &self,
        current_boundary: usize,
        detections: &[(&'static str, usize, usize)],
        anchor_occurrences: &[AnchorOccurrence],
        retention_floor: usize,
    ) -> usize {
        let pending = self.pending.as_str();
        let tail_bytes = self.effective_tail_bytes();
        let mut boundary = current_boundary;
        let mut idx = anchor_occurrences.len();

        while idx > 0 {
            idx -= 1;
            let occurrence = anchor_occurrences[idx];

            // Occurrences are ascending, so `start` only decreases from here.
            // An anchor straddling the boundary was never visible to the
            // windowed scan this replaces; skip it and keep descending.
            if occurrence.start + occurrence.len > boundary {
                continue;
            }

            // ft-5lz32: an unbounded-span anchor retains regardless of the scan
            // window and the retention floor. An unterminated armoured block is
            // undetectable until its `-----END …-----` line arrives, so emitting
            // any part of it emits private-key material in plaintext. Before
            // this, the windowed scan silently stopped seeing the `-----BEGIN `
            // anchor once the block outgrew the window, and the block survived
            // only when its base64 body happened to contain other anchors —
            // which is luck, not a guarantee. Unbounded retention here is
            // bounded by `max_pending_bytes`, and the forced-emission counter is
            // the visible signal when that bound is hit.
            //
            // Everything else must respect both bounds: a bounded pattern that
            // started before the floor is either already a complete detection
            // (the detection rules handle those, unclamped) or can no longer be
            // completed by bytes that have not arrived yet.
            if !occurrence.unbounded_span {
                let scan_start = floor_char_boundary(pending, boundary.saturating_sub(tail_bytes));
                if occurrence.start < scan_start || occurrence.start < retention_floor {
                    continue;
                }
            }

            let covered_by_complete_detection = detections.iter().any(|(_, det_start, det_end)| {
                *det_end < boundary
                    && ((*det_start <= occurrence.start && occurrence.start < *det_end)
                        || keyed_anchor_reaches_detection_value(
                            pending,
                            occurrence.start + occurrence.len,
                            *det_start,
                        ))
            });
            if !covered_by_complete_detection {
                boundary = occurrence.start;
            }
        }

        boundary
    }

    fn effective_tail_bytes(&self) -> usize {
        // Never scan an open-anchor window smaller than
        // `STREAMING_ANCHOR_TAIL_FLOOR`: below it a keyed `key=value` secret
        // whose value prefix has scrolled past a small `with_tail_bytes`
        // window stops pulling the safe-emit boundary back and leaks. The
        // floor is still bounded by `max_pending_bytes` so the overflow path
        // (which compares against the cap) stays consistent under degenerate
        // caps.
        let floor = STREAMING_ANCHOR_TAIL_FLOOR.min(self.max_pending_bytes);
        self.tail_bytes.max(floor).min(self.max_pending_bytes)
    }

    fn emit_prefix(&mut self, boundary: usize) -> RedactionResult {
        if boundary == 0 {
            return RedactionResult {
                bytes: Vec::new(),
                evidence: BytesRedactionEvidence::default(),
            };
        }

        let prefix = self.pending.take_prefix(boundary);
        redact_decoded_text_with_evidence(&self.redactor, &prefix)
    }
}

fn keyed_anchor_reaches_detection_value(
    pending: &str,
    candidate_value_start: usize,
    detection_start: usize,
) -> bool {
    if candidate_value_start > detection_start {
        return false;
    }

    let bridge = &pending[candidate_value_start..detection_start];
    !bridge.is_empty()
        && bridge
            .chars()
            .all(|ch| matches!(ch, '=' | ':' | '"' | '\'') || ch.is_ascii_whitespace())
}

fn trailing_generic_token_or_secret_boundary_start(
    pending: &str,
    current_boundary: usize,
) -> Option<usize> {
    if current_boundary == 0 {
        return None;
    }

    let boundary_start = current_boundary - 1;
    let byte = pending.as_bytes()[boundary_start];
    matches!(byte, b'_' | b'-').then_some(boundary_start)
}

/// Start of the maximal trailing run of bytes the partial-anchor and
/// trailing-separator retention rules would walk back over (ft-aznq6).
///
/// Returns the highest `boundary <= current_boundary` at which neither rule
/// fires, which is the fixed point of applying them repeatedly — see the call
/// site for why iterating them one boundary at a time is equivalent but
/// quadratic. Cost is one byte-table lookup per retained byte in the common
/// case, and at most `sum(anchor.len())` byte comparisons for a byte that only
/// continues a multi-byte anchor prefix.
fn retainable_trailing_run_start(
    pending: &str,
    current_boundary: usize,
    tail_limit: usize,
    retention_floor: usize,
) -> usize {
    let mut boundary = current_boundary.min(pending.len());
    // ft-5lz32: a truncated anchor prefix is at most `longest_streaming_anchor_len()`
    // bytes; a run longer than the tail window is retained only because the rule
    // re-fires at each position, so stop at the floor.
    while boundary > retention_floor
        && retains_trailing_secret_fragment(pending, boundary, tail_limit)
    {
        boundary -= 1;
    }
    boundary
}

/// Whether `pending[..boundary]` ends with something worth holding back for the
/// next chunk: a `_`/`-` separator, or a truncated [`STREAMING_SECRET_ANCHORS`]
/// prefix (ft-aznq6).
///
/// `tail_limit` mirrors [`StreamingRedactor::effective_tail_bytes`]: a fragment
/// longer than the scan window could not have been seen by the windowed scan it
/// replaces, so it must not retain here either.
fn retains_trailing_secret_fragment(pending: &str, boundary: usize, tail_limit: usize) -> bool {
    debug_assert!(boundary > 0 && boundary <= pending.len());

    // The separator rule reads the byte before the boundary directly and is not
    // window-limited.
    if trailing_generic_token_or_secret_boundary_start(pending, boundary).is_some() {
        return true;
    }

    if tail_limit == 0 {
        return false;
    }

    let bytes = pending.as_bytes();
    // Fast path: a single trailing byte that starts some anchor. This covers
    // every `prefix_len == 1` match, which is what a long run of ordinary
    // letters hits.
    if ANCHOR_INITIAL_BYTES[bytes[boundary - 1] as usize] {
        return true;
    }

    // Slow path: the trailing bytes continue an anchor prefix without being an
    // anchor's own first byte (`"aw"` for `aws_secret_access_key`). Bounded by
    // the longest anchor, and only reached for bytes the table rejected.
    let fragment_cap = longest_streaming_anchor_len().min(tail_limit);
    let window_start = boundary.saturating_sub(fragment_cap);
    let tail = &bytes[window_start..boundary];
    STREAMING_SECRET_ANCHORS.iter().any(|anchor| {
        let anchor_bytes = anchor.as_bytes();
        // `prefix_len == anchor.len()` is a complete anchor, which the
        // occurrence scan already handles; only truncated prefixes retain here.
        (2..anchor_bytes.len())
            .filter(|prefix_len| *prefix_len <= tail.len())
            .any(|prefix_len| {
                tail[tail.len() - prefix_len..].eq_ignore_ascii_case(&anchor_bytes[..prefix_len])
            })
    })
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

fn overflow_emit_boundary(text: &str, index: usize) -> usize {
    let floor = floor_char_boundary(text, index);
    if floor > 0 || text.is_empty() {
        return floor;
    }

    text.char_indices()
        .nth(1)
        .map_or_else(|| text.len(), |(boundary, _)| boundary)
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
            replacement_count: first
                .evidence
                .replacement_count
                .saturating_add(second.evidence.replacement_count),
            original_input_bytes: first
                .evidence
                .original_input_bytes
                .saturating_add(second.evidence.original_input_bytes),
            decoded_input_text_bytes: first
                .evidence
                .decoded_input_text_bytes
                .saturating_add(second.evidence.decoded_input_text_bytes),
            redacted_output_bytes: first
                .evidence
                .redacted_output_bytes
                .saturating_add(second.evidence.redacted_output_bytes),
            secret_input_bytes_replaced: first
                .evidence
                .secret_input_bytes_replaced
                .saturating_add(second.evidence.secret_input_bytes_replaced),
            lossy_input_bytes: first
                .evidence
                .lossy_input_bytes
                .saturating_add(second.evidence.lossy_input_bytes),
            lossy_replacement_count: first
                .evidence
                .lossy_replacement_count
                .saturating_add(second.evidence.lossy_replacement_count),
        },
    }
}

/// Count-only evidence the redactor returns to cold-tier integrations.
/// Mirrors the `RedactionEvidence` shape in
/// `scrollback_cold_tier_pipeline.rs` so the integration can pass
/// either through `ChunkBytes::redact_with_evidence`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BytesRedactionEvidence {
    /// Number of non-overlapping redactor spans replaced.
    pub replacement_count: u32,
    /// Exact source bytes represented by this returned result.
    /// Streaming bytes retained in `pending` are counted only when
    /// emitted or flushed.
    pub original_input_bytes: u64,
    /// UTF-8 byte length of the lossy-decoded text scanned before
    /// redaction.
    pub decoded_input_text_bytes: u64,
    /// UTF-8 byte length of the emitted redacted bytes.
    pub redacted_output_bytes: u64,
    /// Exact original source bytes covered by replaced secret spans.
    pub secret_input_bytes_replaced: u64,
    /// Original source bytes represented by lossy replacement
    /// characters in the scanned text.
    pub lossy_input_bytes: u64,
    /// Number of `U+FFFD` replacement characters inserted before
    /// redaction.
    pub lossy_replacement_count: u32,
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
        self.replacement_count > 0
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

    #[test]
    fn replacement_provenance_tracks_later_passes_over_prior_markers() {
        let text = concat!(
            "-----BEGIN PRIVATE KEY-----\n",
            "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
            "-----END PRIVATE KEY-----"
        );
        let redactor = Redactor::new();
        // Original-input overlap suppression reports only the inner token.
        let detections = redactor.detect(text);
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].0, "github_token");

        let trace = redactor.redact_with_replacement_spans(text);
        assert_eq!(trace.redacted, REDACTED_MARKER);
        assert_eq!(trace.replacement_count, 2);
        let mut intervals = trace
            .replacements
            .iter()
            .map(|(_, start, end)| (*start, *end))
            .collect::<Vec<_>>();
        intervals.sort_unstable();
        let mut cursor = 0;
        for (start, end) in intervals {
            assert_eq!(start, cursor, "every original byte is removed exactly once");
            cursor = end;
        }
        assert_eq!(cursor, text.len());
        let bytes = redactor.redact_bytes_with_evidence(text.as_bytes());
        assert_eq!(bytes.bytes, REDACTED_MARKER.as_bytes());
        assert_eq!(bytes.evidence.replacement_count, 2);
        assert_eq!(bytes.evidence.secret_input_bytes_replaced, text.len() as u64);
    }

    #[test]
    fn replacement_provenance_preserves_production_output_across_corpus() {
        for vector in crate::redactor_coverage_matrix::synthesized_corpus() {
            for redactor in [Redactor::new(), Redactor::with_debug_markers()] {
                let trace = redactor.redact_with_replacement_spans(&vector.input);
                let mut original_pipeline = vector.input.clone();
                for pattern in SECRET_PATTERNS {
                    let marker = if redactor.include_pattern_names {
                        format!("[REDACTED:{}]", pattern.name)
                    } else {
                        REDACTED_MARKER.to_string()
                    };
                    original_pipeline = pattern
                        .regex
                        .replace_all(&original_pipeline, &marker)
                        .to_string();
                }
                assert_eq!(trace.redacted, original_pipeline, "vector {}", vector.name);
                assert_eq!(redactor.redact(&vector.input), original_pipeline);
                for (_, start, end) in trace.replacements {
                    assert!(start < end && end <= vector.input.len());
                    assert!(vector.input.is_char_boundary(start));
                    assert!(vector.input.is_char_boundary(end));
                }
            }
        }
    }

    #[test]
    fn replacement_provenance_preserves_unmatched_text_between_shifted_spans() {
        let github = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let anthropic = "sk-ant-api03-1234567890123456789012345678901234567890";
        let input = format!("πleft {github} middle {anthropic} next {github} rightλ");
        let trace = Redactor::new().redact_with_replacement_spans(&input);
        assert_eq!(
            trace.redacted,
            "πleft [REDACTED] middle [REDACTED] next [REDACTED] rightλ"
        );
        let mut replacements = trace.replacements;
        replacements.sort_unstable_by_key(|(_, start, _)| *start);
        let expected = [github, anthropic, github];
        assert_eq!(replacements.len(), expected.len());
        for ((_, start, end), expected) in replacements.iter().zip(expected) {
            assert_eq!(&input[*start..*end], expected);
        }
    }

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

    fn test_evidence_with_replacements(replacement_count: u32) -> BytesRedactionEvidence {
        BytesRedactionEvidence {
            replacement_count,
            original_input_bytes: 100,
            decoded_input_text_bytes: 100,
            redacted_output_bytes: 40,
            secret_input_bytes_replaced: 60,
            lossy_input_bytes: 0,
            lossy_replacement_count: 0,
        }
    }

    // ----------------------------------------------------------------
    // Cold-tier integration adapter (br-ft-95vfk slice 1)
    // ----------------------------------------------------------------

    #[test]
    fn mapped_lossy_decoder_matches_std_lossy_text() {
        let cases: &[&[u8]] = &[
            b"",
            b"plain ascii",
            b"emoji \xf0\x9f\xa6\x80",
            &[0xff, 0xfe, b'a', 0xf0, 0x9f],
            &[b'a', 0xe2, 0x82, 0xac, b'z'],
        ];

        for bytes in cases {
            let decoded = PendingDecodedText::from_lossy_decoded(bytes);
            assert_eq!(decoded.as_str(), String::from_utf8_lossy(bytes));
            assert_eq!(decoded.original_input_bytes(), bytes.len() as u64);
        }
    }

    #[test]
    fn redact_bytes_with_evidence_clean_input_no_match() {
        let r = Redactor::new();
        let input = b"benign log line, nothing secret";
        let result = r.redact_bytes_with_evidence(input);
        assert!(result.evidence.redactor_applied());
        assert!(!result.evidence.made_changes());
        assert_eq!(result.evidence.replacement_count, 0);
        assert_eq!(result.evidence.original_input_bytes, input.len() as u64);
        assert_eq!(result.evidence.decoded_input_text_bytes, input.len() as u64);
        assert_eq!(result.evidence.redacted_output_bytes, input.len() as u64);
        assert_eq!(result.evidence.secret_input_bytes_replaced, 0);
        assert_eq!(result.evidence.lossy_input_bytes, 0);
        assert_eq!(result.evidence.lossy_replacement_count, 0);
        assert_eq!(result.bytes, input);
    }

    #[test]
    fn redact_bytes_with_evidence_single_match_records_count() {
        let r = Redactor::new();
        let input = b"GITLAB_TOKEN=glpat-xxxxxxxxxxxxxxxxxxxx";
        let result = r.redact_bytes_with_evidence(input);
        assert!(result.evidence.redactor_applied());
        assert!(result.evidence.made_changes());
        assert!(result.evidence.replacement_count >= 1);
        assert!(
            !std::str::from_utf8(&result.bytes)
                .unwrap()
                .contains("glpat-")
        );
        assert_eq!(result.evidence.original_input_bytes, input.len() as u64);
        assert_eq!(result.evidence.decoded_input_text_bytes, input.len() as u64);
        assert_eq!(
            result.evidence.redacted_output_bytes,
            result.bytes.len() as u64
        );
        assert!(result.evidence.secret_input_bytes_replaced > 0);
        assert_eq!(result.evidence.lossy_input_bytes, 0);
        assert_eq!(result.evidence.lossy_replacement_count, 0);
    }

    #[test]
    fn redact_bytes_with_evidence_preserves_output_growth() {
        let r = Redactor::with_debug_markers();
        let input = b"password=abcd";
        let result = r.redact_bytes_with_evidence(input);

        assert!(result.evidence.made_changes());
        assert!(result.bytes.len() > input.len());
        assert_eq!(result.evidence.original_input_bytes, input.len() as u64);
        assert_eq!(result.evidence.decoded_input_text_bytes, input.len() as u64);
        assert_eq!(
            result.evidence.redacted_output_bytes,
            result.bytes.len() as u64
        );
        assert_eq!(
            result.evidence.secret_input_bytes_replaced,
            input.len() as u64
        );
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
        assert_eq!(result.evidence.original_input_bytes, input.len() as u64);
        assert_eq!(
            result.evidence.decoded_input_text_bytes,
            String::from_utf8_lossy(&input).len() as u64
        );
        assert_eq!(
            result.evidence.redacted_output_bytes,
            result.bytes.len() as u64
        );
        assert!(result.evidence.secret_input_bytes_replaced > 0);
        assert_eq!(result.evidence.lossy_input_bytes, 3);
        assert_eq!(result.evidence.lossy_replacement_count, 3);
    }

    #[test]
    fn redact_bytes_with_evidence_evidence_made_changes_predicate() {
        let zero = BytesRedactionEvidence::default();
        let some = test_evidence_with_replacements(3);
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
        assert_eq!(result.bytes, [] as [u8; 0]);
        assert_eq!(result.evidence, BytesRedactionEvidence::default());
    }

    #[test]
    fn streaming_redactor_no_emission_reports_zero_byte_evidence() {
        let secret = b"sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRs";
        let split = b"sk-ant-api03-".len();
        let mut streaming = StreamingRedactor::new();

        let first = streaming.redact_chunk(&secret[..split]);
        assert_eq!(first.bytes, [] as [u8; 0]);
        assert_eq!(first.evidence.original_input_bytes, 0);
        assert_eq!(first.evidence.redacted_output_bytes, 0);
        assert_eq!(first.evidence.replacement_count, 0);

        let second = streaming.redact_chunk(&secret[split..]);
        let finish = streaming.finish();
        let merged = merge_redaction_results(second, finish);

        assert!(merged.evidence.made_changes());
        assert_eq!(merged.evidence.original_input_bytes, secret.len() as u64);
        assert_eq!(
            merged.evidence.secret_input_bytes_replaced,
            secret.len() as u64
        );
    }

    #[test]
    fn streaming_redactor_split_invalid_utf8_counts_original_bytes_once() {
        let mut streaming = StreamingRedactor::new();
        let first = streaming.redact_chunk(&[0xf0]);
        let second = streaming.redact_chunk(&[0x9f]);
        let finish = streaming.finish();
        let merged = merge_redaction_results(merge_redaction_results(first, second), finish);

        assert_eq!(merged.evidence.original_input_bytes, 2);
        assert_eq!(merged.evidence.lossy_input_bytes, 2);
        assert_eq!(merged.evidence.lossy_replacement_count, 2);
        assert_eq!(String::from_utf8_lossy(&merged.bytes), "\u{fffd}\u{fffd}");
    }

    #[test]
    fn streaming_redactor_catches_secret_split_at_every_offset() {
        let key = "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRs";
        let input = format!("before:{key}:after");
        let expected = Redactor::new().redact(&input).into_bytes();

        for split in 1..input.len() {
            let mut streaming = StreamingRedactor::new();
            let mut out = Vec::new();
            let mut made_changes = false;

            let c1 = streaming.redact_chunk(&input.as_bytes()[..split]);
            made_changes |= c1.evidence.made_changes();
            out.extend(c1.bytes);

            let c2 = streaming.redact_chunk(&input.as_bytes()[split..]);
            made_changes |= c2.evidence.made_changes();
            out.extend(c2.bytes);

            let finish = streaming.finish();
            made_changes |= finish.evidence.made_changes();
            out.extend(finish.bytes);

            assert!(made_changes, "split={split}");

            assert_eq!(out, expected, "split={split}");
            let rendered = String::from_utf8(out).unwrap();
            assert!(!rendered.contains("sk-ant-api03-"), "split={split}");
        }
    }

    #[test]
    fn streaming_redactor_emits_delimited_complete_secret_without_finish_tail() {
        let secret = "sk-proj-abcdefghijklmnopqrstuvwxyz123456";
        let input = format!("export OPENAI_API_KEY={secret}\n");
        let mut streaming = StreamingRedactor::new();

        let result = streaming.redact_chunk(input.as_bytes());

        assert_eq!(
            String::from_utf8(result.bytes).expect("streaming redactor emits UTF-8"),
            Redactor::new().redact(&input)
        );
        assert_eq!(streaming.pending_bytes(), 0);
        assert_eq!(streaming.finish().bytes, [] as [u8; 0]);
    }

    #[test]
    fn streaming_redactor_retains_open_key_anchor_before_unrelated_detection() {
        let complete_secret = "sk-proj-abcdefghijklmnopqrstuvwxyz123456";
        let input = format!("prefix api_key=short-value {complete_secret}\n");
        let mut streaming = StreamingRedactor::new();

        let result = streaming.redact_chunk(input.as_bytes());
        let emitted = String::from_utf8(result.bytes).expect("streaming redactor emits UTF-8");

        assert_eq!(emitted, "prefix ");
        assert_eq!(
            streaming.pending_text(),
            format!("api_key=short-value {complete_secret}\n")
        );
    }

    #[test]
    fn streaming_redactor_retains_oauth_url_prefix_before_query_token() {
        let input = "redirect=https://example.com/cb?access_token=abc123def456&state=xyz";
        let split = "redirect=https:/".len();
        let mut streaming = StreamingRedactor::new();

        let mut streamed = Vec::new();
        streamed.extend(streaming.redact_chunk(&input.as_bytes()[..split]).bytes);
        streamed.extend(streaming.redact_chunk(&input.as_bytes()[split..]).bytes);
        streamed.extend(streaming.finish().bytes);

        assert_eq!(
            String::from_utf8(streamed).expect("streaming redactor emits UTF-8"),
            Redactor::new().redact(input)
        );
    }

    #[test]
    fn streaming_redactor_retains_key_separator_before_generic_token_or_secret() {
        for (key_prefix, key_suffix, value) in [
            ("auth_", "token", "AAAABBBBCCCCDDDDEEEEFFFF"),
            ("client-", "secret", "AAAABBBBCCCCDDDDEEEEFFFF"),
        ] {
            let input = format!("{key_prefix}{key_suffix}: \"{value}\"");
            let expected = Redactor::new().redact(&input);
            assert_ne!(expected, input, "fixture must actually redact");

            let split = key_prefix.len();
            let mut streaming = StreamingRedactor::new();
            let mut streamed = Vec::new();
            streamed.extend(streaming.redact_chunk(&input.as_bytes()[..split]).bytes);
            streamed.extend(streaming.redact_chunk(&input.as_bytes()[split..]).bytes);
            streamed.extend(streaming.finish().bytes);

            assert_eq!(
                String::from_utf8(streamed).expect("streaming redactor emits UTF-8"),
                expected,
                "key_prefix={key_prefix:?} key_suffix={key_suffix:?}"
            );
        }
    }

    #[test]
    fn streaming_redactor_catches_datadog_long_form_split_at_every_offset() {
        // br-ft-zbnz4 regression: the long `DATADOG_API_KEY=` key-name form was
        // absent from STREAMING_SECRET_ANCHORS, so a value split across a chunk
        // boundary produced no anchor hit and the 32-hex value leaked. The short
        // `DD_API_KEY=` form was already anchored; this guards the long form.
        let value = "deadbeef0123456789abcdef01234567";
        let input = format!("before DATADOG_API_KEY={value} after");
        let expected = Redactor::new().redact(&input).into_bytes();
        assert_ne!(expected, input.as_bytes(), "fixture must actually redact");

        for split in 1..input.len() {
            let mut streaming = StreamingRedactor::new();
            let mut out = Vec::new();

            out.extend(streaming.redact_chunk(&input.as_bytes()[..split]).bytes);
            out.extend(streaming.redact_chunk(&input.as_bytes()[split..]).bytes);
            out.extend(streaming.finish().bytes);

            assert_eq!(out, expected, "split={split}");
            let rendered = String::from_utf8(out).unwrap();
            assert!(
                !rendered.contains(value),
                "split={split} leaked {rendered:?}"
            );
        }
    }

    #[test]
    fn streaming_redactor_catches_uppercase_keyed_secret_split_at_every_offset() {
        // br-ft-zbnz4 regression: keyed patterns are (?i), but streaming anchors
        // were matched case-sensitively, so an UPPERCASE env-var-style key name
        // (`API_KEY=`) split mid-value produced no anchor hit and leaked. Anchor
        // matching is now case-insensitive; this guards that.
        let value = "abcdef0123456789ABCDEFGH";
        let input = format!("before API_KEY={value} after");
        let expected = Redactor::new().redact(&input).into_bytes();
        assert_ne!(expected, input.as_bytes(), "fixture must actually redact");

        for split in 1..input.len() {
            let mut streaming = StreamingRedactor::new();
            let mut out = Vec::new();

            out.extend(streaming.redact_chunk(&input.as_bytes()[..split]).bytes);
            out.extend(streaming.redact_chunk(&input.as_bytes()[split..]).bytes);
            out.extend(streaming.finish().bytes);

            assert_eq!(out, expected, "split={split}");
            let rendered = String::from_utf8(out).unwrap();
            assert!(
                !rendered.contains(value),
                "split={split} leaked {rendered:?}"
            );
        }
    }

    #[test]
    fn streaming_redactor_catches_collapsed_keyname_secrets_split_at_every_offset() {
        // ft-b1p6x regression: AI_PROVIDER_KEYED_VALUE and DEVICE_CODE accept
        // collapsed (no-separator) key-names (`azureopenai_key=`, `devicecode=`,
        // `usercode=`) that batch redaction caught but STREAMING_SECRET_ANCHORS
        // only held the separated forms, so a value split across a chunk
        // boundary leaked. The collapsed anchors now cover them. Mirrors the
        // DATADOG long-form and uppercase regressions.
        let value = "deadbeef0123456789abcdef01234567";
        for key in ["azureopenai_key", "devicecode", "usercode"] {
            let input = format!("before {key}={value} after");
            let expected = Redactor::new().redact(&input).into_bytes();
            assert_ne!(
                expected,
                input.as_bytes(),
                "fixture must actually redact key={key}"
            );

            for split in 1..input.len() {
                let mut streaming = StreamingRedactor::new();
                let mut out = Vec::new();

                out.extend(streaming.redact_chunk(&input.as_bytes()[..split]).bytes);
                out.extend(streaming.redact_chunk(&input.as_bytes()[split..]).bytes);
                out.extend(streaming.finish().bytes);

                assert_eq!(out, expected, "key={key} split={split}");
                let rendered = String::from_utf8(out).unwrap();
                assert!(
                    !rendered.contains(value),
                    "key={key} split={split} leaked {rendered:?}"
                );
            }
        }
    }

    #[test]
    fn streaming_redactor_catches_keyed_secrets_fed_one_byte_per_chunk() {
        // N-chunk coverage. Every other streaming-split test feeds a secret in
        // exactly two chunks (`[..split]` + `[split..]`). Feeding one byte per
        // `redact_chunk` call drives the pending-buffer accumulation and
        // tail-overlap carry across MANY calls — the extreme chunking the
        // `redactor_no_leak` fuzz target explores but no deterministic test
        // pinned. The streamed output must equal batch redaction byte-for-byte
        // and never leak the value. Covers the ft-b1p6x collapsed
        // no-separator keyed class (key name abutting the value).
        let value = "deadbeef0123456789abcdef01234567";
        for key in ["azureopenai_key", "devicecode", "usercode"] {
            let input = format!("before {key}={value} after");
            let expected = Redactor::new().redact(&input).into_bytes();
            assert_ne!(
                expected,
                input.as_bytes(),
                "fixture must actually redact key={key}"
            );

            let mut streaming = StreamingRedactor::new();
            let mut out = Vec::new();
            for byte in input.as_bytes() {
                out.extend(streaming.redact_chunk(std::slice::from_ref(byte)).bytes);
            }
            out.extend(streaming.finish().bytes);

            assert_eq!(
                out, expected,
                "key={key}: byte-at-a-time streaming diverged from batch redaction"
            );
            let rendered = String::from_utf8(out).unwrap();
            assert!(
                !rendered.contains(value),
                "key={key}: byte-at-a-time streaming leaked {rendered:?}"
            );
        }
    }

    #[test]
    fn redact_metamorphic_streaming_equals_batch_across_boundary() {
        // Metamorphic relation (testing-metamorphic) for chunk boundaries:
        //   batch:     redact(x + y)            — catches a boundary-spanning secret
        //   piecewise: redact(x) + redact(y)    — LEAKS (neither half holds the
        //                                          whole secret)
        //   streaming: StreamingRedactor[x, y]  — MUST equal batch
        // The streaming redactor is the boundary-correct primitive: at every
        // split its output must equal batch redaction and never leak the secret.
        // We also assert the naive piecewise relation DOES leak at some split,
        // so the fixture is guaranteed to exercise a genuine cross-boundary
        // secret (and the test fails loudly if streaming is ever "simplified"
        // into the piecewise form).
        let secret = "AKIAIOSFODNN7EXAMPLE"; // AWS access key id (single token)
        let whole = format!("env AWS_ACCESS_KEY_ID={secret} done");
        let batch = Redactor::new();
        let expected = batch.redact(&whole);
        assert!(
            !expected.contains(secret),
            "batch redaction must scrub the secret"
        );

        let mut any_piecewise_leak = false;
        for split in 1..whole.len() {
            let x = &whole[..split];
            let y = &whole[split..];

            let mut streaming = StreamingRedactor::new();
            let mut out = Vec::new();
            out.extend(streaming.redact_chunk(x.as_bytes()).bytes);
            out.extend(streaming.redact_chunk(y.as_bytes()).bytes);
            out.extend(streaming.finish().bytes);
            let streamed = String::from_utf8(out).expect("utf8 stream output");

            assert_eq!(
                streamed, expected,
                "streaming diverged from batch at split={split}"
            );
            assert!(
                !streamed.contains(secret),
                "streaming leaked the secret at split={split}: {streamed:?}"
            );

            let piecewise = format!("{}{}", batch.redact(x), batch.redact(y));
            if piecewise.contains(secret) {
                any_piecewise_leak = true;
            }
        }
        assert!(
            any_piecewise_leak,
            "fixture no longer exercises a cross-boundary secret: naive piecewise \
             redaction never leaked at any split"
        );
    }

    #[test]
    fn streaming_redactor_never_leaks_any_corpus_positive_across_splits() {
        // Class guard (ft-b1p6x). The per-sample streaming-split tests above each
        // pin one secret. STREAMING_SECRET_ANCHORS is a hand-maintained list, so a
        // newly added keyed pattern can silently miss an anchor and leak a value
        // split across a chunk boundary — exactly the failure ft-b1p6x fixed for
        // azureopenai/devicecode/usercode and br-ft-zbnz4 fixed for the DATADOG
        // long form. Rather than enumerate samples by hand, drive EVERY positive
        // vector from the coverage corpus through the streaming redactor at every
        // split offset and assert the secret bytes never survive. Any future
        // pattern that forgets its anchor fails here instead of in production.
        //
        // Every corpus input is far under the 64 KiB default tail window
        // (DEFAULT_STREAMING_REDACTOR_TAIL_BYTES), so the streaming redactor holds
        // each secret in full and its output must equal batch redaction byte for
        // byte — the same invariant the per-sample tests assert.
        use crate::redactor_coverage_matrix::synthesized_corpus;

        let batch = Redactor::new();
        for vector in synthesized_corpus() {
            if vector.expected_matches.is_empty() {
                continue; // negatives carry no secret to leak
            }
            assert!(
                vector.input.len() < DEFAULT_STREAMING_REDACTOR_TAIL_BYTES,
                "corpus vector {} exceeds the default tail window; the \
                 byte-identical streaming invariant below would not hold",
                vector.name
            );

            let expected_str = batch.redact(&vector.input);
            let expected = expected_str.clone().into_bytes();
            // The corpus is only meaningful here if batch actually scrubs the
            // secret; otherwise a streaming "match" would be a false comfort.
            for em in &vector.expected_matches {
                let secret = &vector.input[em.start as usize..em.end as usize];
                assert!(
                    !expected_str.contains(secret),
                    "batch redaction left secret {secret:?} for vector {}",
                    vector.name
                );
            }

            let input = vector.input.as_bytes();
            for split in 1..input.len() {
                let mut streaming = StreamingRedactor::new();
                let mut out = Vec::new();
                out.extend(streaming.redact_chunk(&input[..split]).bytes);
                out.extend(streaming.redact_chunk(&input[split..]).bytes);
                out.extend(streaming.finish().bytes);

                assert_eq!(
                    out, expected,
                    "vector {} split={split}: streaming output diverged from batch",
                    vector.name
                );

                let rendered = String::from_utf8(out).unwrap_or_else(|_| {
                    panic!(
                        "vector {} split={split}: non-UTF8 stream output",
                        vector.name
                    )
                });
                for em in &vector.expected_matches {
                    let secret = &vector.input[em.start as usize..em.end as usize];
                    assert!(
                        !rendered.contains(secret),
                        "vector {} split={split} leaked secret {secret:?} in {rendered:?}",
                        vector.name
                    );
                }
            }
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
        // The bead's tamper-evidence concern: replacement_count
        // should equal the number of distinct secrets in the
        // input, not the cumulative cross-pattern hit count.
        let token = "sk-ant-api03-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let result = Redactor::new().redact_bytes_with_evidence(token.as_bytes());
        assert_eq!(
            result.evidence.replacement_count, 1,
            "replacement_count must equal distinct secret count, got {}",
            result.evidence.replacement_count
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
        assert_ne!(first.bytes, [] as [u8; 0]);
        // Retention here is anchor-driven ("secret" = 6 bytes), so it stays
        // under 8 even though the effective scan window is now floored to
        // `STREAMING_ANCHOR_TAIL_FLOOR`.
        assert!(streaming.pending_bytes() <= 8);

        let mut out = first.bytes;
        out.extend(streaming.finish().bytes);
        assert_eq!(out, b"plain text with no secret");
    }

    /// br-B1 regression: a small `with_tail_bytes` must NOT leak a keyed
    /// secret whose value is split across a chunk boundary. Before the
    /// `STREAMING_ANCHOR_TAIL_FLOOR` floor on `effective_tail_bytes`,
    /// `with_tail_bytes(4)` let the `token=` anchor scroll out of the
    /// open-anchor scan window once a few value bytes arrived, so the prefix
    /// was emitted early and the value leaked unredacted while batch
    /// redaction caught it.
    #[test]
    fn streaming_small_tail_does_not_leak_split_keyed_secret() {
        let value = "AAAABBBBCCCCDDDDEEEE"; // ≥16 chars → a complete token
        let full = format!("token={value}");
        // Precondition: batch redaction actually removes the value, so the
        // streaming assertion below is meaningful.
        let batch = Redactor::new().redact(&full);
        assert!(
            !batch.contains(value),
            "precondition: batch redaction must mask {value}; got {batch:?}"
        );

        let mut streaming = StreamingRedactor::new().with_tail_bytes(4);
        let mut out = Vec::new();
        out.extend(streaming.redact_chunk(b"token=AAAA").bytes);
        out.extend(streaming.redact_chunk(b"BBBBCCCCDDDDEEEE").bytes);
        out.extend(streaming.finish().bytes);
        let out_str = String::from_utf8_lossy(&out);

        assert!(
            !out_str.contains(value),
            "br-B1: streaming with a tiny tail leaked a split keyed secret: {out_str:?}"
        );
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

        // ft-5lz32 changed the input this test uses. The original input was a
        // repeated `"rk_AAAAAAAA"` anchor stream, which grew pending without
        // bound because every anchor occurrence pulled the emit boundary back
        // and the chain walked below the tail window. That growth was itself the
        // ft-5lz32 defect: such a stream now drains (pinned by
        // `ft_5lz32_repeated_anchor_stream_drains_without_forced_emission`), so
        // it can no longer reach the overflow path.
        //
        // What genuinely cannot drain is an unterminated armoured block: the PEM
        // patterns only match once `-----END …-----` arrives, so the block is
        // undetectable and must be retained in full. That is the remaining
        // legitimate route to the ft-4socw cap, and this test now uses it.
        let mut runaway = String::from("-----BEGIN RSA PRIVATE KEY-----\n");
        runaway.push_str(&"MIIEowIBAAKCAQEAvOhL0mE3sk9wQ\n".repeat(24));
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
            "br-ft-4socw: a stream that cannot drain must trigger forced \
             emission at least once; got count={count}"
        );

        // Drain pending so subsequent tests start fresh.
        let _ = streaming.finish();
    }

    /// ft-5lz32: a repeated-anchor stream must now drain instead of growing to
    /// the pending cap. This is the input the ft-4socw overflow test used to
    /// rely on; the growth it modelled was the defect, not the design.
    #[test]
    fn ft_5lz32_repeated_anchor_stream_drains_without_forced_emission() {
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        const TEST_CAP: usize = 1024;
        let mut streaming = StreamingRedactor::new()
            .with_tail_bytes(64)
            .with_max_pending_bytes(TEST_CAP);

        let runaway = "rk_AAAAAAAA".repeat(64); // 704 bytes per chunk.
        let mut streamed = Vec::new();
        for _ in 0..50 {
            streamed.extend_from_slice(&streaming.redact_chunk(runaway.as_bytes()).bytes);
            assert!(
                streaming.pending_bytes() <= TEST_CAP,
                "pending must stay inside the cap without forced emission; got {}",
                streaming.pending_bytes()
            );
        }
        streamed.extend_from_slice(&streaming.finish().bytes);

        assert_eq!(
            super::streaming_redactor_pending_overflow_count(),
            0,
            "an anchor stream must drain on its own rather than reaching the \
             forced-emission path and its weaker guarantee"
        );
        let batch = Redactor::new().redact(&runaway.repeat(50));
        assert_eq!(
            String::from_utf8(streamed).expect("streamed output is utf8"),
            batch,
            "draining must not change what the stream redacts to"
        );
    }

    /// Reference implementation of the pre-ft-aznq6 retention rules: apply the
    /// truncated-anchor-prefix and trailing-separator rules one boundary at a
    /// time until they stop moving, re-scanning the tail window every step.
    ///
    /// This is the semantics [`retainable_trailing_run_start`] replaces, kept in
    /// the test module so the equivalence claim is pinned rather than asserted.
    fn reference_per_byte_retention_walk(
        pending: &str,
        current_boundary: usize,
        tail_limit: usize,
    ) -> usize {
        let mut boundary = current_boundary.min(pending.len());
        loop {
            let mut next = boundary;
            if boundary > 0 {
                let scan_start =
                    super::floor_char_boundary(pending, boundary.saturating_sub(tail_limit));
                let suffix_lower = pending[scan_start..boundary].to_ascii_lowercase();
                for anchor in super::STREAMING_SECRET_ANCHORS {
                    let anchor_lower = anchor.to_ascii_lowercase();
                    for prefix_len in 1..anchor.len() {
                        if suffix_lower.ends_with(&anchor_lower[..prefix_len]) {
                            next = next.min(boundary - prefix_len);
                        }
                    }
                }
                if let Some(start) =
                    super::trailing_generic_token_or_secret_boundary_start(pending, boundary)
                {
                    next = next.min(start);
                }
            }

            if next == boundary {
                return boundary;
            }
            boundary = next;
        }
    }

    /// Reference implementation of the pre-ft-aznq6 anchor-occurrence rule:
    /// re-scan the windowed tail once per anchor, take the earliest last
    /// occurrence, and repeat from there.
    fn reference_anchor_occurrence_chain(
        pending: &str,
        current_boundary: usize,
        tail_limit: usize,
    ) -> usize {
        let mut boundary = current_boundary;
        loop {
            let mut earliest = boundary;
            if boundary > 0 && !pending.is_empty() {
                let scan_start =
                    super::floor_char_boundary(pending, boundary.saturating_sub(tail_limit));
                let suffix_lower = pending[scan_start..boundary].to_ascii_lowercase();
                for anchor in super::STREAMING_SECRET_ANCHORS {
                    let anchor_lower = anchor.to_ascii_lowercase();
                    if let Some(offset) = suffix_lower.rfind(anchor_lower.as_str()) {
                        earliest = earliest.min(scan_start + offset);
                    }
                }
            }
            if earliest == boundary {
                return boundary;
            }
            boundary = earliest;
        }
    }

    /// ft-aznq6: the precomputed-occurrence chain walk must land exactly where
    /// the repeated windowed `rfind` scan it replaced landed (no detections
    /// present, which is the case the coverage rule leaves untouched).
    #[test]
    fn ft_aznq6_anchor_chain_matches_repeated_window_scan() {
        let cases = [
            "",
            "no anchors at all: 12345",
            "one token here",
            "   Compiling frankenterm-core v0.12.0 (cached package, backtrace off)\n",
            "cached package backtrace cached package backtrace cached package",
            "sk-ant-api03-xxxx",
            "https://example.test/callback?code=abc",
            "AC AC AC AC AC AC AC AC",
            "trailing multibyte é and an ac before it",
        ];

        for case in cases {
            for tail_limit in [1_usize, 4, 16, super::STREAMING_ANCHOR_TAIL_FLOOR, 65536] {
                let mut streaming = StreamingRedactor::new()
                    .with_tail_bytes(tail_limit)
                    .with_max_pending_bytes(super::DEFAULT_STREAMING_REDACTOR_MAX_PENDING_BYTES);
                streaming.pending.push_lossy_decoded(case.as_bytes());
                let effective = streaming.effective_tail_bytes();

                let occurrences = super::streaming_anchor_occurrences(case);
                // Floor 0: this test pins the collapse against the loop it
                // replaced, so it must not also apply the ft-5lz32 clamp.
                let actual =
                    streaming.anchor_occurrence_chain_start(case.len(), &[], &occurrences, 0);
                let expected = reference_anchor_occurrence_chain(case, case.len(), effective);

                assert_eq!(
                    actual, expected,
                    "anchor chain diverged for {case:?} at tail_limit={tail_limit} \
                     (effective={effective})"
                );
            }
        }
    }

    /// ft-aznq6: the single-pass run collapse must land exactly where the
    /// per-byte walk it replaced landed. Retaining less would leak a split
    /// secret; retaining more would change where output is cut.
    #[test]
    fn ft_aznq6_run_collapse_matches_per_byte_walk() {
        let cases = [
            "",
            "a",
            "x",
            "1",
            "aaaa",
            "auth_t",
            "auth_token=",
            "aws_",
            "aws_secret_access_ke",
            "plain log line 42",
            "value ends in dash-",
            "value ends in underscore_",
            "-----BEGIN ",
            "----",
            "code=",
            "Authorization: Bearer ab",
            "trailing multibyte é",
            "é",
            "mixed AIza-aaa_bbb-",
            "DATADOG_API_KE",
            "no fragment here.",
            "ends with digit 7",
        ];

        for case in cases {
            for tail_limit in [
                0_usize,
                1,
                2,
                8,
                64,
                super::STREAMING_ANCHOR_TAIL_FLOOR,
                4096,
            ] {
                let expected = reference_per_byte_retention_walk(case, case.len(), tail_limit);
                // Floor 0: pins the collapse against the loop it replaced,
                // without the ft-5lz32 clamp.
                let actual = super::retainable_trailing_run_start(case, case.len(), tail_limit, 0);
                assert_eq!(
                    actual, expected,
                    "run collapse diverged from the per-byte walk for {case:?} \
                     at tail_limit={tail_limit}"
                );
            }
        }
    }

    /// ft-aznq6: a long run of anchor-initial bytes used to walk the emit
    /// boundary backwards one byte per fixed-point iteration, re-scanning the
    /// whole tail window each step (~4.6 MB scanned per byte of progress), so
    /// `print('a' * 1_000_000)` never returned from `redact_chunk`.
    #[test]
    fn ft_aznq6_long_anchor_initial_run_streams_in_bounded_time() {
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        // 'a' begins several anchors (api_key, apikey, access_token, …), so
        // every position in the run satisfies the partial-anchor rule.
        const CHUNK: usize = 4096;
        const TOTAL: usize = 200 * 1024;
        let chunk = "a".repeat(CHUNK);

        let mut streaming = StreamingRedactor::new();
        let started = std::time::Instant::now();
        let mut streamed = 0usize;
        for _ in 0..(TOTAL / CHUNK) {
            streamed += streaming.redact_chunk(chunk.as_bytes()).bytes.len();
        }
        let flushed = streaming.finish();
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "200 KiB of anchor-initial bytes must stream in bounded time; took {elapsed:?}"
        );
        assert_eq!(
            super::streaming_redactor_pending_overflow_count(),
            0,
            "200 KiB is far below the pending cap, so the weaker forced-emission \
             path must not fire"
        );
        assert_eq!(
            streamed + flushed.bytes.len(),
            TOTAL,
            "no output may be lost across the run"
        );
        // ft-5lz32: the run is retained only up to the tail window, so most of
        // it is emitted during streaming rather than held to finish().
        assert!(
            flushed.bytes.len() <= super::DEFAULT_STREAMING_REDACTOR_TAIL_BYTES,
            "retention must stay within the tail window; {} bytes were still \
             pending at finish()",
            flushed.bytes.len()
        );
    }

    /// ft-aznq6: same defect via the trailing-separator rule. A long `-----`
    /// separator line stepped back 5 bytes per iteration (prefixes of
    /// `-----BEGIN `), which is ~20k full-window rescans for 100k bytes.
    #[test]
    fn ft_aznq6_long_separator_run_streams_in_bounded_time() {
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        const CHUNK: usize = 4096;
        const TOTAL: usize = 200 * 1024;
        let chunk = "-".repeat(CHUNK);

        let mut streaming = StreamingRedactor::new();
        let started = std::time::Instant::now();
        let mut streamed = 0usize;
        for _ in 0..(TOTAL / CHUNK) {
            streamed += streaming.redact_chunk(chunk.as_bytes()).bytes.len();
        }
        let flushed = streaming.finish();
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "200 KiB of separator bytes must stream in bounded time; took {elapsed:?}"
        );
        assert_eq!(super::streaming_redactor_pending_overflow_count(), 0);
        assert_eq!(streamed + flushed.bytes.len(), TOTAL);
        assert!(flushed.bytes.len() <= super::DEFAULT_STREAMING_REDACTOR_TAIL_BYTES);
    }

    /// ft-5lz32: ordinary output must actually drain while streaming.
    ///
    /// Before the retention floor, anchor occurrences chained below the tail
    /// window on every chunk — `AC` alone occurs in `cached`, `package` and
    /// `backtrace` — so the emit boundary sat at 0 and 205 KiB of secret-free
    /// build output was still buffered at `finish()`. `pending` then grew to the
    /// 8 MiB cap on any real pane and took the forced-overflow path, whose own
    /// contract cannot guarantee partial secrets weren't emitted unredacted.
    #[test]
    fn ft_5lz32_ordinary_prose_drains_while_streaming() {
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        let line = "   Compiling frankenterm-core v0.12.0 (cached package, backtrace off)\n";
        let chunk = line.repeat(60);
        let total = chunk.len() * 50;

        let mut streaming = StreamingRedactor::new();
        let mut streamed_bytes = 0usize;
        let mut peak_pending = 0usize;
        for _ in 0..50 {
            streamed_bytes += streaming.redact_chunk(chunk.as_bytes()).bytes.len();
            peak_pending = peak_pending.max(streaming.pending_bytes());
        }
        let tail_cap = super::DEFAULT_STREAMING_REDACTOR_TAIL_BYTES;
        let flushed = streaming.finish();

        assert!(
            peak_pending <= tail_cap + chunk.len(),
            "retention must stay within the tail window (plus the in-flight chunk); \
             peak pending was {peak_pending} for a {tail_cap}-byte window"
        );
        assert!(
            streamed_bytes >= total / 2,
            "most of {total} bytes must be emitted during streaming, not held to \
             finish(); only {streamed_bytes} were emitted, {} flushed at the end",
            flushed.bytes.len()
        );
        assert_eq!(super::streaming_redactor_pending_overflow_count(), 0);
    }

    /// ft-5lz32: the retention floor must not open a leak. A secret that arrives
    /// after a long secret-free prefix — so the floor has been clamping for many
    /// chunks — must still be redacted, and the streamed output must equal what
    /// batch redaction produces for the same text.
    #[test]
    fn ft_5lz32_secret_after_clamped_prefix_is_still_redacted() {
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        let prefix =
            "   Compiling frankenterm-core v0.12.0 (cached package, backtrace off)\n".repeat(1200); // ~84 KiB: comfortably past the 64 KiB window.
        let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n";
        let full = format!("{prefix}{secret}");

        // The filler must itself be secret-free, so any difference from batch
        // output below is attributable to the clamp rather than to a
        // false-positive detection being split by a chunk boundary.
        assert_eq!(
            Redactor::new().redact(&prefix),
            prefix,
            "test filler must contain no detections"
        );

        let mut streaming = StreamingRedactor::new();
        let mut streamed = Vec::new();
        // Split every chunk boundary at 4 KiB, including inside the secret.
        for chunk in full.as_bytes().chunks(4096) {
            streamed.extend_from_slice(&streaming.redact_chunk(chunk).bytes);
        }
        streamed.extend_from_slice(&streaming.finish().bytes);

        let batch = Redactor::new().redact(&full);
        let streamed_text = String::from_utf8(streamed).expect("streamed output is utf8");

        assert!(
            !streamed_text.contains("sk-ant-api03-AAAA"),
            "the secret must not survive the clamped stream"
        );
        assert_eq!(
            streamed_text, batch,
            "streaming across a clamped prefix must match batch redaction"
        );
    }

    /// ft-5lz32: armoured blocks are exempt from the retention floor. A PEM
    /// private key is only detectable once its `-----END …-----` line arrives,
    /// so clamping retention inside the block would emit the key body in
    /// plaintext. Uses a small tail window so the block outgrows it cheaply.
    #[test]
    fn ft_5lz32_unterminated_armour_block_is_retained_past_the_floor() {
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        let body = "MIIEowIBAAKCAQEAvOhL0mE3sk9wQ\n".repeat(32); // ~928 bytes of base64
        let block =
            format!("-----BEGIN RSA PRIVATE KEY-----\n{body}-----END RSA PRIVATE KEY-----\n");

        let mut streaming = StreamingRedactor::new().with_tail_bytes(128);
        let mut streamed = Vec::new();
        for chunk in block.as_bytes().chunks(64) {
            streamed.extend_from_slice(&streaming.redact_chunk(chunk).bytes);
        }
        streamed.extend_from_slice(&streaming.finish().bytes);

        let streamed_text = String::from_utf8(streamed).expect("streamed output is utf8");
        assert!(
            !streamed_text.contains("MIIEowIBAAKCAQEAvOhL0mE3sk9wQ"),
            "an unterminated armour block must stay retained until its END line \
             arrives; got: {streamed_text}"
        );
        assert_eq!(streamed_text, Redactor::new().redact(&block));
    }

    /// ft-aznq6: cost of ordinary secret-free prose through the streaming path.
    ///
    /// This is the *other* rule in the same function: the anchor-occurrence scan
    /// pulls the boundary back to the earliest anchor occurrence in its window,
    /// one full-window rescan per occurrence. Short anchors like `AC` occur in
    /// ordinary English, so this measures the common path rather than an
    /// adversarial one. Prints its own timing so the cost is on the record; the
    /// assertion is a loose regression guard, not an SLO.
    #[test]
    fn ft_aznq6_ordinary_prose_streams_in_bounded_time() {
        let _guard = streaming_overflow_test_lock();
        super::reset_streaming_redactor_pending_overflow_count_for_test();

        // Realistic build/agent output: no secrets, but plenty of short-anchor
        // substrings ("ac" in "cache"/"package", "code=" absent, etc.).
        let line = "   Compiling frankenterm-core v0.12.0 (cached package, backtrace off)\n";
        let chunk = line.repeat(60); // ~4 KiB
        let total = chunk.len() * 50; // ~200 KiB

        let mut streaming = StreamingRedactor::new();
        let started = std::time::Instant::now();
        for _ in 0..50 {
            let _ = streaming.redact_chunk(chunk.as_bytes());
        }
        let flushed = streaming.finish();
        let elapsed = started.elapsed();

        eprintln!(
            "[ft-aznq6] ordinary prose: {} KiB in {elapsed:?} ({} bytes flushed at end)",
            total / 1024,
            flushed.bytes.len()
        );
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "200 KiB of ordinary prose must stream in bounded time; took {elapsed:?}"
        );
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

            assert!(
                super::streaming_redactor_pending_overflow_count() > 0,
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
    fn streaming_redactor_degenerate_pending_caps_handle_four_byte_prefix() {
        let _guard = streaming_overflow_test_lock();

        for requested_cap in [0, 1, 2] {
            super::reset_streaming_redactor_pending_overflow_count_for_test();

            let mut streaming = StreamingRedactor::new()
                .with_tail_bytes(64)
                .with_max_pending_bytes(requested_cap);

            let result =
                streaming.redact_chunk("\u{1F9EA}rk_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".as_bytes());

            assert!(
                super::streaming_redactor_pending_overflow_count() > 0,
                "br-ft-wjjkp.1: cap={requested_cap} must trip overflow telemetry"
            );
            assert!(
                !result.bytes.is_empty(),
                "br-ft-wjjkp.1: cap={requested_cap} must emit the leading UTF-8 scalar instead of a zero-byte drain"
            );
            assert!(
                String::from_utf8(result.bytes).is_ok(),
                "br-ft-wjjkp.1: forced overflow emission must preserve UTF-8 boundaries"
            );
            assert!(
                streaming.pending_bytes() <= super::MIN_STREAMING_REDACTOR_MAX_PENDING_BYTES,
                "br-ft-wjjkp.1: cap={requested_cap} left pending above normalized minimum: {}",
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
    fn redact_oauth_implicit_flow_fragment_token() {
        let r = redactor_with_named_markers();
        // OAuth implicit flow returns the token in the URL *fragment*, not a
        // query param. The old `[?&]`-only delimiter let this leak.
        for (raw, secret) in [
            (
                "https://app.example.com/cb#access_token=SECRETtok123456&token_type=bearer",
                "SECRETtok123456",
            ),
            (
                "https://app.example.com/cb#code=AUTHCODE7890",
                "AUTHCODE7890",
            ),
        ] {
            let msg = format!("redirected to {raw}");
            let out = r.redact(&msg);
            // The real security property: the secret value must not survive.
            // (Which named marker wins the overlap is an implementation detail.)
            assert!(
                !out.contains(secret),
                "OAuth fragment token leaked: {out:?}"
            );
            assert!(out.contains("[REDACTED"), "expected a redaction: {out:?}");
        }
        // A fragment with no token must NOT be redacted (no false positive).
        let plain = "see https://example.com/docs#installation for setup";
        assert_eq!(r.redact(plain), plain);
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
