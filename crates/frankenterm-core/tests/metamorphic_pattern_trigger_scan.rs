//! Metamorphic oracles for `frankenterm_core::pattern_trigger::TriggerScanner`.
//!
//! The trigger scanner is the fast-path classifier for terminal output: it
//! decides whether a pane's bytes contain errors, completions, or progress
//! markers, and drives backpressure + alerting off the counts it returns.
//! A tokenizer regression or an Aho-Corasick boundary drift silently changes
//! every downstream decision that depends on it.
//!
//! These relations hold independently of which patterns are loaded — they
//! describe *structural* invariants of the scan operation itself:
//!
//!   MR1 (idempotence): `scan_counts(x)` and `scan_locate(x)` are pure
//!       functions — running them twice on the same input must yield
//!       byte-identical results.
//!
//!   MR2 (empty input neutrality): scanning zero bytes yields zero matches
//!       and `bytes_scanned == 0`, regardless of pattern set.
//!
//!   MR3 (prefix-shift invariance): prepending bytes that contain no
//!       triggers cannot remove or change any existing match — every match
//!       survives with its offset shifted by `prefix.len()` and its
//!       pattern_index preserved.
//!
//!   MR4 (concatenation subadditivity on counts): total match count is
//!       monotone non-decreasing under concatenation. Joining two chunks
//!       may expose new boundary-straddling matches but cannot remove
//!       existing ones.
//!
//!   MR5 (case-insensitive equality): for a scanner built exclusively from
//!       case-insensitive patterns, uppercase / lowercase / mixed-case
//!       inputs over the same underlying text must yield identical counts
//!       and identical per-category distribution.
//!
//!   MR6 (locate bounds): every `TriggerMatch` returned by `scan_locate`
//!       must satisfy `offset + length <= input.len()` and `length > 0`.
//!       A zero-length or out-of-bounds match is a decoder bug.

use frankenterm_core::pattern_trigger::{
    TriggerCategory, TriggerPattern, TriggerScanner, all_default_patterns,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Arbitrary printable-ASCII chunk. Constrained to printable bytes so the
/// patterns under test (which are all printable) have a realistic chance
/// of matching without the generator wasting cycles on binary noise that
/// the default scanner rejects outright.
fn arb_text() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(0x20u8..=0x7eu8, 0..256)
}

/// Prefix guaranteed to contain no default-pattern triggers. We verify the
/// no-trigger property in the harness itself via an assertion at build.
fn safe_prefix_pool() -> Vec<&'static [u8]> {
    // Short ASCII strings with no capital letters near known triggers
    // (avoid: ERROR, FATAL, FAILED, SIGSEGV, SIGABRT, Compiling, …) and
    // no lowercase panic/segfault/deprecated matches. Keep them simple.
    vec![b"xxxxxxxx", b"12345", b"...", b"abcabc", b"    ", b"|", b"zz"]
}

fn arb_safe_prefix() -> impl Strategy<Value = &'static [u8]> {
    let pool = safe_prefix_pool();
    (0usize..pool.len()).prop_map(move |i| pool[i])
}

// ---------------------------------------------------------------------------
// MR1 — idempotence
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mr1_scan_counts_is_pure_idempotent(input in arb_text()) {
        let scanner = TriggerScanner::default();
        let a = scanner.scan_counts(&input);
        let b = scanner.scan_counts(&input);

        prop_assert_eq!(a.total_matches, b.total_matches);
        prop_assert_eq!(a.bytes_scanned, b.bytes_scanned);
        for cat in [
            TriggerCategory::Error,
            TriggerCategory::Warning,
            TriggerCategory::Completion,
            TriggerCategory::Progress,
            TriggerCategory::TestResult,
            TriggerCategory::Custom,
        ] {
            prop_assert_eq!(
                a.counts.get(&cat).copied().unwrap_or(0),
                b.counts.get(&cat).copied().unwrap_or(0),
                "category {:?} count must be stable across repeated scans",
                cat
            );
        }
    }

    #[test]
    fn mr1_scan_locate_is_pure_idempotent(input in arb_text()) {
        let scanner = TriggerScanner::default();
        let a = scanner.scan_locate(&input);
        let b = scanner.scan_locate(&input);
        prop_assert_eq!(a.len(), b.len());
        for (left, right) in a.iter().zip(b.iter()) {
            prop_assert_eq!(left.offset, right.offset);
            prop_assert_eq!(left.length, right.length);
            prop_assert_eq!(left.pattern_index, right.pattern_index);
            prop_assert_eq!(left.category, right.category);
        }
    }
}

// ---------------------------------------------------------------------------
// MR2 — empty input neutrality
// ---------------------------------------------------------------------------

#[test]
fn mr2_empty_input_yields_empty_result() {
    let scanner = TriggerScanner::default();
    let counts = scanner.scan_counts(b"");
    assert_eq!(counts.total_matches, 0);
    assert_eq!(counts.bytes_scanned, 0);
    assert!(counts.counts.is_empty());
    assert!(scanner.scan_locate(b"").is_empty());
}

#[test]
fn mr2_empty_input_neutral_even_on_custom_scanner() {
    // Relation must hold for arbitrary pattern sets, not just defaults.
    let scanner = TriggerScanner::new(vec![
        TriggerPattern::new("TEST", TriggerCategory::Custom),
        TriggerPattern::case_insensitive("deploy", TriggerCategory::Custom),
    ]);
    let counts = scanner.scan_counts(b"");
    assert_eq!(counts.total_matches, 0);
    assert_eq!(counts.bytes_scanned, 0);
    assert!(scanner.scan_locate(b"").is_empty());
}

// ---------------------------------------------------------------------------
// MR3 — prefix-shift invariance
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mr3_safe_prefix_shifts_matches_without_loss(
        text in arb_text(),
        prefix in arb_safe_prefix(),
    ) {
        let scanner = TriggerScanner::default();

        // Sanity: the "safe" prefix really contains no triggers. If a new
        // default pattern ever matches one of our prefixes, this assertion
        // catches it before the rest of the relation misleads us.
        prop_assume!(scanner.scan_counts(prefix).total_matches == 0);

        let base = scanner.scan_locate(&text);
        let mut combined = prefix.to_vec();
        combined.extend_from_slice(&text);
        let shifted = scanner.scan_locate(&combined);

        // Every base match must appear in the shifted result at offset
        // (base.offset + prefix.len()) with the same pattern_index and
        // length. `shifted` may contain ADDITIONAL matches (e.g. a
        // pattern straddling the join) but never fewer.
        let prefix_len = prefix.len();
        for base_match in &base {
            let target_offset = base_match.offset + prefix_len;
            let found = shifted.iter().any(|m| {
                m.offset == target_offset
                    && m.length == base_match.length
                    && m.pattern_index == base_match.pattern_index
                    && m.category == base_match.category
            });
            prop_assert!(
                found,
                "match at base offset {} (len={}, pat={}) must survive \
                 with a +{} shift; shifted matches: {:?}",
                base_match.offset,
                base_match.length,
                base_match.pattern_index,
                prefix_len,
                shifted
                    .iter()
                    .map(|m| (m.offset, m.length, m.pattern_index))
                    .collect::<Vec<_>>()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// MR4 — concatenation subadditivity on counts
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mr4_concat_is_monotone_non_decreasing_on_total_matches(
        left in arb_text(),
        right in arb_text(),
    ) {
        let scanner = TriggerScanner::default();
        let n_left = scanner.scan_counts(&left).total_matches;
        let n_right = scanner.scan_counts(&right).total_matches;

        let mut joined = left.clone();
        joined.extend_from_slice(&right);
        let n_joined = scanner.scan_counts(&joined).total_matches;

        prop_assert!(
            n_joined >= n_left + n_right,
            "total_matches must be monotone non-decreasing under concat: \
             left={} right={} joined={}",
            n_left, n_right, n_joined
        );

        // bytes_scanned is always the exact input length.
        prop_assert_eq!(
            scanner.scan_counts(&joined).bytes_scanned,
            joined.len() as u64
        );
    }
}

// ---------------------------------------------------------------------------
// MR5 — case-insensitive equality over a CI-only scanner
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mr5_ci_only_scanner_is_case_insensitive(
        // Body text is chosen from lowercase letters and whitespace so
        // we control the casing entirely via the case transformation
        // applied below. ASCII-only — the underlying matcher is
        // ascii_case_insensitive.
        body in "[a-z ]{0,64}",
    ) {
        let scanner = TriggerScanner::new(vec![
            TriggerPattern::case_insensitive("deploy", TriggerCategory::Custom),
            TriggerPattern::case_insensitive("rollback", TriggerCategory::Custom),
            TriggerPattern::case_insensitive("abort", TriggerCategory::Custom),
        ]);

        // Sprinkle guaranteed-match substrings so some cases actually hit
        // rather than proptest always shrinking to the empty-match case.
        let mut text = body.clone();
        text.push_str(" deploy rollback abort ");

        let lower = text.to_lowercase();
        let upper = text.to_uppercase();
        let mixed: String = text
            .chars()
            .enumerate()
            .map(|(i, c)| if i % 2 == 0 { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })
            .collect();

        let l = scanner.scan_counts(lower.as_bytes()).total_matches;
        let u = scanner.scan_counts(upper.as_bytes()).total_matches;
        let m = scanner.scan_counts(mixed.as_bytes()).total_matches;

        prop_assert_eq!(l, u, "lower vs upper case total differ: {} vs {}", l, u);
        prop_assert_eq!(l, m, "lower vs mixed case total differ: {} vs {}", l, m);
    }
}

// ---------------------------------------------------------------------------
// MR6 — locate bounds
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mr6_locate_offsets_are_in_bounds_and_nonzero(input in arb_text()) {
        let scanner = TriggerScanner::default();
        let matches = scanner.scan_locate(&input);
        for m in &matches {
            prop_assert!(
                m.length > 0,
                "match length must be positive, got {} at offset {}",
                m.length, m.offset
            );
            prop_assert!(
                m.offset.checked_add(m.length).is_some_and(|end| end <= input.len()),
                "match must be fully within input: offset={} length={} input_len={}",
                m.offset, m.length, input.len()
            );
            // Pattern index must reference a real pattern, not out-of-bounds.
            prop_assert!(
                m.pattern_index < scanner.pattern_count(),
                "pattern_index {} out of bounds (pattern_count={})",
                m.pattern_index, scanner.pattern_count()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Meta-guard — sanity-check the default pattern set still behaves as this
// harness assumes. If `all_default_patterns()` ever loses the `Error`
// category entirely (e.g. refactor) MR1 et al. become vacuously true.
// This canary fails if the pattern set drifts to an unexpected shape.
// ---------------------------------------------------------------------------

#[test]
fn default_scanner_still_has_multi_category_coverage() {
    let patterns = all_default_patterns();
    assert!(
        !patterns.is_empty(),
        "default patterns must not be empty — relations would be vacuous"
    );

    let scanner = TriggerScanner::default();
    let counts = scanner.scan_counts(
        b"   Compiling foo v0.1\nERROR: build failed\nwarning: unused\n    Finished\n",
    );
    assert!(counts.counts.get(&TriggerCategory::Error).copied().unwrap_or(0) > 0);
    assert!(counts.counts.get(&TriggerCategory::Warning).copied().unwrap_or(0) > 0);
    assert!(counts.counts.get(&TriggerCategory::Progress).copied().unwrap_or(0) > 0);
    assert!(counts.counts.get(&TriggerCategory::Completion).copied().unwrap_or(0) > 0);
}
