//! LabRuntime-ported recorder_lexical_ingest tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` functions from `recorder_lexical_ingest.rs` to
//! asupersync-based `RuntimeFixture`, gaining seed-based reproducibility for
//! the full Tantivy-backed lexical indexer pipeline and checkpoint resumption.
//!
//! Bead: ft-22x4r

#![cfg(all(feature = "asupersync-runtime", feature = "recorder-lexical"))]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::recorder_lexical_ingest::{LexicalIndexer, LexicalIndexerConfig};
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, DurabilityLevel,
    RecorderSourceDescriptor, RecorderStorage,
};
use frankenterm_core::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
    RECORDER_EVENT_SCHEMA_VERSION_V1,
};
use frankenterm_core::tantivy_ingest::{
    IncrementalIndexer, IndexerConfig, LEXICAL_INDEXER_CONSUMER,
};
use tempfile::tempdir;

// ===========================================================================
// Helpers (mirrors recorder_lexical_ingest.rs test helpers)
// ===========================================================================

fn sample_event(event_id: &str, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id: 1,
        session_id: Some("sess-1".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: 1_700_000_000_000,
        recorded_at_ms: 1_700_000_000_001,
        sequence: 0,
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

fn egress_event(event_id: &str, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id: 2,
        session_id: None,
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: 1_700_000_000_100,
        recorded_at_ms: 1_700_000_000_101,
        sequence: 1,
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

// ===========================================================================
// Integration with IncrementalIndexer
// ===========================================================================

#[test]
fn full_pipeline_with_tantivy_writer() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();

        // Set up append-log storage
        let storage_config = AppendLogStorageConfig {
            data_path: dir.path().join("events.log"),
            state_path: dir.path().join("state.json"),
            queue_capacity: 4,
            max_batch_events: 256,
            max_batch_bytes: 1024 * 1024,
            max_idempotency_entries: 64,
        };
        let storage = AppendLogRecorderStorage::open(storage_config.clone()).unwrap();

        // Populate with events
        let events = vec![
            sample_event("ev-1", "cargo build --release"),
            sample_event("ev-2", "cargo test"),
            egress_event("ev-3", "Compiling frankenterm v0.1.0"),
        ];
        storage
            .append_batch(AppendRequest {
                batch_id: "batch-1".to_string(),
                events,
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1_700_000_000_000,
            })
            .await
            .unwrap();

        // Create Tantivy index and writer
        let indexer_config = LexicalIndexerConfig {
            index_dir: dir.path().join("tantivy-idx"),
            writer_memory_bytes: 15_000_000,
        };
        let lexical_indexer = LexicalIndexer::open(indexer_config).unwrap();
        let writer = lexical_indexer
            .create_writer_with_memory(15_000_000)
            .unwrap();

        // Run incremental indexer
        let pipeline_config = IndexerConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: storage_config.data_path.clone(),
            },
            consumer_id: LEXICAL_INDEXER_CONSUMER.to_string(),
            batch_size: 10,
            dedup_on_replay: true,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let mut pipeline = IncrementalIndexer::new(pipeline_config, writer);
        let result = pipeline.run(&storage).await.unwrap();

        assert_eq!(result.events_read, 3);
        assert_eq!(result.events_indexed, 3);
        assert!(result.caught_up);

        // Verify docs are searchable
        assert_eq!(lexical_indexer.doc_count().unwrap(), 3);
    });
}

#[test]
fn incremental_pipeline_resumes_from_checkpoint() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();

        let storage_config = AppendLogStorageConfig {
            data_path: dir.path().join("events.log"),
            state_path: dir.path().join("state.json"),
            queue_capacity: 4,
            max_batch_events: 256,
            max_batch_bytes: 1024 * 1024,
            max_idempotency_entries: 64,
        };
        let storage = AppendLogRecorderStorage::open(storage_config.clone()).unwrap();

        // Append 4 events
        let events: Vec<_> = (0..4)
            .map(|i| sample_event(&format!("ev-{i}"), &format!("cmd-{i}")))
            .collect();
        storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events,
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1_700_000_000_000,
            })
            .await
            .unwrap();

        let indexer_config = LexicalIndexerConfig {
            index_dir: dir.path().join("tantivy-idx"),
            writer_memory_bytes: 15_000_000,
        };
        let lexical_indexer = LexicalIndexer::open(indexer_config).unwrap();

        // First run: index first 2 (batch_size=2, max_batches=1)
        let pipeline_config = IndexerConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: storage_config.data_path.clone(),
            },
            consumer_id: "resume-test".to_string(),
            batch_size: 2,
            dedup_on_replay: true,
            max_batches: 1,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let writer1 = lexical_indexer
            .create_writer_with_memory(15_000_000)
            .unwrap();
        let mut pipeline1 = IncrementalIndexer::new(pipeline_config.clone(), writer1);
        let r1 = pipeline1.run(&storage).await.unwrap();
        assert_eq!(r1.events_indexed, 2);
        assert!(!r1.caught_up);

        // Drop the first writer to release the index lock
        drop(pipeline1);

        // Second run: should resume at event 2 and index remaining 2
        let pipeline_config2 = IndexerConfig {
            max_batches: 0,
            ..pipeline_config
        };
        let writer2 = lexical_indexer
            .create_writer_with_memory(15_000_000)
            .unwrap();
        let mut pipeline2 = IncrementalIndexer::new(pipeline_config2, writer2);
        let r2 = pipeline2.run(&storage).await.unwrap();
        assert_eq!(r2.events_indexed, 2);
        assert!(r2.caught_up);

        // All 4 docs should be in the index
        assert_eq!(lexical_indexer.doc_count().unwrap(), 4);
    });
}
