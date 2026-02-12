//! Cross-module integration tests for the Tantivy pipeline.
//!
//! Bead: wa-x1vd
//!
//! Tests end-to-end flows across:
//! - `tantivy_ingest` (event → document mapping, append-log reader, incremental indexer)
//! - `tantivy_query` (search service, filters, pagination, snippets)
//! - `tantivy_quality` (golden query harness, relevance assertions, latency budgets)
//! - `tantivy_reindex` (full reindex, backfill, integrity verification)
//!
//! Each test exercises the interaction between at least two of these modules,
//! verifying that data flows correctly through the entire pipeline.

use std::collections::HashMap;
use std::path::Path;

use tempfile::tempdir;

use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest,
    DurabilityLevel, RecorderStorage,
};
use frankenterm_core::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
    RECORDER_EVENT_SCHEMA_VERSION_V1,
};
use frankenterm_core::tantivy_ingest::{
    compute_indexer_lag, IndexCommitStats, IndexDocumentFields,
    IndexWriteError, IndexWriter, IndexerConfig, IncrementalIndexer,
    LEXICAL_SCHEMA_VERSION,
};
use frankenterm_core::tantivy_quality::{
    build_forensic_corpus, forensic_golden_queries, agent_workflow_golden_queries,
    GoldenQuery, QualityHarness, QueryClass, RelevanceAssertion,
};
use frankenterm_core::tantivy_query::{
    EventDirection, InMemorySearchService, LexicalSearchService, SearchFilter, SearchQuery,
};
use frankenterm_core::tantivy_reindex::{
    BackfillConfig, BackfillRange, IntegrityCheckConfig, IntegrityChecker, IndexLookup,
    ReindexConfig, ReindexPipeline, ReindexableWriter,
};

// ===========================================================================
// Shared test infrastructure
// ===========================================================================

fn storage_config(path: &Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 4,
        max_batch_events: 256,
        max_batch_bytes: 1024 * 1024,
        max_idempotency_entries: 64,
    }
}

fn indexer_config(path: &Path, consumer_id: &str) -> IndexerConfig {
    IndexerConfig {
        data_path: path.join("events.log"),
        consumer_id: consumer_id.to_string(),
        batch_size: 50,
        dedup_on_replay: true,
        max_batches: 0,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    }
}

fn ingress_event(id: &str, pane: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: id.to_string(),
        pane_id: pane,
        session_id: Some("integration-sess".to_string()),
        workflow_id: None,
        correlation_id: Some("corr-integ".to_string()),
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: 1_700_000_000_000 + seq * 100,
        recorded_at_ms: 1_700_000_000_001 + seq * 100,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn egress_event(id: &str, pane: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: id.to_string(),
        pane_id: pane,
        session_id: Some("integration-sess".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: 1_700_000_000_000 + seq * 100,
        recorded_at_ms: 1_700_000_000_001 + seq * 100,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::EgressOutput {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        },
    }
}

async fn append_events(storage: &AppendLogRecorderStorage, events: Vec<RecorderEvent>) {
    for (i, chunk) in events.chunks(4).enumerate() {
        storage
            .append_batch(AppendRequest {
                batch_id: format!("batch-{i}"),
                events: chunk.to_vec(),
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1_700_000_000_000 + i as u64,
            })
            .await
            .unwrap();
    }
}

/// Mock IndexWriter that accumulates documents for use with InMemorySearchService.
struct CollectingWriter {
    docs: Vec<IndexDocumentFields>,
    deleted_ids: Vec<String>,
    commits: u64,
}

impl CollectingWriter {
    fn new() -> Self {
        Self {
            docs: Vec::new(),
            deleted_ids: Vec::new(),
            commits: 0,
        }
    }

    /// Convert accumulated docs into an InMemorySearchService.
    fn into_search_service(self) -> InMemorySearchService {
        InMemorySearchService::from_docs(self.docs)
    }
}

impl IndexWriter for CollectingWriter {
    fn add_document(&mut self, doc: &IndexDocumentFields) -> Result<(), IndexWriteError> {
        self.docs.push(doc.clone());
        Ok(())
    }

    fn commit(&mut self) -> Result<IndexCommitStats, IndexWriteError> {
        self.commits += 1;
        Ok(IndexCommitStats {
            docs_added: self.docs.len() as u64,
            docs_deleted: self.deleted_ids.len() as u64,
            segment_count: 1,
        })
    }

    fn delete_by_event_id(&mut self, event_id: &str) -> Result<(), IndexWriteError> {
        self.deleted_ids.push(event_id.to_string());
        Ok(())
    }
}

impl ReindexableWriter for CollectingWriter {
    fn clear_all(&mut self) -> Result<u64, IndexWriteError> {
        let count = self.docs.len() as u64;
        self.docs.clear();
        self.deleted_ids.clear();
        Ok(count)
    }
}

/// Mock IndexLookup built from collected docs.
struct DocLookup {
    index: HashMap<String, u64>,
    total: u64,
}

impl DocLookup {
    fn from_docs(docs: &[IndexDocumentFields]) -> Self {
        let mut index = HashMap::new();
        for doc in docs {
            index.insert(doc.event_id.clone(), doc.log_offset);
        }
        Self {
            total: docs.len() as u64,
            index,
        }
    }
}

impl IndexLookup for DocLookup {
    fn has_event_id(&self, event_id: &str) -> Result<bool, IndexWriteError> {
        Ok(self.index.contains_key(event_id))
    }

    fn get_log_offset(&self, event_id: &str) -> Result<Option<u64>, IndexWriteError> {
        Ok(self.index.get(event_id).copied())
    }

    fn count_total(&self) -> Result<u64, IndexWriteError> {
        Ok(self.total)
    }
}

// ===========================================================================
// Integration: Ingest → Query
// ===========================================================================

#[tokio::test]
async fn ingest_then_query_end_to_end() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Write mixed ingress/egress events
    let events = vec![
        ingress_event("cmd-1", 1, 0, "cargo test --release"),
        egress_event("out-1", 1, 1, "   Compiling frankenterm v0.1.0"),
        egress_event("out-2", 1, 2, "test result: ok. 47 passed; 0 failed"),
        ingress_event("cmd-2", 2, 3, "git push origin main"),
        egress_event("out-3", 2, 4, "error: failed to push some refs"),
    ];
    append_events(&storage, events).await;

    // Run incremental indexer
    let icfg = indexer_config(dir.path(), "integ-indexer");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    let result = indexer.run(&storage).await.unwrap();

    assert_eq!(result.events_indexed, 5);
    assert!(result.caught_up);

    // Convert indexed docs to search service
    let svc = indexer.into_writer().into_search_service();
    assert_eq!(svc.len(), 5);

    // Query: simple text search for "error"
    let results = svc.search(&SearchQuery::simple("error")).unwrap();
    assert!(results.total_hits >= 1);
    assert!(results.hits.iter().any(|h| h.doc.event_id == "out-3"));

    // Query: filter by pane
    let pane1_results = svc
        .search(
            &SearchQuery::simple("Compiling test cargo")
                .with_filter(SearchFilter::PaneId { values: vec![1] }),
        )
        .unwrap();
    assert!(pane1_results.hits.iter().all(|h| h.doc.pane_id == 1));

    // Query: direction filter (ingress only)
    let ingress_results = svc
        .search(
            &SearchQuery::simple("cargo git")
                .with_filter(SearchFilter::Direction {
                    direction: EventDirection::Ingress,
                }),
        )
        .unwrap();
    assert!(ingress_results
        .hits
        .iter()
        .all(|h| h.doc.event_type == "ingress_text"));

    // Verify get_by_event_id matches indexed doc
    let doc = svc.get_by_event_id("cmd-1").unwrap().unwrap();
    assert_eq!(doc.text, "cargo test --release");
    assert_eq!(doc.pane_id, 1);
    assert_eq!(doc.source, "robot_mode");
}

#[tokio::test]
async fn incremental_ingest_query_consistency() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Phase 1: initial events
    append_events(
        &storage,
        vec![
            ingress_event("e1", 1, 0, "first command"),
            egress_event("e2", 1, 1, "first output"),
        ],
    )
    .await;

    let icfg = indexer_config(dir.path(), "incr-integ");
    let mut indexer1 = IncrementalIndexer::new(icfg.clone(), CollectingWriter::new());
    let r1 = indexer1.run(&storage).await.unwrap();
    assert_eq!(r1.events_indexed, 2);
    assert!(r1.caught_up);

    let svc1 = indexer1.into_writer().into_search_service();
    assert_eq!(svc1.count(&SearchQuery::simple("first")).unwrap(), 2);

    // Phase 2: append more events
    storage
        .append_batch(AppendRequest {
            batch_id: "late-1".to_string(),
            events: vec![
                ingress_event("e3", 2, 2, "second command"),
                egress_event("e4", 2, 3, "second output with error"),
            ],
            required_durability: DurabilityLevel::Appended,
            producer_ts_ms: 999,
        })
        .await
        .unwrap();

    // Phase 2 indexer picks up only new events
    let mut indexer2 = IncrementalIndexer::new(icfg, CollectingWriter::new());
    let r2 = indexer2.run(&storage).await.unwrap();
    assert_eq!(r2.events_indexed, 2);
    assert!(r2.caught_up);

    // The second indexer's docs start at e3
    let svc2 = indexer2.into_writer().into_search_service();
    assert!(svc2.get_by_event_id("e3").unwrap().is_some());
    assert!(svc2.get_by_event_id("e1").unwrap().is_none()); // not in this batch
}

// ===========================================================================
// Integration: Ingest → Lag Monitor
// ===========================================================================

#[tokio::test]
async fn lag_monitor_reflects_indexing_progress() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events: Vec<_> = (0..10)
        .map(|i| ingress_event(&format!("ev{i}"), 1, i, &format!("text-{i}")))
        .collect();
    append_events(&storage, events).await;

    let consumer = "lag-integ";

    // Before indexing: full lag
    let lag0 = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag0.log_head_ordinal, Some(9));
    assert_eq!(lag0.indexer_ordinal, None);
    assert_eq!(lag0.records_behind, 10);
    assert!(!lag0.caught_up);

    // Index first 5
    let icfg = IndexerConfig {
        consumer_id: consumer.to_string(),
        batch_size: 5,
        max_batches: 1,
        ..indexer_config(dir.path(), consumer)
    };
    let mut indexer = IncrementalIndexer::new(icfg.clone(), CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let lag1 = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag1.indexer_ordinal, Some(4));
    assert_eq!(lag1.records_behind, 5);
    assert!(!lag1.caught_up);

    // Index remaining
    let icfg2 = IndexerConfig {
        max_batches: 0,
        ..icfg
    };
    let mut indexer2 = IncrementalIndexer::new(icfg2, CollectingWriter::new());
    indexer2.run(&storage).await.unwrap();

    let lag2 = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag2.records_behind, 0);
    assert!(lag2.caught_up);
}

// ===========================================================================
// Integration: Ingest → Reindex → Query
// ===========================================================================

#[tokio::test]
async fn reindex_produces_queryable_results() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events = vec![
        ingress_event("ri-1", 1, 0, "cargo build"),
        egress_event("ri-2", 1, 1, "Compiling project"),
        egress_event("ri-3", 1, 2, "error[E0308]: mismatched types"),
        ingress_event("ri-4", 2, 3, "npm install"),
        egress_event("ri-5", 2, 4, "added 42 packages"),
    ];
    append_events(&storage, events).await;

    // Full reindex
    let config = ReindexConfig {
        data_path: dir.path().join("events.log"),
        consumer_id: "reindex-query-integ".to_string(),
        batch_size: 10,
        dedup_on_replay: true,
        clear_before_start: true,
        max_batches: 0,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    };

    let mut pipeline = ReindexPipeline::new(CollectingWriter::new());
    let progress = pipeline.full_reindex(&storage, &config).await.unwrap();
    assert_eq!(progress.events_indexed, 5);
    assert!(progress.caught_up);

    // Build search service from reindexed docs
    let svc = pipeline.into_writer().into_search_service();

    // Verify all events are queryable
    assert_eq!(svc.len(), 5);

    let error_results = svc.search(&SearchQuery::simple("error")).unwrap();
    assert!(error_results.total_hits >= 1);
    assert!(error_results.hits.iter().any(|h| h.doc.event_id == "ri-3"));

    // Pane filter works on reindexed data
    let pane2 = svc
        .search(
            &SearchQuery::simple("npm install packages added")
                .with_filter(SearchFilter::PaneId { values: vec![2] }),
        )
        .unwrap();
    assert!(pane2.hits.iter().all(|h| h.doc.pane_id == 2));
}

#[tokio::test]
async fn backfill_range_produces_queryable_subset() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events: Vec<_> = (0..10)
        .map(|i| ingress_event(&format!("bf{i}"), 1, i, &format!("backfill text {i}")))
        .collect();
    append_events(&storage, events).await;

    // Backfill only ordinals 3-6
    let config = BackfillConfig {
        data_path: dir.path().join("events.log"),
        consumer_id: "backfill-query".to_string(),
        batch_size: 20,
        range: BackfillRange::OrdinalRange { start: 3, end: 6 },
        dedup_on_replay: false,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        max_batches: 0,
    };

    let mut pipeline = ReindexPipeline::new_for_backfill(CollectingWriter::new());
    let progress = pipeline.backfill(&storage, &config).await.unwrap();
    assert_eq!(progress.events_indexed, 4);

    let svc = InMemorySearchService::from_docs(pipeline.backfill_writer().docs.clone());

    // Only backfilled docs are searchable
    assert_eq!(svc.len(), 4);
    assert!(svc.get_by_event_id("bf3").unwrap().is_some());
    assert!(svc.get_by_event_id("bf6").unwrap().is_some());
    assert!(svc.get_by_event_id("bf0").unwrap().is_none()); // outside range
    assert!(svc.get_by_event_id("bf9").unwrap().is_none()); // outside range
}

// ===========================================================================
// Integration: Ingest → Reindex → Integrity Verification
// ===========================================================================

#[tokio::test]
async fn integrity_check_after_full_indexing() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events: Vec<_> = (0..15)
        .map(|i| ingress_event(&format!("ic{i}"), 1, i, &format!("integrity text {i}")))
        .collect();
    append_events(&storage, events).await;

    // Index everything via incremental indexer
    let icfg = indexer_config(dir.path(), "integrity-test");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    let result = indexer.run(&storage).await.unwrap();
    assert_eq!(result.events_indexed, 15);

    // Verify integrity
    let lookup = DocLookup::from_docs(&indexer.writer().docs);
    let check_config = IntegrityCheckConfig {
        data_path: dir.path().join("events.log"),
        ordinal_range: None,
        max_events: 0,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    };

    let report = IntegrityChecker::check(&lookup, &check_config).unwrap();
    assert!(report.is_consistent);
    assert_eq!(report.index_matches, 15);
    assert!(report.missing_from_index.is_empty());
    assert!(report.offset_mismatches.is_empty());
    assert_eq!(report.total_index_docs, Some(15));
}

#[tokio::test]
async fn integrity_detects_gap_after_partial_backfill() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events: Vec<_> = (0..10)
        .map(|i| ingress_event(&format!("gap{i}"), 1, i, &format!("gap text {i}")))
        .collect();
    append_events(&storage, events).await;

    // Only backfill ordinals 5-9
    let config = BackfillConfig {
        data_path: dir.path().join("events.log"),
        consumer_id: "gap-detect".to_string(),
        batch_size: 20,
        range: BackfillRange::OrdinalRange { start: 5, end: 9 },
        dedup_on_replay: false,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        max_batches: 0,
    };

    let mut pipeline = ReindexPipeline::new_for_backfill(CollectingWriter::new());
    pipeline.backfill(&storage, &config).await.unwrap();

    let lookup = DocLookup::from_docs(&pipeline.backfill_writer().docs);

    // Full check detects the gap
    let check_config = IntegrityCheckConfig {
        data_path: dir.path().join("events.log"),
        ordinal_range: None,
        max_events: 0,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    };

    let report = IntegrityChecker::check(&lookup, &check_config).unwrap();
    assert!(!report.is_consistent);
    assert_eq!(report.missing_from_index.len(), 5); // gap0-gap4
    assert_eq!(report.index_matches, 5);

    // Scoped check on the backfilled range passes
    let scoped_config = IntegrityCheckConfig {
        ordinal_range: Some((5, 9)),
        ..check_config
    };
    let scoped_report = IntegrityChecker::check(&lookup, &scoped_config).unwrap();
    assert!(scoped_report.is_consistent);
}

// ===========================================================================
// Integration: Ingest → Query → Quality Harness
// ===========================================================================

#[tokio::test]
async fn quality_harness_on_indexed_forensic_corpus() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let _storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Build the forensic corpus as events, write to log, index, then query
    let corpus_docs = build_forensic_corpus();

    // Convert corpus docs back to events to write through the full pipeline
    // Instead, we can directly use InMemorySearchService for this test
    // since build_forensic_corpus already produces IndexDocumentFields.
    let svc = InMemorySearchService::from_docs(corpus_docs.clone());

    // Run forensic golden queries
    let forensic_queries = forensic_golden_queries();
    let harness = QualityHarness::new(forensic_queries);
    let report = harness.run(&svc);

    assert!(
        report.all_passed,
        "forensic suite failed on indexed corpus: {:#?}",
        report
    );
    assert_eq!(report.errors, 0);

    // Run agent workflow queries
    let agent_queries = agent_workflow_golden_queries();
    let harness2 = QualityHarness::new(agent_queries);
    let report2 = harness2.run(&svc);

    assert!(
        report2.all_passed,
        "agent workflow suite failed: {:#?}",
        report2
    );
}

#[tokio::test]
async fn custom_golden_queries_on_ingested_data() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Write a specific scenario: agent runs tests, gets errors, fixes them
    let events = vec![
        ingress_event("q-cmd1", 5, 0, "cargo test"),
        egress_event("q-out1", 5, 1, "running 10 tests\ntest result: FAILED. 2 failed"),
        ingress_event("q-cmd2", 5, 2, "cargo clippy -- -D warnings"),
        egress_event("q-out2", 5, 3, "warning: unused import `std::io`"),
        ingress_event("q-cmd3", 5, 4, "cargo test"),
        egress_event("q-out3", 5, 5, "running 10 tests\ntest result: ok. 10 passed; 0 failed"),
    ];
    append_events(&storage, events).await;

    let icfg = indexer_config(dir.path(), "quality-integ");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let svc = indexer.into_writer().into_search_service();

    // Build custom golden queries for this scenario
    let queries = vec![
        GoldenQuery {
            name: "test_failure_search".to_string(),
            class: QueryClass::SimpleTerm,
            query: SearchQuery::simple("FAILED"),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::MustHit {
                    event_id: "q-out1".to_string(),
                },
                // q-out3 also matches ("0 failed") but q-out1 ranks higher
                // due to more "failed" occurrences
                RelevanceAssertion::RankedBefore {
                    higher: "q-out1".to_string(),
                    lower: "q-out3".to_string(),
                },
            ],
            description: "Failure output ranks above success output".to_string(),
        },
        GoldenQuery {
            name: "clippy_warning_search".to_string(),
            class: QueryClass::SimpleTerm,
            query: SearchQuery::simple("warning"),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::MustHit {
                    event_id: "q-out2".to_string(),
                },
            ],
            description: "Find clippy warnings".to_string(),
        },
        GoldenQuery {
            name: "cargo_commands_ingress_only".to_string(),
            class: QueryClass::Filtered,
            query: SearchQuery::simple("cargo")
                .with_filter(SearchFilter::Direction {
                    direction: EventDirection::Ingress,
                }),
            assertions: vec![
                RelevanceAssertion::ExactTotalHits(3), // cmd1, cmd2, cmd3
                RelevanceAssertion::AllMatchFilter(SearchFilter::EventType {
                    values: vec!["ingress_text".to_string()],
                }),
            ],
            description: "All cargo commands are ingress events".to_string(),
        },
        GoldenQuery {
            name: "pane_5_scoped".to_string(),
            class: QueryClass::Filtered,
            query: SearchQuery::simple("test cargo warning FAILED ok")
                .with_filter(SearchFilter::PaneId { values: vec![5] }),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::AllMatchFilter(SearchFilter::PaneId { values: vec![5] }),
            ],
            description: "All results scoped to pane 5".to_string(),
        },
    ];

    let harness = QualityHarness::new(queries);
    let report = harness.run(&svc);

    for r in &report.results {
        if !r.passed {
            for a in &r.assertion_results {
                if !a.passed {
                    panic!(
                        "Query '{}' assertion '{}' failed: {}",
                        r.name,
                        a.description,
                        a.message.as_deref().unwrap_or("no message")
                    );
                }
            }
        }
    }
    assert!(report.all_passed);
    assert_eq!(report.errors, 0);
}

// ===========================================================================
// Integration: Multi-pane pipeline
// ===========================================================================

#[tokio::test]
async fn multi_pane_ingest_query_filter() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Pane 1: build agent, Pane 2: test agent, Pane 3: deploy agent
    let events = vec![
        ingress_event("p1-cmd", 1, 0, "cargo build --release"),
        egress_event("p1-out", 1, 1, "Compiling project v0.1.0"),
        ingress_event("p2-cmd", 2, 2, "cargo test"),
        egress_event("p2-out", 2, 3, "test result: ok. 100 passed"),
        ingress_event("p3-cmd", 3, 4, "kubectl apply -f deploy.yaml"),
        egress_event("p3-out", 3, 5, "deployment.apps/my-app configured"),
    ];
    append_events(&storage, events).await;

    let icfg = indexer_config(dir.path(), "multi-pane");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    let result = indexer.run(&storage).await.unwrap();
    assert_eq!(result.events_indexed, 6);

    let svc = indexer.into_writer().into_search_service();

    // Filter by each pane
    for pane in 1..=3u64 {
        let q = SearchQuery {
            text: String::new(),
            filters: vec![SearchFilter::PaneId { values: vec![pane] }],
            ..SearchQuery::simple("")
        };
        let results = svc.search(&q).unwrap();
        assert_eq!(results.total_hits, 2, "pane {} should have 2 events", pane);
        assert!(results.hits.iter().all(|h| h.doc.pane_id == pane));
    }

    // Cross-pane search for "cargo"
    let cargo_q = SearchQuery::simple("cargo");
    let cargo_results = svc.search(&cargo_q).unwrap();
    assert_eq!(cargo_results.total_hits, 2); // p1-cmd and p2-cmd
    let panes: Vec<u64> = cargo_results.hits.iter().map(|h| h.doc.pane_id).collect();
    assert!(panes.contains(&1));
    assert!(panes.contains(&2));
    assert!(!panes.contains(&3));
}

// ===========================================================================
// Integration: Pagination across indexed data
// ===========================================================================

#[tokio::test]
async fn pagination_works_on_indexed_data() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Write 20 events all containing "alpha"
    let events: Vec<_> = (0..20)
        .map(|i| ingress_event(&format!("pg{i}"), 1, i, &format!("alpha command {i}")))
        .collect();
    append_events(&storage, events).await;

    let icfg = indexer_config(dir.path(), "pagination-integ");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let svc = indexer.into_writer().into_search_service();

    // Page 1: 5 results
    let q1 = SearchQuery::simple("alpha").with_limit(5);
    let r1 = svc.search(&q1).unwrap();
    assert_eq!(r1.hits.len(), 5);
    assert!(r1.has_more);
    assert!(r1.next_cursor.is_some());

    // Page 2: next 5
    let q2 = SearchQuery::simple("alpha")
        .with_limit(5)
        .with_cursor(r1.next_cursor.unwrap());
    let r2 = svc.search(&q2).unwrap();
    assert_eq!(r2.hits.len(), 5);

    // Verify no overlap between pages
    let ids1: Vec<&str> = r1.hits.iter().map(|h| h.doc.event_id.as_str()).collect();
    let ids2: Vec<&str> = r2.hits.iter().map(|h| h.doc.event_id.as_str()).collect();
    for id in &ids2 {
        assert!(!ids1.contains(id), "page overlap detected: {}", id);
    }
}

// ===========================================================================
// Integration: Snippet extraction on indexed data
// ===========================================================================

#[tokio::test]
async fn snippets_extracted_from_indexed_data() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events = vec![
        egress_event(
            "snip-1",
            1,
            0,
            "error[E0308]: mismatched types in src/main.rs at line 42",
        ),
        egress_event(
            "snip-2",
            1,
            1,
            "warning: unused variable `result` in function handle_request",
        ),
    ];
    append_events(&storage, events).await;

    let icfg = indexer_config(dir.path(), "snippet-integ");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let svc = indexer.into_writer().into_search_service();

    // Search for "error" and check snippets
    let results = svc.search(&SearchQuery::simple("error")).unwrap();
    assert!(results.total_hits >= 1);
    let hit = results.hits.iter().find(|h| h.doc.event_id == "snip-1").unwrap();
    assert!(!hit.snippets.is_empty());
    // Default snippet markers are « and »
    assert!(hit.snippets[0].fragment.contains("«"));
    assert!(hit.snippets[0].fragment.contains("»"));
}

// ===========================================================================
// Integration: Count consistency between search and count
// ===========================================================================

#[tokio::test]
async fn count_matches_search_total_hits() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events = vec![
        ingress_event("cnt-1", 1, 0, "alpha beta gamma"),
        ingress_event("cnt-2", 1, 1, "alpha delta"),
        egress_event("cnt-3", 1, 2, "alpha epsilon zeta"),
        egress_event("cnt-4", 2, 3, "beta gamma delta"),
        ingress_event("cnt-5", 2, 4, "alpha alpha alpha"), // triple alpha
    ];
    append_events(&storage, events).await;

    let icfg = indexer_config(dir.path(), "count-integ");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let svc = indexer.into_writer().into_search_service();

    // Verify count matches search total_hits for various queries
    let queries = vec![
        SearchQuery::simple("alpha"),
        SearchQuery::simple("beta"),
        SearchQuery::simple("gamma"),
        SearchQuery::simple("alpha")
            .with_filter(SearchFilter::PaneId { values: vec![1] }),
        SearchQuery::simple("alpha")
            .with_filter(SearchFilter::Direction {
                direction: EventDirection::Ingress,
            }),
    ];

    for q in queries {
        let count = svc.count(&q).unwrap();
        let results = svc.search(&q).unwrap();
        assert_eq!(
            count, results.total_hits,
            "count mismatch for query '{}'",
            q.text
        );
    }
}

// ===========================================================================
// Integration: Time range filter with indexed data
// ===========================================================================

#[tokio::test]
async fn time_range_filter_on_indexed_data() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events = vec![
        ingress_event("tr-1", 1, 0, "early event"),  // occurred_at_ms = 1_700_000_000_000
        ingress_event("tr-2", 1, 5, "mid event"),     // occurred_at_ms = 1_700_000_000_500
        ingress_event("tr-3", 1, 10, "late event"),   // occurred_at_ms = 1_700_000_001_000
    ];
    append_events(&storage, events).await;

    let icfg = indexer_config(dir.path(), "time-range-integ");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let svc = indexer.into_writer().into_search_service();

    // Query with time range filter
    let q = SearchQuery::simple("event")
        .with_filter(SearchFilter::TimeRange {
            min_ms: Some(1_700_000_000_100),
            max_ms: Some(1_700_000_000_900),
        });

    let results = svc.search(&q).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.event_id, "tr-2");
}

// ===========================================================================
// Integration: Document field fidelity through pipeline
// ===========================================================================

#[tokio::test]
async fn document_fields_preserved_through_pipeline() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let event = RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: "field-check".to_string(),
        pane_id: 42,
        session_id: Some("sess-x".to_string()),
        workflow_id: Some("wf-y".to_string()),
        correlation_id: Some("corr-z".to_string()),
        source: RecorderEventSource::WorkflowEngine,
        occurred_at_ms: 1_700_000_111_222,
        recorded_at_ms: 1_700_000_111_333,
        sequence: 77,
        causality: RecorderEventCausality {
            parent_event_id: Some("parent-p".to_string()),
            trigger_event_id: Some("trigger-t".to_string()),
            root_event_id: Some("root-r".to_string()),
        },
        payload: RecorderEventPayload::IngressText {
            text: "detailed field test".to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::WorkflowAction,
        },
    };

    append_events(&storage, vec![event]).await;

    let icfg = indexer_config(dir.path(), "field-fidelity");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let svc = indexer.into_writer().into_search_service();
    let doc = svc.get_by_event_id("field-check").unwrap().unwrap();

    // Verify all fields preserved through ingest → index → query
    assert_eq!(doc.event_id, "field-check");
    assert_eq!(doc.pane_id, 42);
    assert_eq!(doc.session_id, Some("sess-x".to_string()));
    assert_eq!(doc.workflow_id, Some("wf-y".to_string()));
    assert_eq!(doc.correlation_id, Some("corr-z".to_string()));
    assert_eq!(doc.source, "workflow_engine");
    assert_eq!(doc.event_type, "ingress_text");
    assert_eq!(doc.ingress_kind, Some("workflow_action".to_string()));
    assert_eq!(doc.parent_event_id, Some("parent-p".to_string()));
    assert_eq!(doc.trigger_event_id, Some("trigger-t".to_string()));
    assert_eq!(doc.root_event_id, Some("root-r".to_string()));
    assert_eq!(doc.text, "detailed field test");
    assert_eq!(doc.text_symbols, "detailed field test");
    assert_eq!(doc.occurred_at_ms, 1_700_000_111_222);
    assert_eq!(doc.recorded_at_ms, 1_700_000_111_333);
    assert_eq!(doc.sequence, 77);
    assert_eq!(doc.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
    assert_eq!(doc.lexical_schema_version, LEXICAL_SCHEMA_VERSION);
    assert!(!doc.is_gap);
}

// ===========================================================================
// Integration: Reindex → Query → Quality (full pipeline)
// ===========================================================================

#[tokio::test]
async fn full_pipeline_reindex_query_quality() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Write a realistic multi-pane session
    let events = vec![
        // Agent 1 building
        ingress_event("fp-build-cmd", 10, 0, "cargo build --release"),
        egress_event("fp-build-out", 10, 1, "   Compiling frankenterm v0.1.0"),
        egress_event("fp-build-err", 10, 2, "error[E0308]: mismatched types\n  --> src/lib.rs:42:5"),
        // Agent 2 testing
        ingress_event("fp-test-cmd", 20, 3, "cargo test"),
        egress_event("fp-test-out", 20, 4, "running 50 tests\ntest result: ok. 50 passed; 0 failed"),
        // Agent 1 retrying after fix
        ingress_event("fp-fix-cmd", 10, 5, "cargo build --release"),
        egress_event("fp-fix-out", 10, 6, "   Compiling frankenterm v0.1.0\n    Finished release"),
    ];
    append_events(&storage, events).await;

    // Reindex everything
    let config = ReindexConfig {
        data_path: dir.path().join("events.log"),
        consumer_id: "full-pipeline".to_string(),
        batch_size: 20,
        dedup_on_replay: true,
        clear_before_start: true,
        max_batches: 0,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    };

    let mut pipeline = ReindexPipeline::new(CollectingWriter::new());
    let progress = pipeline.full_reindex(&storage, &config).await.unwrap();
    assert_eq!(progress.events_indexed, 7);

    // Verify integrity
    let lookup = DocLookup::from_docs(&pipeline.writer().docs);
    let check_config = IntegrityCheckConfig {
        data_path: dir.path().join("events.log"),
        ordinal_range: None,
        max_events: 0,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    };
    let integrity = IntegrityChecker::check(&lookup, &check_config).unwrap();
    assert!(integrity.is_consistent);

    // Build search service
    let svc = pipeline.into_writer().into_search_service();

    // Run custom quality queries
    let queries = vec![
        GoldenQuery {
            name: "build_errors".to_string(),
            class: QueryClass::SimpleTerm,
            query: SearchQuery::simple("error"),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::MustHit {
                    event_id: "fp-build-err".to_string(),
                },
            ],
            description: "Find build errors".to_string(),
        },
        GoldenQuery {
            name: "agent1_scoped".to_string(),
            class: QueryClass::Filtered,
            query: SearchQuery::simple("cargo Compiling Finished error")
                .with_filter(SearchFilter::PaneId { values: vec![10] }),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::AllMatchFilter(SearchFilter::PaneId { values: vec![10] }),
            ],
            description: "Agent 1 activity scoped to pane 10".to_string(),
        },
        GoldenQuery {
            name: "test_results".to_string(),
            class: QueryClass::SimpleTerm,
            query: SearchQuery::simple("passed"),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::MustHit {
                    event_id: "fp-test-out".to_string(),
                },
            ],
            description: "Test results found".to_string(),
        },
    ];

    let harness = QualityHarness::new(queries);
    let report = harness.run(&svc);

    for r in &report.results {
        if !r.passed {
            for a in &r.assertion_results {
                if !a.passed {
                    panic!(
                        "Query '{}' failed: {} — {}",
                        r.name,
                        a.description,
                        a.message.as_deref().unwrap_or("no detail")
                    );
                }
            }
        }
    }
    assert!(report.all_passed);
}

// ===========================================================================
// Integration: Incremental index + backfill merge correctness
// ===========================================================================

#[tokio::test]
async fn incremental_index_plus_backfill_coverage() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events: Vec<_> = (0..12)
        .map(|i| ingress_event(&format!("m{i}"), 1, i, &format!("merge text {i}")))
        .collect();
    append_events(&storage, events).await;

    // Incremental indexer: index first 6
    let icfg = IndexerConfig {
        consumer_id: "merge-incr".to_string(),
        batch_size: 6,
        max_batches: 1,
        ..indexer_config(dir.path(), "merge-incr")
    };
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    let r1 = indexer.run(&storage).await.unwrap();
    assert_eq!(r1.events_indexed, 6);
    assert!(!r1.caught_up);

    let docs_phase1 = indexer.into_writer().docs;

    // Backfill the remaining range (6-11)
    let bf_config = BackfillConfig {
        data_path: dir.path().join("events.log"),
        consumer_id: "merge-backfill".to_string(),
        batch_size: 20,
        range: BackfillRange::OrdinalRange { start: 6, end: 11 },
        dedup_on_replay: false,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        max_batches: 0,
    };
    let mut bf_pipeline = ReindexPipeline::new_for_backfill(CollectingWriter::new());
    let r2 = bf_pipeline.backfill(&storage, &bf_config).await.unwrap();
    assert_eq!(r2.events_indexed, 6);

    // Combine both sets and verify full coverage
    let mut all_docs = docs_phase1;
    all_docs.extend(bf_pipeline.backfill_writer().docs.clone());
    assert_eq!(all_docs.len(), 12);

    let svc = InMemorySearchService::from_docs(all_docs.clone());

    // All 12 events searchable
    for i in 0..12 {
        let doc = svc.get_by_event_id(&format!("m{i}")).unwrap();
        assert!(doc.is_some(), "missing event m{i}");
    }

    // Integrity check on combined set
    let lookup = DocLookup::from_docs(&all_docs);
    let check = IntegrityCheckConfig {
        data_path: dir.path().join("events.log"),
        ordinal_range: None,
        max_events: 0,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    };
    let report = IntegrityChecker::check(&lookup, &check).unwrap();
    assert!(report.is_consistent);
    assert_eq!(report.index_matches, 12);
}

// ===========================================================================
// Integration: Redacted events through pipeline
// ===========================================================================

#[tokio::test]
async fn redacted_events_through_pipeline() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let mut partial_event = ingress_event("red-partial", 1, 0, "secret password");
    if let RecorderEventPayload::IngressText { ref mut redaction, .. } = partial_event.payload {
        *redaction = RecorderRedactionLevel::Partial;
    }

    let mut full_event = ingress_event("red-full", 1, 1, "top secret data");
    if let RecorderEventPayload::IngressText { ref mut redaction, .. } = full_event.payload {
        *redaction = RecorderRedactionLevel::Full;
    }

    let normal_event = ingress_event("red-none", 1, 2, "normal visible text");

    append_events(&storage, vec![partial_event, full_event, normal_event]).await;

    let icfg = indexer_config(dir.path(), "redacted-integ");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let svc = indexer.into_writer().into_search_service();

    // Partially redacted has [REDACTED] as text
    let partial_doc = svc.get_by_event_id("red-partial").unwrap().unwrap();
    assert_eq!(partial_doc.text, "[REDACTED]");
    assert_eq!(partial_doc.redaction, Some("partial".to_string()));

    // Fully redacted has empty text
    let full_doc = svc.get_by_event_id("red-full").unwrap().unwrap();
    assert_eq!(full_doc.text, "");
    assert_eq!(full_doc.redaction, Some("full".to_string()));

    // Normal text is searchable
    let results = svc.search(&SearchQuery::simple("normal visible")).unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].doc.event_id, "red-none");

    // Redacted text is not searchable by original content
    let secret_results = svc.search(&SearchQuery::simple("secret password")).unwrap();
    // Should NOT find the redacted events by their original text
    assert!(secret_results
        .hits
        .iter()
        .all(|h| h.doc.event_id != "red-partial"));
}

// ===========================================================================
// Integration: Source filter across pipeline
// ===========================================================================

#[tokio::test]
async fn source_filter_across_pipeline() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Events from different sources
    let robot_event = ingress_event("src-robot", 1, 0, "automated command");
    // Default source is RobotMode for ingress_event helper

    let mut operator_event = ingress_event("src-operator", 1, 1, "manual command");
    operator_event.source = RecorderEventSource::OperatorAction;

    let mux_event = egress_event("src-mux", 1, 2, "terminal output");
    // Default source is WeztermMux for egress_event helper

    append_events(&storage, vec![robot_event, operator_event, mux_event]).await;

    let icfg = indexer_config(dir.path(), "source-integ");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let svc = indexer.into_writer().into_search_service();

    // Filter by robot_mode source
    let robot_results = svc
        .search(
            &SearchQuery::simple("command output automated manual terminal")
                .with_filter(SearchFilter::Source {
                    values: vec!["robot_mode".to_string()],
                }),
        )
        .unwrap();
    assert_eq!(robot_results.total_hits, 1);
    assert_eq!(robot_results.hits[0].doc.event_id, "src-robot");

    // Filter by operator_action source
    let operator_results = svc
        .search(
            &SearchQuery::simple("command output automated manual terminal")
                .with_filter(SearchFilter::Source {
                    values: vec!["operator_action".to_string()],
                }),
        )
        .unwrap();
    assert_eq!(operator_results.total_hits, 1);
    assert_eq!(operator_results.hits[0].doc.event_id, "src-operator");
}

// ===========================================================================
// Integration: Sort order with indexed data
// ===========================================================================

#[tokio::test]
async fn sort_by_occurred_at_on_indexed_data() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events = vec![
        ingress_event("sort-1", 1, 0, "sort test alpha"),
        ingress_event("sort-2", 1, 5, "sort test alpha"),
        ingress_event("sort-3", 1, 10, "sort test alpha"),
    ];
    append_events(&storage, events).await;

    let icfg = indexer_config(dir.path(), "sort-integ");
    let mut indexer = IncrementalIndexer::new(icfg, CollectingWriter::new());
    indexer.run(&storage).await.unwrap();

    let svc = indexer.into_writer().into_search_service();

    // Sort ascending by occurred_at
    let q = SearchQuery {
        text: "sort test alpha".to_string(),
        sort: frankenterm_core::tantivy_query::SearchSortOrder {
            primary: frankenterm_core::tantivy_query::SortField::OccurredAt,
            descending: false,
        },
        ..SearchQuery::simple("")
    };

    let results = svc.search(&q).unwrap();
    assert_eq!(results.hits.len(), 3);
    assert_eq!(results.hits[0].doc.event_id, "sort-1");
    assert_eq!(results.hits[1].doc.event_id, "sort-2");
    assert_eq!(results.hits[2].doc.event_id, "sort-3");
}
