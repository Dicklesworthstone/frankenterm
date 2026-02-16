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
//! - String API consistency (from_str, search_str, count_str)
//! - Text preservation, prefix/suffix search, absent patterns
//! - Uniform/monotonic text edge cases

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
        if max_len < text.len() {
            for i in 0..=text.len() - (max_len + 1) {
                let pattern = &text[i..=(i + max_len)];
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

    // ══════════════════════════════════════════════════════════════
    // NEW TESTS (17-32)
    // ══════════════════════════════════════════════════════════════

    // ── from_str produces same result as new ──────────────────────

    #[test]
    fn from_str_matches_new(text in text_strategy()) {
        let text_str = std::str::from_utf8(&text).unwrap();
        let sa_bytes = SuffixArray::new(&text);
        let sa_str = SuffixArray::from_str(text_str);

        prop_assert_eq!(sa_bytes.suffix_array(), sa_str.suffix_array());
        prop_assert_eq!(sa_bytes.lcp_array(), sa_str.lcp_array());
        prop_assert_eq!(sa_bytes.len(), sa_str.len());
    }

    // ── search_str matches search ────────────────────────────────

    #[test]
    fn search_str_matches_search(
        text in text_strategy(),
        pattern in pattern_strategy()
    ) {
        let sa = SuffixArray::new(&text);
        let byte_results = sa.search(&pattern);

        let pattern_str = std::str::from_utf8(&pattern).unwrap();
        let str_results = sa.search_str(pattern_str);

        prop_assert_eq!(byte_results, str_results, "search_str differs from search");
    }

    // ── count_str matches count ──────────────────────────────────

    #[test]
    fn count_str_matches_count(
        text in text_strategy(),
        pattern in pattern_strategy()
    ) {
        let sa = SuffixArray::new(&text);
        let byte_count = sa.count(&pattern);

        let pattern_str = std::str::from_utf8(&pattern).unwrap();
        let str_count = sa.count_str(pattern_str);

        prop_assert_eq!(byte_count, str_count, "count_str differs from count");
    }

    // ── text() returns original text ─────────────────────────────

    #[test]
    fn text_preserved(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        prop_assert_eq!(sa.text(), &text[..], "text() doesn't match input");
    }

    // ── Search for prefix of text ────────────────────────────────

    #[test]
    fn search_prefix(text in text_strategy()) {
        if text.len() < 2 {
            return Ok(());
        }
        let prefix_len = text.len() / 2;
        let prefix = &text[..prefix_len];
        let sa = SuffixArray::new(&text);
        let results = sa.search(prefix);
        let expected = brute_force_search(&text, prefix);
        prop_assert_eq!(results, expected, "prefix search mismatch");
    }

    // ── Search for suffix of text ────────────────────────────────

    #[test]
    fn search_suffix(text in text_strategy()) {
        if text.len() < 2 {
            return Ok(());
        }
        let suffix_start = text.len() / 2;
        let suffix = &text[suffix_start..];
        let sa = SuffixArray::new(&text);
        let results = sa.search(suffix);
        let expected = brute_force_search(&text, suffix);
        prop_assert_eq!(results, expected, "suffix search mismatch");
    }

    // ── Absent pattern returns empty ─────────────────────────────

    #[test]
    fn absent_pattern_empty(text in prop::collection::vec(b'a'..=b'c', 1..50)) {
        // Use only a-c in text, search for 'd' to guarantee absence
        let sa = SuffixArray::new(&text);
        let results = sa.search(b"d");
        prop_assert!(results.is_empty(), "should not find 'd' in text of a-c only");
        prop_assert_eq!(sa.count(b"d"), 0);
    }

    // ── Uniform text (all same character) ────────────────────────

    #[test]
    fn uniform_text(len in 1usize..50, byte in b'a'..=b'z') {
        let text: Vec<u8> = vec![byte; len];
        let sa = SuffixArray::new(&text);

        // SA should be [n-1, n-2, ..., 0] for uniform text
        let expected_sa: Vec<usize> = (0..len).rev().collect();
        prop_assert_eq!(sa.suffix_array().to_vec(), expected_sa, "uniform text SA wrong");

        // LCP should be [0, 1, 2, ..., n-1]
        let expected_lcp: Vec<usize> = (0..len).collect();
        prop_assert_eq!(sa.lcp_array().to_vec(), expected_lcp, "uniform text LCP wrong");

        // Search for single byte should find len positions
        prop_assert_eq!(sa.count(&[byte]), len);
    }

    // ── Monotonic (sorted) text ──────────────────────────────────

    #[test]
    fn monotonic_text(len in 2usize..26) {
        // abcdef...
        let text: Vec<u8> = (0..len).map(|i| b'a' + i as u8).collect();
        let sa = SuffixArray::new(&text);

        // SA should be [0, 1, 2, ..., n-1] for strictly increasing text
        let expected_sa: Vec<usize> = (0..len).collect();
        prop_assert_eq!(sa.suffix_array().to_vec(), expected_sa, "monotonic SA wrong");

        // All characters are distinct, so all substrings are distinct
        let n = len;
        let distinct = sa.distinct_substring_count();
        prop_assert_eq!(distinct, n * (n + 1) / 2, "monotonic distinct count wrong");
    }

    // ── LCP values are non-negative ──────────────────────────────

    #[test]
    fn lcp_non_negative(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let lcp = sa.lcp_array();

        for (i, &val) in lcp.iter().enumerate() {
            // usize is inherently non-negative, but verify no logical overflow
            prop_assert!(val <= text.len(), "LCP[{}] = {} exceeds text length {}", i, val, text.len());
        }
    }

    // ── SA inverse: rank and SA are inverse permutations ─────────

    #[test]
    fn sa_inverse_permutation(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let arr = sa.suffix_array();
        let n = arr.len();

        // Build inverse (rank array): rank[sa[i]] = i
        let mut rank = vec![0usize; n];
        for (i, &pos) in arr.iter().enumerate() {
            rank[pos] = i;
        }

        // Verify: sa[rank[j]] == j for all j
        for j in 0..n {
            prop_assert_eq!(arr[rank[j]], j, "SA inverse failed at position {}", j);
        }
    }

    // ── Search completeness: brute force doesn't find extras ────

    #[test]
    fn search_completeness(
        text in small_text_strategy(),
        pattern in prop::collection::vec(b'a'..=b'd', 1..4)
    ) {
        let sa = SuffixArray::new(&text);
        let sa_results = sa.search(&pattern);
        let bf_results = brute_force_search(&text, &pattern);

        // SA should find every occurrence brute force finds
        for &pos in &bf_results {
            let found = sa_results.contains(&pos);
            prop_assert!(found, "SA missed occurrence at {}", pos);
        }
        // SA should not find any false positives
        for &pos in &sa_results {
            let found = bf_results.contains(&pos);
            prop_assert!(found, "SA false positive at {}", pos);
        }
    }

    // ── Pattern longer than text returns empty ───────────────────

    #[test]
    fn pattern_longer_than_text_empty(
        text in prop::collection::vec(b'a'..=b'z', 1..10)
    ) {
        let sa = SuffixArray::new(&text);
        let long_pattern: Vec<u8> = vec![b'a'; text.len() + 5];
        let results = sa.search(&long_pattern);
        prop_assert!(results.is_empty(), "pattern longer than text should return empty");
        prop_assert_eq!(sa.count(&long_pattern), 0);
    }

    // ── Longest repeated for all-distinct text is 0 ─────────────

    #[test]
    fn longest_repeated_distinct_text(len in 2usize..26) {
        // All distinct characters: no repeated substring
        let text: Vec<u8> = (0..len).map(|i| b'a' + i as u8).collect();
        let sa = SuffixArray::new(&text);
        let (_, max_len) = sa.longest_repeated_substring();
        prop_assert_eq!(max_len, 0, "all-distinct text should have no repeated substring");
    }

    // ── Display produces non-empty output ────────────────────────

    #[test]
    fn display_format(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let displayed = format!("{}", sa);
        prop_assert!(!displayed.is_empty(), "Display should produce non-empty output");
    }

    // ── Serde preserves search results ──────────────────────────

    #[test]
    fn serde_preserves_search(
        text in small_text_strategy(),
        pattern in prop::collection::vec(b'a'..=b'd', 1..4)
    ) {
        let sa = SuffixArray::new(&text);
        let json = serde_json::to_string(&sa).unwrap();
        let restored: SuffixArray = serde_json::from_str(&json).unwrap();

        let original_results = sa.search(&pattern);
        let restored_results = restored.search(&pattern);
        prop_assert_eq!(original_results, restored_results, "serde broke search results");
    }

    // ── longest_repeated_substring_str matches byte version ─────

    #[test]
    fn longest_repeated_str_matches_bytes(text in text_strategy()) {
        let sa = SuffixArray::new(&text);
        let (pos, len) = sa.longest_repeated_substring();
        let str_result = sa.longest_repeated_substring_str();

        if len == 0 {
            prop_assert!(str_result.is_empty(), "str version should be empty when len=0");
        } else {
            prop_assert_eq!(str_result, &text[pos..pos + len], "str version mismatch");
        }
    }
}
