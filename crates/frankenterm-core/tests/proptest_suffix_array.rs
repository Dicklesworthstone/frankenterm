//! Property-based tests for `suffix_array` module.
//!
//! Verifies correctness invariants of the suffix array and LCP array:
//! - SA is a valid permutation
//! - Suffixes are lexicographically sorted
//! - LCP values match actual common prefix lengths
//! - Search finds all and only correct occurrences
//! - Count matches search result length
//! - Distinct substring count formula correctness
//! - Serde roundtrip

use frankenterm_core::suffix_array::SuffixArray;
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn text_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(b'a'..=b'z', 1..100)
}

fn small_text_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(b'a'..=b'd', 1..30)
}

fn pattern_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(b'a'..=b'z', 1..10)
}

// ── Brute-force reference ──────────────────────────────────────────────

fn brute_force_search(text: &[u8], pattern: &[u8]) -> Vec<usize> {
    let mut results = Vec::new();
    if pattern.is_empty() || pattern.len() > text.len() {
        return results;
    }
    for i in 0..=text.len() - pattern.len() {
        if text[i..i + pattern.len()] == *pattern {
            results.push(i);
        }
    }
    results
}

fn brute_force_count(text: &[u8], pattern: &[u8]) -> usize {
    brute_force_search(text, pattern).len()
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── SA is a valid permutation ──────────────────────────────────

    #[test]
    fn sa_is_permutation(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let arr = sa.suffix_array();

        prop_assert_eq!(arr.len(), text.len());

        let mut sorted = arr.to_vec();
        sorted.sort_unstable();
        let expected: Vec<usize> = (0..text.len()).collect();
        prop_assert_eq!(sorted, expected, "SA is not a permutation");
    }

    // ── Suffixes are sorted ────────────────────────────────────────

    #[test]
    fn suffixes_lexicographically_sorted(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let arr = sa.suffix_array();

        for w in arr.windows(2) {
            let s1 = &text[w[0]..];
            let s2 = &text[w[1]..];
            prop_assert!(
                s1 <= s2,
                "suffixes not sorted at positions {}, {}", w[0], w[1]
            );
        }
    }

    // ── LCP values correct ─────────────────────────────────────────

    #[test]
    fn lcp_values_match_actual(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let arr = sa.suffix_array();
        let lcp = sa.lcp_array();

        prop_assert_eq!(lcp[0], 0, "LCP[0] should always be 0");

        for i in 1..arr.len() {
            let s1 = &text[arr[i - 1]..];
            let s2 = &text[arr[i]..];
            let common = s1.iter().zip(s2.iter()).take_while(|(a, b)| a == b).count();
            prop_assert_eq!(
                lcp[i], common,
                "LCP[{}] mismatch: expected {}, got {}", i, common, lcp[i]
            );
        }
    }

    // ── Search correctness ─────────────────────────────────────────

    #[test]
    fn search_matches_brute_force(
        text in small_text_strategy(),
        pattern in prop::collection::vec(b'a'..=b'd', 1..5)
    ) {
        let sa = SuffixArray::new(&text);
        let sa_results = sa.search(&pattern);
        let bf_results = brute_force_search(&text, &pattern);

        prop_assert_eq!(
            sa_results, bf_results,
            "search mismatch for pattern {:?} in text {:?}", pattern, text
        );
    }

    #[test]
    fn search_results_are_valid(
        text in text_strategy(),
        pattern in pattern_strategy()
    ) {
        let sa = SuffixArray::new(&text);
        let results = sa.search(&pattern);

        // Every result must be a valid occurrence
        for &pos in &results {
            prop_assert!(pos + pattern.len() <= text.len());
            prop_assert_eq!(
                &text[pos..pos + pattern.len()], &pattern[..],
                "invalid search result at position {}", pos
            );
        }
    }

    #[test]
    fn search_results_sorted(
        text in text_strategy(),
        pattern in pattern_strategy()
    ) {
        let sa = SuffixArray::new(&text);
        let results = sa.search(&pattern);

        for w in results.windows(2) {
            prop_assert!(w[0] < w[1], "results not sorted: {} >= {}", w[0], w[1]);
        }
    }

    // ── Count matches search length ────────────────────────────────

    #[test]
    fn count_matches_search_len(
        text in text_strategy(),
        pattern in pattern_strategy()
    ) {
        let sa = SuffixArray::new(&text);
        let count = sa.count(&pattern);
        let search_len = sa.search(&pattern).len();
        prop_assert_eq!(count, search_len);
    }

    // ── Distinct substring count ───────────────────────────────────

    #[test]
    fn distinct_count_positive(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let count = sa.distinct_substring_count();
        // At minimum, n distinct substrings (each single char position is a length-1 substr)
        // But could be fewer if chars repeat... actually minimum for n-length text is n
        // because the last character's suffix is always unique
        prop_assert!(count >= text.len(), "distinct count {} < text len {}", count, text.len());
    }

    #[test]
    fn distinct_count_upper_bound(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let n = text.len();
        let count = sa.distinct_substring_count();
        let max = n * (n + 1) / 2;
        prop_assert!(count <= max, "distinct count {} > max {}", count, max);
    }

    // ── Longest repeated substring ─────────────────────────────────

    #[test]
    fn longest_repeated_is_valid(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let (pos, len) = sa.longest_repeated_substring();

        if len > 0 {
            // The substring should appear at least twice
            let pattern = &text[pos..pos + len];
            let occurrences = brute_force_count(&text, pattern);
            prop_assert!(
                occurrences >= 2,
                "longest repeated {:?} at pos {} appears only {} times",
                pattern, pos, occurrences
            );
        }
    }

    #[test]
    fn longest_repeated_is_maximal(text in small_text_strategy()) {
        let sa = SuffixArray::new(&text);
        let (_, max_len) = sa.longest_repeated_substring();

        // No longer repeated substring should exist
        // Check all substrings of length max_len + 1
        if max_len + 1 <= text.len() {
            for i in 0..=text.len() - (max_len + 1) {
                let pattern = &text[i..i + max_len + 1];
                let count = brute_force_count(&text, pattern);
                prop_assert!(
                    count < 2,
                    "found repeated substring of length {} > max {}",
                    max_len + 1, max_len
                );
            }
        }
    }

    // ── Serde roundtrip ────────────────────────────────────────────

    #[test]
    fn serde_roundtrip(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let json = serde_json::to_string(&sa).unwrap();
        let restored: SuffixArray = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), sa.len());
        prop_assert_eq!(restored.suffix_array(), sa.suffix_array());
        prop_assert_eq!(restored.lcp_array(), sa.lcp_array());
    }

    // ── Length consistency ──────────────────────────────────────────

    #[test]
    fn length_consistent(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        prop_assert_eq!(sa.len(), text.len());
        prop_assert_eq!(sa.suffix_array().len(), text.len());
        prop_assert_eq!(sa.lcp_array().len(), text.len());
        let is_empty = text.is_empty();
        prop_assert_eq!(sa.is_empty(), is_empty);
    }

    // ── Search for full text always finds position 0 ───────────────

    #[test]
    fn search_full_text_finds_start(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let results = sa.search(&text);
        prop_assert_eq!(results, vec![0]);
    }

    // ── Empty pattern returns empty ────────────────────────────────

    #[test]
    fn empty_pattern_returns_empty(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let results = sa.search(&[]);
        prop_assert!(results.is_empty());
        prop_assert_eq!(sa.count(&[]), 0);
    }

    // ── Search for single byte ─────────────────────────────────────

    #[test]
    fn search_single_byte(text in text_strategy()) {
        if text.is_empty() {
            return Ok(());
        }
        let byte = text[0];
        let sa = SuffixArray::new(&text);
        let results = sa.search(&[byte]);
        let expected = brute_force_search(&text, &[byte]);
        prop_assert_eq!(results, expected);
    }
}
