//! Property-based tests for the vendored WezTerm integration module.
//!
//! Tests invariants of WeztermVersion parsing, VendoredCompatibilityStatus
//! serde, VendoredCompatibilityReport roundtrips, and commit extraction logic.

#![cfg(feature = "vendored")]

use frankenterm_core::vendored::{
    VendoredCompatibilityReport, VendoredCompatibilityStatus, VendoredWeztermMetadata,
    WeztermVersion,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_compat_status() -> impl Strategy<Value = VendoredCompatibilityStatus> {
    prop_oneof![
        Just(VendoredCompatibilityStatus::Matched),
        Just(VendoredCompatibilityStatus::Compatible),
        Just(VendoredCompatibilityStatus::Incompatible),
    ]
}

/// Generate a hex string of given length range (valid git commit hash fragment).
fn arb_hex_string(min_len: usize, max_len: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(
        proptest::char::range('0', 'f').prop_filter("hex only", |c| c.is_ascii_hexdigit()),
        min_len..=max_len,
    )
    .prop_map(|chars: Vec<char>| chars.into_iter().collect::<String>())
}

/// Generate a hex string with at least one a-f character (recognized as a commit).
fn arb_commit_hash() -> impl Strategy<Value = String> {
    (arb_hex_string(7, 40), 0usize..6).prop_map(|(mut hash, letter_idx)| {
        let hex_letters: [u8; 6] = [b'a', b'b', b'c', b'd', b'e', b'f'];
        let pos = letter_idx % hash.len();
        let letter = hex_letters[letter_idx % hex_letters.len()];
        let mut bytes = hash.into_bytes();
        bytes[pos] = letter;
        hash = String::from_utf8(bytes).unwrap();
        hash
    })
}

/// Generate a pure-numeric string (will NOT be treated as a commit hash).
fn arb_numeric_date() -> impl Strategy<Value = String> {
    (2020u32..2030, 1u32..13, 1u32..29)
        .prop_map(|(y, m, d)| format!("{y}{m:02}{d:02}"))
}

/// Generate WezTerm-style version strings.
fn arb_wezterm_version() -> impl Strategy<Value = String> {
    prop_oneof![
        // "wezterm <date>-<time>-<commit>"
        (arb_numeric_date(), arb_commit_hash()).prop_map(|(date, commit)| {
            format!("wezterm {date}-110809-{commit}")
        }),
        // "wezterm <date>" (no commit)
        arb_numeric_date().prop_map(|date| format!("wezterm {date}")),
        // "wezterm-gui 0.0.0+<commit>"
        arb_commit_hash().prop_map(|commit| format!("wezterm-gui 0.0.0+{commit}")),
        // empty string
        Just(String::new()),
    ]
}

fn arb_compat_report() -> impl Strategy<Value = VendoredCompatibilityReport> {
    (
        arb_compat_status(),
        any::<bool>(),
        any::<bool>(),
        prop::option::of("[a-z0-9. -]{1,30}"),
        prop::option::of(arb_hex_string(7, 40)),
        prop::option::of(arb_hex_string(7, 40)),
        prop::option::of("[0-9]+\\.[0-9]+\\.[0-9]+"),
        "[a-zA-Z ]{1,50}",
        prop::option::of("[a-zA-Z ]{1,50}"),
    )
        .prop_map(
            |(
                status,
                vendored_enabled,
                allow_vendored,
                local_version,
                local_commit,
                vendored_commit,
                vendored_version,
                message,
                recommendation,
            )| {
                VendoredCompatibilityReport {
                    status,
                    vendored_enabled,
                    allow_vendored,
                    local_version,
                    local_commit,
                    vendored_commit,
                    vendored_version,
                    message,
                    recommendation,
                }
            },
        )
}

// ── WeztermVersion::parse ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// parse() never panics on arbitrary input.
    #[test]
    fn parse_never_panics(raw in "[\\x00-\\x7f]{0,100}") {
        let _ = WeztermVersion::parse(&raw);
    }

    /// parse() trims whitespace from raw.
    #[test]
    fn parse_trims_whitespace(raw in "[a-z0-9 -]{0,40}") {
        let padded = format!("  {raw}  ");
        let v = WeztermVersion::parse(&padded);
        prop_assert_eq!(v.raw.as_str(), padded.trim(), "raw should be trimmed");
    }

    /// parse() preserves raw string (trimmed).
    #[test]
    fn parse_preserves_raw(raw in "[a-z0-9 -]{0,60}") {
        let v = WeztermVersion::parse(&raw);
        prop_assert_eq!(v.raw.as_str(), raw.trim());
    }

    /// Parsing the same string twice gives equal results.
    #[test]
    fn parse_is_deterministic(raw in arb_wezterm_version()) {
        let v1 = WeztermVersion::parse(&raw);
        let v2 = WeztermVersion::parse(&raw);
        prop_assert_eq!(v1, v2, "parse should be deterministic");
    }
}

// ── Commit extraction invariants ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// When a commit hash is in the version, the extracted commit has expected properties.
    #[test]
    fn parse_commit_properties(
        date in arb_numeric_date(),
        commit in arb_commit_hash(),
    ) {
        let raw = format!("wezterm {date}-110809-{commit}");
        let v = WeztermVersion::parse(&raw);
        match v.commit {
            Some(extracted) => {
                // Extracted commit is lowercase
                let lower = extracted.to_lowercase();
                prop_assert_eq!(extracted.as_str(), lower.as_str(),
                    "commit should be lowercase");
                // Extracted commit is all hex
                prop_assert!(extracted.chars().all(|c: char| c.is_ascii_hexdigit()),
                    "commit should be all hex: {}", extracted);
                // Extracted commit has at least 7 chars
                prop_assert!(extracted.len() >= 7,
                    "commit should be >= 7 chars: {}", extracted);
                // Extracted commit has at least one a-f character
                prop_assert!(extracted.chars().any(|c: char| matches!(c, 'a'..='f')),
                    "commit should have hex letter: {}", extracted);
            }
            None => {
                // If no commit extracted, the input commit must have failed validation
                // (e.g., all digits, too short, etc.)
            }
        }
    }

    /// Pure numeric tokens are never extracted as commits.
    #[test]
    fn parse_ignores_pure_numeric(date in arb_numeric_date()) {
        let raw = format!("wezterm {date}");
        let v = WeztermVersion::parse(&raw);
        prop_assert!(v.commit.is_none(),
            "pure numeric '{}' should not yield commit, got: {:?}", date, v.commit);
    }

    /// Extracted commits are always lowercase.
    #[test]
    fn parse_commit_is_lowercase(raw in arb_wezterm_version()) {
        let v = WeztermVersion::parse(&raw);
        if let Some(commit) = v.commit {
            let lower = commit.to_lowercase();
            prop_assert_eq!(commit.as_str(), lower.as_str(),
                "commit should be lowercase");
        }
    }

    /// Extracted commits are always >= 7 hex chars.
    #[test]
    fn parse_commit_min_length(raw in arb_wezterm_version()) {
        let v = WeztermVersion::parse(&raw);
        if let Some(commit) = v.commit {
            prop_assert!(commit.len() >= 7,
                "commit length {} < 7", commit.len());
        }
    }

    /// Extracted commits contain at least one a-f character.
    #[test]
    fn parse_commit_has_hex_letter(raw in arb_wezterm_version()) {
        let v = WeztermVersion::parse(&raw);
        if let Some(commit) = v.commit {
            prop_assert!(commit.chars().any(|c: char| matches!(c, 'a'..='f')),
                "commit should have hex letter: {}", commit);
        }
    }

    /// Git source URLs with long hashes are extracted.
    #[test]
    fn parse_git_source_url(commit in arb_hex_string(40, 40)) {
        // Ensure it has at least one hex letter
        prop_assume!(commit.chars().any(|c: char| matches!(c, 'a'..='f')));
        let url = format!("git+https://github.com/wez/wezterm#{commit}");
        let v = WeztermVersion::parse(&url);
        prop_assert!(v.commit.is_some(),
            "should extract commit from git URL, raw: {}", url);
    }
}

// ── WeztermVersion equality ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Reflexivity: parse(s) == parse(s).
    #[test]
    fn wezterm_version_reflexive(raw in arb_wezterm_version()) {
        let v = WeztermVersion::parse(&raw);
        prop_assert_eq!(&v, &v);
    }

    /// Clone produces an equal version.
    #[test]
    fn wezterm_version_clone(raw in arb_wezterm_version()) {
        let v = WeztermVersion::parse(&raw);
        let cloned = v.clone();
        prop_assert_eq!(v, cloned);
    }

    /// Debug format is non-empty and contains type name.
    #[test]
    fn wezterm_version_debug(raw in arb_wezterm_version()) {
        let v = WeztermVersion::parse(&raw);
        let debug = format!("{:?}", v);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("WeztermVersion"));
    }
}

// ── VendoredCompatibilityStatus: serde ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// JSON serde roundtrip preserves the status variant.
    #[test]
    fn compat_status_serde_roundtrip(status in arb_compat_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let parsed: VendoredCompatibilityStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, status);
    }

    /// Serde produces snake_case strings.
    #[test]
    fn compat_status_serde_snake_case(status in arb_compat_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let unquoted = json.trim_matches('"');
        let lower = unquoted.to_lowercase();
        prop_assert_eq!(unquoted, lower.as_str(),
            "serde should produce lowercase");
    }

    /// Each status variant serializes to its expected string.
    #[test]
    fn compat_status_known_serializations(status in arb_compat_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let expected = match status {
            VendoredCompatibilityStatus::Matched => "\"matched\"",
            VendoredCompatibilityStatus::Compatible => "\"compatible\"",
            VendoredCompatibilityStatus::Incompatible => "\"incompatible\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// Copy semantics work correctly.
    #[test]
    fn compat_status_copy(status in arb_compat_status()) {
        let copied = status;
        prop_assert_eq!(status, copied);
    }
}

// ── VendoredCompatibilityReport: serde ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// JSON serde roundtrip preserves all report fields.
    #[test]
    fn compat_report_serde_roundtrip(report in arb_compat_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let parsed: VendoredCompatibilityReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.status, report.status);
        prop_assert_eq!(parsed.vendored_enabled, report.vendored_enabled);
        prop_assert_eq!(parsed.allow_vendored, report.allow_vendored);
        prop_assert_eq!(parsed.local_version, report.local_version);
        prop_assert_eq!(parsed.local_commit, report.local_commit);
        prop_assert_eq!(parsed.vendored_commit, report.vendored_commit);
        prop_assert_eq!(parsed.vendored_version, report.vendored_version);
        prop_assert_eq!(parsed.message, report.message);
        prop_assert_eq!(parsed.recommendation, report.recommendation);
    }

    /// Serialized report is always valid JSON object.
    #[test]
    fn compat_report_valid_json(report in arb_compat_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Report JSON always has required fields.
    #[test]
    fn compat_report_has_required_fields(report in arb_compat_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("status").is_some(), "missing 'status'");
        prop_assert!(value.get("vendored_enabled").is_some(), "missing 'vendored_enabled'");
        prop_assert!(value.get("allow_vendored").is_some(), "missing 'allow_vendored'");
        prop_assert!(value.get("message").is_some(), "missing 'message'");
    }

    /// When recommendation is None, it's absent from JSON (skip_serializing_if).
    #[test]
    fn compat_report_skips_none_recommendation(report in arb_compat_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        if report.recommendation.is_none() {
            prop_assert!(value.get("recommendation").is_none(),
                "recommendation should be absent when None");
        } else {
            prop_assert!(value.get("recommendation").is_some(),
                "recommendation should be present when Some");
        }
    }

    /// Pretty-printed JSON also roundtrips.
    #[test]
    fn compat_report_pretty_json_roundtrip(report in arb_compat_report()) {
        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: VendoredCompatibilityReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.status, report.status);
        prop_assert_eq!(parsed.message, report.message);
    }

    /// Clone produces an equivalent report.
    #[test]
    fn compat_report_clone(report in arb_compat_report()) {
        let cloned: VendoredCompatibilityReport = report.clone();
        prop_assert_eq!(cloned.status, report.status);
        prop_assert_eq!(cloned.vendored_enabled, report.vendored_enabled);
        prop_assert_eq!(cloned.allow_vendored, report.allow_vendored);
        prop_assert_eq!(cloned.message, report.message);
        prop_assert_eq!(cloned.recommendation, report.recommendation);
    }
}

// ── VendoredWeztermMetadata ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default metadata is stable.
    #[test]
    fn metadata_default_stable(_i in 0..5u8) {
        let a = VendoredWeztermMetadata::default();
        let b = VendoredWeztermMetadata::default();
        prop_assert_eq!(a.commit, b.commit);
        prop_assert_eq!(a.version, b.version);
        prop_assert_eq!(a.source, b.source);
        prop_assert_eq!(a.enabled, b.enabled);
    }

    /// Default metadata has all Nones and enabled=false.
    #[test]
    fn metadata_default_values(_i in 0..1u8) {
        let meta = VendoredWeztermMetadata::default();
        prop_assert!(meta.commit.is_none());
        prop_assert!(meta.version.is_none());
        prop_assert!(meta.source.is_none());
        prop_assert!(!meta.enabled);
    }
}

// ── compatibility_report (public API) ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// compatibility_report never panics with arbitrary parsed versions.
    #[test]
    fn compat_report_never_panics(raw in arb_wezterm_version()) {
        let v = WeztermVersion::parse(&raw);
        let _ = frankenterm_core::vendored::compatibility_report(Some(&v));
    }

    /// compatibility_report with None local never panics.
    #[test]
    fn compat_report_none_local_never_panics(_i in 0..10u8) {
        let _ = frankenterm_core::vendored::compatibility_report(None);
    }

    /// Report from compatibility_report is always serializable.
    #[test]
    fn compat_report_always_serializable(raw in arb_wezterm_version()) {
        let v = WeztermVersion::parse(&raw);
        let report = frankenterm_core::vendored::compatibility_report(Some(&v));
        let json = serde_json::to_string(&report);
        prop_assert!(json.is_ok(), "report should serialize");
    }

    /// Report always has a non-empty message.
    #[test]
    fn compat_report_non_empty_message(raw in arb_wezterm_version()) {
        let v = WeztermVersion::parse(&raw);
        let report = frankenterm_core::vendored::compatibility_report(Some(&v));
        prop_assert!(!report.message.is_empty(), "message should be non-empty");
    }
}
