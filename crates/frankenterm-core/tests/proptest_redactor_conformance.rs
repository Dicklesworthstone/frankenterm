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

use frankenterm_core::redactor::Redactor;
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

const ANTHROPIC_KEY: &str = "sk-ant-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
const OPENAI_KEY: &str = "sk-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHi";
const GITHUB_TOKEN: &str = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890ab";
const GITHUB_FINE_GRAINED_PAT: &str =
    "github_pat_11ABCDEFG0aBcDeFg_HiJkLmNoPqRsTuVwXyZ1234567890ABCDE";
const STRIPE_SK_LIVE: &str = "sk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
const STRIPE_RK_LIVE: &str = "rk_live_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
const STRIPE_WHSEC: &str = "whsec_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
const AWS_ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const SLACK_TOKEN: &str = "xoxb-1234567890-aBcDeFgHiJkLmNoPqRsTuVwX";

const ALL_KNOWN_FORMATS: &[(&str, &str)] = &[
    ("anthropic_key", ANTHROPIC_KEY),
    ("openai_key", OPENAI_KEY),
    ("github_token", GITHUB_TOKEN),
    ("github_fine_grained_pat", GITHUB_FINE_GRAINED_PAT),
    ("stripe_sk_live", STRIPE_SK_LIVE),
    ("stripe_rk_live", STRIPE_RK_LIVE),
    ("stripe_whsec", STRIPE_WHSEC),
    ("aws_access_key_id", AWS_ACCESS_KEY),
    ("slack_token", SLACK_TOKEN),
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

// ---------------------------------------------------------------------------
// Structural invariants — proptest
// ---------------------------------------------------------------------------

/// Generator that produces text mixing plain printable noise with
/// occasional catalog-format insertions. Captures the realistic
/// shape: log lines / pasted snippets that may or may not carry
/// secrets.
fn mixed_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            // Plain noise: short alphanumeric word.
            "[a-zA-Z0-9_]{1,16}".prop_map(String::from),
            // Whitespace / structural punctuation.
            Just(" ".to_string()),
            Just("\n".to_string()),
            Just("=".to_string()),
            Just(": ".to_string()),
            Just(", ".to_string()),
            // Catalog samples — mixed in at low probability via the
            // outer collection's variability.
            Just(ANTHROPIC_KEY.to_string()),
            Just(STRIPE_SK_LIVE.to_string()),
            Just(STRIPE_RK_LIVE.to_string()),
            Just(STRIPE_WHSEC.to_string()),
            Just(GITHUB_FINE_GRAINED_PAT.to_string()),
            Just(AWS_ACCESS_KEY.to_string()),
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
    assert!(out.contains(":anthropic_key]"), "expected pattern name in marker: {out:?}");
}
