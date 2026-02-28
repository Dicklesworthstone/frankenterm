//! Property-based tests for pattern_trigger module (ft-2oph2).
//!
//! Validates that the Aho-Corasick trigger scanner produces consistent,
//! correct results across random inputs and pattern configurations.

use proptest::prelude::*;

use frankenterm_core::pattern_trigger::{TriggerCategory, TriggerPattern, TriggerScanner};

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
        let rt: frankenterm_core::pattern_trigger::TriggerScanResult =
            serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.total_matches, result.total_matches);
        prop_assert_eq!(rt.bytes_scanned, result.bytes_scanned);
    }
}
