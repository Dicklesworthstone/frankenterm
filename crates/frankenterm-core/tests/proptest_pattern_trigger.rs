//! Property-based tests for pattern_trigger module (ft-2oph2).
//!
//! Validates that the Aho-Corasick trigger scanner produces consistent,
//! correct results across random inputs and pattern configurations.

use proptest::prelude::*;

use frankenterm_core::pattern_trigger::{
    TriggerCategory, TriggerMatch, TriggerPattern, TriggerScanResult, TriggerScanner,
    all_default_patterns,
};

// =============================================================================
// Strategies
// =============================================================================

fn random_text_lines() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            "[a-zA-Z0-9 _\\-.:;,=/()]{1,120}\n".prop_map(|s| s.into_bytes()),
            // Occasional ANSI escape
            Just(b"\x1b[32mOK\x1b[0m\n".to_vec()),
        ],
        1..200,
    )
    .prop_map(|lines| lines.into_iter().flatten().collect())
}

fn random_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..4096)
}

// =============================================================================
// Basic invariant tests
// =============================================================================

proptest! {
    /// Total matches equals sum of per-category counts.
    #[test]
    fn total_matches_equals_category_sum(data in random_text_lines()) {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(&data);
        let category_sum: u64 = result.counts.values().sum();
        prop_assert_eq!(
            result.total_matches, category_sum,
            "total {} != sum of categories {}",
            result.total_matches, category_sum
        );
    }

    /// Bytes scanned always matches input length.
    #[test]
    fn bytes_scanned_matches_input(data in random_bytes()) {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(&data);
        prop_assert_eq!(result.bytes_scanned, data.len() as u64);
    }

    /// Locate mode returns same total as count mode.
    #[test]
    fn locate_count_consistency(data in random_text_lines()) {
        let scanner = TriggerScanner::default();
        let counts_result = scanner.scan_counts(&data);
        let locate_result = scanner.scan_locate(&data);
        prop_assert_eq!(
            counts_result.total_matches,
            locate_result.len() as u64,
            "count mode ({}) != locate mode ({})",
            counts_result.total_matches,
            locate_result.len()
        );
    }

    /// All located matches have valid byte ranges within input.
    #[test]
    fn locate_offsets_within_bounds(data in random_bytes()) {
        let scanner = TriggerScanner::default();
        let matches = scanner.scan_locate(&data);
        for m in &matches {
            prop_assert!(
                m.offset + m.length <= data.len(),
                "match at offset {} + length {} > input len {}",
                m.offset, m.length, data.len()
            );
        }
    }

    /// Located matches are sorted by offset.
    #[test]
    fn locate_sorted_by_offset(data in random_text_lines()) {
        let scanner = TriggerScanner::default();
        let matches = scanner.scan_locate(&data);
        for pair in matches.windows(2) {
            prop_assert!(
                pair[0].offset <= pair[1].offset,
                "out of order: {} > {}",
                pair[0].offset, pair[1].offset
            );
        }
    }

    /// Empty scanner never finds matches.
    #[test]
    fn empty_scanner_no_matches(data in random_bytes()) {
        let scanner = TriggerScanner::new(Vec::new());
        let result = scanner.scan_counts(&data);
        prop_assert_eq!(result.total_matches, 0);
    }
}

// =============================================================================
// Injection tests — plant known patterns and verify detection
// =============================================================================

proptest! {
    /// Injecting "ERROR" into random text always detects at least one error.
    #[test]
    fn injected_error_detected(
        prefix in prop::collection::vec(any::<u8>(), 0..512),
        suffix in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let scanner = TriggerScanner::default();
        let mut input = prefix;
        input.extend_from_slice(b"ERROR: injected failure");
        input.extend(suffix);
        let result = scanner.scan_counts(&input);
        prop_assert!(result.has_errors(), "ERROR not detected in injected input");
    }

    /// Injecting "Finished" detects completion.
    #[test]
    fn injected_completion_detected(
        prefix in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let scanner = TriggerScanner::default();
        let mut input = prefix;
        input.extend_from_slice(b"    Finished `dev` profile in 2s\n");
        let result = scanner.scan_counts(&input);
        prop_assert!(result.has_completions(), "Finished not detected");
    }

    /// Injecting N copies of "Compiling" detects exactly N progress matches.
    #[test]
    fn injected_progress_count(count in 1..20usize) {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::new("Compiling", TriggerCategory::Progress),
        ]);
        let mut input = Vec::new();
        for i in 0..count {
            input.extend_from_slice(format!("   Compiling crate-{i}\n").as_bytes());
        }
        let result = scanner.scan_counts(&input);
        prop_assert_eq!(
            result.get(&TriggerCategory::Progress).copied().unwrap_or(0),
            count as u64,
            "expected {} progress matches, got {:?}",
            count, result.counts
        );
    }
}

// =============================================================================
// Custom pattern tests
// =============================================================================

proptest! {
    /// Custom single-pattern scanner matches exactly where the pattern occurs.
    #[test]
    fn custom_single_pattern(
        needle in "[A-Z]{3,8}",
        haystack_parts in prop::collection::vec("[a-z ]{5,30}\n", 5..20),
        inject_positions in prop::collection::vec(0..20usize, 1..4),
    ) {
        let mut parts = haystack_parts;
        // Deduplicate positions and inject needle
        let mut positions: Vec<usize> = inject_positions
            .into_iter()
            .map(|p| p % parts.len())
            .collect();
        positions.sort();
        positions.dedup();

        for &pos in &positions {
            parts[pos] = format!("{} {} rest\n", &parts[pos].trim_end(), needle);
        }

        let input: String = parts.join("");
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::new(&needle, TriggerCategory::Custom),
        ]);
        let result = scanner.scan_counts(input.as_bytes());

        // Should find at least as many as we injected (could be more if the needle
        // appeared in the random haystack too, but that's very unlikely for 3-8 uppercase)
        let found = result.get(&TriggerCategory::Custom).copied().unwrap_or(0);
        prop_assert!(
            found >= positions.len() as u64,
            "expected >= {} matches for '{}', found {}",
            positions.len(), needle, found
        );
    }
}

// =============================================================================
// Serde roundtrip
// =============================================================================

proptest! {
    /// TriggerScanResult survives JSON roundtrip.
    #[test]
    fn scan_result_serde_roundtrip(data in random_text_lines()) {
        let scanner = TriggerScanner::default();
        let result = scanner.scan_counts(&data);
        let json = serde_json::to_string(&result).unwrap();
        let rt: TriggerScanResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.total_matches, result.total_matches);
        prop_assert_eq!(rt.bytes_scanned, result.bytes_scanned);
    }
}

// =============================================================================
// Category and type serde/Display tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// PT-14: TriggerCategory serde roundtrip for all 6 variants.
    #[test]
    fn pt14_category_serde_roundtrip(
        cat in prop_oneof![
            Just(TriggerCategory::Error),
            Just(TriggerCategory::Warning),
            Just(TriggerCategory::Completion),
            Just(TriggerCategory::Progress),
            Just(TriggerCategory::TestResult),
            Just(TriggerCategory::Custom),
        ]
    ) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: TriggerCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    /// PT-15: TriggerCategory Display is non-empty and lowercase.
    #[test]
    fn pt15_category_display_non_empty(
        cat in prop_oneof![
            Just(TriggerCategory::Error),
            Just(TriggerCategory::Warning),
            Just(TriggerCategory::Completion),
            Just(TriggerCategory::Progress),
            Just(TriggerCategory::TestResult),
            Just(TriggerCategory::Custom),
        ]
    ) {
        let display = format!("{cat}");
        prop_assert!(!display.is_empty());
        let is_lower = display.chars().all(|c| c.is_lowercase() || c == '_');
        prop_assert!(is_lower, "Display should be lowercase: {}", display);
    }

    /// PT-16: TriggerPattern serde roundtrip.
    #[test]
    fn pt16_pattern_serde_roundtrip(
        pattern in "[A-Za-z]{3,20}",
        cat in prop_oneof![
            Just(TriggerCategory::Error),
            Just(TriggerCategory::Custom),
        ],
        case_insensitive in any::<bool>(),
    ) {
        let tp = if case_insensitive {
            TriggerPattern::case_insensitive(&pattern, cat)
        } else {
            TriggerPattern::new(&pattern, cat)
        };
        let json = serde_json::to_string(&tp).unwrap();
        let back: TriggerPattern = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tp.pattern, back.pattern);
        prop_assert_eq!(tp.category, back.category);
        prop_assert_eq!(tp.case_insensitive, back.case_insensitive);
    }

    /// PT-17: TriggerMatch serde roundtrip.
    #[test]
    fn pt17_match_serde_roundtrip(
        offset in 0usize..10000,
        length in 1usize..100,
        pattern_index in 0usize..50,
        cat in prop_oneof![
            Just(TriggerCategory::Error),
            Just(TriggerCategory::Warning),
            Just(TriggerCategory::Completion),
        ],
    ) {
        let m = TriggerMatch {
            offset,
            length,
            pattern_index,
            category: cat,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: TriggerMatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(m.offset, back.offset);
        prop_assert_eq!(m.length, back.length);
        prop_assert_eq!(m.pattern_index, back.pattern_index);
        prop_assert_eq!(m.category, back.category);
    }
}

// =============================================================================
// Helper method and scanner property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// PT-18: has_errors is false when input contains no error patterns.
    #[test]
    fn pt18_no_errors_means_has_errors_false(
        text in "[a-z ]{10,200}\n",
    ) {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::new("ERROR", TriggerCategory::Error),
        ]);
        let result = scanner.scan_counts(text.as_bytes());
        prop_assert!(!result.has_errors());
    }

    /// PT-19: has_completions is false when no completion patterns present.
    #[test]
    fn pt19_no_completions_means_false(
        text in "[a-z ]{10,200}\n",
    ) {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::new("Finished", TriggerCategory::Completion),
        ]);
        let result = scanner.scan_counts(text.as_bytes());
        prop_assert!(!result.has_completions());
    }

    /// PT-20: pattern_count matches the number of patterns passed to new().
    #[test]
    fn pt20_pattern_count_matches_input(n in 0usize..20) {
        let patterns: Vec<TriggerPattern> = (0..n)
            .map(|i| TriggerPattern::new(&format!("PAT{i}"), TriggerCategory::Custom))
            .collect();
        let scanner = TriggerScanner::new(patterns);
        prop_assert_eq!(scanner.pattern_count(), n);
    }

    /// PT-21: Case-insensitive patterns match both upper and lower case.
    #[test]
    fn pt21_case_insensitive_matches_variants(
        word in "[A-Z]{4,8}",
    ) {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::case_insensitive(&word, TriggerCategory::Custom),
        ]);
        // Upper case
        let upper_result = scanner.scan_counts(word.as_bytes());
        // Lower case
        let lower = word.to_lowercase();
        let lower_result = scanner.scan_counts(lower.as_bytes());
        prop_assert_eq!(
            upper_result.get(&TriggerCategory::Custom).copied().unwrap_or(0), 1,
            "Case-insensitive should match uppercase '{}'", word
        );
        prop_assert_eq!(
            lower_result.get(&TriggerCategory::Custom).copied().unwrap_or(0), 1,
            "Case-insensitive should match lowercase '{}'", lower
        );
    }

    /// PT-22: Case-sensitive patterns do NOT match different case.
    #[test]
    fn pt22_case_sensitive_rejects_wrong_case(
        word in "[A-Z]{4,8}",
    ) {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::new(&word, TriggerCategory::Custom),
        ]);
        // Should match exact case
        let exact_result = scanner.scan_counts(word.as_bytes());
        prop_assert_eq!(
            exact_result.get(&TriggerCategory::Custom).copied().unwrap_or(0), 1
        );
        // Should NOT match lowercase
        let lower = word.to_lowercase();
        let lower_result = scanner.scan_counts(lower.as_bytes());
        prop_assert_eq!(
            lower_result.get(&TriggerCategory::Custom).copied().unwrap_or(0), 0,
            "Case-sensitive '{}' should not match '{}'", word, lower
        );
    }

    /// PT-23: scan_locate match offsets point to actual pattern text.
    #[test]
    fn pt23_locate_offsets_match_pattern(
        prefix_len in 0usize..100,
        suffix_len in 0usize..100,
    ) {
        let pattern = "XMARKER";
        let prefix: String = (0..prefix_len).map(|_| 'a').collect();
        let suffix: String = (0..suffix_len).map(|_| 'b').collect();
        let input = format!("{prefix}{pattern}{suffix}");

        let scanner = TriggerScanner::new(vec![
            TriggerPattern::new(pattern, TriggerCategory::Custom),
        ]);
        let matches = scanner.scan_locate(input.as_bytes());
        prop_assert_eq!(matches.len(), 1);
        let m = &matches[0];
        prop_assert_eq!(m.offset, prefix_len);
        prop_assert_eq!(m.length, pattern.len());
        let matched_bytes = &input.as_bytes()[m.offset..m.offset + m.length];
        prop_assert_eq!(matched_bytes, pattern.as_bytes());
    }

    /// PT-24: Default scanner pattern_count matches all_default_patterns().
    #[test]
    fn pt24_default_scanner_count(_dummy in 0u8..1) {
        let scanner = TriggerScanner::default();
        let all = all_default_patterns();
        prop_assert_eq!(scanner.pattern_count(), all.len());
    }

    /// PT-25: Scanning never panics on arbitrary binary input.
    #[test]
    fn pt25_no_panic_on_binary(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let scanner = TriggerScanner::default();
        let _ = scanner.scan_counts(&data);
        let _ = scanner.scan_locate(&data);
    }
}
