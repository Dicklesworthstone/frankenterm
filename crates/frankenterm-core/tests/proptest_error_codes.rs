//! Property-based tests for error_codes module
//!
//! Tests invariants for ErrorCategory, RecoveryStep, ErrorCodeDef, and catalog functions.

use frankenterm_core::error_codes::*;
use proptest::prelude::*;
use std::borrow::Cow;

// ============================================================================
// Strategies
// ============================================================================

/// Generate arbitrary ErrorCategory
fn arb_error_category() -> impl Strategy<Value = ErrorCategory> {
    prop_oneof![
        Just(ErrorCategory::Wezterm),
        Just(ErrorCategory::Storage),
        Just(ErrorCategory::Pattern),
        Just(ErrorCategory::Policy),
        Just(ErrorCategory::Workflow),
        Just(ErrorCategory::Network),
        Just(ErrorCategory::Config),
        Just(ErrorCategory::Internal),
    ]
}

/// Generate arbitrary code number (full u16 range)
fn arb_code_number() -> impl Strategy<Value = u16> {
    0u16..=65535u16
}

/// Generate arbitrary string that may or may not be a valid error code
fn arb_code_string() -> impl Strategy<Value = String> {
    prop_oneof![
        // Valid-looking codes
        arb_code_number().prop_map(|n| format!("FT-{}", n)),
        // Invalid prefix
        "[A-Z]{2}-[0-9]{4}".prop_map(|s| s.to_string()),
        // No prefix
        "[0-9]{4}",
        // Random strings
        "[a-zA-Z0-9-]{0,20}",
        // Empty
        Just(String::new()),
    ]
}

/// Generate arbitrary RecoveryStep
fn arb_recovery_step() -> impl Strategy<Value = RecoveryStep> {
    prop_oneof![
        "[a-zA-Z0-9 ]{1,100}".prop_map(|desc| RecoveryStep {
            description: Cow::Owned(desc),
            command: None,
        }),
        ("[a-zA-Z0-9 ]{1,100}", "[a-z ]{1,50}").prop_map(|(desc, cmd)| RecoveryStep {
            description: Cow::Owned(desc),
            command: Some(Cow::Owned(cmd)),
        }),
    ]
}

// ============================================================================
// Property Tests: ErrorCategory
// ============================================================================

proptest! {
    /// Property 1: from_code roundtrip for valid codes within category ranges
    #[test]
    fn prop_from_code_roundtrip_within_range(cat in arb_error_category()) {
        let (lo, hi) = cat.range();
        // Test at boundaries and middle
        #[allow(clippy::manual_midpoint)]
        let mid = (lo + hi) / 2;
        for num in [lo, mid, hi] {
            let code = format!("FT-{}", num);
            let parsed = ErrorCategory::from_code(&code);
            prop_assert_eq!(parsed, Some(cat), "Code {} should parse to {:?}", code, cat);
        }
    }

    /// Property 2: All category ranges are non-overlapping
    #[test]
    fn prop_category_ranges_non_overlapping(cat1 in arb_error_category(), cat2 in arb_error_category()) {
        if cat1 != cat2 {
            let (lo1, hi1) = cat1.range();
            let (lo2, hi2) = cat2.range();
            // Ranges must not overlap
            prop_assert!(hi1 < lo2 || hi2 < lo1, "Ranges {:?}({}-{}) and {:?}({}-{}) overlap",
                        cat1, lo1, hi1, cat2, lo2, hi2);
        }
    }

    /// Property 3: All category ranges are valid (lo <= hi)
    #[test]
    fn prop_category_range_ordering(cat in arb_error_category()) {
        let (lo, hi) = cat.range();
        prop_assert!(lo <= hi, "Category {:?} has inverted range: {} > {}", cat, lo, hi);
    }

    /// Property 4: from_code rejects codes outside all ranges
    #[test]
    fn prop_from_code_rejects_gap_codes(num in prop::num::u16::ANY) {
        // Codes in gaps (0-999, 8000-8999) should return None
        if (num < 1000) || (8000..=8999).contains(&num) {
            let code = format!("FT-{}", num);
            let parsed = ErrorCategory::from_code(&code);
            prop_assert_eq!(parsed, None, "Code {} in gap should return None", code);
        }
    }

    /// Property 5: from_code rejects codes without FT- prefix
    #[test]
    fn prop_from_code_rejects_bad_prefix(s in "[A-Z]{2}-[0-9]{4}") {
        if !s.starts_with("FT-") {
            let parsed = ErrorCategory::from_code(&s);
            prop_assert_eq!(parsed, None, "Code {} without FT- prefix should return None", s);
        }
    }

    /// Property 6: from_code rejects malformed strings
    #[test]
    fn prop_from_code_rejects_malformed(s in arb_code_string()) {
        let parsed = ErrorCategory::from_code(&s);
        // If it parsed successfully, verify it's in a valid range
        if let Some(cat) = parsed {
            let num: u16 = s.strip_prefix("FT-").unwrap().parse().unwrap();
            let (lo, hi) = cat.range();
            prop_assert!(num >= lo && num <= hi,
                        "Parsed code {} to {:?} but {} not in range {}-{}",
                        s, cat, num, lo, hi);
        }
    }

    /// Property 7: Serde roundtrip for ErrorCategory
    #[test]
    fn prop_error_category_serde_roundtrip(cat in arb_error_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: ErrorCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, cat, "Serde roundtrip failed for {:?}", cat);
    }

    /// Property 8: Serde uses snake_case naming
    #[test]
    fn prop_error_category_snake_case(cat in arb_error_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        // Should be lowercase with underscores
        prop_assert!(!json.contains(char::is_uppercase),
                    "Serialized {:?} contains uppercase: {}", cat, json);
    }

    // ========================================================================
    // Property Tests: RecoveryStep
    // ========================================================================

    /// Property 9: Serde roundtrip for RecoveryStep
    #[test]
    fn prop_recovery_step_serde_roundtrip(step in arb_recovery_step()) {
        let json = serde_json::to_string(&step).unwrap();
        let back: RecoveryStep = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.description, step.description,
                       "Description mismatch after roundtrip");
        prop_assert_eq!(back.command, step.command,
                       "Command mismatch after roundtrip");
    }

    /// Property 10: RecoveryStep::text creates step with no command
    #[test]
    fn prop_recovery_step_text_no_command(desc in "[a-zA-Z0-9 ]{1,100}") {
        // Note: We can't use the const constructor in proptest, so test behavior
        let step = RecoveryStep {
            description: Cow::Owned(desc.clone()),
            command: None,
        };
        prop_assert_eq!(step.description.as_ref(), desc.as_str(),
                       "Description mismatch");
        prop_assert!(step.command.is_none(), "text() step should have no command");
    }

    /// Property 11: RecoveryStep with command preserves both fields
    #[test]
    fn prop_recovery_step_with_command_preserves_both(
        desc in "[a-zA-Z0-9 ]{1,100}",
        cmd in "[a-z ]{1,50}"
    ) {
        let step = RecoveryStep {
            description: Cow::Owned(desc.clone()),
            command: Some(Cow::Owned(cmd.clone())),
        };
        prop_assert_eq!(step.description.as_ref(), desc.as_str(),
                       "Description mismatch");
        prop_assert_eq!(step.command.as_ref().map(|c| c.as_ref()), Some(cmd.as_str()),
                       "Command mismatch");
    }

    // ========================================================================
    // Property Tests: Catalog Functions
    // ========================================================================

    /// Property 12: get_error_code returns None for non-existent codes
    #[test]
    fn prop_get_error_code_none_for_random(s in arb_code_string()) {
        let result = get_error_code(&s);
        // If it returns Some, the key must be in the catalog
        if result.is_some() {
            prop_assert!(ERROR_CATALOG.contains_key(s.as_str()),
                        "get_error_code returned Some for {} but not in catalog", s);
        }
    }

    /// Property 13: All catalog codes are within their category range
    #[test]
    fn prop_catalog_codes_within_category_range(_dummy in Just(())) {
        for def in ERROR_CATALOG.values() {
            let num: u16 = def.code.strip_prefix("FT-")
                .expect("code missing FT- prefix")
                .parse()
                .expect("code not numeric");
            let (lo, hi) = def.category.range();
            prop_assert!(num >= lo && num <= hi,
                        "Code {} (num={}) outside {:?} range ({}-{})",
                        def.code, num, def.category, lo, hi);
        }
    }

    /// Property 14: Catalog key matches def.code
    #[test]
    fn prop_catalog_key_matches_def_code(_dummy in Just(())) {
        for (key, def) in ERROR_CATALOG.iter() {
            prop_assert_eq!(*key, def.code,
                           "Catalog key {} doesn't match def.code {}", key, def.code);
        }
    }

    /// Property 15: All catalog entries have non-empty title
    #[test]
    fn prop_catalog_entries_have_title(_dummy in Just(())) {
        for (code, def) in ERROR_CATALOG.iter() {
            prop_assert!(!def.title.trim().is_empty(),
                        "Code {} has empty title", code);
        }
    }

    /// Property 16: All catalog entries have non-empty description
    #[test]
    fn prop_catalog_entries_have_description(_dummy in Just(())) {
        for (code, def) in ERROR_CATALOG.iter() {
            prop_assert!(!def.description.trim().is_empty(),
                        "Code {} has empty description", code);
        }
    }

    /// Property 17: All catalog entries have non-empty causes
    #[test]
    fn prop_catalog_entries_have_causes(_dummy in Just(())) {
        for (code, def) in ERROR_CATALOG.iter() {
            prop_assert!(!def.causes.is_empty(),
                        "Code {} has no causes", code);
        }
    }

    /// Property 18: All catalog entries have non-empty recovery_steps
    #[test]
    fn prop_catalog_entries_have_recovery_steps(_dummy in Just(())) {
        for (code, def) in ERROR_CATALOG.iter() {
            prop_assert!(!def.recovery_steps.is_empty(),
                        "Code {} has no recovery_steps", code);
        }
    }

    /// Property 19: list_error_codes is sorted
    #[test]
    fn prop_list_error_codes_sorted(_dummy in Just(())) {
        let codes = list_error_codes();
        for window in codes.windows(2) {
            prop_assert!(window[0] <= window[1],
                        "Codes not sorted: {} > {}", window[0], window[1]);
        }
    }

    /// Property 20: list_codes_by_category only returns matching category
    #[test]
    fn prop_list_codes_by_category_matches(cat in arb_error_category()) {
        let codes = list_codes_by_category(cat);
        for def in &codes {
            prop_assert_eq!(def.category, cat,
                           "Code {} has category {:?} but was in {:?} list",
                           def.code, def.category, cat);
        }
    }

    /// Property 21: format_plain contains code and title
    #[test]
    fn prop_format_plain_contains_code_and_title(_dummy in Just(())) {
        for def in ERROR_CATALOG.values() {
            let formatted = def.format_plain();
            prop_assert!(formatted.contains(def.code),
                        "Formatted output missing code {}", def.code);
            prop_assert!(formatted.contains(def.title),
                        "Formatted output missing title for {}", def.code);
        }
    }

    /// Property 22: format_plain includes causes section
    #[test]
    fn prop_format_plain_includes_causes(_dummy in Just(())) {
        for def in ERROR_CATALOG.values() {
            if !def.causes.is_empty() {
                let formatted = def.format_plain();
                prop_assert!(formatted.contains("Common causes:"),
                            "Formatted output missing causes section for {}", def.code);
                for cause in def.causes {
                    prop_assert!(formatted.contains(cause),
                                "Formatted output missing cause '{}' for {}", cause, def.code);
                }
            }
        }
    }

    /// Property 23: format_plain includes recovery steps section
    #[test]
    fn prop_format_plain_includes_recovery_steps(_dummy in Just(())) {
        for def in ERROR_CATALOG.values() {
            if !def.recovery_steps.is_empty() {
                let formatted = def.format_plain();
                prop_assert!(formatted.contains("Recovery steps:"),
                            "Formatted output missing recovery steps section for {}", def.code);
                for step in def.recovery_steps {
                    prop_assert!(formatted.contains(step.description.as_ref()),
                                "Formatted output missing step '{}' for {}",
                                step.description, def.code);
                }
            }
        }
    }

    /// Property 24: format_plain includes command prefix when command exists
    #[test]
    fn prop_format_plain_includes_command_prefix(_dummy in Just(())) {
        for def in ERROR_CATALOG.values() {
            let has_command = def.recovery_steps.iter().any(|s| s.command.is_some());
            if has_command {
                let formatted = def.format_plain();
                prop_assert!(formatted.contains("$ "),
                            "Formatted output missing command prefix for {}", def.code);
            }
        }
    }

    /// Property 25: Catalog codes match category from from_code
    #[test]
    fn prop_catalog_codes_match_from_code(_dummy in Just(())) {
        for (key, def) in ERROR_CATALOG.iter() {
            let parsed = ErrorCategory::from_code(key);
            prop_assert_eq!(parsed, Some(def.category),
                           "from_code({}) = {:?} but def.category = {:?}",
                           key, parsed, def.category);
        }
    }

    /// Property 26: list_error_codes returns all catalog entries
    #[test]
    fn prop_list_error_codes_complete(_dummy in Just(())) {
        let codes = list_error_codes();
        prop_assert_eq!(codes.len(), ERROR_CATALOG.len(),
                       "list_error_codes length {} != catalog length {}",
                       codes.len(), ERROR_CATALOG.len());
    }

    /// Property 27: from_code boundary testing (just before/after ranges)
    #[test]
    fn prop_from_code_boundary_cases(cat in arb_error_category()) {
        let (lo, hi) = cat.range();

        // At boundaries - should match
        prop_assert_eq!(ErrorCategory::from_code(&format!("FT-{}", lo)), Some(cat),
                       "Lower boundary {} should match {:?}", lo, cat);
        prop_assert_eq!(ErrorCategory::from_code(&format!("FT-{}", hi)), Some(cat),
                       "Upper boundary {} should match {:?}", hi, cat);

        // Just before lower boundary (if not at 0)
        if lo > 0 {
            let before = lo - 1;
            let result = ErrorCategory::from_code(&format!("FT-{}", before));
            // Should either be None or a different category
            prop_assert!(result != Some(cat),
                        "Code {} before range should not match {:?}", before, cat);
        }

        // Just after upper boundary (if not at u16::MAX)
        if hi < u16::MAX {
            let after = hi + 1;
            let result = ErrorCategory::from_code(&format!("FT-{}", after));
            // Should either be None or a different category
            prop_assert!(result != Some(cat),
                        "Code {} after range should not match {:?}", after, cat);
        }
    }

    /// Property 28: Every category has at least one catalog entry
    #[test]
    fn prop_every_category_has_entries(cat in arb_error_category()) {
        let entries = list_codes_by_category(cat);
        prop_assert!(!entries.is_empty(),
                    "Category {:?} has no catalog entries", cat);
    }
}
