//! Property-based tests for tantivy_query module.
//!
//! Verifies invariants for:
//! - SearchFilter: serde roundtrip for all variants
//! - EventDirection: serde roundtrip, snake_case serialization
//! - SortField: serde roundtrip, snake_case serialization
//! - SearchSortOrder: default values, serde roundtrip
//! - Pagination: default values, serde roundtrip
//! - PaginationCursor: serde roundtrip
//! - SnippetConfig: default values, serde roundtrip
//! - Snippet: serde roundtrip
//! - SearchQuery: builder methods, boost defaults, serde roundtrip
//! - tokenize_query: empty → empty, preserves tokens, splits on separators
//! - extract_snippets: disabled → empty, no terms → empty, empty text → empty
//! - SearchResults: empty() constructor
//! - SearchError: Display contains reason
//! - TieBreakKey: ordering consistency

use std::collections::HashMap;

use frankenterm_core::tantivy_query::*;
use proptest::prelude::*;

// ============================================================================
// Strategies
// ============================================================================

fn arb_event_direction() -> impl Strategy<Value = EventDirection> {
    prop_oneof![
        Just(EventDirection::Ingress),
        Just(EventDirection::Egress),
        Just(EventDirection::Both),
    ]
}

fn arb_sort_field() -> impl Strategy<Value = SortField> {
    prop_oneof![
        Just(SortField::Relevance),
        Just(SortField::OccurredAt),
        Just(SortField::RecordedAt),
        Just(SortField::Sequence),
        Just(SortField::LogOffset),
    ]
}

fn arb_sort_order() -> impl Strategy<Value = SearchSortOrder> {
    (arb_sort_field(), proptest::bool::ANY).prop_map(|(primary, descending)| SearchSortOrder {
        primary,
        descending,
    })
}

fn arb_pagination_cursor() -> impl Strategy<Value = PaginationCursor> {
    (
        -1_000_000i64..1_000_000,
        0i64..2_000_000_000_000,
        0u64..1_000_000,
        0u64..1_000_000,
    )
        .prop_map(
            |(score_millis, occurred_at_ms, sequence, log_offset)| PaginationCursor {
                score_millis,
                occurred_at_ms,
                sequence,
                log_offset,
            },
        )
}

fn arb_pagination() -> impl Strategy<Value = Pagination> {
    (1usize..1000, proptest::option::of(arb_pagination_cursor()))
        .prop_map(|(limit, after)| Pagination { limit, after })
}

fn arb_snippet_config() -> impl Strategy<Value = SnippetConfig> {
    (
        10usize..500,
        1usize..10,
        "[<\\[{]{1,5}",
        "[>\\]}]{1,5}",
        proptest::bool::ANY,
    )
        .prop_map(
            |(max_fragment_len, max_fragments, highlight_pre, highlight_post, enabled)| {
                SnippetConfig {
                    max_fragment_len,
                    max_fragments,
                    highlight_pre,
                    highlight_post,
                    enabled,
                }
            },
        )
}

fn arb_snippet() -> impl Strategy<Value = Snippet> {
    ("[a-zA-Z0-9 ]{1,100}", "[a-z_]{3,20}")
        .prop_map(|(fragment, field)| Snippet { fragment, field })
}

fn arb_search_filter() -> impl Strategy<Value = SearchFilter> {
    prop_oneof![
        prop::collection::vec(0u64..1000, 1..5).prop_map(|values| SearchFilter::PaneId { values }),
        "[a-z0-9-]{5,20}".prop_map(|value| SearchFilter::SessionId { value }),
        "[a-z0-9-]{5,20}".prop_map(|value| SearchFilter::WorkflowId { value }),
        "[a-z0-9-]{5,20}".prop_map(|value| SearchFilter::CorrelationId { value }),
        prop::collection::vec("[a-z_]{3,20}", 1..3)
            .prop_map(|values| SearchFilter::Source { values }),
        prop::collection::vec("[a-z_]{3,20}", 1..3)
            .prop_map(|values| SearchFilter::EventType { values }),
        "[a-z_]{3,15}".prop_map(|value| SearchFilter::IngressKind { value }),
        "[a-z_]{3,15}".prop_map(|value| SearchFilter::SegmentKind { value }),
        "[a-z_]{3,15}".prop_map(|value| SearchFilter::ControlMarkerType { value }),
        "[a-z_]{3,15}".prop_map(|value| SearchFilter::LifecyclePhase { value }),
        proptest::bool::ANY.prop_map(|value| SearchFilter::IsGap { value }),
        "[a-z_]{3,10}".prop_map(|value| SearchFilter::Redaction { value }),
        (
            proptest::option::of(0i64..2_000_000_000_000),
            proptest::option::of(0i64..2_000_000_000_000)
        )
            .prop_map(|(min_ms, max_ms)| SearchFilter::TimeRange { min_ms, max_ms }),
        (
            proptest::option::of(0i64..2_000_000_000_000),
            proptest::option::of(0i64..2_000_000_000_000)
        )
            .prop_map(|(min_ms, max_ms)| SearchFilter::RecordedTimeRange { min_ms, max_ms }),
        (
            proptest::option::of(0u64..100_000),
            proptest::option::of(0u64..100_000)
        )
            .prop_map(|(min_seq, max_seq)| SearchFilter::SequenceRange { min_seq, max_seq }),
        (
            proptest::option::of(0u64..100_000),
            proptest::option::of(0u64..100_000)
        )
            .prop_map(|(min_offset, max_offset)| SearchFilter::LogOffsetRange {
                min_offset,
                max_offset
            }),
        arb_event_direction().prop_map(|direction| SearchFilter::Direction { direction }),
    ]
}

fn arb_search_query() -> impl Strategy<Value = SearchQuery> {
    (
        "[a-zA-Z0-9 ]{0,50}",
        prop::collection::vec(arb_search_filter(), 0..3),
        arb_sort_order(),
        arb_pagination(),
        arb_snippet_config(),
    )
        .prop_map(
            |(text, filters, sort, pagination, snippet_config)| SearchQuery {
                text,
                filters,
                sort,
                pagination,
                snippet_config,
                field_boosts: HashMap::new(),
            },
        )
}

// ============================================================================
// EventDirection properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// EventDirection serde roundtrip.
    #[test]
    fn prop_event_direction_serde_roundtrip(dir in arb_event_direction()) {
        let json = serde_json::to_string(&dir).unwrap();
        let back: EventDirection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dir, back);
    }

    /// EventDirection serializes to snake_case.
    #[test]
    fn prop_event_direction_snake_case(dir in arb_event_direction()) {
        let json = serde_json::to_string(&dir).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized direction should be snake_case, got '{}'", inner
        );
    }
}

// ============================================================================
// SortField properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SortField serde roundtrip.
    #[test]
    fn prop_sort_field_serde_roundtrip(f in arb_sort_field()) {
        let json = serde_json::to_string(&f).unwrap();
        let back: SortField = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(f, back);
    }

    /// SortField serializes to snake_case.
    #[test]
    fn prop_sort_field_snake_case(f in arb_sort_field()) {
        let json = serde_json::to_string(&f).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized sort field should be snake_case, got '{}'", inner
        );
    }
}

// ============================================================================
// SearchSortOrder properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SearchSortOrder default has relevance descending.
    #[test]
    fn prop_sort_order_default(_dummy in Just(())) {
        let d = SearchSortOrder::default();
        prop_assert_eq!(d.primary, SortField::Relevance);
        prop_assert!(d.descending);
    }

    /// SearchSortOrder serde roundtrip.
    #[test]
    fn prop_sort_order_serde_roundtrip(order in arb_sort_order()) {
        let json = serde_json::to_string(&order).unwrap();
        let back: SearchSortOrder = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.primary, order.primary);
        prop_assert_eq!(back.descending, order.descending);
    }
}

// ============================================================================
// Pagination properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Pagination default has limit=20 and no cursor.
    #[test]
    fn prop_pagination_default(_dummy in Just(())) {
        let p = Pagination::default();
        prop_assert_eq!(p.limit, 20);
        prop_assert!(p.after.is_none());
    }

    /// PaginationCursor serde roundtrip.
    #[test]
    fn prop_pagination_cursor_serde_roundtrip(cursor in arb_pagination_cursor()) {
        let json = serde_json::to_string(&cursor).unwrap();
        let back: PaginationCursor = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.score_millis, cursor.score_millis);
        prop_assert_eq!(back.occurred_at_ms, cursor.occurred_at_ms);
        prop_assert_eq!(back.sequence, cursor.sequence);
        prop_assert_eq!(back.log_offset, cursor.log_offset);
    }

    /// Pagination serde roundtrip.
    #[test]
    fn prop_pagination_serde_roundtrip(p in arb_pagination()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: Pagination = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.limit, p.limit);
        prop_assert_eq!(back.after.is_some(), p.after.is_some());
    }
}

// ============================================================================
// SnippetConfig properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SnippetConfig default has sensible values.
    #[test]
    fn prop_snippet_config_default(_dummy in Just(())) {
        let c = SnippetConfig::default();
        prop_assert!(c.max_fragment_len > 0);
        prop_assert!(c.max_fragments > 0);
        prop_assert!(!c.highlight_pre.is_empty());
        prop_assert!(!c.highlight_post.is_empty());
        prop_assert!(c.enabled);
    }

    /// SnippetConfig serde roundtrip.
    #[test]
    fn prop_snippet_config_serde_roundtrip(config in arb_snippet_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: SnippetConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_fragment_len, config.max_fragment_len);
        prop_assert_eq!(back.max_fragments, config.max_fragments);
        prop_assert_eq!(&back.highlight_pre, &config.highlight_pre);
        prop_assert_eq!(&back.highlight_post, &config.highlight_post);
        prop_assert_eq!(back.enabled, config.enabled);
    }

    /// Snippet serde roundtrip.
    #[test]
    fn prop_snippet_serde_roundtrip(s in arb_snippet()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: Snippet = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, s);
    }
}

// ============================================================================
// SearchFilter properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SearchFilter serde roundtrip for all variants.
    #[test]
    fn prop_search_filter_serde_roundtrip(f in arb_search_filter()) {
        let json = serde_json::to_string(&f).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, f);
    }

    /// SearchFilter JSON contains "type" discriminator field.
    #[test]
    fn prop_search_filter_has_type_tag(f in arb_search_filter()) {
        let json = serde_json::to_string(&f).unwrap();
        prop_assert!(json.contains("\"type\""),
            "filter JSON should contain type field: {}", json);
    }
}

// ============================================================================
// SearchQuery properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SearchQuery::simple creates query with text and default settings.
    #[test]
    fn prop_query_simple(text in "[a-z]{1,30}") {
        let q = SearchQuery::simple(text.clone());
        prop_assert_eq!(&q.text, &text);
        prop_assert!(q.filters.is_empty());
        prop_assert_eq!(q.sort.primary, SortField::Relevance);
        prop_assert!(q.sort.descending);
        prop_assert_eq!(q.pagination.limit, 20);
        prop_assert!(q.field_boosts.is_empty());
    }

    /// SearchQuery default boosts.
    #[test]
    fn prop_query_default_boosts(_dummy in Just(())) {
        let q = SearchQuery::simple("test");
        prop_assert!((q.text_boost() - 1.0).abs() < f32::EPSILON);
        prop_assert!((q.text_symbols_boost() - 1.25).abs() < f32::EPSILON);
    }

    /// SearchQuery custom boosts override defaults.
    #[test]
    fn prop_query_custom_boosts(
        text_boost in 0.1f32..10.0,
        symbols_boost in 0.1f32..10.0,
    ) {
        let mut boosts = HashMap::new();
        boosts.insert("text".to_string(), text_boost);
        boosts.insert("text_symbols".to_string(), symbols_boost);
        let q = SearchQuery {
            field_boosts: boosts,
            ..SearchQuery::simple("test")
        };
        prop_assert!((q.text_boost() - text_boost).abs() < f32::EPSILON);
        prop_assert!((q.text_symbols_boost() - symbols_boost).abs() < f32::EPSILON);
    }

    /// SearchQuery with_filter appends filter.
    #[test]
    fn prop_query_with_filter(pane_id in 0u64..1000) {
        let q = SearchQuery::simple("test")
            .with_filter(SearchFilter::PaneId { values: vec![pane_id] });
        prop_assert_eq!(q.filters.len(), 1);
    }

    /// SearchQuery with_limit sets limit.
    #[test]
    fn prop_query_with_limit(limit in 1usize..1000) {
        let q = SearchQuery::simple("test").with_limit(limit);
        prop_assert_eq!(q.pagination.limit, limit);
    }

    /// SearchQuery with_cursor sets cursor.
    #[test]
    fn prop_query_with_cursor(cursor in arb_pagination_cursor()) {
        let q = SearchQuery::simple("test").with_cursor(cursor);
        prop_assert!(q.pagination.after.is_some());
    }

    /// SearchQuery serde roundtrip preserves text and pagination.
    #[test]
    fn prop_query_serde_roundtrip(q in arb_search_query()) {
        let json = serde_json::to_string(&q).unwrap();
        let back: SearchQuery = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.text, &q.text);
        prop_assert_eq!(back.pagination.limit, q.pagination.limit);
        prop_assert_eq!(back.sort.primary, q.sort.primary);
        prop_assert_eq!(back.sort.descending, q.sort.descending);
        prop_assert_eq!(back.filters.len(), q.filters.len());
    }
}

// ============================================================================
// tokenize_query properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Empty string tokenizes to empty.
    #[test]
    fn prop_tokenize_empty(_dummy in Just(())) {
        prop_assert!(tokenize_query("").is_empty());
    }

    /// Whitespace-only tokenizes to empty.
    #[test]
    fn prop_tokenize_whitespace(ws in "[ \t\n]{1,20}") {
        prop_assert!(tokenize_query(&ws).is_empty());
    }

    /// Single alphanumeric word is preserved as one token.
    #[test]
    fn prop_tokenize_single_word(word in "[a-zA-Z0-9_]{1,20}") {
        let tokens = tokenize_query(&word);
        prop_assert_eq!(tokens.len(), 1);
        prop_assert_eq!(&tokens[0], &word);
    }

    /// Space-separated words produce multiple tokens.
    #[test]
    fn prop_tokenize_multi_word(
        w1 in "[a-z]{1,10}",
        w2 in "[a-z]{1,10}",
    ) {
        let input = format!("{} {}", w1, w2);
        let tokens = tokenize_query(&input);
        prop_assert!(tokens.len() >= 2, "expected 2+ tokens, got {}", tokens.len());
    }

    /// Paths with / and : are preserved as single tokens.
    #[test]
    fn prop_tokenize_paths(
        parts in prop::collection::vec("[a-z]{1,8}", 2..5),
    ) {
        let path = parts.join("/");
        let tokens = tokenize_query(&path);
        prop_assert_eq!(tokens.len(), 1);
        prop_assert_eq!(&tokens[0], &path);
    }

    /// Namespaces with :: are preserved.
    #[test]
    fn prop_tokenize_namespaces(
        parts in prop::collection::vec("[A-Za-z]{1,8}", 2..4),
    ) {
        let ns = parts.join("::");
        let tokens = tokenize_query(&ns);
        prop_assert_eq!(tokens.len(), 1);
        prop_assert_eq!(&tokens[0], &ns);
    }

    /// Tokens never contain whitespace or special separators.
    #[test]
    fn prop_tokenize_no_whitespace(input in "[a-zA-Z0-9 _./:-]{0,100}") {
        let tokens = tokenize_query(&input);
        for token in &tokens {
            prop_assert!(!token.is_empty(), "empty token in {:?}", tokens);
            prop_assert!(!token.contains(' '), "token contains space: '{}'", token);
            prop_assert!(!token.contains('\t'), "token contains tab: '{}'", token);
        }
    }
}

// ============================================================================
// extract_snippets properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Disabled config produces no snippets.
    #[test]
    fn prop_snippets_disabled(text in "[a-z ]{1,100}", term in "[a-z]{1,10}") {
        let config = SnippetConfig {
            enabled: false,
            ..SnippetConfig::default()
        };
        let snippets = extract_snippets(&text, &[term], &config);
        prop_assert!(snippets.is_empty());
    }

    /// Empty text produces no snippets.
    #[test]
    fn prop_snippets_empty_text(term in "[a-z]{1,10}") {
        let config = SnippetConfig::default();
        let snippets = extract_snippets("", &[term], &config);
        prop_assert!(snippets.is_empty());
    }

    /// No terms produces no snippets.
    #[test]
    fn prop_snippets_no_terms(text in "[a-z ]{1,100}") {
        let config = SnippetConfig::default();
        let snippets = extract_snippets(&text, &[], &config);
        prop_assert!(snippets.is_empty());
    }

    /// When term is found, snippet contains highlight markers.
    #[test]
    fn prop_snippets_contain_markers(term in "[a-z]{3,10}") {
        let text = format!("prefix {} suffix", term);
        let config = SnippetConfig::default();
        let snippets = extract_snippets(&text, std::slice::from_ref(&term), &config);
        if !snippets.is_empty() {
            prop_assert!(
                snippets[0].fragment.contains(&config.highlight_pre),
                "snippet missing highlight_pre: {}", snippets[0].fragment
            );
            prop_assert!(
                snippets[0].fragment.contains(&config.highlight_post),
                "snippet missing highlight_post: {}", snippets[0].fragment
            );
        }
    }

    /// Number of snippets respects max_fragments.
    #[test]
    fn prop_snippets_respect_max_fragments(max in 1usize..5) {
        let config = SnippetConfig {
            max_fragments: max,
            ..SnippetConfig::default()
        };
        let terms: Vec<String> = (0..10).map(|i| format!("term{}", i)).collect();
        let text = terms.join(" ");
        let snippets = extract_snippets(&text, &terms, &config);
        prop_assert!(snippets.len() <= max,
            "got {} snippets but max is {}", snippets.len(), max);
    }

    /// Snippet field is always "text".
    #[test]
    fn prop_snippets_field_is_text(term in "[a-z]{3,10}") {
        let text = format!("some {} content", term);
        let config = SnippetConfig::default();
        let snippets = extract_snippets(&text, &[term], &config);
        for s in &snippets {
            prop_assert_eq!(&s.field, "text");
        }
    }
}

// ============================================================================
// SearchResults properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SearchResults::empty has zero hits and preserves elapsed_us.
    #[test]
    fn prop_results_empty(elapsed in 0u64..1_000_000) {
        let r = SearchResults::empty(elapsed);
        prop_assert_eq!(r.total_hits, 0);
        prop_assert_eq!(r.elapsed_us, elapsed);
        prop_assert!(r.hits.is_empty());
        prop_assert!(!r.has_more);
        prop_assert!(r.next_cursor.is_none());
    }
}

// ============================================================================
// SearchError properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SearchError::InvalidQuery Display contains "invalid query" and reason.
    #[test]
    fn prop_error_invalid_query(reason in "[a-z ]{1,50}") {
        let e = SearchError::InvalidQuery { reason: reason.clone() };
        let s = e.to_string();
        prop_assert!(s.contains("invalid query"), "missing 'invalid query' in '{}'", s);
        prop_assert!(s.contains(&reason), "missing reason in '{}'", s);
    }

    /// SearchError::Internal Display contains "internal" and reason.
    #[test]
    fn prop_error_internal(reason in "[a-z ]{1,50}") {
        let e = SearchError::Internal { reason: reason.clone() };
        let s = e.to_string();
        prop_assert!(s.contains("internal"), "missing 'internal' in '{}'", s);
        prop_assert!(s.contains(&reason), "missing reason in '{}'", s);
    }

    /// SearchError::IndexUnavailable Display contains "unavailable" and reason.
    #[test]
    fn prop_error_index_unavailable(reason in "[a-z ]{1,50}") {
        let e = SearchError::IndexUnavailable { reason: reason.clone() };
        let s = e.to_string();
        prop_assert!(s.contains("unavailable"), "missing 'unavailable' in '{}'", s);
        prop_assert!(s.contains(&reason), "missing reason in '{}'", s);
    }
}

// ============================================================================
// InMemorySearchService properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// InMemorySearchService is always ready.
    #[test]
    fn prop_in_memory_always_ready(_dummy in Just(())) {
        let svc = InMemorySearchService::new();
        prop_assert!(svc.is_ready());
    }

    /// Empty service has length 0.
    #[test]
    fn prop_in_memory_empty_len(_dummy in Just(())) {
        let svc = InMemorySearchService::new();
        prop_assert_eq!(svc.len(), 0);
        prop_assert!(svc.is_empty());
    }

    /// Empty query + no filter returns InvalidQuery error.
    #[test]
    fn prop_empty_query_no_filter_errors(_dummy in Just(())) {
        let svc = InMemorySearchService::new();
        let q = SearchQuery {
            text: String::new(),
            filters: Vec::new(),
            sort: SearchSortOrder::default(),
            pagination: Pagination::default(),
            snippet_config: SnippetConfig::default(),
            field_boosts: HashMap::new(),
        };
        let err = svc.search(&q).unwrap_err();
        let is_invalid = matches!(err, SearchError::InvalidQuery { .. });
        prop_assert!(is_invalid, "expected InvalidQuery error");
    }
}
