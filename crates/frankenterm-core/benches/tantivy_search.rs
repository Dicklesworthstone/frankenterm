//! Criterion benchmarks for the tantivy search pipeline.
//!
//! Bead: wa-xx5r
//!
//! Measures:
//! - Document mapping throughput (RecorderEvent → IndexDocumentFields)
//! - InMemorySearchService search latency vs corpus size
//! - Filter evaluation overhead
//! - Pagination cursor traversal cost
//! - Snippet extraction throughput

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use frankenterm_core::tantivy_ingest::{IndexDocumentFields, map_event_to_document};
use frankenterm_core::tantivy_query::{
    EventDirection, InMemorySearchService, LexicalSearchService, Pagination, PaginationCursor,
    SearchFilter, SearchQuery, SearchSortOrder, SnippetConfig, SortField, extract_snippets,
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

fn make_ingress(pane_id: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("evt-i-{}-{}", pane_id, seq),
        pane_id,
        session_id: Some(format!("sess-{}", pane_id)),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        payload: RecorderEventPayload::IngressText {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
        occurred_at_ms: 1_700_000_000_000 + seq * 100,
        recorded_at_ms: 1_700_000_000_001 + seq * 100,
        sequence: seq,
        causality: default_causality(),
    }
}

fn make_egress(pane_id: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("evt-e-{}-{}", pane_id, seq),
        pane_id,
        session_id: Some(format!("sess-{}", pane_id)),
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
        occurred_at_ms: 1_700_000_000_000 + seq * 100,
        recorded_at_ms: 1_700_000_000_001 + seq * 100,
        sequence: seq,
        causality: default_causality(),
    }
}

/// Sample terminal commands for realistic text content.
const TERMINAL_COMMANDS: &[&str] = &[
    "cargo test --release --all-targets",
    "git push origin main --force-with-lease",
    "npm run build && npm test",
    "python3 -m pytest tests/ -v --cov",
    "docker compose up -d --build",
    "kubectl get pods -n production",
    "rustc --edition 2024 src/main.rs",
    "ls -la /tmp/frankenterm/data",
    "echo 'hello world' > output.txt",
    "grep -rn 'TODO' src/",
];

const TERMINAL_OUTPUTS: &[&str] = &[
    "Compiling frankenterm v0.1.0 (/home/user/frankenterm)\n   Finished dev [unoptimized + debuginfo]",
    "error[E0308]: mismatched types\n  --> src/main.rs:42:5\n   |\n42 |     foo(bar)\n   |     ^^^^^^^ expected `u64`, found `i32`",
    "test result: ok. 42 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
    "NAME                READY   STATUS    RESTARTS   AGE\nfrankenterm-pod-1   1/1     Running   0          5d",
    "Already up to date.\nEverything up-to-date",
    "drwxr-xr-x  12 user staff  384 Feb 12 10:30 .\ndrwxr-xr-x   4 user staff  128 Feb 12 09:00 ..",
    "Successfully built 3a4b5c6d7e8f\nSuccessfully tagged frankenterm:latest",
    "src/lib.rs:100:// TODO: implement retry logic\nsrc/main.rs:50:// TODO: add error handling",
];

/// Build a corpus of N documents with realistic mixed events.
fn build_corpus(n: usize) -> Vec<IndexDocumentFields> {
    let mut docs = Vec::with_capacity(n);
    for i in 0..n {
        let pane_id = (i % 10) as u64 + 1;
        let seq = i as u64;
        let event = if i % 3 == 0 {
            make_ingress(pane_id, seq, TERMINAL_COMMANDS[i % TERMINAL_COMMANDS.len()])
        } else {
            make_egress(pane_id, seq, TERMINAL_OUTPUTS[i % TERMINAL_OUTPUTS.len()])
        };
        docs.push(map_event_to_document(&event, seq));
    }
    docs
}

/// Build an InMemorySearchService from a corpus.
fn build_service(docs: Vec<IndexDocumentFields>) -> InMemorySearchService {
    InMemorySearchService::from_docs(docs)
}

// ---------------------------------------------------------------------------
// Bench: map_event_to_document throughput
// ---------------------------------------------------------------------------

fn bench_map_event(c: &mut Criterion) {
    let mut group = c.benchmark_group("map_event_to_document");

    let ingress = make_ingress(1, 0, "cargo test --release --all-targets");
    let egress = make_egress(1, 1, "Compiling frankenterm v0.1.0\nFinished dev target(s)");
    let control = RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: "ctrl-1".to_string(),
        pane_id: 1,
        session_id: None,
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        payload: RecorderEventPayload::ControlMarker {
            control_marker_type: RecorderControlMarkerType::PromptBoundary,
            details: serde_json::json!({"cols": 80, "rows": 24}),
        },
        occurred_at_ms: 1_700_000_000_200,
        recorded_at_ms: 1_700_000_000_201,
        sequence: 2,
        causality: default_causality(),
    };

    group.bench_function("ingress", |b| {
        b.iter(|| map_event_to_document(&ingress, 0));
    });

    group.bench_function("egress", |b| {
        b.iter(|| map_event_to_document(&egress, 1));
    });

    group.bench_function("control", |b| {
        b.iter(|| map_event_to_document(&control, 2));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: search latency vs corpus size
// ---------------------------------------------------------------------------

fn bench_search_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_scaling");
    group.sample_size(30);

    for &corpus_size in &[100, 500, 1000, 5000] {
        let docs = build_corpus(corpus_size);
        let svc = build_service(docs);

        // Text query (requires scoring)
        group.bench_with_input(
            BenchmarkId::new("text_query", corpus_size),
            &corpus_size,
            |b, _| {
                let q = SearchQuery::simple("cargo test").with_limit(20);
                b.iter(|| svc.search(&q).unwrap());
            },
        );

        // Filter-only query (no scoring)
        group.bench_with_input(
            BenchmarkId::new("filter_only", corpus_size),
            &corpus_size,
            |b, _| {
                let q = SearchQuery {
                    text: String::new(),
                    filters: vec![SearchFilter::PaneId {
                        values: vec![1, 2, 3],
                    }],
                    sort: SearchSortOrder::default(),
                    pagination: Pagination {
                        limit: 20,
                        after: None,
                    },
                    snippet_config: SnippetConfig {
                        enabled: false,
                        ..Default::default()
                    },
                    field_boosts: std::collections::HashMap::new(),
                };
                b.iter(|| svc.search(&q).unwrap());
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: filter evaluation overhead
// ---------------------------------------------------------------------------

fn bench_filter_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_overhead");
    group.sample_size(30);

    let docs = build_corpus(1000);
    let svc = build_service(docs);

    // Baseline: no filters (text only)
    group.bench_function("no_filter", |b| {
        let q = SearchQuery::simple("cargo test").with_limit(20);
        b.iter(|| svc.search(&q).unwrap());
    });

    // Single filter: PaneId
    group.bench_function("pane_id_filter", |b| {
        let q = SearchQuery::simple("cargo test")
            .with_filter(SearchFilter::PaneId {
                values: vec![1, 2, 3],
            })
            .with_limit(20);
        b.iter(|| svc.search(&q).unwrap());
    });

    // Single filter: TimeRange
    group.bench_function("time_range_filter", |b| {
        let q = SearchQuery::simple("cargo test")
            .with_filter(SearchFilter::TimeRange {
                min_ms: Some(1_700_000_000_000),
                max_ms: Some(1_700_000_050_000),
            })
            .with_limit(20);
        b.iter(|| svc.search(&q).unwrap());
    });

    // Combined filters: PaneId + Direction + TimeRange
    group.bench_function("combined_3_filters", |b| {
        let q = SearchQuery::simple("cargo test")
            .with_filter(SearchFilter::PaneId {
                values: vec![1, 2, 3],
            })
            .with_filter(SearchFilter::Direction {
                direction: EventDirection::Ingress,
            })
            .with_filter(SearchFilter::TimeRange {
                min_ms: Some(1_700_000_000_000),
                max_ms: Some(1_700_000_050_000),
            })
            .with_limit(20);
        b.iter(|| svc.search(&q).unwrap());
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: sort order performance
// ---------------------------------------------------------------------------

fn bench_sort_orders(c: &mut Criterion) {
    let mut group = c.benchmark_group("sort_orders");
    group.sample_size(30);

    let docs = build_corpus(1000);
    let svc = build_service(docs);

    let sort_configs = [
        ("relevance_desc", SortField::Relevance, true),
        ("occurred_at_desc", SortField::OccurredAt, true),
        ("occurred_at_asc", SortField::OccurredAt, false),
        ("sequence_desc", SortField::Sequence, true),
        ("log_offset_asc", SortField::LogOffset, false),
    ];

    for (name, field, descending) in &sort_configs {
        group.bench_function(*name, |b| {
            let q = SearchQuery {
                text: "cargo test error".to_string(),
                filters: Vec::new(),
                sort: SearchSortOrder {
                    primary: *field,
                    descending: *descending,
                },
                pagination: Pagination {
                    limit: 50,
                    after: None,
                },
                snippet_config: SnippetConfig {
                    enabled: false,
                    ..Default::default()
                },
                field_boosts: std::collections::HashMap::new(),
            };
            b.iter(|| svc.search(&q).unwrap());
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: pagination cursor traversal
// ---------------------------------------------------------------------------

fn bench_pagination(c: &mut Criterion) {
    let mut group = c.benchmark_group("pagination");
    group.sample_size(20);

    let docs = build_corpus(2000);
    let svc = build_service(docs);

    // Measure: traversing all pages of size 20
    group.bench_function("full_traversal_page20", |b| {
        b.iter(|| {
            let mut cursor: Option<PaginationCursor> = None;
            let mut total_hits = 0u64;
            loop {
                let q = SearchQuery {
                    text: String::new(),
                    filters: vec![SearchFilter::Direction {
                        direction: EventDirection::Both,
                    }],
                    sort: SearchSortOrder {
                        primary: SortField::OccurredAt,
                        descending: true,
                    },
                    pagination: Pagination {
                        limit: 20,
                        after: cursor.clone(),
                    },
                    snippet_config: SnippetConfig {
                        enabled: false,
                        ..Default::default()
                    },
                    field_boosts: std::collections::HashMap::new(),
                };
                match svc.search(&q) {
                    Ok(results) => {
                        total_hits += results.hits.len() as u64;
                        if !results.has_more {
                            break;
                        }
                        cursor = results.next_cursor;
                    }
                    Err(_) => break,
                }
            }
            total_hits
        });
    });

    // Measure: first page only (baseline)
    group.bench_function("first_page_only", |b| {
        let q = SearchQuery {
            text: String::new(),
            filters: vec![SearchFilter::Direction {
                direction: EventDirection::Both,
            }],
            sort: SearchSortOrder {
                primary: SortField::OccurredAt,
                descending: true,
            },
            pagination: Pagination {
                limit: 20,
                after: None,
            },
            snippet_config: SnippetConfig {
                enabled: false,
                ..Default::default()
            },
            field_boosts: std::collections::HashMap::new(),
        };
        b.iter(|| svc.search(&q).unwrap());
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: snippet extraction
// ---------------------------------------------------------------------------

fn bench_snippets(c: &mut Criterion) {
    let mut group = c.benchmark_group("snippet_extraction");

    let short_text = "cargo test --release";
    let medium_text = "error[E0308]: mismatched types\n  --> src/main.rs:42:5\n   |\n42 |     foo(bar)\n   |     ^^^^^^^ expected `u64`, found `i32`\n\nnote: this expression has type `i32`";
    let long_text = medium_text.repeat(10);
    let terms = vec!["error".to_string(), "types".to_string(), "foo".to_string()];
    let config = SnippetConfig::default();

    group.bench_function("short_text", |b| {
        b.iter(|| extract_snippets(short_text, &terms, &config));
    });

    group.bench_function("medium_text", |b| {
        b.iter(|| extract_snippets(medium_text, &terms, &config));
    });

    group.bench_function("long_text", |b| {
        b.iter(|| extract_snippets(&long_text, &terms, &config));
    });

    group.bench_function("no_match", |b| {
        let no_match_terms = vec!["xyznonexistent".to_string()];
        b.iter(|| extract_snippets(medium_text, &no_match_terms, &config));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: count vs search
// ---------------------------------------------------------------------------

fn bench_count_vs_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("count_vs_search");
    group.sample_size(30);

    let docs = build_corpus(2000);
    let svc = build_service(docs);

    let q = SearchQuery::simple("cargo test error").with_limit(50);

    group.bench_function("search", |b| {
        b.iter(|| svc.search(&q).unwrap());
    });

    group.bench_function("count", |b| {
        b.iter(|| svc.count(&q).unwrap());
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench: get_by_event_id / get_by_log_offset lookup
// ---------------------------------------------------------------------------

fn bench_point_lookups(c: &mut Criterion) {
    let mut group = c.benchmark_group("point_lookups");

    let docs = build_corpus(5000);
    let svc = build_service(docs);

    // Lookup in the middle of the corpus
    group.bench_function("get_by_event_id", |b| {
        b.iter(|| svc.get_by_event_id("evt-i-3-2400").unwrap());
    });

    group.bench_function("get_by_log_offset", |b| {
        b.iter(|| svc.get_by_log_offset(2500).unwrap());
    });

    // Lookup that misses
    group.bench_function("get_by_event_id_miss", |b| {
        b.iter(|| svc.get_by_event_id("nonexistent").unwrap());
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion setup
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_map_event,
    bench_search_scaling,
    bench_filter_overhead,
    bench_sort_orders,
    bench_pagination,
    bench_snippets,
    bench_count_vs_search,
    bench_point_lookups,
);
criterion_main!(benches);
