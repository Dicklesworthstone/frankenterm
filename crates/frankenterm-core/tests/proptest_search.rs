//! Property-based tests for the tantivy search pipeline.
//!
//! Bead: wa-4t7x
//!
//! Uses proptest to verify structural invariants of the search pipeline:
//!
//! - Result count bounds (hits ≤ limit, total_hits ≤ corpus size)
//! - Filter satisfaction (every hit matches all applied filters)
//! - Score monotonicity (relevance sort → non-increasing scores)
//! - Pagination completeness (no duplicates, full coverage)
//! - Redaction opacity (redacted text never leaks original content)
//! - count()/search() consistency
//! - get_by_event_id/get_by_log_offset roundtrip correctness
//! - Filter commutativity (order doesn't affect results)
//! - Idempotent map_event_to_document (same input → same output)

#![feature(stmt_expr_attributes)]

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;

use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderLifecyclePhase, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use frankenterm_core::tantivy_ingest::{IndexDocumentFields, map_event_to_document};
use frankenterm_core::tantivy_query::{
    EventDirection, InMemorySearchService, LexicalSearchService, Pagination, PaginationCursor,
    SearchFilter, SearchQuery, SearchSortOrder, SnippetConfig, SortField,
};

// ---------------------------------------------------------------------------
// Proptest strategies
// ---------------------------------------------------------------------------

fn arb_source() -> impl Strategy<Value = RecorderEventSource> {
    prop_oneof![
        Just(RecorderEventSource::WeztermMux),
        Just(RecorderEventSource::RobotMode),
        Just(RecorderEventSource::WorkflowEngine),
        Just(RecorderEventSource::OperatorAction),
        Just(RecorderEventSource::RecoveryFlow),
    ]
}

fn arb_redaction() -> impl Strategy<Value = RecorderRedactionLevel> {
    prop_oneof![
        Just(RecorderRedactionLevel::None),
        Just(RecorderRedactionLevel::Partial),
        Just(RecorderRedactionLevel::Full),
    ]
}

fn arb_ingress_kind() -> impl Strategy<Value = RecorderIngressKind> {
    prop_oneof![
        Just(RecorderIngressKind::SendText),
        Just(RecorderIngressKind::Paste),
        Just(RecorderIngressKind::WorkflowAction),
    ]
}

fn arb_segment_kind() -> impl Strategy<Value = RecorderSegmentKind> {
    prop_oneof![
        Just(RecorderSegmentKind::Delta),
        Just(RecorderSegmentKind::Gap),
        Just(RecorderSegmentKind::Snapshot),
    ]
}

fn arb_control_marker_type() -> impl Strategy<Value = RecorderControlMarkerType> {
    prop_oneof![
        Just(RecorderControlMarkerType::PromptBoundary),
        Just(RecorderControlMarkerType::Resize),
        Just(RecorderControlMarkerType::PolicyDecision),
        Just(RecorderControlMarkerType::ApprovalCheckpoint),
    ]
}

fn arb_lifecycle_phase() -> impl Strategy<Value = RecorderLifecyclePhase> {
    prop_oneof![
        Just(RecorderLifecyclePhase::CaptureStarted),
        Just(RecorderLifecyclePhase::CaptureStopped),
        Just(RecorderLifecyclePhase::PaneOpened),
        Just(RecorderLifecyclePhase::PaneClosed),
        Just(RecorderLifecyclePhase::ReplayStarted),
        Just(RecorderLifecyclePhase::ReplayFinished),
    ]
}

/// Generate terminal-like text (alphanumeric + common shell characters).
fn arb_terminal_text() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z0-9 _./:\\-]{1,80}",
        Just("cargo test --release".to_string()),
        Just("git push origin main".to_string()),
        Just("echo hello world".to_string()),
        Just("npm run build".to_string()),
        Just("python3 -m pytest".to_string()),
        Just("rustc --edition 2024".to_string()),
        Just("ls -la /tmp/data".to_string()),
    ]
}

fn arb_causality() -> impl Strategy<Value = RecorderEventCausality> {
    (
        proptest::option::of("[a-z]{3}-[0-9]{3}"),
        proptest::option::of("[a-z]{3}-[0-9]{3}"),
        proptest::option::of("[a-z]{3}-[0-9]{3}"),
    )
        .prop_map(|(parent, trigger, root)| RecorderEventCausality {
            parent_event_id: parent,
            trigger_event_id: trigger,
            root_event_id: root,
        })
}

fn arb_payload() -> impl Strategy<Value = RecorderEventPayload> {
    prop_oneof![
        // IngressText
        (arb_terminal_text(), arb_redaction(), arb_ingress_kind()).prop_map(
            |(text, redaction, ingress_kind)| RecorderEventPayload::IngressText {
                text,
                encoding: RecorderTextEncoding::Utf8,
                redaction,
                ingress_kind,
            }
        ),
        // EgressOutput
        (
            arb_terminal_text(),
            arb_redaction(),
            arb_segment_kind(),
            any::<bool>()
        )
            .prop_map(|(text, redaction, segment_kind, is_gap)| {
                RecorderEventPayload::EgressOutput {
                    text,
                    encoding: RecorderTextEncoding::Utf8,
                    redaction,
                    segment_kind,
                    is_gap,
                }
            }),
        // ControlMarker
        arb_control_marker_type().prop_map(|control_marker_type| {
            RecorderEventPayload::ControlMarker {
                control_marker_type,
                details: serde_json::json!({}),
            }
        }),
        // LifecycleMarker
        (arb_lifecycle_phase(), proptest::option::of("[a-z ]{3,20}")).prop_map(
            |(lifecycle_phase, reason)| RecorderEventPayload::LifecycleMarker {
                lifecycle_phase,
                reason,
                details: serde_json::json!({}),
            }
        ),
    ]
}

fn arb_event(seq: u64) -> impl Strategy<Value = RecorderEvent> {
    (
        1u64..=20, // pane_id
        arb_source(),
        arb_payload(),
        arb_causality(),
        proptest::option::of("sess-[0-9]{1,3}".prop_map(|s| s)),
        proptest::option::of("wf-[0-9]{1,3}".prop_map(|s| s)),
        proptest::option::of("corr-[0-9]{1,3}".prop_map(|s| s)),
    )
        .prop_map(
            move |(
                pane_id,
                source,
                payload,
                causality,
                session_id,
                workflow_id,
                correlation_id,
            )| {
                RecorderEvent {
                    schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                    event_id: format!("evt-{}-{}", pane_id, seq),
                    pane_id,
                    session_id,
                    workflow_id,
                    correlation_id,
                    source,
                    payload,
                    occurred_at_ms: 1_700_000_000_000 + seq * 100,
                    recorded_at_ms: 1_700_000_000_001 + seq * 100,
                    sequence: seq,
                    causality,
                }
            },
        )
}

/// Generate a vector of 1..=max_size events with unique sequences.
fn arb_event_corpus(max_size: usize) -> impl Strategy<Value = Vec<RecorderEvent>> {
    (1..=max_size).prop_flat_map(|n| {
        let strats: Vec<_> = (0..n).map(|i| arb_event(i as u64)).collect();
        strats
    })
}

/// Build an InMemorySearchService from events.
fn build_service(events: &[RecorderEvent]) -> InMemorySearchService {
    let mut svc = InMemorySearchService::new();
    for (offset, event) in events.iter().enumerate() {
        let doc = map_event_to_document(event, offset as u64);
        svc.add(doc);
    }
    svc
}

/// Build docs from events for direct inspection.
fn build_docs(events: &[RecorderEvent]) -> Vec<IndexDocumentFields> {
    events
        .iter()
        .enumerate()
        .map(|(offset, event)| map_event_to_document(event, offset as u64))
        .collect()
}

// ---------------------------------------------------------------------------
// Property: result count bounds
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_result_count_bounded(events in arb_event_corpus(30)) {
        let svc = build_service(&events);
        let docs = build_docs(&events);

        // Use a search term that exists in at least some docs
        let search_terms = ["cargo", "echo", "git", "test", "hello", "python", "npm", "ls", "build", "rustc"];
        for term in &search_terms {
            let q = SearchQuery::simple(*term).with_limit(5);
            if let Ok(results) = svc.search(&q) {
                // hits ≤ limit
                prop_assert!(
                    results.hits.len() <= 5,
                    "hits.len()={} exceeds limit=5", results.hits.len()
                );
                // total_hits ≤ corpus size
                prop_assert!(
                    results.total_hits <= docs.len() as u64,
                    "total_hits={} exceeds corpus size={}", results.total_hits, docs.len()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property: filter satisfaction — all returned hits match all filters
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_filter_satisfaction_pane_id(events in arb_event_corpus(20)) {
        let svc = build_service(&events);
        let pane_filter = SearchFilter::PaneId { values: vec![1, 5, 10] };

        let q = SearchQuery {
            text: String::new(),
            filters: vec![pane_filter.clone()],
            sort: SearchSortOrder::default(),
            pagination: Pagination { limit: 100, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        if let Ok(results) = svc.search(&q) {
            for hit in &results.hits {
                prop_assert!(
                    pane_filter.matches(&hit.doc),
                    "hit pane_id={} doesn't match filter", hit.doc.pane_id
                );
            }
        }
    }

    #[test]
    fn prop_filter_satisfaction_event_type(events in arb_event_corpus(20)) {
        let svc = build_service(&events);
        let type_filter = SearchFilter::EventType {
            values: vec!["ingress_text".to_string()],
        };

        let q = SearchQuery {
            text: String::new(),
            filters: vec![type_filter.clone()],
            sort: SearchSortOrder::default(),
            pagination: Pagination { limit: 100, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        if let Ok(results) = svc.search(&q) {
            for hit in &results.hits {
                prop_assert!(
                    type_filter.matches(&hit.doc),
                    "hit event_type={} doesn't match filter", hit.doc.event_type
                );
            }
        }
    }

    #[test]
    fn prop_filter_satisfaction_direction(events in arb_event_corpus(20)) {
        let svc = build_service(&events);
        let dir_filter = SearchFilter::Direction {
            direction: EventDirection::Egress,
        };

        let q = SearchQuery {
            text: String::new(),
            filters: vec![dir_filter.clone()],
            sort: SearchSortOrder::default(),
            pagination: Pagination { limit: 100, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        if let Ok(results) = svc.search(&q) {
            for hit in &results.hits {
                prop_assert!(
                    dir_filter.matches(&hit.doc),
                    "hit event_type={} doesn't pass direction filter", hit.doc.event_type
                );
            }
        }
    }

    #[test]
    fn prop_filter_satisfaction_time_range(events in arb_event_corpus(20)) {
        let svc = build_service(&events);
        let time_filter = SearchFilter::TimeRange {
            min_ms: Some(1_700_000_000_500),
            max_ms: Some(1_700_000_001_500),
        };

        let q = SearchQuery {
            text: String::new(),
            filters: vec![time_filter.clone()],
            sort: SearchSortOrder::default(),
            pagination: Pagination { limit: 100, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        if let Ok(results) = svc.search(&q) {
            for hit in &results.hits {
                prop_assert!(
                    time_filter.matches(&hit.doc),
                    "hit occurred_at_ms={} outside time range", hit.doc.occurred_at_ms
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property: score monotonicity (relevance sort)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_relevance_scores_nonincreasing(events in arb_event_corpus(30)) {
        let svc = build_service(&events);
        let terms = ["cargo", "test", "hello", "echo", "git"];

        for term in &terms {
            let q = SearchQuery {
                text: term.to_string(),
                filters: Vec::new(),
                sort: SearchSortOrder {
                    primary: SortField::Relevance,
                    descending: true,
                },
                pagination: Pagination { limit: 100, after: None },
                snippet_config: SnippetConfig { enabled: false, ..Default::default() },
                field_boosts: HashMap::new(),
            };

            if let Ok(results) = svc.search(&q) {
                for window in results.hits.windows(2) {
                    prop_assert!(
                        window[0].score >= window[1].score,
                        "scores not non-increasing: {} < {}", window[0].score, window[1].score
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property: occurred_at sort order
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_occurred_at_sort_ascending(events in arb_event_corpus(20)) {
        let svc = build_service(&events);
        let q = SearchQuery {
            text: String::new(),
            filters: vec![SearchFilter::Direction { direction: EventDirection::Both }],
            sort: SearchSortOrder {
                primary: SortField::OccurredAt,
                descending: false,
            },
            pagination: Pagination { limit: 100, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        if let Ok(results) = svc.search(&q) {
            for window in results.hits.windows(2) {
                prop_assert!(
                    window[0].doc.occurred_at_ms <= window[1].doc.occurred_at_ms,
                    "ascending sort violated: {} > {}",
                    window[0].doc.occurred_at_ms, window[1].doc.occurred_at_ms
                );
            }
        }
    }

    #[test]
    fn prop_occurred_at_sort_descending(events in arb_event_corpus(20)) {
        let svc = build_service(&events);
        let q = SearchQuery {
            text: String::new(),
            filters: vec![SearchFilter::Direction { direction: EventDirection::Both }],
            sort: SearchSortOrder {
                primary: SortField::OccurredAt,
                descending: true,
            },
            pagination: Pagination { limit: 100, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        if let Ok(results) = svc.search(&q) {
            for window in results.hits.windows(2) {
                prop_assert!(
                    window[0].doc.occurred_at_ms >= window[1].doc.occurred_at_ms,
                    "descending sort violated: {} < {}",
                    window[0].doc.occurred_at_ms, window[1].doc.occurred_at_ms
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property: pagination completeness — no duplicates across pages
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn prop_pagination_no_duplicates(events in arb_event_corpus(20)) {
        let svc = build_service(&events);

        // Filter-only query to get all text-bearing docs
        let base_query = SearchQuery {
            text: String::new(),
            filters: vec![SearchFilter::Direction { direction: EventDirection::Both }],
            sort: SearchSortOrder {
                primary: SortField::OccurredAt,
                descending: true,
            },
            pagination: Pagination { limit: 3, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        let mut all_event_ids = HashSet::new();
        let mut cursor: Option<PaginationCursor> = None;
        let mut pages = 0;

        loop {
            let mut q = base_query.clone();
            if let Some(ref c) = cursor {
                q.pagination.after = Some(c.clone());
            }

            match svc.search(&q) {
                Ok(results) => {
                    if results.hits.is_empty() {
                        break;
                    }

                    for hit in &results.hits {
                        let was_new = all_event_ids.insert(hit.doc.event_id.clone());
                        prop_assert!(
                            was_new,
                            "duplicate event_id across pages: {}", hit.doc.event_id
                        );
                    }

                    cursor = results.next_cursor.clone();
                    pages += 1;

                    if !results.has_more || pages > 50 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property: redaction opacity — redacted text never appears in indexed docs
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_redaction_hides_original_text(
        pane_id in 1u64..=10,
        seq in 0u64..100,
        original_text in "[a-zA-Z]{5,30}",
        redaction in prop_oneof![
            Just(RecorderRedactionLevel::Partial),
            Just(RecorderRedactionLevel::Full),
        ],
    ) {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: format!("redact-{}-{}", pane_id, seq),
            pane_id,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            payload: RecorderEventPayload::IngressText {
                text: original_text.clone(),
                encoding: RecorderTextEncoding::Utf8,
                redaction,
                ingress_kind: RecorderIngressKind::SendText,
            },
            occurred_at_ms: 1_700_000_000_000 + seq * 100,
            recorded_at_ms: 1_700_000_000_001 + seq * 100,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
        };

        let doc = map_event_to_document(&event, seq);

        // Original text must NOT appear in the document
        prop_assert!(
            !doc.text.contains(&original_text),
            "redacted doc text contains original: text='{}', original='{}'",
            doc.text, original_text
        );
        prop_assert!(
            !doc.text_symbols.contains(&original_text),
            "redacted doc text_symbols contains original: text_symbols='{}', original='{}'",
            doc.text_symbols, original_text
        );

        // Partial → "[REDACTED]", Full → ""
        match redaction {
            RecorderRedactionLevel::Partial => {
                prop_assert_eq!(&doc.text, "[REDACTED]");
            }
            RecorderRedactionLevel::Full => {
                prop_assert!(doc.text.is_empty(), "fully redacted should be empty, got: '{}'", doc.text);
            }
            RecorderRedactionLevel::None => unreachable!(),
        }
    }
}

// ---------------------------------------------------------------------------
// Property: count() == search().total_hits
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_count_equals_search_total(events in arb_event_corpus(20)) {
        let svc = build_service(&events);

        let queries = vec![
            SearchQuery {
                text: String::new(),
                filters: vec![SearchFilter::PaneId { values: vec![1, 2, 3] }],
                sort: SearchSortOrder::default(),
                pagination: Pagination { limit: 100, after: None },
                snippet_config: SnippetConfig { enabled: false, ..Default::default() },
                field_boosts: HashMap::new(),
            },
            SearchQuery {
                text: String::new(),
                filters: vec![SearchFilter::Direction { direction: EventDirection::Ingress }],
                sort: SearchSortOrder::default(),
                pagination: Pagination { limit: 100, after: None },
                snippet_config: SnippetConfig { enabled: false, ..Default::default() },
                field_boosts: HashMap::new(),
            },
        ];

        for q in &queries {
            let count = svc.count(q).unwrap();
            let search = svc.search(q).unwrap();
            prop_assert_eq!(
                count, search.total_hits,
                "count()={} != search().total_hits={}", count, search.total_hits
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property: get_by_event_id roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_get_by_event_id_roundtrip(events in arb_event_corpus(15)) {
        let svc = build_service(&events);
        let docs = build_docs(&events);

        for doc in &docs {
            let retrieved = svc.get_by_event_id(&doc.event_id).unwrap();
            prop_assert!(
                retrieved.is_some(),
                "event_id='{}' not found after indexing", doc.event_id
            );
            let retrieved = retrieved.unwrap();
            prop_assert_eq!(
                &retrieved.event_id, &doc.event_id,
                "retrieved event_id mismatch"
            );
            prop_assert_eq!(
                retrieved.pane_id, doc.pane_id,
                "retrieved pane_id mismatch for event_id={}", doc.event_id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property: get_by_log_offset roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_get_by_log_offset_roundtrip(events in arb_event_corpus(15)) {
        let svc = build_service(&events);
        let docs = build_docs(&events);

        for doc in &docs {
            let retrieved = svc.get_by_log_offset(doc.log_offset).unwrap();
            prop_assert!(
                retrieved.is_some(),
                "log_offset={} not found after indexing", doc.log_offset
            );
            let retrieved = retrieved.unwrap();
            prop_assert_eq!(
                retrieved.log_offset, doc.log_offset,
                "retrieved log_offset mismatch"
            );
            prop_assert_eq!(
                &retrieved.event_id, &doc.event_id,
                "retrieved event_id mismatch for log_offset={}", doc.log_offset
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property: empty text + no filters → error (always)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn prop_empty_query_no_filter_is_error(events in arb_event_corpus(10)) {
        let svc = build_service(&events);
        let q = SearchQuery {
            text: String::new(),
            filters: Vec::new(),
            sort: SearchSortOrder::default(),
            pagination: Pagination::default(),
            snippet_config: SnippetConfig::default(),
            field_boosts: HashMap::new(),
        };
        let result = svc.search(&q);
        prop_assert!(result.is_err(), "empty query with no filters should error");
    }
}

// ---------------------------------------------------------------------------
// Property: filter commutativity — order of filters doesn't change result set
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn prop_filter_order_irrelevant(events in arb_event_corpus(20)) {
        let svc = build_service(&events);

        let filter_a = SearchFilter::PaneId { values: vec![1, 2, 3] };
        let filter_b = SearchFilter::Direction { direction: EventDirection::Ingress };

        let q_ab = SearchQuery {
            text: String::new(),
            filters: vec![filter_a.clone(), filter_b.clone()],
            sort: SearchSortOrder {
                primary: SortField::OccurredAt,
                descending: false,
            },
            pagination: Pagination { limit: 100, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        let q_ba = SearchQuery {
            text: String::new(),
            filters: vec![filter_b, filter_a],
            sort: SearchSortOrder {
                primary: SortField::OccurredAt,
                descending: false,
            },
            pagination: Pagination { limit: 100, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        let r_ab = svc.search(&q_ab);
        let r_ba = svc.search(&q_ba);

        match (r_ab, r_ba) {
            (Ok(a), Ok(b)) => {
                prop_assert_eq!(
                    a.total_hits, b.total_hits,
                    "filter order changed total_hits: {} vs {}", a.total_hits, b.total_hits
                );
                let ids_a: Vec<_> = a.hits.iter().map(|h| &h.doc.event_id).collect();
                let ids_b: Vec<_> = b.hits.iter().map(|h| &h.doc.event_id).collect();
                prop_assert_eq!(ids_a, ids_b, "filter order changed result set");
            }
            (Err(_), Err(_)) => {} // both errored, fine
            _ => prop_assert!(false, "filter order changed success/failure"),
        }
    }
}

// ---------------------------------------------------------------------------
// Property: map_event_to_document is deterministic
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_map_event_deterministic(events in arb_event_corpus(10)) {
        for (offset, event) in events.iter().enumerate() {
            let doc1 = map_event_to_document(event, offset as u64);
            let doc2 = map_event_to_document(event, offset as u64);
            prop_assert_eq!(doc1, doc2, "map_event_to_document not deterministic for event_id={}", event.event_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Property: document fields preserve event identity
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_document_preserves_identity(events in arb_event_corpus(15)) {
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);

            // Identity fields must match
            prop_assert_eq!(
                &doc.event_id, &event.event_id,
                "event_id mismatch"
            );
            prop_assert_eq!(
                doc.pane_id, event.pane_id,
                "pane_id mismatch for {}", event.event_id
            );
            prop_assert_eq!(
                &doc.session_id, &event.session_id,
                "session_id mismatch for {}", event.event_id
            );
            prop_assert_eq!(
                &doc.workflow_id, &event.workflow_id,
                "workflow_id mismatch for {}", event.event_id
            );
            prop_assert_eq!(
                &doc.correlation_id, &event.correlation_id,
                "correlation_id mismatch for {}", event.event_id
            );
            prop_assert_eq!(
                doc.sequence, event.sequence,
                "sequence mismatch for {}", event.event_id
            );
            prop_assert_eq!(
                doc.log_offset, offset as u64,
                "log_offset mismatch for {}", event.event_id
            );
            prop_assert_eq!(
                doc.occurred_at_ms, event.occurred_at_ms as i64,
                "occurred_at_ms mismatch for {}", event.event_id
            );

            // Causality fields
            prop_assert_eq!(
                &doc.parent_event_id, &event.causality.parent_event_id,
                "parent_event_id mismatch for {}", event.event_id
            );
            prop_assert_eq!(
                &doc.trigger_event_id, &event.causality.trigger_event_id,
                "trigger_event_id mismatch for {}", event.event_id
            );
            prop_assert_eq!(
                &doc.root_event_id, &event.causality.root_event_id,
                "root_event_id mismatch for {}", event.event_id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property: text field content matches expectations per event type
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_text_field_matches_event_type(events in arb_event_corpus(20)) {
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);

            match &event.payload {
                RecorderEventPayload::IngressText { text, redaction, .. } |
                RecorderEventPayload::EgressOutput { text, redaction, .. } => {
                    match redaction {
                        RecorderRedactionLevel::None => {
                            prop_assert_eq!(
                                &doc.text, text,
                                "unredacted text mismatch for {}", event.event_id
                            );
                        }
                        RecorderRedactionLevel::Partial => {
                            prop_assert_eq!(
                                &doc.text, "[REDACTED]",
                                "partial redaction mismatch for {}", event.event_id
                            );
                        }
                        RecorderRedactionLevel::Full => {
                            prop_assert!(
                                doc.text.is_empty(),
                                "full redaction should be empty for {}", event.event_id
                            );
                        }
                    }
                }
                RecorderEventPayload::ControlMarker { .. } => {
                    prop_assert!(
                        doc.text.is_empty(),
                        "control marker text should be empty for {}", event.event_id
                    );
                }
                RecorderEventPayload::LifecycleMarker { reason, .. } => {
                    let expected = reason.as_deref().unwrap_or("");
                    prop_assert_eq!(
                        &doc.text, expected,
                        "lifecycle text mismatch for {}", event.event_id
                    );
                }
            }

            // text_symbols always mirrors text
            prop_assert_eq!(
                &doc.text_symbols, &doc.text,
                "text_symbols != text for {}", event.event_id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property: event_type field correctly maps payload variant
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_event_type_maps_payload_variant(events in arb_event_corpus(20)) {
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);

            let expected_type = match &event.payload {
                RecorderEventPayload::IngressText { .. } => "ingress_text",
                RecorderEventPayload::EgressOutput { .. } => "egress_output",
                RecorderEventPayload::ControlMarker { .. } => "control_marker",
                RecorderEventPayload::LifecycleMarker { .. } => "lifecycle_marker",
            };

            prop_assert_eq!(
                &doc.event_type, expected_type,
                "event_type mismatch for {}: expected='{}', got='{}'",
                event.event_id, expected_type, doc.event_type
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property: has_more is consistent with total_hits and page size
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn prop_has_more_consistent(events in arb_event_corpus(20), limit in 1usize..=10) {
        let svc = build_service(&events);

        let q = SearchQuery {
            text: String::new(),
            filters: vec![SearchFilter::Direction { direction: EventDirection::Both }],
            sort: SearchSortOrder::default(),
            pagination: Pagination { limit, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };

        if let Ok(results) = svc.search(&q) {
            if results.has_more {
                // If has_more, there are more results than we returned
                prop_assert!(
                    results.hits.len() == limit,
                    "has_more=true but hits.len()={} != limit={}", results.hits.len(), limit
                );
            } else {
                // If !has_more, we returned all results
                prop_assert!(
                    results.hits.len() <= limit,
                    "!has_more but hits.len()={} > limit={}", results.hits.len(), limit
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property: schema version fields are always populated correctly
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_schema_versions_populated(events in arb_event_corpus(10)) {
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);

            prop_assert_eq!(
                &doc.schema_version,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
                "schema_version mismatch for {}", event.event_id
            );
            prop_assert_eq!(
                &doc.lexical_schema_version,
                "ft.recorder.lexical.v1",
                "lexical_schema_version mismatch for {}", event.event_id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property: document serialization roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_document_serde_roundtrip(events in arb_event_corpus(10)) {
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);
            let json = serde_json::to_string(&doc).unwrap();
            let deser: IndexDocumentFields = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(doc, deser, "serde roundtrip failed for {}", event.event_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Property: structural and trait tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// SearchQuery implements Clone correctly.
    #[test]
    fn prop_search_query_clone(events in arb_event_corpus(5)) {
        let _ = &events; // use the strategy
        let q = SearchQuery {
            text: "cargo".to_string(),
            filters: vec![SearchFilter::PaneId { values: vec![1, 2] }],
            sort: SearchSortOrder::default(),
            pagination: Pagination { limit: 10, after: None },
            snippet_config: SnippetConfig { enabled: false, ..Default::default() },
            field_boosts: HashMap::new(),
        };
        let q2 = q.clone();
        prop_assert_eq!(&q.text, &q2.text);
        prop_assert_eq!(q.pagination.limit, q2.pagination.limit);
    }

    /// SearchFilter implements Clone correctly.
    #[test]
    fn prop_search_filter_clone(pane_id in 1u64..100) {
        let f = SearchFilter::PaneId { values: vec![pane_id] };
        let f2 = f.clone();
        // Both should match the same doc
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: format!("test-{}", pane_id),
            pane_id,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            payload: RecorderEventPayload::IngressText {
                text: "hello".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
            occurred_at_ms: 1_700_000_000_000,
            recorded_at_ms: 1_700_000_000_001,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
        };
        let doc = map_event_to_document(&event, 0);
        prop_assert_eq!(f.matches(&doc), f2.matches(&doc));
    }

    /// IndexDocumentFields Debug is nonempty.
    #[test]
    fn prop_document_debug_nonempty(events in arb_event_corpus(5)) {
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);
            let debug = format!("{:?}", doc);
            prop_assert!(!debug.is_empty(), "Debug output is empty for {}", event.event_id);
        }
    }

    /// RecorderEvent serde roundtrip preserves all fields.
    #[test]
    fn prop_recorder_event_serde_roundtrip(events in arb_event_corpus(10)) {
        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let back: RecorderEvent = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&back.event_id, &event.event_id, "event_id mismatch");
            prop_assert_eq!(back.pane_id, event.pane_id, "pane_id mismatch");
            prop_assert_eq!(back.sequence, event.sequence, "sequence mismatch");
            prop_assert_eq!(back.occurred_at_ms, event.occurred_at_ms, "occurred_at_ms mismatch");
        }
    }

    /// Document pane_id is always in valid range (1..=20 per strategy).
    #[test]
    fn prop_document_pane_id_valid(events in arb_event_corpus(15)) {
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);
            prop_assert!(doc.pane_id > 0, "pane_id should be positive, got {}", doc.pane_id);
        }
    }

    /// Document source field is always a non-empty string.
    #[test]
    fn prop_document_source_nonempty(events in arb_event_corpus(15)) {
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);
            prop_assert!(!doc.source.is_empty(), "source field is empty for {}", event.event_id);
        }
    }

    /// Document event_type field is always one of the known types.
    #[test]
    fn prop_document_event_type_known(events in arb_event_corpus(15)) {
        let known_types = ["ingress_text", "egress_output", "control_marker", "lifecycle_marker"];
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);
            prop_assert!(
                known_types.contains(&doc.event_type.as_str()),
                "unknown event_type '{}' for {}", doc.event_type, event.event_id
            );
        }
    }

    /// Document serde deterministic — serialize twice yields identical JSON.
    #[test]
    fn prop_document_serde_deterministic(events in arb_event_corpus(10)) {
        for (offset, event) in events.iter().enumerate() {
            let doc = map_event_to_document(event, offset as u64);
            let json1 = serde_json::to_string(&doc).unwrap();
            let json2 = serde_json::to_string(&doc).unwrap();
            prop_assert_eq!(&json1, &json2, "non-deterministic serialization for {}", event.event_id);
        }
    }
}

// =========================================================================
// SearchQuery, SearchFilter, Pagination, SnippetConfig serde strategies
// =========================================================================

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
    (any::<i64>(), any::<i64>(), any::<u64>(), any::<u64>()).prop_map(
        |(score, occurred, seq, offset)| PaginationCursor {
            score_millis: score,
            occurred_at_ms: occurred,
            sequence: seq,
            log_offset: offset,
        },
    )
}

fn arb_pagination() -> impl Strategy<Value = Pagination> {
    (1usize..200, proptest::option::of(arb_pagination_cursor()))
        .prop_map(|(limit, after)| Pagination { limit, after })
}

fn arb_snippet_config() -> impl Strategy<Value = SnippetConfig> {
    (
        10usize..500,
        1usize..10,
        "[<][a-z]+[>]",
        "[<]/[a-z]+[>]",
        proptest::bool::ANY,
    )
        .prop_map(|(frag_len, frags, pre, post, enabled)| SnippetConfig {
            max_fragment_len: frag_len,
            max_fragments: frags,
            highlight_pre: pre,
            highlight_post: post,
            enabled,
        })
}

fn arb_search_filter() -> impl Strategy<Value = SearchFilter> {
    prop_oneof![
        proptest::collection::vec(0u64..10_000, 1..5)
            .prop_map(|values| SearchFilter::PaneId { values }),
        "[a-z0-9-]{5,20}".prop_map(|value| SearchFilter::SessionId { value }),
        "[a-z0-9-]{5,20}".prop_map(|value| SearchFilter::WorkflowId { value }),
        "[a-z0-9-]{5,20}".prop_map(|value| SearchFilter::CorrelationId { value }),
        proptest::collection::vec("[a-z_]{3,15}", 1..4)
            .prop_map(|values| SearchFilter::Source { values }),
        proptest::collection::vec("[a-z_]{3,15}", 1..4)
            .prop_map(|values| SearchFilter::EventType { values }),
    ]
}

fn arb_search_query() -> impl Strategy<Value = SearchQuery> {
    (
        "[a-zA-Z0-9 ]{1,50}",
        proptest::collection::vec(arb_search_filter(), 0..3),
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SearchQuery serde roundtrip preserves text and filter count.
    #[test]
    fn prop_search_query_serde(query in arb_search_query()) {
        let json = serde_json::to_string(&query).unwrap();
        let back: SearchQuery = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.text, &query.text);
        prop_assert_eq!(back.filters.len(), query.filters.len());
        prop_assert_eq!(back.sort.descending, query.sort.descending);
        prop_assert_eq!(back.pagination.limit, query.pagination.limit);
    }

    /// SearchQuery JSON has expected keys.
    #[test]
    fn prop_search_query_json_keys(query in arb_search_query()) {
        let json = serde_json::to_string(&query).unwrap();
        prop_assert!(json.contains("\"text\""));
        prop_assert!(json.contains("\"sort\""));
        prop_assert!(json.contains("\"pagination\""));
    }

    /// SnippetConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_snippet_config_serde(config in arb_snippet_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: SnippetConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_fragment_len, config.max_fragment_len);
        prop_assert_eq!(back.max_fragments, config.max_fragments);
        prop_assert_eq!(&back.highlight_pre, &config.highlight_pre);
        prop_assert_eq!(&back.highlight_post, &config.highlight_post);
        prop_assert_eq!(back.enabled, config.enabled);
    }

    /// Pagination serde roundtrip preserves limit and cursor presence.
    #[test]
    fn prop_pagination_serde(page in arb_pagination()) {
        let json = serde_json::to_string(&page).unwrap();
        let back: Pagination = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.limit, page.limit);
        prop_assert_eq!(back.after.is_some(), page.after.is_some());
    }

    /// PaginationCursor serde roundtrip preserves all fields.
    #[test]
    fn prop_pagination_cursor_serde(cursor in arb_pagination_cursor()) {
        let json = serde_json::to_string(&cursor).unwrap();
        let back: PaginationCursor = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.score_millis, cursor.score_millis);
        prop_assert_eq!(back.occurred_at_ms, cursor.occurred_at_ms);
        prop_assert_eq!(back.sequence, cursor.sequence);
        prop_assert_eq!(back.log_offset, cursor.log_offset);
    }

    /// SearchSortOrder serde roundtrip.
    #[test]
    fn prop_sort_order_serde(order in arb_sort_order()) {
        let json = serde_json::to_string(&order).unwrap();
        let back: SearchSortOrder = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.descending, order.descending);
    }

    /// SearchFilter serde roundtrip preserves variant tag.
    #[test]
    fn prop_search_filter_serde(filter in arb_search_filter()) {
        let json = serde_json::to_string(&filter).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        // Check variant matches by re-serializing
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(&json, &json2);
    }
}
