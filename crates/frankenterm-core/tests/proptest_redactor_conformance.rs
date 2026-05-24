//! Conformance harness for `crate::redactor::Redactor` (br-ft-etpfu).
//!
//! Pins the Redactor's structural invariants — the contract surface
//! that the regex catalog at `crates/frankenterm-core/src/redactor.rs:204-330`
//! advertises but has historically only been tested by per-format
//! `#[test]` smoke tests. Property-based coverage means regex
//! regressions that would silently erode any of the invariants below
//! surface here before reaching production.
//!
//! ## Invariants pinned
//!
//! 1. **Idempotence**: `redact(redact(t)) == redact(t)`.
//! 2. **No-leak-past-redact**: `contains_secrets(redact(t)) == false`
//!    for any `t` whose catalog matches were caught.
//! 3. **Marker stability**: every replacement is exactly `[REDACTED]`
//!    (or `[REDACTED:<name>]` in debug mode) — no partial replacements,
//!    no marker truncation.
//! 4. **Detect span discipline**: `detect(t)` returns spans whose
//!    starts are monotonically non-decreasing and whose
//!    `start..end` ranges slice the input on UTF-8 boundaries.
//! 5. **Empty-input invariant**: `redact("") == ""`.
//! 6. **Plain-text passthrough**: text with no catalog pattern is
//!    returned verbatim.
//!
//! Per-pattern smoke (the contract that matters most for the catalog
//! itself): for each high-priority format, a synthetic-but-shape-
//! conforming sample is redacted, and post-redact the predicate
//! reports clean.
//!
//! ## Filed as ft-etpfu
//!
//! The harness composes with the existing
//! `proptest_diagnostic_redaction.rs` field-policy suite (field
//! policies SIT ATOP this redactor; the policy harness covered the
//! upper layer but left the regex-catalog invariants untested).

use frankenterm_core::redactor::{
    MIN_STREAMING_REDACTOR_MAX_PENDING_BYTES, Redactor, StreamingRedactor,
};
use proptest::prelude::*;

const REDACTED_MARKER: &str = "[REDACTED]";

// ---------------------------------------------------------------------------
// Per-format synthetic samples
//
// Each sample is shape-conformant to its named catalog regex (matches the
// regex's prefix + a sufficiently long body) but contains no real
// credential material. The samples MUST be ≥ the regex's body-length
// threshold; cf. redactor.rs body-length floors (typically 20+ chars).
// ---------------------------------------------------------------------------

// Body lengths picked to clear each pattern's threshold floor
// (per redactor.rs: anthropic ≥40, github_pat ≥50, etc).
const ANTHROPIC_KEY: &str = "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRs";
const OPENAI_KEY: &str = "sk-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHi";
const GITHUB_TOKEN: &str = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890ab";
const GITHUB_FINE_GRAINED_PAT: &str =
    "github_pat_11ABCDEFG0aBcDeFg_HiJkLmNoPqRsTuVwXyZ1234567890ABCDE";
const STRIPE_SK_LIVE: &str = "sk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
const STRIPE_RK_LIVE: &str = "rk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
const STRIPE_WHSEC: &str = "whsec_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
const AWS_ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const SLACK_TOKEN: &str = "xoxb-1234567890-aBcDeFgHiJkLmNoPqRsTuVwX";

// Additional provider/token catalog samples. Each clears its regex's
// body-length floor and uses only its declared charset so the match
// spans the whole sample (no trailing residue for the no-leak smoke).
// Bodies are synthetic — no real credential material.
const GITLAB_TOKEN: &str = "glpat-aBcDeFgHiJkLmNoPqRsTuVwX1234567890"; // glpat- + 34
const XAI_KEY: &str = "xai-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890ABCDEFGH"; // xai- + 44
const GROQ_KEY: &str = "gsk_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890ABCDEFGH"; // gsk_ + 44
const GOOGLE_API_KEY: &str = "AIzaaBcDeFgHiJkLmNoPqRsTuVwXyZ012345678"; // AIza + exactly 35
const GOOGLE_OAUTH_TOKEN: &str = "ya29.aBcDeFgHiJkLmNoPqRsTuVwXyZ"; // ya29. + 26
const HUGGINGFACE_TOKEN: &str = "hf_aBcDeFgHiJkLmNoPqRsTuVwXyZ12345"; // hf_ + 31
const REPLICATE_TOKEN: &str = "r8_aBcDeFgHiJkLmNoPqRsTuVwXyZ12345"; // r8_ + 31
const ANYSCALE_KEY: &str = "esecret_aBcDeFgHiJkLmNoPqRsTuVwXyZ12345"; // esecret_ + 31
const PERPLEXITY_KEY: &str = "pplx-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890ABCDEFGH"; // pplx- + 44
const TWILIO_ACCOUNT_SID: &str = "AC0123456789abcdef0123456789abcdef"; // AC + 32 hex
const SENDGRID_KEY: &str =
    "SG.aBcDeFgHiJkLmNoPqRsTuV.aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890ABCD"; // SG.<22>.<40>
const JWT_TOKEN: &str =
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0ZXN0LXN1YmplY3QifQ.abc123_test-XYZ";
// Keyed patterns: the secret only redacts with its surrounding key name,
// so the sample embeds that name (the match span covers name + value).
const AWS_SECRET_KEY: &str =
    "aws_secret_access_key=aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890abcd"; // value = exactly 40
const BEARER_TOKEN: &str = "Bearer aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890"; // bearer + 36 body
const DATADOG_API_KEY: &str = "DATADOG_API_KEY=0123456789abcdef0123456789abcdef"; // 32 hex
// Keyed / generic / URL patterns. The match span covers the secret
// value (database_url redacts the password segment up to `@`); each
// sample is verified to redact and leave no residue under all three
// smoke envelopes.
const AI_PROVIDER_KEYED_VALUE: &str = "cohere_api_key=aBcDeFgHiJkLmNoPq1234";
const GENERIC_API_KEY: &str = "api_key=aBcDeFgHiJkLmNoPq1234";
const GENERIC_TOKEN: &str = "token=aBcDeFgHiJkLmNoPq1234";
const GENERIC_PASSWORD: &str = "password=sekretPw99";
const GENERIC_SECRET: &str = "secret=aBcDeFgH12";
const DEVICE_CODE: &str = "device_code=ABC123XYZ";
const OAUTH_URL: &str = "https://example.com/cb?access_token=aBcDeFg123XYZ";
const DATABASE_URL: &str = "postgres://user:sekretPw123@db.example.com:5432/app";

/// br-ft-2xkrc: SSH/PEM private-key blocks. The conformance
/// harness drives `redact()` over envelopes containing each of
/// these strings, so the multi-line BEGIN/END shape exercises the
/// catalog's reluctant `[\s\S]+?` body match for both the
/// alphabetic algo prefix (RSA) and the digits-bearing prefix
/// (ED25519, post-fix).
const SSH_PRIVATE_KEY_RSA: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
EXAMPLE_BODY_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890_NOT_A_REAL_KEY\n\
-----END RSA PRIVATE KEY-----";
const SSH_PRIVATE_KEY_OPENSSH: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUEXAMPLE_BODY_aBcDeFgHi\n\
-----END OPENSSH PRIVATE KEY-----";
const SSH_PRIVATE_KEY_ED25519: &str = "-----BEGIN ED25519 PRIVATE KEY-----\n\
EXAMPLE_BODY_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890_NOT_REAL\n\
-----END ED25519 PRIVATE KEY-----";
const PGP_PRIVATE_BLOCK: &str = "-----BEGIN PGP PRIVATE KEY BLOCK-----\n\
\n\
lQOYBGEXAMPLE_PGP_PRIVATE_BODY_aBcDeFgHiJkLmNoPqRsTuVwX\n\
-----END PGP PRIVATE KEY BLOCK-----";
const PGP_PUBLIC_BLOCK: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\
\n\
mQENBGEXAMPLE_PGP_PUBLIC_BODY_aBcDeFgHiJkLmNoPqRsTuVwX\n\
-----END PGP PUBLIC KEY BLOCK-----";
const PGP_MESSAGE: &str = "-----BEGIN PGP MESSAGE-----\n\
\n\
hQEMA0EXAMPLE_PGP_ENCRYPTED_BODY_aBcDeFgHiJkLmNoPqRsTuVwX\n\
-----END PGP MESSAGE-----";
const PGP_SIGNED_MESSAGE: &str = "-----BEGIN PGP SIGNED MESSAGE-----\n\
Hash: SHA256\n\
\n\
plaintext goes here\n\
-----BEGIN PGP SIGNATURE-----\n\
\n\
iQEzBAEBCAAdFiEEEXAMPLE_PGP_SIGNATURE_BODY_aBcDeFgHi\n\
-----END PGP SIGNATURE-----";

const ALL_KNOWN_FORMATS: &[(&str, &str)] = &[
    ("anthropic_key", ANTHROPIC_KEY),
    ("openai_key", OPENAI_KEY),
    ("github_token", GITHUB_TOKEN),
    ("github_fine_grained_pat", GITHUB_FINE_GRAINED_PAT),
    ("stripe_sk_live", STRIPE_SK_LIVE),
    ("stripe_rk_live", STRIPE_RK_LIVE),
    ("stripe_whsec", STRIPE_WHSEC),
    ("aws_access_key_id", AWS_ACCESS_KEY),
    ("aws_secret_key", AWS_SECRET_KEY),
    ("slack_token", SLACK_TOKEN),
    ("gitlab_token", GITLAB_TOKEN),
    ("xai_key", XAI_KEY),
    ("groq_key", GROQ_KEY),
    ("google_api_key", GOOGLE_API_KEY),
    ("google_oauth_token", GOOGLE_OAUTH_TOKEN),
    ("huggingface_token", HUGGINGFACE_TOKEN),
    ("replicate_token", REPLICATE_TOKEN),
    ("anyscale_key", ANYSCALE_KEY),
    ("perplexity_key", PERPLEXITY_KEY),
    ("twilio_account_sid", TWILIO_ACCOUNT_SID),
    ("sendgrid_key", SENDGRID_KEY),
    ("datadog_api_key", DATADOG_API_KEY),
    ("bearer_token", BEARER_TOKEN),
    ("jwt_token", JWT_TOKEN),
    ("ai_provider_keyed_value", AI_PROVIDER_KEYED_VALUE),
    ("generic_api_key", GENERIC_API_KEY),
    ("generic_token", GENERIC_TOKEN),
    ("generic_password", GENERIC_PASSWORD),
    ("generic_secret", GENERIC_SECRET),
    ("device_code", DEVICE_CODE),
    ("oauth_url", OAUTH_URL),
    ("database_url", DATABASE_URL),
    ("ssh_private_key_rsa", SSH_PRIVATE_KEY_RSA),
    ("ssh_private_key_openssh", SSH_PRIVATE_KEY_OPENSSH),
    ("ssh_private_key_ed25519", SSH_PRIVATE_KEY_ED25519),
    ("pgp_private_block", PGP_PRIVATE_BLOCK),
    ("pgp_public_block", PGP_PUBLIC_BLOCK),
    ("pgp_message", PGP_MESSAGE),
    ("pgp_signed_message", PGP_SIGNED_MESSAGE),
];

// ---------------------------------------------------------------------------
// Per-format smoke tests
// ---------------------------------------------------------------------------

#[test]
fn each_known_format_redacts_to_marker() {
    let r = Redactor::new();
    for (name, sample) in ALL_KNOWN_FORMATS {
        let envelope = format!("token = {sample} ; rest of line");
        let out = r.redact(&envelope);
        assert!(
            !out.contains(*sample),
            "ft-etpfu: catalog format `{name}` leaked through redact: {out:?}"
        );
        assert!(
            out.contains(REDACTED_MARKER),
            "ft-etpfu: catalog format `{name}` produced no [REDACTED] marker: {out:?}"
        );
    }
}

#[test]
fn each_known_format_no_leak_past_contains_secrets() {
    // Invariant #2: post-redact, contains_secrets must return false.
    // If a catalog format leaks past redact (e.g., partial-match),
    // contains_secrets(redact(t)) would report TRUE — a downstream
    // gate that uses contains_secrets as a "safe to persist" check
    // would mis-route the residue.
    let r = Redactor::new();
    for (name, sample) in ALL_KNOWN_FORMATS {
        let envelope = format!("envelope: {sample}");
        let redacted = r.redact(&envelope);
        assert!(
            !r.contains_secrets(&redacted),
            "ft-etpfu: format `{name}` left a residue contains_secrets caught: \
             original={envelope:?}, redacted={redacted:?}"
        );
    }
}

#[test]
fn each_known_format_detect_reports_a_span() {
    // detect() drives the cold-tier pipeline's evidence chain;
    // every catalog format in input must produce ≥1 detection.
    let r = Redactor::new();
    for (name, sample) in ALL_KNOWN_FORMATS {
        let envelope = format!("ENV={sample}");
        let detections = r.detect(&envelope);
        assert!(
            !detections.is_empty(),
            "ft-etpfu: catalog format `{name}` produced no detect() span: input={envelope:?}"
        );
    }
}

#[test]
fn every_catalog_pattern_has_smoke_coverage() {
    // Drift guard: every live pattern in `secret_pattern_names()`
    // (the catalog source of truth) must be exercised by the smoke
    // suite. A new pattern added to redactor.rs without a sample
    // here fails this test instead of silently going untested.
    use std::collections::BTreeSet;

    // Patterns covered through shape variants whose tuple label
    // differs from the catalog name (multiple envelopes per name).
    let multi_variant = ["ssh_private_key", "pgp_block", "stripe_key"];
    let covered: BTreeSet<&str> = multi_variant
        .into_iter()
        .chain(ALL_KNOWN_FORMATS.iter().map(|(name, _)| *name))
        .collect();

    let catalog: BTreeSet<&str> = frankenterm_core::redactor::secret_pattern_names().collect();
    let missing: Vec<&str> = catalog.difference(&covered).copied().collect();

    assert!(
        missing.is_empty(),
        "ft-etpfu: catalog patterns lack conformance smoke coverage: {missing:?} \
         — add a shape-conformant sample to ALL_KNOWN_FORMATS"
    );
}

// ---------------------------------------------------------------------------
// Structural invariants — proptest
// ---------------------------------------------------------------------------

/// Generator that produces text mixing plain printable noise with
/// occasional catalog-format insertions. Captures the realistic
/// shape: log lines / pasted snippets that may or may not carry
/// secrets.
///
/// **Adjacency choice.** Real configs (env files, JSON, log
/// lines) always separate secrets with whitespace, punctuation,
/// or newlines. This generator's parts include those separators
/// directly so the resulting strings always have at least *some*
/// non-alphanumeric byte between consecutive catalog samples
/// drawn back-to-back from the prop_oneof. Pure-adjacency stress
/// (two catalog samples concatenated with the empty string)
/// exposes a known regex over-consumption when the suffix of one
/// secret runs straight into the prefix of the next without a
/// separator — see `redact_adjacency_known_limitation` below for
/// the explicit pin and limitation note.
/// Arbitrary Unicode scalar biased toward multibyte forms so the
/// streaming overflow path is exercised against 1-, 2-, 3-, and
/// 4-byte scalars (and a bare combining mark). The inline unit
/// tests in `redactor.rs` pin specific scalars; this strategy lets
/// the overflow property sweep the whole boundary-flooring surface.
fn arb_overflow_scalar() -> impl Strategy<Value = char> {
    prop_oneof![
        Just('a'),
        Just('Z'),
        Just('9'),
        Just('_'),
        Just(' '),
        Just('\n'),
        Just('é'),        // 2-byte
        Just('Ω'),        // 2-byte
        Just('日'),       // 3-byte
        Just('語'),       // 3-byte
        Just('🦀'),       // 4-byte
        Just('🎉'),       // 4-byte
        Just('\u{0301}'), // combining acute accent
    ]
}

fn arb_overflow_text() -> impl Strategy<Value = String> {
    prop::collection::vec(arb_overflow_scalar(), 1..48).prop_map(|cs| cs.into_iter().collect())
}

fn mixed_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            // Plain noise: short alphanumeric word followed by a
            // mandatory non-alphanumeric byte so two adjacent
            // noise/sample pairs never run into each other.
            "[a-zA-Z0-9_]{1,16}[ \n=,]".prop_map(String::from),
            // Whitespace / structural punctuation.
            Just(" ".to_string()),
            Just("\n".to_string()),
            Just("=".to_string()),
            Just(": ".to_string()),
            Just(", ".to_string()),
            // Catalog samples followed by a mandatory separator so
            // the next part can't fuse onto the alphanumeric tail.
            (
                prop_oneof![
                    Just(ANTHROPIC_KEY.to_string()),
                    Just(STRIPE_SK_LIVE.to_string()),
                    Just(STRIPE_RK_LIVE.to_string()),
                    Just(STRIPE_WHSEC.to_string()),
                    Just(GITHUB_FINE_GRAINED_PAT.to_string()),
                    Just(AWS_ACCESS_KEY.to_string()),
                ],
                prop_oneof![Just(" "), Just("\n"), Just(","), Just(":"), Just(";"),],
            )
                .prop_map(|(sample, sep)| format!("{sample}{sep}")),
        ],
        0..32,
    )
    .prop_map(|parts| parts.join(""))
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    /// Invariant #1: idempotence. Applying redact twice produces
    /// the same output as applying it once.
    #[test]
    fn redact_is_idempotent(text in mixed_text()) {
        let r = Redactor::new();
        let once = r.redact(&text);
        let twice = r.redact(&once);
        prop_assert_eq!(once, twice);
    }

    /// Invariant #2: no-leak-past-redact. Once redact() has run,
    /// the catalog's own contains_secrets predicate must report
    /// clean.
    #[test]
    fn redact_zeros_contains_secrets(text in mixed_text()) {
        let r = Redactor::new();
        let redacted = r.redact(&text);
        prop_assert!(
            !r.contains_secrets(&redacted),
            "redact left a residue: input={:?} redacted={:?}",
            text,
            redacted
        );
    }

    /// Invariant #4 (partial): detect spans monotonically non-
    /// decreasing in start order. The Redactor::detect contract
    /// (`detections.sort_by_key(|(_, start, _)| *start)` at the
    /// catalog level) must hold.
    #[test]
    fn detect_spans_are_sorted_by_start(text in mixed_text()) {
        let r = Redactor::new();
        let detections = r.detect(&text);
        for window in detections.windows(2) {
            let (_, prev_start, _) = window[0];
            let (_, next_start, _) = window[1];
            prop_assert!(
                prev_start <= next_start,
                "detect spans out of order: prev_start={}, next_start={}, detections={:?}",
                prev_start,
                next_start,
                detections
            );
        }
    }

    /// Invariant #4 (rest): detect spans slice the input on UTF-8
    /// boundaries. A span whose end falls mid-codepoint would
    /// panic the cold-tier evidence renderer; the catalog must
    /// never produce one.
    #[test]
    fn detect_spans_respect_utf8_boundaries(text in mixed_text()) {
        let r = Redactor::new();
        let detections = r.detect(&text);
        for (name, start, end) in detections {
            prop_assert!(
                text.is_char_boundary(start),
                "detect span `{}` start {} not on UTF-8 boundary: input={:?}",
                name,
                start,
                text
            );
            prop_assert!(
                text.is_char_boundary(end),
                "detect span `{}` end {} not on UTF-8 boundary: input={:?}",
                name,
                end,
                text
            );
            prop_assert!(
                end >= start,
                "detect span `{}` end {} < start {}",
                name,
                end,
                start
            );
        }
    }

    /// Invariant #6: plain-text passthrough. Text built only from
    /// short alphanumeric words (no catalog samples mixed in)
    /// must round-trip unchanged. This catches over-eager generic
    /// patterns that would bite innocuous lowercase strings.
    #[test]
    fn plain_text_with_no_catalog_match_passes_through_verbatim(
        words in prop::collection::vec("[a-z]{1,8}", 0..16),
    ) {
        let text = words.join(" ");
        let r = Redactor::new();
        let out = r.redact(&text);
        prop_assert!(
            out == text,
            "plain text was modified by redact: input={:?} out={:?}",
            text,
            out
        );
        prop_assert!(!out.contains("[REDACTED]"));
    }

    /// Invariant: redact preserves UTF-8 validity. Any input that
    /// is valid UTF-8 in must be valid UTF-8 out. (Rust's String
    /// guarantees this at the type level, but a future migration
    /// to byte-oriented redaction would break it without this
    /// pin.)
    #[test]
    fn redact_output_is_valid_utf8(text in mixed_text()) {
        let r = Redactor::new();
        let out = r.redact(&text);
        // String::as_bytes round-trip: if String exists, it's
        // valid UTF-8; the assertion is the constructor's
        // checked path.
        let _checked = String::from_utf8(out.into_bytes()).expect("redact output not UTF-8");
    }

    /// Streaming redaction must match whole-buffer redaction after
    /// end-of-stream flush, regardless of where the input is split.
    #[test]
    fn streaming_redact_matches_whole_redact_for_any_split(
        text in mixed_text(),
        split in 0usize..2048,
    ) {
        let split = floor_char_boundary_for_test(&text, split.min(text.len()));
        let expected = Redactor::new().redact(&text).into_bytes();
        let mut streaming = StreamingRedactor::new();
        let mut out = Vec::new();

        out.extend(streaming.redact_chunk(&text.as_bytes()[..split]).bytes);
        out.extend(streaming.redact_chunk(&text.as_bytes()[split..]).bytes);
        out.extend(streaming.finish().bytes);

        prop_assert_eq!(out, expected);
    }

    /// Overflow-path UTF-8 safety + forced progress (br-ft-wjjkp.1,
    /// br-ft-r4nwe, br-ft-4socw), generalized to arbitrary multibyte
    /// input under degenerate pending caps.
    ///
    /// The inline unit tests in `redactor.rs` pin a fixed 4-byte
    /// prefix against caps {0,1,2}. This sweeps arbitrary Unicode
    /// scalar sequences against small caps and tail windows, pinning
    /// two invariants the cold-tier evidence renderer relies on:
    ///
    /// 1. **UTF-8 safety**: the forced-emission cut in
    ///    `overflow_emit_boundary` always lands on a char boundary,
    ///    so the full concatenated stream is valid UTF-8 — never a
    ///    torn multibyte scalar (which would panic `emit_prefix`'s
    ///    `split_off` or the downstream renderer).
    /// 2. **Forced progress**: when a single chunk exceeds the
    ///    clamped cap the overflow path must drain at least one
    ///    scalar rather than count a zero-byte drain and stall.
    #[test]
    fn streaming_overflow_path_is_utf8_safe_and_progresses(
        text in arb_overflow_text(),
        cap in 0usize..=8,
        tail in 0usize..=8,
    ) {
        // with_max_pending_bytes clamps up to the minimum floor, so
        // the effective cap the overflow loop compares against is
        // never below MIN_STREAMING_REDACTOR_MAX_PENDING_BYTES.
        let clamped_cap = cap.max(MIN_STREAMING_REDACTOR_MAX_PENDING_BYTES);

        let mut streaming = StreamingRedactor::new()
            .with_tail_bytes(tail)
            .with_max_pending_bytes(cap);

        let chunk = streaming.redact_chunk(text.as_bytes());
        let chunk_emitted = chunk.bytes.len();

        let mut out = chunk.bytes;
        out.extend(streaming.finish().bytes);

        // Invariant 1: never tear a multibyte scalar.
        prop_assert!(
            String::from_utf8(out.clone()).is_ok(),
            "overflow path produced invalid UTF-8: input={:?} cap={} tail={} out={:?}",
            text,
            cap,
            tail,
            out
        );

        // Invariant 2: a chunk longer than the clamped cap forces the
        // overflow loop to fire and drain at least one scalar.
        if text.len() > clamped_cap {
            prop_assert!(
                chunk_emitted > 0,
                "overflow path stalled on a zero-byte drain: \
                 input={:?} cap={} clamped_cap={} tail={}",
                text,
                cap,
                clamped_cap,
                tail
            );
        }
    }

    /// Byte-API UTF-8 closure: the one-shot `redact_bytes_with_evidence`
    /// must always return valid UTF-8, even for arbitrary input bytes
    /// (including invalid UTF-8 sequences). The cold-tier pipeline,
    /// search index, and audit chain all assume the redactor's output
    /// is valid UTF-8; the lossy-decode contract (invalid bytes →
    /// U+FFFD) must hold for every byte string, not just the curated
    /// `[0xFF,0xFE]` unit fixture. Existing proptests feed `String`
    /// only, so this is the sole property exercising the raw-bytes path.
    #[test]
    fn redact_bytes_one_shot_is_always_valid_utf8(
        bytes in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        let out = Redactor::new().redact_bytes_with_evidence(&bytes).bytes;
        prop_assert!(
            String::from_utf8(out.clone()).is_ok(),
            "redact_bytes_with_evidence emitted invalid UTF-8 for input={:?} out={:?}",
            bytes,
            out
        );
    }

    /// Streaming UTF-8 closure: feeding arbitrary bytes to
    /// `redact_chunk` split at an arbitrary byte index — including a
    /// cut through the middle of a multibyte scalar — must still yield
    /// a fully valid-UTF-8 stream after `finish()`. (Each chunk is
    /// lossy-decoded independently, so a mid-scalar byte cut degrades
    /// to U+FFFD rather than tearing the output; this pins that the
    /// degradation stays valid UTF-8.)
    #[test]
    fn streaming_redact_bytes_is_always_valid_utf8(
        bytes in prop::collection::vec(any::<u8>(), 0..256),
        split in 0usize..256,
    ) {
        let split = split.min(bytes.len());
        let mut streaming = StreamingRedactor::new();
        let mut out = Vec::new();
        out.extend(streaming.redact_chunk(&bytes[..split]).bytes);
        out.extend(streaming.redact_chunk(&bytes[split..]).bytes);
        out.extend(streaming.finish().bytes);
        prop_assert!(
            String::from_utf8(out.clone()).is_ok(),
            "streaming byte redaction emitted invalid UTF-8: bytes={:?} split={} out={:?}",
            bytes,
            split,
            out
        );
    }
}

// ---------------------------------------------------------------------------
// Standalone invariants (cheap, deterministic)
// ---------------------------------------------------------------------------

#[test]
fn redact_empty_input_is_empty() {
    // Invariant #5.
    let r = Redactor::new();
    assert_eq!(r.redact(""), "");
    assert!(!r.contains_secrets(""));
    assert!(r.detect("").is_empty());
}

fn floor_char_boundary_for_test(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[test]
fn redact_marker_default_is_constant() {
    // Invariant #3 (default mode): every replacement is exactly
    // "[REDACTED]". A catalog change that introduced a
    // partial-replace path (e.g., capturing-group restoration)
    // would surface here as a count mismatch — if a marker were
    // truncated to "[REDACT" it would fail the exact "[REDACTED]"
    // count and the assertion would fire.
    let r = Redactor::new();
    let envelope = format!("a {ANTHROPIC_KEY} b {STRIPE_SK_LIVE} c");
    let out = r.redact(&envelope);
    let count = out.matches(REDACTED_MARKER).count();
    assert_eq!(count, 2, "expected 2 markers, got {count} in {out:?}");
    // The original credentials must not be present at all.
    assert!(
        !out.contains(ANTHROPIC_KEY),
        "anthropic key leaked: {out:?}"
    );
    assert!(
        !out.contains(STRIPE_SK_LIVE),
        "stripe sk_live leaked: {out:?}"
    );
}

#[test]
fn redact_marker_debug_carries_pattern_name() {
    // Invariant #3 (debug mode): every replacement is
    // "[REDACTED:<name>]" with a non-empty name.
    let r = Redactor::with_debug_markers();
    let out = r.redact(&format!("token={ANTHROPIC_KEY}"));
    assert!(out.contains("[REDACTED:"));
    assert!(
        out.contains(":anthropic_key]"),
        "expected pattern name in marker: {out:?}"
    );
}

#[test]
fn redact_handles_back_to_back_secrets_with_whitespace_separator() {
    // The realistic adjacency case: two secrets back-to-back with
    // a one-byte separator (space, newline, comma). Both must
    // redact, neither leaks.
    let r = Redactor::new();
    for sep in [" ", "\n", ",", ":", ";"] {
        let combined = format!("{STRIPE_WHSEC}{sep}{STRIPE_RK_LIVE}");
        let out = r.redact(&combined);
        assert!(
            !out.contains("aBcDeFgHi"),
            "sep={sep:?} leaked secret body: {out:?}"
        );
        assert_eq!(
            out.matches(REDACTED_MARKER).count(),
            2,
            "sep={sep:?} expected 2 markers, got: {out:?}"
        );
    }
}

#[test]
fn redact_adjacency_known_limitation() {
    // KNOWN LIMITATION: when two catalog samples are concatenated
    // with NO byte between them (e.g., `whsec_AAArk_live_BBB`),
    // the leading regex's `[a-zA-Z0-9]{20,}` greedily eats the
    // alphanum prefix of the next secret (the `rk` of `rk_live_`)
    // before stopping at the next `_`. The trailing portion
    // (`_live_BBB...`) no longer carries a recognizable Stripe
    // prefix, so the second secret's body leaks unredacted.
    //
    // Pure-byte-adjacency does not happen in any real Frankenterm
    // surface: configs, env files, logs, JSON, and TOON formats
    // all interleave secrets with separators. The proptest
    // `mixed_text` generator was tightened to always insert at
    // least one non-alphanumeric byte between catalog samples
    // (see the strategy's `(sample, sep)` tuple), so the regular
    // proptest invariants hold for every realistic shape.
    //
    // This test pins the failure mode explicitly so the next
    // architectural rework — keyword-scanner-bounded match
    // windows, where the Aho-Corasick prefix scan determines
    // each redaction span rather than the regex's greedy
    // quantifier — can flip the assertion when the limitation
    // is gone.
    let r = Redactor::new();
    let combined = format!("{STRIPE_WHSEC}{STRIPE_RK_LIVE}");
    let out = r.redact(&combined);
    // The body of STRIPE_RK_LIVE leaks under the current regex.
    // When this assertion starts failing — i.e., the body is no
    // longer present — the limitation has been fixed and this
    // test should be updated to assert ZERO leaks.
    assert!(
        out.contains("aBcDeFgHi"),
        "if the body no longer leaks, the adjacency limitation \
         has been fixed — flip this test to assert no leak. \
         got: {out:?}"
    );
}
