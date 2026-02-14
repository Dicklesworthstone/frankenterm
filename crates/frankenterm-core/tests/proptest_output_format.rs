//! Property-based tests for the output format module.
//!
//! Tests invariants of OutputFormat parsing, Display↔FromStr roundtrip,
//! boolean method consistency, Style enable/disable, and EffectiveFormat.

use frankenterm_core::output::OutputFormat;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_output_format() -> impl Strategy<Value = OutputFormat> {
    prop_oneof![
        Just(OutputFormat::Auto),
        Just(OutputFormat::Plain),
        Just(OutputFormat::Json),
    ]
}

/// Valid format strings (including aliases).
fn arb_valid_format_string() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("auto".to_string()),
        Just("plain".to_string()),
        Just("text".to_string()),
        Just("json".to_string()),
        Just("AUTO".to_string()),
        Just("PLAIN".to_string()),
        Just("TEXT".to_string()),
        Just("JSON".to_string()),
        Just("Auto".to_string()),
        Just("Plain".to_string()),
        Just("Json".to_string()),
    ]
}

// ── OutputFormat: Display ↔ FromStr roundtrip ───────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Display → parse roundtrip preserves the variant.
    #[test]
    fn format_display_parse_roundtrip(fmt in arb_output_format()) {
        let displayed = fmt.to_string();
        let parsed = OutputFormat::parse(&displayed);
        prop_assert_eq!(parsed, Some(fmt),
            "roundtrip failed: Display='{}', parsed={:?}", displayed, parsed);
    }

    /// Display always produces lowercase output.
    #[test]
    fn format_display_is_lowercase(fmt in arb_output_format()) {
        let s = fmt.to_string();
        let lower = s.to_lowercase();
        prop_assert_eq!(s.as_str(), lower.as_str(),
            "Display should be lowercase");
    }

    /// Display is non-empty.
    #[test]
    fn format_display_non_empty(fmt in arb_output_format()) {
        let s = fmt.to_string();
        prop_assert!(!s.is_empty());
    }

    /// Each format variant has a distinct Display string.
    #[test]
    fn format_display_distinct(_i in 0..1u8) {
        let auto = OutputFormat::Auto.to_string();
        let plain = OutputFormat::Plain.to_string();
        let json = OutputFormat::Json.to_string();
        prop_assert_ne!(auto.as_str(), plain.as_str());
        prop_assert_ne!(auto.as_str(), json.as_str());
        prop_assert_ne!(plain.as_str(), json.as_str());
    }
}

// ── OutputFormat: FromStr / parse ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// All valid format strings parse successfully.
    #[test]
    fn format_valid_strings_parse(s in arb_valid_format_string()) {
        let result = OutputFormat::parse(&s);
        prop_assert!(result.is_some(),
            "expected successful parse for '{}', got None", s);
    }

    /// Parsing is case-insensitive.
    #[test]
    fn format_parse_case_insensitive(fmt in arb_output_format()) {
        let lower = fmt.to_string();
        let upper = lower.to_uppercase();
        let mixed = {
            let mut chars = lower.chars();
            let mut s = String::new();
            if let Some(c) = chars.next() {
                s.push(c.to_uppercase().next().unwrap());
            }
            for c in chars {
                s.push(c);
            }
            s
        };
        prop_assert_eq!(OutputFormat::parse(&lower), Some(fmt));
        prop_assert_eq!(OutputFormat::parse(&upper), Some(fmt));
        prop_assert_eq!(OutputFormat::parse(&mixed), Some(fmt));
    }

    /// "text" is an alias for Plain.
    #[test]
    fn format_text_alias(_i in 0..1u8) {
        prop_assert_eq!(OutputFormat::parse("text"), Some(OutputFormat::Plain));
        prop_assert_eq!(OutputFormat::parse("TEXT"), Some(OutputFormat::Plain));
        prop_assert_eq!(OutputFormat::parse("Text"), Some(OutputFormat::Plain));
    }

    /// Invalid strings return None.
    #[test]
    fn format_invalid_strings(s in "[a-z]{1,10}") {
        let valid = ["auto", "plain", "text", "json"];
        if !valid.contains(&s.as_str()) {
            prop_assert_eq!(OutputFormat::parse(&s), None,
                "expected None for invalid format '{}'" , s);
        }
    }

    /// Parsing is deterministic.
    #[test]
    fn format_parse_deterministic(s in "[a-z]{1,8}") {
        let r1 = OutputFormat::parse(&s);
        let r2 = OutputFormat::parse(&s);
        prop_assert_eq!(r1, r2, "parse is non-deterministic for '{}'", s);
    }
}

// ── OutputFormat: Default ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default is Auto.
    #[test]
    fn format_default_is_auto(_i in 0..1u8) {
        prop_assert_eq!(OutputFormat::default(), OutputFormat::Auto);
    }
}

// ── OutputFormat: boolean methods ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Only Json format returns true for is_json().
    #[test]
    fn format_is_json_only_json(fmt in arb_output_format()) {
        let is_json = fmt.is_json();
        if fmt == OutputFormat::Json {
            prop_assert!(is_json, "Json should be is_json()");
        } else {
            prop_assert!(!is_json, "{:?} should not be is_json()", fmt);
        }
    }

    /// Plain format always returns true for is_plain().
    #[test]
    fn format_plain_is_plain(_i in 0..1u8) {
        prop_assert!(OutputFormat::Plain.is_plain());
    }

    /// Json format never returns true for is_plain().
    #[test]
    fn format_json_not_plain(_i in 0..1u8) {
        prop_assert!(!OutputFormat::Json.is_plain());
    }

    /// Json and Plain are mutually exclusive.
    #[test]
    fn format_json_plain_exclusive(fmt in arb_output_format()) {
        // A format can't be both JSON and plain
        prop_assert!(!(fmt.is_json() && fmt.is_plain()),
            "{:?} is both json and plain", fmt);
    }

    /// Json never returns true for is_rich().
    #[test]
    fn format_json_not_rich(_i in 0..1u8) {
        prop_assert!(!OutputFormat::Json.is_rich());
    }

    /// Plain never returns true for is_rich().
    #[test]
    fn format_plain_not_rich(_i in 0..1u8) {
        prop_assert!(!OutputFormat::Plain.is_rich());
    }
}

// ── OutputFormat: effective() ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Plain format always resolves to EffectiveFormat::Plain.
    #[test]
    fn format_plain_effective_is_plain(_i in 0..1u8) {
        let eff = OutputFormat::Plain.effective();
        prop_assert_eq!(eff, frankenterm_core::output::detect_format().effective().clone());
        // Actually, just check the deterministic ones:
        let eff = OutputFormat::Plain.effective();
        let debug = format!("{:?}", eff);
        prop_assert!(debug.contains("Plain"));
    }

    /// Json format always resolves to EffectiveFormat::Json.
    #[test]
    fn format_json_effective_is_json(_i in 0..1u8) {
        let eff = OutputFormat::Json.effective();
        let debug = format!("{:?}", eff);
        prop_assert!(debug.contains("Json"));
    }
}

// ── OutputFormat: Copy/Clone/Debug ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Copy semantics work.
    #[test]
    fn format_copy(fmt in arb_output_format()) {
        let copied = fmt;
        prop_assert_eq!(fmt, copied);
    }

    /// Debug format is non-empty.
    #[test]
    fn format_debug_non_empty(fmt in arb_output_format()) {
        let debug = format!("{:?}", fmt);
        prop_assert!(!debug.is_empty());
    }

    /// Reflexivity.
    #[test]
    fn format_reflexive(fmt in arb_output_format()) {
        prop_assert_eq!(fmt, fmt);
    }
}
