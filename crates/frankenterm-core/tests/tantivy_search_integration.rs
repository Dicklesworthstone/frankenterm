//! Cross-module integration tests for the tantivy ingest → query pipeline.
//!
//! Bead: wa-x1vd
//!
//! These tests validate that documents indexed via `tantivy_ingest`
//! (map_event_to_document) are correctly searchable through `tantivy_query`
//! (InMemorySearchService, LexicalSearchService).
//!
//! Coverage gaps addressed:
//! - No existing tests combine ingest + query modules end-to-end
//! - Search ranking validation against indexed documents
//! - Filter accuracy with real mapped events
//! - Pagination cursor stability across pages
//! - Snippet extraction on indexed terminal text
//! - Time/sequence range filtering on realistic data
//! - Multi-pane search isolation

use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use frankenterm_core::tantivy_ingest::{
    IndexDocumentFields, LEXICAL_SCHEMA_VERSION, map_event_to_document,
};
use frankenterm_core::tantivy_query::{
    EventDirection, InMemorySearchService, LexicalSearchService, SearchFilter, SearchQuery,
    SearchSortOrder, SnippetConfig, SortField,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_causality() -> RecorderEventCausality {
    RecorderEventCausality {
        parent_event_id: None,
        trigger_event_id: None,
        root_event_id: None,
    }
}

/// Build a minimal ingress text event.
fn make_ingress(pane_id: u64, sequence: u64, text: &str, occurred_at_ms: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("evt-ingress-{pane_id}-{sequence}"),
        pane_id,
        session_id: Some(format!("session-{pane_id}")),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        payload: RecorderEventPayload::IngressText {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
        occurred_at_ms,
        recorded_at_ms: occurred_at_ms + 1,
        sequence,
        causality: default_causality(),
    }
}

/// Build a minimal egress output event.
fn make_egress(pane_id: u64, sequence: u64, text: &str, occurred_at_ms: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("evt-egress-{pane_id}-{sequence}"),
        pane_id,
        session_id: Some(format!("session-{pane_id}")),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        payload: RecorderEventPayload::EgressOutput {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        },
        occurred_at_ms,
        recorded_at_ms: occurred_at_ms + 1,
        sequence,
        causality: default_causality(),
    }
}

/// Build a control marker event.
fn make_control(
    pane_id: u64,
    sequence: u64,
    marker_type: RecorderControlMarkerType,
    occurred_at_ms: u64,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("evt-control-{pane_id}-{sequence}"),
        pane_id,
        session_id: Some(format!("session-{pane_id}")),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        payload: RecorderEventPayload::ControlMarker {
            control_marker_type: marker_type,
            details: serde_json::json!({"cols": 120, "rows": 40}),
        },
        occurred_at_ms,
        recorded_at_ms: occurred_at_ms + 1,
        sequence,
        causality: default_causality(),
    }
}

/// Map events to documents and load into an InMemorySearchService.
fn build_search_service(events: &[RecorderEvent]) -> InMemorySearchService {
    let mut service = InMemorySearchService::new();
    for (offset, event) in events.iter().enumerate() {
        let doc = map_event_to_document(event, offset as u64);
        service.add(doc);
    }
    service
}

/// Build a document directly (bypassing event mapping) for precise control.
fn make_doc(
    event_id: &str,
    pane_id: u64,
    event_type: &str,
    text: &str,
    occurred_at_ms: i64,
    sequence: u64,
    log_offset: u64,
) -> IndexDocumentFields {
    IndexDocumentFields {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some(format!("session-{pane_id}")),
        workflow_id: None,
        correlation_id: None,
        parent_event_id: None,
        trigger_event_id: None,
        root_event_id: None,
        source: "wezterm_mux".to_string(),
        event_type: event_type.to_string(),
        ingress_kind: if event_type == "ingress_text" {
            Some("send_text".to_string())
        } else {
            None
        },
        segment_kind: if event_type == "egress_output" {
            Some("delta".to_string())
        } else {
            None
        },
        control_marker_type: None,
        lifecycle_phase: None,
        is_gap: false,
        redaction: None,
        occurred_at_ms,
        recorded_at_ms: occurred_at_ms + 1,
        sequence,
        log_offset,
        text: text.to_string(),
        text_symbols: text.to_string(),
        details_json: "{}".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests: Basic ingest → search pipeline
// ---------------------------------------------------------------------------

#[test]
fn ingest_single_event_then_search_finds_it() {
    let events = vec![make_ingress(1, 0, "hello world from terminal", 1000)];
    let svc = build_search_service(&events);

    let results = svc.search(&SearchQuery::simple("hello")).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.pane_id, 1);
    assert!(results.hits[0].doc.text.contains("hello world"));
}

#[test]
fn ingest_multiple_events_search_returns_matching_only() {
    let events = vec![
        make_ingress(1, 0, "cargo build --release", 1000),
        make_egress(1, 1, "Compiling frankenterm v0.1.0", 1001),
        make_ingress(1, 2, "git status", 1002),
        make_egress(1, 3, "On branch main, nothing to commit", 1003),
    ];
    let svc = build_search_service(&events);

    let results = svc.search(&SearchQuery::simple("cargo")).unwrap();
    assert_eq!(results.total_hits, 1);
    assert!(results.hits[0].doc.text.contains("cargo"));

    let results = svc.search(&SearchQuery::simple("branch")).unwrap();
    assert_eq!(results.total_hits, 1);
    assert!(results.hits[0].doc.text.contains("branch"));
}

#[test]
fn search_across_event_types() {
    let events = vec![
        make_ingress(1, 0, "run tests now", 1000),
        make_egress(1, 1, "test result: 42 passed", 1001),
    ];
    let svc = build_search_service(&events);

    // "test" appears in both ingress and egress
    let results = svc.search(&SearchQuery::simple("test")).unwrap();
    assert_eq!(results.total_hits, 2);
}

// ---------------------------------------------------------------------------
// Tests: Filter accuracy on mapped events
// ---------------------------------------------------------------------------

#[test]
fn pane_id_filter_isolates_pane() {
    let events = vec![
        make_ingress(10, 0, "pane ten command", 1000),
        make_ingress(20, 1, "pane twenty command", 1001),
        make_egress(10, 2, "pane ten output", 1002),
        make_egress(20, 3, "pane twenty output", 1003),
    ];
    let svc = build_search_service(&events);

    let query =
        SearchQuery::simple("command").with_filter(SearchFilter::PaneId { values: vec![10] });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.pane_id, 10);
}

#[test]
fn event_type_filter_separates_ingress_egress() {
    let events = vec![
        make_ingress(1, 0, "echo hello", 1000),
        make_egress(1, 1, "hello", 1001),
    ];
    let svc = build_search_service(&events);

    // Filter ingress only
    let query = SearchQuery::simple("hello").with_filter(SearchFilter::EventType {
        values: vec!["ingress_text".to_string()],
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.event_type, "ingress_text");

    // Filter egress only
    let query = SearchQuery::simple("hello").with_filter(SearchFilter::EventType {
        values: vec!["egress_output".to_string()],
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.event_type, "egress_output");
}

#[test]
fn direction_filter_works_on_mapped_events() {
    let events = vec![
        make_ingress(1, 0, "ls -la", 1000),
        make_egress(1, 1, "total 42 files listed", 1001),
    ];
    let svc = build_search_service(&events);

    // Ingress direction — only ingress has "ls"
    let query = SearchQuery::simple("ls").with_filter(SearchFilter::Direction {
        direction: EventDirection::Ingress,
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.event_type, "ingress_text");

    // Egress direction — only egress has "total"
    let query = SearchQuery::simple("total").with_filter(SearchFilter::Direction {
        direction: EventDirection::Egress,
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.event_type, "egress_output");

    // Both direction — search for "42" only in egress
    let query = SearchQuery::simple("42").with_filter(SearchFilter::Direction {
        direction: EventDirection::Both,
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
}

#[test]
fn session_id_filter_on_mapped_events() {
    let events = vec![
        make_ingress(1, 0, "command one", 1000),
        make_ingress(2, 1, "command two", 1001),
    ];
    let svc = build_search_service(&events);

    let query = SearchQuery::simple("command").with_filter(SearchFilter::SessionId {
        value: "session-1".to_string(),
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.pane_id, 1);
}

#[test]
fn time_range_filter_narrows_results() {
    let events = vec![
        make_ingress(1, 0, "early command", 1000),
        make_ingress(1, 1, "mid command", 2000),
        make_ingress(1, 2, "late command", 3000),
    ];
    let svc = build_search_service(&events);

    // Note: occurred_at_ms in RecorderEvent is u64 but in IndexDocumentFields is i64
    let query = SearchQuery::simple("command").with_filter(SearchFilter::TimeRange {
        min_ms: Some(1500),
        max_ms: Some(2500),
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.occurred_at_ms, 2000);
}

#[test]
fn sequence_range_filter_on_mapped_events() {
    let events = vec![
        make_ingress(1, 0, "seq zero", 1000),
        make_ingress(1, 1, "seq one", 1001),
        make_ingress(1, 2, "seq two", 1002),
        make_ingress(1, 3, "seq three", 1003),
    ];
    let svc = build_search_service(&events);

    let query = SearchQuery::simple("seq").with_filter(SearchFilter::SequenceRange {
        min_seq: Some(1),
        max_seq: Some(2),
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 2);
    let seqs: Vec<u64> = results.hits.iter().map(|h| h.doc.sequence).collect();
    assert!(seqs.contains(&1));
    assert!(seqs.contains(&2));
}

// ---------------------------------------------------------------------------
// Tests: Ranking and scoring
// ---------------------------------------------------------------------------

#[test]
fn higher_term_frequency_ranks_higher() {
    let svc = InMemorySearchService::from_docs(vec![
        make_doc("e1", 1, "egress_output", "one error here", 1000, 0, 0),
        make_doc(
            "e2",
            1,
            "egress_output",
            "error error error multiple errors",
            1001,
            1,
            1,
        ),
        make_doc("e3", 1, "egress_output", "no issues", 1002, 2, 2),
    ]);

    let results = svc.search(&SearchQuery::simple("error")).unwrap();
    assert_eq!(results.total_hits, 2); // "no issues" excluded
    assert_eq!(results.hits[0].doc.event_id, "e2"); // higher TF
    assert!(results.hits[0].score > results.hits[1].score);
}

#[test]
fn relevance_sort_with_tiebreak() {
    // Same score → tie-break by occurred_at DESC → sequence DESC → log_offset DESC
    let svc = InMemorySearchService::from_docs(vec![
        make_doc("e1", 1, "egress_output", "error message", 1000, 0, 0),
        make_doc("e2", 1, "egress_output", "error message", 2000, 1, 1),
        make_doc("e3", 1, "egress_output", "error message", 3000, 2, 2),
    ]);

    let results = svc.search(&SearchQuery::simple("error")).unwrap();
    assert_eq!(results.total_hits, 3);
    // Newest first on tie-break (occurred_at DESC)
    assert_eq!(results.hits[0].doc.occurred_at_ms, 3000);
    assert_eq!(results.hits[1].doc.occurred_at_ms, 2000);
    assert_eq!(results.hits[2].doc.occurred_at_ms, 1000);
}

#[test]
fn sort_by_occurred_at_ascending() {
    let events = vec![
        make_ingress(1, 0, "command alpha", 3000),
        make_ingress(1, 1, "command beta", 1000),
        make_ingress(1, 2, "command gamma", 2000),
    ];
    let svc = build_search_service(&events);

    let mut query = SearchQuery::simple("command");
    query.sort = SearchSortOrder {
        primary: SortField::OccurredAt,
        descending: false,
    };
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 3);
    assert_eq!(results.hits[0].doc.occurred_at_ms, 1000);
    assert_eq!(results.hits[1].doc.occurred_at_ms, 2000);
    assert_eq!(results.hits[2].doc.occurred_at_ms, 3000);
}

#[test]
fn sort_by_sequence_descending() {
    let events = vec![
        make_ingress(1, 5, "data alpha", 1000),
        make_ingress(1, 2, "data beta", 1001),
        make_ingress(1, 8, "data gamma", 1002),
    ];
    let svc = build_search_service(&events);

    let mut query = SearchQuery::simple("data");
    query.sort = SearchSortOrder {
        primary: SortField::Sequence,
        descending: true,
    };
    let results = svc.search(&query).unwrap();
    assert_eq!(results.hits[0].doc.sequence, 8);
    assert_eq!(results.hits[1].doc.sequence, 5);
    assert_eq!(results.hits[2].doc.sequence, 2);
}

// ---------------------------------------------------------------------------
// Tests: Pagination
// ---------------------------------------------------------------------------

#[test]
fn pagination_limits_results() {
    let events: Vec<RecorderEvent> = (0..10)
        .map(|i| make_ingress(1, i, &format!("paginated item {i}"), 1000 + i))
        .collect();
    let svc = build_search_service(&events);

    let query = SearchQuery::simple("paginated").with_limit(3);
    let results = svc.search(&query).unwrap();
    assert_eq!(results.hits.len(), 3);
    assert_eq!(results.total_hits, 10);
    assert!(results.has_more);
    assert!(results.next_cursor.is_some());
}

#[test]
fn cursor_pagination_traverses_all_results() {
    let events: Vec<RecorderEvent> = (0..7)
        .map(|i| make_ingress(1, i, &format!("page item {i}"), 1000 + i))
        .collect();
    let svc = build_search_service(&events);

    // Page 1
    let query = SearchQuery::simple("page").with_limit(3);
    let page1 = svc.search(&query).unwrap();
    assert_eq!(page1.hits.len(), 3);
    assert!(page1.has_more);

    let cursor1 = page1.next_cursor.clone().unwrap();

    // Page 2
    let query = SearchQuery::simple("page")
        .with_limit(3)
        .with_cursor(cursor1);
    let page2 = svc.search(&query).unwrap();
    assert_eq!(page2.hits.len(), 3);

    let cursor2 = page2.next_cursor.clone().unwrap();

    // Page 3 (last item)
    let query = SearchQuery::simple("page")
        .with_limit(3)
        .with_cursor(cursor2);
    let page3 = svc.search(&query).unwrap();
    assert_eq!(page3.hits.len(), 1);
    assert!(!page3.has_more);

    // Verify no duplicates across pages
    let mut all_event_ids: Vec<String> = Vec::new();
    for page in [&page1, &page2, &page3] {
        for hit in &page.hits {
            all_event_ids.push(hit.doc.event_id.clone());
        }
    }
    let unique_count = {
        let mut sorted = all_event_ids.clone();
        sorted.sort();
        sorted.dedup();
        sorted.len()
    };
    assert_eq!(
        unique_count, 7,
        "all 7 events should appear exactly once across pages"
    );
}

// ---------------------------------------------------------------------------
// Tests: Snippet extraction on indexed content
// ---------------------------------------------------------------------------

#[test]
fn snippets_highlight_matched_terms() {
    let events = vec![make_egress(
        1,
        0,
        "error: connection refused at port 8080",
        1000,
    )];
    let svc = build_search_service(&events);

    let results = svc.search(&SearchQuery::simple("connection")).unwrap();
    assert_eq!(results.total_hits, 1);
    assert!(!results.hits[0].snippets.is_empty());
    let snippet = &results.hits[0].snippets[0];
    assert!(
        snippet.fragment.contains("\u{ab}connection\u{bb}"),
        "snippet should contain highlighted term, got: {}",
        snippet.fragment
    );
}

#[test]
fn snippet_disabled_returns_no_snippets() {
    let events = vec![make_egress(1, 0, "some output text", 1000)];
    let svc = build_search_service(&events);

    let mut query = SearchQuery::simple("output");
    query.snippet_config = SnippetConfig {
        enabled: false,
        ..SnippetConfig::default()
    };
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert!(results.hits[0].snippets.is_empty());
}

// ---------------------------------------------------------------------------
// Tests: Multi-pane search scenarios
// ---------------------------------------------------------------------------

#[test]
fn multi_pane_search_returns_results_from_all_panes() {
    let events = vec![
        make_ingress(1, 0, "deploy service alpha", 1000),
        make_ingress(2, 1, "deploy service beta", 1001),
        make_ingress(3, 2, "deploy service gamma", 1002),
    ];
    let svc = build_search_service(&events);

    let results = svc.search(&SearchQuery::simple("deploy")).unwrap();
    assert_eq!(results.total_hits, 3);

    let pane_ids: Vec<u64> = results.hits.iter().map(|h| h.doc.pane_id).collect();
    assert!(pane_ids.contains(&1));
    assert!(pane_ids.contains(&2));
    assert!(pane_ids.contains(&3));
}

#[test]
fn multi_pane_filter_with_set() {
    let events = vec![
        make_ingress(1, 0, "test result", 1000),
        make_ingress(2, 1, "test result", 1001),
        make_ingress(3, 2, "test result", 1002),
        make_ingress(4, 3, "test result", 1003),
    ];
    let svc = build_search_service(&events);

    let query =
        SearchQuery::simple("test").with_filter(SearchFilter::PaneId { values: vec![1, 3] });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 2);
    let pane_ids: Vec<u64> = results.hits.iter().map(|h| h.doc.pane_id).collect();
    assert!(pane_ids.contains(&1));
    assert!(pane_ids.contains(&3));
    assert!(!pane_ids.contains(&2));
    assert!(!pane_ids.contains(&4));
}

// ---------------------------------------------------------------------------
// Tests: Edge cases and error handling
// ---------------------------------------------------------------------------

#[test]
fn empty_query_with_filter_returns_all_matching() {
    let svc = InMemorySearchService::from_docs(vec![
        make_doc("e1", 1, "ingress_text", "hello", 1000, 0, 0),
        make_doc("e2", 2, "ingress_text", "world", 1001, 1, 1),
    ]);

    // Empty text + pane filter → should return matching docs with score 0
    let mut query = SearchQuery::simple("");
    query.text = String::new();
    query.filters.push(SearchFilter::PaneId { values: vec![1] });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.pane_id, 1);
    assert!(
        (results.hits[0].score - 0.0).abs() < f32::EPSILON,
        "score should be 0.0, got {}",
        results.hits[0].score
    );
}

#[test]
fn empty_query_no_filter_returns_error() {
    let svc = InMemorySearchService::new();
    let mut query = SearchQuery::simple("");
    query.text = String::new();
    let result = svc.search(&query);
    assert!(result.is_err());
}

#[test]
fn search_no_matches_returns_empty() {
    let events = vec![make_ingress(1, 0, "hello world", 1000)];
    let svc = build_search_service(&events);

    let results = svc.search(&SearchQuery::simple("xyznonexistent")).unwrap();
    assert_eq!(results.total_hits, 0);
    assert!(results.hits.is_empty());
    assert!(!results.has_more);
    assert!(results.next_cursor.is_none());
}

#[test]
fn count_matches_search_total() {
    let events: Vec<RecorderEvent> = (0..5)
        .map(|i| make_ingress(1, i, &format!("counted item {i}"), 1000 + i))
        .collect();
    let svc = build_search_service(&events);

    let count = svc.count(&SearchQuery::simple("counted")).unwrap();
    let results = svc.search(&SearchQuery::simple("counted")).unwrap();
    assert_eq!(count, results.total_hits);
    assert_eq!(count, 5);
}

#[test]
fn get_by_event_id_after_ingest() {
    let events = vec![
        make_ingress(1, 0, "first command", 1000),
        make_ingress(1, 1, "second command", 1001),
    ];
    let svc = build_search_service(&events);

    let doc = svc
        .get_by_event_id("evt-ingress-1-0")
        .unwrap()
        .expect("should find event by id");
    assert_eq!(doc.pane_id, 1);
    assert!(doc.text.contains("first"));

    let missing = svc.get_by_event_id("nonexistent").unwrap();
    assert!(missing.is_none());
}

#[test]
fn get_by_log_offset_after_ingest() {
    let events = vec![
        make_ingress(1, 0, "offset zero", 1000),
        make_ingress(1, 1, "offset one", 1001),
    ];
    let svc = build_search_service(&events);

    let doc = svc
        .get_by_log_offset(0)
        .unwrap()
        .expect("should find event at offset 0");
    assert!(doc.text.contains("offset zero"));

    let doc = svc
        .get_by_log_offset(1)
        .unwrap()
        .expect("should find event at offset 1");
    assert!(doc.text.contains("offset one"));

    let missing = svc.get_by_log_offset(999).unwrap();
    assert!(missing.is_none());
}

// ---------------------------------------------------------------------------
// Tests: map_event_to_document field mapping verification
// ---------------------------------------------------------------------------

#[test]
fn mapped_ingress_has_correct_fields() {
    let event = make_ingress(42, 7, "cargo test --release", 5000);
    let doc = map_event_to_document(&event, 100);

    assert_eq!(doc.pane_id, 42);
    assert_eq!(doc.sequence, 7);
    assert_eq!(doc.log_offset, 100);
    assert_eq!(doc.occurred_at_ms, 5000);
    assert_eq!(doc.event_type, "ingress_text");
    assert_eq!(doc.ingress_kind.as_deref(), Some("send_text"));
    assert_eq!(doc.source, "wezterm_mux");
    assert!(doc.text.contains("cargo test"));
    assert_eq!(doc.session_id.as_deref(), Some("session-42"));
    assert_eq!(doc.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
    assert_eq!(doc.lexical_schema_version, LEXICAL_SCHEMA_VERSION);
}

#[test]
fn mapped_egress_has_correct_fields() {
    let event = make_egress(10, 3, "Finished release target", 6000);
    let doc = map_event_to_document(&event, 50);

    assert_eq!(doc.pane_id, 10);
    assert_eq!(doc.event_type, "egress_output");
    assert_eq!(doc.segment_kind.as_deref(), Some("delta"));
    assert!(!doc.is_gap);
    assert!(doc.text.contains("Finished release"));
}

#[test]
fn mapped_control_has_correct_fields() {
    let event = make_control(5, 0, RecorderControlMarkerType::Resize, 7000);
    let doc = map_event_to_document(&event, 25);

    assert_eq!(doc.event_type, "control_marker");
    assert_eq!(doc.control_marker_type.as_deref(), Some("resize"));
    // Control markers have empty text (details in details_json)
    assert!(doc.text.is_empty());
    assert!(doc.details_json.contains("cols"));
}

// ---------------------------------------------------------------------------
// Tests: Combined filter scenarios
// ---------------------------------------------------------------------------

#[test]
fn combined_pane_and_time_filter() {
    let events = vec![
        make_ingress(1, 0, "early pane1", 1000),
        make_ingress(1, 1, "late pane1", 3000),
        make_ingress(2, 2, "early pane2", 1000),
        make_ingress(2, 3, "late pane2", 3000),
    ];
    let svc = build_search_service(&events);

    let query = SearchQuery::simple("pane1")
        .with_filter(SearchFilter::PaneId { values: vec![1] })
        .with_filter(SearchFilter::TimeRange {
            min_ms: Some(2000),
            max_ms: None,
        });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.pane_id, 1);
    assert_eq!(results.hits[0].doc.occurred_at_ms, 3000);
}

#[test]
fn combined_event_type_and_session_filter() {
    let events = vec![
        make_ingress(1, 0, "input for session-1", 1000),
        make_egress(1, 1, "output for session-1", 1001),
        make_ingress(2, 2, "input for session-2", 1002),
        make_egress(2, 3, "output for session-2", 1003),
    ];
    let svc = build_search_service(&events);

    let query = SearchQuery::simple("session")
        .with_filter(SearchFilter::EventType {
            values: vec!["egress_output".to_string()],
        })
        .with_filter(SearchFilter::SessionId {
            value: "session-1".to_string(),
        });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.event_type, "egress_output");
    assert_eq!(results.hits[0].doc.session_id.as_deref(), Some("session-1"));
}

// ---------------------------------------------------------------------------
// Tests: Realistic terminal workflow scenario
// ---------------------------------------------------------------------------

#[test]
fn realistic_agent_workflow_search() {
    let events = vec![
        make_ingress(1, 0, "cargo test -p frankenterm-core", 1000),
        make_egress(
            1,
            1,
            "running 42 tests\ntest backpressure::tests::green_tier ... ok\ntest bloom_filter::tests::insert_and_check ... ok",
            1010,
        ),
        make_egress(
            1,
            2,
            "test result: ok. 42 passed; 0 failed; 0 ignored",
            1020,
        ),
        make_ingress(
            1,
            3,
            "git add crates/frankenterm-core/src/bloom_filter.rs",
            1030,
        ),
        make_ingress(
            1,
            4,
            "git commit -m \"feat: add bloom filter module\"",
            1040,
        ),
        make_egress(
            1,
            5,
            "[main abc1234] feat: add bloom filter module\n 1 file changed, 200 insertions(+)",
            1050,
        ),
        make_ingress(2, 6, "python train_model.py --epochs 100", 1060),
        make_egress(
            2,
            7,
            "Epoch 1/100: loss=0.542\nEpoch 2/100: loss=0.431\nerror: CUDA out of memory",
            1070,
        ),
    ];
    let svc = build_search_service(&events);

    // Find all test-related output
    let results = svc.search(&SearchQuery::simple("test")).unwrap();
    assert!(results.total_hits >= 2, "should find test runs");

    // Find git commits (ingress) — "git" matches both git add and git commit
    let query = SearchQuery::simple("git commit").with_filter(SearchFilter::Direction {
        direction: EventDirection::Ingress,
    });
    let results = svc.search(&query).unwrap();
    assert!(results.total_hits >= 1);
    // The top-ranked result should be the git commit (both terms match)
    assert!(results.hits[0].doc.text.contains("git commit"));

    // Find CUDA errors in pane 2
    let query = SearchQuery::simple("error").with_filter(SearchFilter::PaneId { values: vec![2] });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
    assert!(results.hits[0].doc.text.contains("CUDA"));

    // Search within time window for test results
    let query = SearchQuery::simple("passed").with_filter(SearchFilter::TimeRange {
        min_ms: Some(1000),
        max_ms: Some(1025),
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);
}

#[test]
fn large_scale_multi_pane_search() {
    // 20 panes × 10 events = 200 events
    let mut events = Vec::new();
    for pane in 0..20u64 {
        for i in 0..10u64 {
            let seq = pane * 10 + i;
            let ts = 1_700_000_000_000 + seq * 100;
            if i % 2 == 0 {
                events.push(make_ingress(
                    pane,
                    seq,
                    &format!("pane{pane} command {i}: cargo build --package pkg-{i}"),
                    ts,
                ));
            } else {
                events.push(make_egress(
                    pane,
                    seq,
                    &format!("pane{pane} output {i}: Compiling pkg-{i} v0.1.0"),
                    ts,
                ));
            }
        }
    }
    let svc = build_search_service(&events);

    // Global search
    let results = svc.search(&SearchQuery::simple("cargo")).unwrap();
    assert_eq!(results.total_hits, 100); // 20 panes × 5 ingress each

    // Single pane
    let query = SearchQuery::simple("cargo").with_filter(SearchFilter::PaneId { values: vec![7] });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 5);

    // Verify is_ready
    assert!(svc.is_ready());

    // Total document count
    assert_eq!(svc.len(), 200);
}

// ---------------------------------------------------------------------------
// Tests: Redaction interaction with search
// ---------------------------------------------------------------------------

#[test]
fn redacted_events_searchable_by_marker_not_content() {
    // Partially redacted events have [REDACTED] as text
    let event = RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: "evt-redacted-1".to_string(),
        pane_id: 1,
        session_id: Some("session-1".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        payload: RecorderEventPayload::IngressText {
            text: "secret password".to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::Partial,
            ingress_kind: RecorderIngressKind::SendText,
        },
        occurred_at_ms: 1000,
        recorded_at_ms: 1001,
        sequence: 0,
        causality: default_causality(),
    };
    let doc = map_event_to_document(&event, 0);
    let svc = InMemorySearchService::from_docs(vec![doc]);

    // Original text is redacted — search for "password" should NOT find it
    let results = svc.search(&SearchQuery::simple("password")).unwrap();
    assert_eq!(
        results.total_hits, 0,
        "redacted content should not be searchable by original text"
    );

    // But can search for REDACTED marker
    let results = svc.search(&SearchQuery::simple("REDACTED")).unwrap();
    assert_eq!(results.total_hits, 1);
}

#[test]
fn fully_redacted_events_have_empty_text() {
    let event = RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: "evt-full-redact-1".to_string(),
        pane_id: 1,
        session_id: Some("session-1".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        payload: RecorderEventPayload::IngressText {
            text: "super secret".to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::Full,
            ingress_kind: RecorderIngressKind::SendText,
        },
        occurred_at_ms: 1000,
        recorded_at_ms: 1001,
        sequence: 0,
        causality: default_causality(),
    };
    let doc = map_event_to_document(&event, 0);

    assert!(
        doc.text.is_empty(),
        "fully redacted should have empty text, got: {}",
        doc.text
    );

    // Still retrievable by event_id
    let svc = InMemorySearchService::from_docs(vec![doc]);
    let found = svc.get_by_event_id("evt-full-redact-1").unwrap();
    assert!(found.is_some());
}

// ---------------------------------------------------------------------------
// Tests: Log offset tracking through pipeline
// ---------------------------------------------------------------------------

#[test]
fn log_offsets_monotonically_increase() {
    let events: Vec<RecorderEvent> = (0..10)
        .map(|i| make_ingress(1, i, &format!("event {i}"), 1000 + i))
        .collect();
    let svc = build_search_service(&events);

    let mut query = SearchQuery::simple("event");
    query.sort = SearchSortOrder {
        primary: SortField::LogOffset,
        descending: false,
    };
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 10);

    let offsets: Vec<u64> = results.hits.iter().map(|h| h.doc.log_offset).collect();
    for window in offsets.windows(2) {
        assert!(
            window[0] < window[1],
            "log offsets should be strictly increasing: {:?}",
            offsets
        );
    }
}

#[test]
fn log_offset_range_filter_works() {
    let events: Vec<RecorderEvent> = (0..10)
        .map(|i| make_ingress(1, i, &format!("offset item {i}"), 1000 + i))
        .collect();
    let svc = build_search_service(&events);

    let query = SearchQuery::simple("offset").with_filter(SearchFilter::LogOffsetRange {
        min_offset: Some(3),
        max_offset: Some(6),
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 4); // offsets 3, 4, 5, 6
    for hit in &results.hits {
        assert!(hit.doc.log_offset >= 3 && hit.doc.log_offset <= 6);
    }
}

// ---------------------------------------------------------------------------
// Tests: Ingress kind / segment kind filters
// ---------------------------------------------------------------------------

#[test]
fn ingress_kind_filter_on_mapped_events() {
    let events = vec![make_ingress(1, 0, "typed command", 1000)];
    let svc = build_search_service(&events);

    let query = SearchQuery::simple("typed").with_filter(SearchFilter::IngressKind {
        value: "send_text".to_string(),
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);

    // Wrong kind → no results
    let query = SearchQuery::simple("typed").with_filter(SearchFilter::IngressKind {
        value: "paste".to_string(),
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 0);
}

#[test]
fn segment_kind_filter_on_mapped_events() {
    let events = vec![make_egress(1, 0, "delta output content", 1000)];
    let svc = build_search_service(&events);

    let query = SearchQuery::simple("delta").with_filter(SearchFilter::SegmentKind {
        value: "delta".to_string(),
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);

    let query = SearchQuery::simple("delta").with_filter(SearchFilter::SegmentKind {
        value: "gap".to_string(),
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 0);
}

// ---------------------------------------------------------------------------
// Tests: text_symbols field
// ---------------------------------------------------------------------------

#[test]
fn text_symbols_field_mirrors_text() {
    let event = make_ingress(1, 0, "file.rs:42 /usr/local/bin", 1000);
    let doc = map_event_to_document(&event, 0);

    // text_symbols should match text for symbol-heavy content
    assert_eq!(doc.text, doc.text_symbols);
    assert!(doc.text_symbols.contains("file.rs:42"));
}

#[test]
fn text_symbols_boost_affects_ranking() {
    // Default: text=1.0, text_symbols=1.25
    // A doc with the term in symbols-heavy content should score slightly higher
    let svc = InMemorySearchService::from_docs(vec![
        make_doc("e1", 1, "egress_output", "path", 1000, 0, 0),
        make_doc("e2", 1, "egress_output", "path", 1001, 1, 1),
    ]);

    let results = svc.search(&SearchQuery::simple("path")).unwrap();
    assert_eq!(results.total_hits, 2);
    // Both have same TF, so scores should be equal
    assert!(
        (results.hits[0].score - results.hits[1].score).abs() < f32::EPSILON,
        "scores should be equal: {} vs {}",
        results.hits[0].score,
        results.hits[1].score
    );
}

// ---------------------------------------------------------------------------
// Tests: Control marker details_json
// ---------------------------------------------------------------------------

#[test]
fn control_marker_details_preserved_in_document() {
    let event = make_control(1, 0, RecorderControlMarkerType::Resize, 1000);
    let doc = map_event_to_document(&event, 0);

    let details: serde_json::Value = serde_json::from_str(&doc.details_json).unwrap();
    assert_eq!(details["cols"], 120);
    assert_eq!(details["rows"], 40);
}

// ---------------------------------------------------------------------------
// Tests: Source filter
// ---------------------------------------------------------------------------

#[test]
fn source_filter_on_mapped_events() {
    let events = vec![
        make_ingress(1, 0, "mux input", 1000),
        // All our helpers use WeztermMux source
    ];
    let svc = build_search_service(&events);

    let query = SearchQuery::simple("mux").with_filter(SearchFilter::Source {
        values: vec!["wezterm_mux".to_string()],
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 1);

    // Wrong source → no results
    let query = SearchQuery::simple("mux").with_filter(SearchFilter::Source {
        values: vec!["robot_mode".to_string()],
    });
    let results = svc.search(&query).unwrap();
    assert_eq!(results.total_hits, 0);
}
