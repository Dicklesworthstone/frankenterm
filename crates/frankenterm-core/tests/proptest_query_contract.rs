//! Property-based tests for the query_contract module.
//!
//! Verifies structural invariants of unified search query parsing,
//! validation, mode checking, and storage option mapping across
//! randomized inputs.

use proptest::prelude::*;

use frankenterm_core::query_contract::{
    SEARCH_HIGHLIGHT_PREFIX, SEARCH_HIGHLIGHT_SUFFIX, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX,
    SEARCH_SNIPPET_MAX_TOKENS, SearchQueryDefaults, SearchQueryInput, SearchQueryValidationError,
    UnifiedSearchMode, UnifiedSearchQuery, ensure_mode_supported, format_lint_hint,
    lints_have_errors, parse_unified_search_query, to_storage_search_options,
};
use frankenterm_core::storage::{SearchLint, SearchLintSeverity};

// ── Strategies ────────────────────────────────────────────────────────

fn arb_search_mode() -> impl Strategy<Value = UnifiedSearchMode> {
    prop_oneof![
        Just(UnifiedSearchMode::Lexical),
        Just(UnifiedSearchMode::Semantic),
        Just(UnifiedSearchMode::Hybrid),
    ]
}

fn arb_lint_severity() -> impl Strategy<Value = SearchLintSeverity> {
    prop_oneof![
        Just(SearchLintSeverity::Warning),
        Just(SearchLintSeverity::Error),
    ]
}

fn arb_search_lint() -> impl Strategy<Value = SearchLint> {
    (
        "[a-z]{3,10}\\.[a-z_]{3,15}",
        arb_lint_severity(),
        "[A-Za-z ]{5,30}",
        proptest::option::of("[A-Za-z ]{5,20}"),
    )
        .prop_map(|(code, severity, message, suggestion)| SearchLint {
            code,
            severity,
            message,
            suggestion,
        })
}

/// Generates valid FTS query strings (alphanumeric words, no leading operators).
fn arb_valid_query() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z]{3,15}",
        "[a-z]{3,10} [a-z]{3,10}",
        "\"[a-z]{3,10} [a-z]{3,10}\"",
    ]
}

/// Generates search query inputs with valid parameters.
fn arb_valid_search_input() -> impl Strategy<Value = SearchQueryInput> {
    (
        arb_valid_query(),
        proptest::option::of(1_usize..=SEARCH_LIMIT_MAX),
        proptest::option::of(0_u64..10_000),
        proptest::option::of(0_i64..100_000),
        proptest::option::of(0_i64..100_000),
        proptest::option::of(proptest::bool::ANY),
        proptest::option::of(arb_search_mode()),
    )
        .prop_map(|(query, limit, pane, since, until, snippets, mode)| {
            // Ensure since <= until when both are present
            let (since, until) = match (since, until) {
                (Some(s), Some(u)) if s > u => (Some(u), Some(s)),
                other => other,
            };
            SearchQueryInput {
                query,
                limit,
                pane,
                since,
                until,
                snippets,
                mode,
            }
        })
}

fn arb_unified_search_query() -> impl Strategy<Value = UnifiedSearchQuery> {
    (
        arb_valid_query(),
        1_usize..=SEARCH_LIMIT_MAX,
        proptest::option::of(0_u64..10_000),
        proptest::option::of(0_i64..100_000),
        proptest::option::of(0_i64..100_000),
        proptest::bool::ANY,
        arb_search_mode(),
    )
        .prop_map(|(query, limit, pane, since, until, snippets, mode)| {
            let (since, until) = match (since, until) {
                (Some(s), Some(u)) if s > u => (Some(u), Some(s)),
                other => other,
            };
            UnifiedSearchQuery {
                query,
                limit,
                pane,
                since,
                until,
                snippets,
                mode,
            }
        })
}

// ── Parse validation invariants ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Valid inputs always parse successfully.
    #[test]
    fn valid_inputs_parse_successfully(input in arb_valid_search_input()) {
        let result = parse_unified_search_query(input, SearchQueryDefaults::default());
        prop_assert!(result.is_ok(), "valid input should parse: {:?}", result.err());
    }

    /// Parsed query limit is always within [1, max_limit].
    #[test]
    fn parsed_limit_within_bounds(input in arb_valid_search_input()) {
        if let Ok(output) = parse_unified_search_query(input, SearchQueryDefaults::default()) {
            prop_assert!(output.query.limit >= 1, "limit must be >= 1");
            prop_assert!(output.query.limit <= SEARCH_LIMIT_MAX, "limit must be <= max");
        }
    }

    /// When limit is None, defaults are applied.
    #[test]
    fn none_limit_uses_default(query in arb_valid_query()) {
        let input = SearchQueryInput {
            query,
            limit: None,
            ..SearchQueryInput::default()
        };
        let output = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect("should parse");
        prop_assert_eq!(output.query.limit, SEARCH_LIMIT_DEFAULT);
    }

    /// When snippets is None, defaults are applied.
    #[test]
    fn none_snippets_uses_default(query in arb_valid_query()) {
        let input = SearchQueryInput {
            query,
            snippets: None,
            ..SearchQueryInput::default()
        };
        let defaults = SearchQueryDefaults::default();
        let output = parse_unified_search_query(input, defaults).expect("should parse");
        prop_assert_eq!(output.query.snippets, defaults.snippets);
    }

    /// When mode is None, defaults are applied.
    #[test]
    fn none_mode_uses_default(query in arb_valid_query()) {
        let input = SearchQueryInput {
            query,
            mode: None,
            ..SearchQueryInput::default()
        };
        let defaults = SearchQueryDefaults::default();
        let output = parse_unified_search_query(input, defaults).expect("should parse");
        prop_assert_eq!(output.query.mode, defaults.mode);
    }

    /// Explicit values are preserved in parsed output.
    #[test]
    fn explicit_values_preserved(
        query in arb_valid_query(),
        limit in 1_usize..=SEARCH_LIMIT_MAX,
        pane in proptest::option::of(0_u64..1000),
        snippets in proptest::bool::ANY,
        mode in arb_search_mode(),
    ) {
        let input = SearchQueryInput {
            query: query.clone(),
            limit: Some(limit),
            pane,
            since: None,
            until: None,
            snippets: Some(snippets),
            mode: Some(mode),
        };
        let output = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect("should parse");
        prop_assert_eq!(output.query.limit, limit);
        prop_assert_eq!(output.query.pane, pane);
        prop_assert_eq!(output.query.snippets, snippets);
        prop_assert_eq!(output.query.mode, mode);
    }

    /// Query text is trimmed of leading/trailing whitespace.
    #[test]
    fn query_whitespace_trimmed(query in arb_valid_query()) {
        let padded = format!("  {}  ", query);
        let input = SearchQueryInput {
            query: padded,
            ..SearchQueryInput::default()
        };
        let output = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect("should parse");
        prop_assert_eq!(output.query.query, query.trim());
    }
}

// ── Limit validation ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Limit = 0 always produces InvalidLimit error.
    #[test]
    fn zero_limit_rejected(query in arb_valid_query()) {
        let input = SearchQueryInput {
            query,
            limit: Some(0),
            ..SearchQueryInput::default()
        };
        let err = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect_err("limit=0 should fail");
        match err {
            SearchQueryValidationError::InvalidLimit { provided, max_limit } => {
                prop_assert_eq!(provided, 0_usize);
                prop_assert_eq!(max_limit, SEARCH_LIMIT_MAX);
            }
            other => prop_assert!(false, "expected InvalidLimit, got {:?}", other),
        }
    }

    /// Limit > max always produces InvalidLimit error.
    #[test]
    fn over_max_limit_rejected(
        query in arb_valid_query(),
        excess in 1_usize..10_000,
    ) {
        let over_limit = SEARCH_LIMIT_MAX + excess;
        let input = SearchQueryInput {
            query,
            limit: Some(over_limit),
            ..SearchQueryInput::default()
        };
        let err = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect_err("over-max limit should fail");
        match err {
            SearchQueryValidationError::InvalidLimit { provided, .. } => {
                prop_assert_eq!(provided, over_limit);
            }
            other => prop_assert!(false, "expected InvalidLimit, got {:?}", other),
        }
    }

    /// All limits in [1, max] are accepted.
    #[test]
    fn valid_limits_accepted(
        query in arb_valid_query(),
        limit in 1_usize..=SEARCH_LIMIT_MAX,
    ) {
        let input = SearchQueryInput {
            query,
            limit: Some(limit),
            ..SearchQueryInput::default()
        };
        let output = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect("valid limit should parse");
        prop_assert_eq!(output.query.limit, limit);
    }
}

// ── Time range validation ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// since > until always produces InvalidTimeRange error.
    #[test]
    fn inverted_time_range_rejected(
        query in arb_valid_query(),
        since in 1_i64..100_000,
        gap in 1_i64..50_000,
    ) {
        let until = since - gap;
        let input = SearchQueryInput {
            query,
            since: Some(since),
            until: Some(until),
            ..SearchQueryInput::default()
        };
        let err = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect_err("inverted range should fail");
        match err {
            SearchQueryValidationError::InvalidTimeRange { since: s, until: u } => {
                prop_assert_eq!(s, since);
                prop_assert_eq!(u, until);
            }
            other => prop_assert!(false, "expected InvalidTimeRange, got {:?}", other),
        }
    }

    /// since == until is always accepted.
    #[test]
    fn equal_since_until_accepted(
        query in arb_valid_query(),
        ts in 0_i64..100_000,
    ) {
        let input = SearchQueryInput {
            query,
            since: Some(ts),
            until: Some(ts),
            ..SearchQueryInput::default()
        };
        let output = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect("equal since/until should parse");
        prop_assert_eq!(output.query.since, Some(ts));
        prop_assert_eq!(output.query.until, Some(ts));
    }

    /// since < until is always accepted.
    #[test]
    fn valid_time_range_accepted(
        query in arb_valid_query(),
        since in 0_i64..50_000,
        gap in 0_i64..50_000,
    ) {
        let until = since + gap;
        let input = SearchQueryInput {
            query,
            since: Some(since),
            until: Some(until),
            ..SearchQueryInput::default()
        };
        let output = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect("valid range should parse");
        prop_assert_eq!(output.query.since, Some(since));
        prop_assert_eq!(output.query.until, Some(until));
    }

    /// Only since present (no until) is always accepted.
    #[test]
    fn since_only_accepted(
        query in arb_valid_query(),
        since in 0_i64..100_000,
    ) {
        let input = SearchQueryInput {
            query,
            since: Some(since),
            until: None,
            ..SearchQueryInput::default()
        };
        let output = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect("since-only should parse");
        prop_assert_eq!(output.query.since, Some(since));
        prop_assert!(output.query.until.is_none());
    }

    /// Only until present (no since) is always accepted.
    #[test]
    fn until_only_accepted(
        query in arb_valid_query(),
        until in 0_i64..100_000,
    ) {
        let input = SearchQueryInput {
            query,
            since: None,
            until: Some(until),
            ..SearchQueryInput::default()
        };
        let output = parse_unified_search_query(input, SearchQueryDefaults::default())
            .expect("until-only should parse");
        prop_assert!(output.query.since.is_none());
        prop_assert_eq!(output.query.until, Some(until));
    }
}

// ── UnifiedSearchMode invariants ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// All modes survive JSON serde roundtrip.
    #[test]
    fn mode_serde_roundtrip(mode in arb_search_mode()) {
        let json = serde_json::to_string(&mode).expect("serialize mode");
        let restored: UnifiedSearchMode = serde_json::from_str(&json).expect("deserialize mode");
        prop_assert_eq!(restored, mode);
    }

    /// as_str() returns non-empty lowercase string for all modes.
    #[test]
    fn mode_as_str_non_empty(mode in arb_search_mode()) {
        let s = mode.as_str();
        prop_assert!(!s.is_empty(), "as_str should not be empty");
        prop_assert_eq!(s, s.to_lowercase(), "as_str should be lowercase");
    }

    /// Serde serialization matches as_str() (wrapped in quotes).
    #[test]
    fn mode_serde_matches_as_str(mode in arb_search_mode()) {
        let json = serde_json::to_string(&mode).expect("serialize mode");
        let expected = format!("\"{}\"", mode.as_str());
        prop_assert_eq!(json, expected);
    }
}

// ── ensure_mode_supported invariants ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// A mode that's in the supported list always succeeds.
    #[test]
    fn mode_in_list_succeeds(mode in arb_search_mode()) {
        let all_modes = [
            UnifiedSearchMode::Lexical,
            UnifiedSearchMode::Semantic,
            UnifiedSearchMode::Hybrid,
        ];
        ensure_mode_supported(mode, &all_modes).expect("mode in full list should succeed");
    }

    /// A mode that's not in an empty list always fails.
    #[test]
    fn mode_in_empty_list_fails(mode in arb_search_mode()) {
        let err = ensure_mode_supported(mode, &[]).expect_err("empty list should fail");
        match err {
            SearchQueryValidationError::UnsupportedMode { mode: m, supported } => {
                prop_assert_eq!(m, mode);
                prop_assert!(supported.is_empty());
            }
            other => prop_assert!(false, "expected UnsupportedMode, got {:?}", other),
        }
    }
}

// ── UnifiedSearchQuery serde roundtrip ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// UnifiedSearchQuery survives JSON roundtrip with all field values preserved.
    #[test]
    fn unified_query_serde_roundtrip(query in arb_unified_search_query()) {
        let json = serde_json::to_string(&query).expect("serialize query");
        let restored: UnifiedSearchQuery = serde_json::from_str(&json).expect("deserialize query");
        prop_assert_eq!(restored.query, query.query);
        prop_assert_eq!(restored.limit, query.limit);
        prop_assert_eq!(restored.pane, query.pane);
        prop_assert_eq!(restored.since, query.since);
        prop_assert_eq!(restored.until, query.until);
        prop_assert_eq!(restored.snippets, query.snippets);
        prop_assert_eq!(restored.mode, query.mode);
    }

    /// None fields are omitted from JSON (skip_serializing_if).
    #[test]
    fn none_fields_omitted_in_json(query in arb_valid_query(), limit in 1_usize..100) {
        let q = UnifiedSearchQuery {
            query,
            limit,
            pane: None,
            since: None,
            until: None,
            snippets: false,
            mode: UnifiedSearchMode::Lexical,
        };
        let json = serde_json::to_string(&q).expect("serialize");
        prop_assert!(!json.contains("pane"), "pane:None should be omitted");
        prop_assert!(!json.contains("since"), "since:None should be omitted");
        prop_assert!(!json.contains("until"), "until:None should be omitted");
    }
}

// ── to_storage_search_options mapping ─────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Storage options map all query fields correctly.
    #[test]
    fn storage_options_field_mapping(query in arb_unified_search_query()) {
        let opts = to_storage_search_options(&query);
        prop_assert_eq!(opts.limit, Some(query.limit));
        prop_assert_eq!(opts.pane_id, query.pane);
        prop_assert_eq!(opts.since, query.since);
        prop_assert_eq!(opts.until, query.until);
        prop_assert_eq!(opts.include_snippets, Some(query.snippets));
        prop_assert_eq!(opts.snippet_max_tokens, Some(SEARCH_SNIPPET_MAX_TOKENS));
        prop_assert_eq!(opts.highlight_prefix.as_deref(), Some(SEARCH_HIGHLIGHT_PREFIX));
        prop_assert_eq!(opts.highlight_suffix.as_deref(), Some(SEARCH_HIGHLIGHT_SUFFIX));
    }
}

// ── Error code stability ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// InvalidLimit error code is always "search.invalid_limit".
    #[test]
    fn invalid_limit_code_stable(
        provided in 0_usize..10_000,
        max_limit in 1_usize..5_000,
    ) {
        let err = SearchQueryValidationError::InvalidLimit { provided, max_limit };
        prop_assert_eq!(err.code(), "search.invalid_limit");
        let msg = err.message();
        prop_assert!(msg.contains(&provided.to_string()), "message should contain provided");
        prop_assert!(msg.contains(&max_limit.to_string()), "message should contain max_limit");
        let display = format!("{}", err);
        prop_assert_eq!(display, msg, "Display should match message()");
    }

    /// InvalidTimeRange error code is always "search.invalid_time_range".
    #[test]
    fn invalid_time_range_code_stable(
        since in 0_i64..100_000,
        until in 0_i64..100_000,
    ) {
        let err = SearchQueryValidationError::InvalidTimeRange { since, until };
        prop_assert_eq!(err.code(), "search.invalid_time_range");
        let msg = err.message();
        prop_assert!(msg.contains(&since.to_string()), "message should contain since");
        prop_assert!(msg.contains(&until.to_string()), "message should contain until");
    }

    /// UnsupportedMode error code is always "search.unsupported_mode".
    #[test]
    fn unsupported_mode_code_stable(mode in arb_search_mode()) {
        let err = SearchQueryValidationError::UnsupportedMode {
            mode,
            supported: vec![],
        };
        prop_assert_eq!(err.code(), "search.unsupported_mode");
        let msg = err.message();
        prop_assert!(msg.contains(mode.as_str()), "message should contain mode name");
    }
}

// ── lints_have_errors invariants ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// lints_have_errors returns true iff at least one lint is Error severity.
    #[test]
    fn lints_have_errors_iff_error_present(lints in proptest::collection::vec(arb_search_lint(), 0..10)) {
        let has_error = lints.iter().any(|l| l.severity == SearchLintSeverity::Error);
        prop_assert_eq!(lints_have_errors(&lints), has_error);
    }

    /// Empty lint list never has errors.
    #[test]
    fn empty_lints_no_errors(_dummy in 0..1_u8) {
        prop_assert!(!lints_have_errors(&[]));
    }
}

// ── format_lint_hint invariants ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Empty lint list returns None.
    #[test]
    fn empty_lints_no_hint(_dummy in 0..1_u8) {
        prop_assert!(format_lint_hint(&[]).is_none());
    }

    /// Non-empty lint list returns Some hint.
    #[test]
    fn nonempty_lints_have_hint(lints in proptest::collection::vec(arb_search_lint(), 1..8)) {
        let hint = format_lint_hint(&lints);
        prop_assert!(hint.is_some(), "non-empty lints should produce hint");
    }

    /// Hint includes at most 3 lint messages (truncation).
    #[test]
    fn hint_truncates_at_three(lints in proptest::collection::vec(arb_search_lint(), 4..10)) {
        let hint = format_lint_hint(&lints).expect("hint should be present");
        // Pipe-separated: 3 items → 2 separators
        let separator_count = hint.matches(" | ").count();
        prop_assert!(separator_count <= 2,
            "hint should have at most 2 separators (3 items), got {}", separator_count);
    }

    /// First lint message always appears in hint.
    #[test]
    fn first_lint_in_hint(lints in proptest::collection::vec(arb_search_lint(), 1..5)) {
        let hint = format_lint_hint(&lints).expect("hint should be present");
        prop_assert!(hint.contains(&lints[0].message),
            "hint should contain first lint message");
    }

    /// Suggestion text appears when present.
    #[test]
    fn suggestion_appears_in_hint(
        message in "[A-Za-z]{5,20}",
        suggestion in "[A-Za-z]{5,15}",
    ) {
        let lints = vec![SearchLint {
            code: "test.code".to_string(),
            severity: SearchLintSeverity::Warning,
            message,
            suggestion: Some(suggestion.clone()),
        }];
        let hint = format_lint_hint(&lints).expect("hint should be present");
        prop_assert!(hint.contains(&format!("suggestion: {}", suggestion)),
            "hint should include suggestion text");
    }
}

// ── Constants sanity ──────────────────────────────────────────────────

#[test]
fn constants_are_consistent() {
    assert!(SEARCH_LIMIT_DEFAULT > 0);
    assert!(SEARCH_LIMIT_DEFAULT <= SEARCH_LIMIT_MAX);
    assert!(SEARCH_SNIPPET_MAX_TOKENS > 0);
    assert!(!SEARCH_HIGHLIGHT_PREFIX.is_empty());
    assert!(!SEARCH_HIGHLIGHT_SUFFIX.is_empty());
    assert_ne!(SEARCH_HIGHLIGHT_PREFIX, SEARCH_HIGHLIGHT_SUFFIX);
}

#[test]
fn default_mode_is_lexical() {
    assert_eq!(UnifiedSearchMode::default(), UnifiedSearchMode::Lexical);
}
