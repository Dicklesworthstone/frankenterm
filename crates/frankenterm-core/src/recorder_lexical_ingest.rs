//! Concrete Tantivy-backed lexical indexer for recorder events.
//!
//! Bead: wa-oegrb.4.2
//!
//! This module provides:
//!
//! - [`TantivyIndexWriterAdapter`]: Implements the abstract [`IndexWriter`]
//!   trait from [`tantivy_ingest`](crate::tantivy_ingest) using a real
//!   Tantivy [`IndexWriter`](tantivy::IndexWriter).
//! - [`LexicalIndexer`]: Manages the full lifecycle of a Tantivy lexical
//!   index — creation, opening, schema fingerprint verification, tokenizer
//!   registration, and writer provisioning.
//!
//! Together with [`IncrementalIndexer`](crate::tantivy_ingest::IncrementalIndexer)
//! and [`AppendLogReader`](crate::tantivy_ingest::AppendLogReader), this
//! forms the complete checkpoint-driven ingestion pipeline.

use std::path::{Path, PathBuf};

use tantivy::schema::Term;
use tantivy::{Index, IndexWriter as TantivyWriter};

use crate::recorder_lexical_schema::{
    build_lexical_schema_v1, fields_to_document, register_tokenizers, schema_fingerprint,
    LexicalFieldHandles,
};
use crate::tantivy_ingest::{IndexCommitStats, IndexDocumentFields, IndexWriteError, IndexWriter};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default Tantivy writer heap size (50 MB).
const DEFAULT_WRITER_MEMORY_BYTES: usize = 50_000_000;

/// Filename for the persisted schema fingerprint alongside the index.
const FINGERPRINT_FILENAME: &str = ".ft_schema_fingerprint";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the concrete Tantivy-backed lexical indexer.
#[derive(Debug, Clone)]
pub struct LexicalIndexerConfig {
    /// Directory where the Tantivy index lives.
    pub index_dir: PathBuf,
    /// Tantivy writer heap budget in bytes.
    pub writer_memory_bytes: usize,
}

impl Default for LexicalIndexerConfig {
    fn default() -> Self {
        Self {
            index_dir: PathBuf::from(".ft/tantivy-lexical"),
            writer_memory_bytes: DEFAULT_WRITER_MEMORY_BYTES,
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error type for lexical ingest operations.
///
/// Kept separate from `crate::Error` because tantivy types are feature-gated.
#[derive(Debug)]
pub enum LexicalIngestError {
    /// Tantivy internal error.
    Tantivy(tantivy::TantivyError),
    /// I/O error (directory creation, fingerprint file, etc.).
    Io(std::io::Error),
    /// The on-disk index was created with a different schema version.
    SchemaFingerprintMismatch { expected: String, found: String },
}

impl std::fmt::Display for LexicalIngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tantivy(e) => write!(f, "tantivy error: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::SchemaFingerprintMismatch { expected, found } => {
                write!(
                    f,
                    "schema fingerprint mismatch: expected={expected}, found={found}"
                )
            }
        }
    }
}

impl std::error::Error for LexicalIngestError {}

impl From<tantivy::TantivyError> for LexicalIngestError {
    fn from(e: tantivy::TantivyError) -> Self {
        Self::Tantivy(e)
    }
}

impl From<std::io::Error> for LexicalIngestError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// TantivyIndexWriterAdapter — concrete IndexWriter implementation
// ---------------------------------------------------------------------------

/// Adapts a real Tantivy [`IndexWriter`](tantivy::IndexWriter) to the
/// abstract [`IndexWriter`] trait from `tantivy_ingest`.
pub struct TantivyIndexWriterAdapter {
    writer: TantivyWriter,
    handles: LexicalFieldHandles,
    docs_added: u64,
    docs_deleted: u64,
}

impl TantivyIndexWriterAdapter {
    /// Wrap a Tantivy writer with the given field handles.
    pub fn new(writer: TantivyWriter, handles: LexicalFieldHandles) -> Self {
        Self {
            writer,
            handles,
            docs_added: 0,
            docs_deleted: 0,
        }
    }
}

impl IndexWriter for TantivyIndexWriterAdapter {
    fn add_document(&mut self, doc: &IndexDocumentFields) -> Result<(), IndexWriteError> {
        let tantivy_doc = fields_to_document(doc, &self.handles);
        self.writer.add_document(tantivy_doc).map_err(|e| {
            IndexWriteError::Rejected {
                reason: e.to_string(),
            }
        })?;
        self.docs_added += 1;
        Ok(())
    }

    fn commit(&mut self) -> Result<IndexCommitStats, IndexWriteError> {
        self.writer.commit().map_err(|e| {
            IndexWriteError::CommitFailed {
                reason: e.to_string(),
            }
        })?;

        let stats = IndexCommitStats {
            docs_added: self.docs_added,
            docs_deleted: self.docs_deleted,
            segment_count: 0,
        };
        self.docs_added = 0;
        self.docs_deleted = 0;
        Ok(stats)
    }

    fn delete_by_event_id(&mut self, event_id: &str) -> Result<(), IndexWriteError> {
        let term = Term::from_field_text(self.handles.event_id, event_id);
        self.writer.delete_term(term);
        self.docs_deleted += 1;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// LexicalIndexer — index lifecycle manager
// ---------------------------------------------------------------------------

/// Manages the creation, opening, and configuration of a Tantivy lexical index.
///
/// Handles schema construction, tokenizer registration, fingerprint
/// verification, and writer provisioning. The actual ingestion loop is
/// driven by [`IncrementalIndexer`](crate::tantivy_ingest::IncrementalIndexer).
pub struct LexicalIndexer {
    index: Index,
    handles: LexicalFieldHandles,
    fingerprint: String,
    config: LexicalIndexerConfig,
}

impl LexicalIndexer {
    /// Open or create a lexical index at the configured directory.
    ///
    /// If the directory already contains an index, its schema fingerprint
    /// is verified against the current schema. A mismatch returns
    /// [`LexicalIngestError::SchemaFingerprintMismatch`].
    pub fn open(config: LexicalIndexerConfig) -> Result<Self, LexicalIngestError> {
        let (schema, handles) = build_lexical_schema_v1();
        let fingerprint = schema_fingerprint(&schema);

        std::fs::create_dir_all(&config.index_dir)?;

        let fp_path = config.index_dir.join(FINGERPRINT_FILENAME);
        let meta_path = config.index_dir.join("meta.json");

        let index = if meta_path.exists() {
            // Open existing index
            let index = Index::open_in_dir(&config.index_dir)?;
            register_tokenizers(&index);

            // Verify fingerprint
            if fp_path.exists() {
                let stored_fp = std::fs::read_to_string(&fp_path)?.trim().to_string();
                if stored_fp != fingerprint {
                    return Err(LexicalIngestError::SchemaFingerprintMismatch {
                        expected: fingerprint,
                        found: stored_fp,
                    });
                }
            }

            index
        } else {
            // Create new index
            let index = Index::create_in_dir(&config.index_dir, schema)?;
            register_tokenizers(&index);

            // Persist fingerprint
            std::fs::write(&fp_path, &fingerprint)?;

            index
        };

        Ok(Self {
            index,
            handles,
            fingerprint,
            config,
        })
    }

    /// Create a [`TantivyIndexWriterAdapter`] with the configured heap budget.
    pub fn create_writer(&self) -> Result<TantivyIndexWriterAdapter, LexicalIngestError> {
        let writer = self
            .index
            .writer_with_num_threads(1, self.config.writer_memory_bytes)?;
        Ok(TantivyIndexWriterAdapter::new(writer, self.handles))
    }

    /// Create a writer with a custom memory budget (useful for tests).
    pub fn create_writer_with_memory(
        &self,
        memory_bytes: usize,
    ) -> Result<TantivyIndexWriterAdapter, LexicalIngestError> {
        let writer = self
            .index
            .writer_with_num_threads(1, memory_bytes)?;
        Ok(TantivyIndexWriterAdapter::new(writer, self.handles))
    }

    /// Get a reference to the underlying Tantivy index.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// The schema fingerprint for this index version.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The field handles for direct document construction.
    pub fn handles(&self) -> LexicalFieldHandles {
        self.handles
    }

    /// The number of indexed documents (requires a reader reload).
    pub fn doc_count(&self) -> Result<u64, LexicalIngestError> {
        let reader = self.index.reader()?;
        Ok(reader.searcher().num_docs())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read the stored fingerprint from an index directory, if present.
pub fn read_stored_fingerprint(index_dir: &Path) -> Option<String> {
    let fp_path = index_dir.join(FINGERPRINT_FILENAME);
    std::fs::read_to_string(fp_path)
        .ok()
        .map(|s| s.trim().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{
        RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
        RecorderIngressKind, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
        RECORDER_EVENT_SCHEMA_VERSION_V1,
    };
    use crate::tantivy_ingest::map_event_to_document;
    use tempfile::tempdir;

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

    fn test_config(dir: &Path) -> LexicalIndexerConfig {
        LexicalIndexerConfig {
            index_dir: dir.join("tantivy-idx"),
            writer_memory_bytes: 15_000_000,
        }
    }

    // =========================================================================
    // Index creation and opening
    // =========================================================================

    #[test]
    fn create_new_index() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config.clone()).unwrap();

        assert!(!indexer.fingerprint().is_empty());
        assert_eq!(indexer.doc_count().unwrap(), 0);

        // Fingerprint file should exist
        let fp_path = config.index_dir.join(FINGERPRINT_FILENAME);
        assert!(fp_path.exists());
    }

    #[test]
    fn reopen_existing_index() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());

        // Create
        let indexer1 = LexicalIndexer::open(config.clone()).unwrap();
        let fp1 = indexer1.fingerprint().to_string();
        drop(indexer1);

        // Reopen
        let indexer2 = LexicalIndexer::open(config).unwrap();
        assert_eq!(indexer2.fingerprint(), fp1);
    }

    #[test]
    fn fingerprint_mismatch_detected() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());

        // Create index
        let _indexer = LexicalIndexer::open(config.clone()).unwrap();
        drop(_indexer);

        // Tamper with fingerprint
        let fp_path = config.index_dir.join(FINGERPRINT_FILENAME);
        std::fs::write(&fp_path, "tampered_fingerprint_value").unwrap();

        // Reopen should fail
        let result = LexicalIndexer::open(config);
        assert!(result.is_err());
        if let Err(LexicalIngestError::SchemaFingerprintMismatch { expected, found }) =
            result
        {
            assert_ne!(expected, found);
            assert_eq!(found, "tampered_fingerprint_value");
        } else {
            panic!("expected SchemaFingerprintMismatch error");
        }
    }

    // =========================================================================
    // Writer operations
    // =========================================================================

    #[test]
    fn writer_add_and_commit() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();
        let event = sample_event("ev-1", "cargo build");
        let doc_fields = map_event_to_document(&event, 0);

        writer.add_document(&doc_fields).unwrap();
        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_added, 1);

        assert_eq!(indexer.doc_count().unwrap(), 1);
    }

    #[test]
    fn writer_multiple_documents() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();
        for i in 0..5 {
            let event = sample_event(&format!("ev-{i}"), &format!("command-{i}"));
            let doc_fields = map_event_to_document(&event, i);
            writer.add_document(&doc_fields).unwrap();
        }
        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_added, 5);
        assert_eq!(indexer.doc_count().unwrap(), 5);
    }

    #[test]
    fn writer_delete_by_event_id() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // Add two documents
        let ev1 = sample_event("ev-keep", "keep this");
        let ev2 = sample_event("ev-delete", "delete this");
        writer
            .add_document(&map_event_to_document(&ev1, 0))
            .unwrap();
        writer
            .add_document(&map_event_to_document(&ev2, 1))
            .unwrap();
        writer.commit().unwrap();
        assert_eq!(indexer.doc_count().unwrap(), 2);

        // Delete one
        writer.delete_by_event_id("ev-delete").unwrap();
        writer.commit().unwrap();

        // After merge/reload, only 1 doc remains
        assert_eq!(indexer.doc_count().unwrap(), 1);
    }

    #[test]
    fn writer_stats_reset_after_commit() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // First batch
        let ev1 = sample_event("ev-1", "first");
        writer
            .add_document(&map_event_to_document(&ev1, 0))
            .unwrap();
        let stats1 = writer.commit().unwrap();
        assert_eq!(stats1.docs_added, 1);

        // Second batch
        let ev2 = sample_event("ev-2", "second");
        let ev3 = sample_event("ev-3", "third");
        writer
            .add_document(&map_event_to_document(&ev2, 1))
            .unwrap();
        writer
            .add_document(&map_event_to_document(&ev3, 2))
            .unwrap();
        let stats2 = writer.commit().unwrap();
        assert_eq!(stats2.docs_added, 2);
    }

    // =========================================================================
    // Mixed event types
    // =========================================================================

    #[test]
    fn index_mixed_event_types() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        let ingress = sample_event("ev-in", "echo hello");
        let egress = egress_event("ev-out", "hello world output");

        writer
            .add_document(&map_event_to_document(&ingress, 0))
            .unwrap();
        writer
            .add_document(&map_event_to_document(&egress, 1))
            .unwrap();
        writer.commit().unwrap();

        assert_eq!(indexer.doc_count().unwrap(), 2);
    }

    // =========================================================================
    // Integration with IncrementalIndexer
    // =========================================================================

    #[tokio::test]
    async fn full_pipeline_with_tantivy_writer() {
        use crate::recorder_storage::{
            AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, DurabilityLevel,
            RecorderStorage,
        };
        use crate::tantivy_ingest::{IndexerConfig, IncrementalIndexer, LEXICAL_INDEXER_CONSUMER};

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
        let writer = lexical_indexer.create_writer_with_memory(15_000_000).unwrap();

        // Run incremental indexer
        let pipeline_config = IndexerConfig {
            data_path: storage_config.data_path.clone(),
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
    }

    #[tokio::test]
    async fn incremental_pipeline_resumes_from_checkpoint() {
        use crate::recorder_storage::{
            AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, DurabilityLevel,
            RecorderStorage,
        };
        use crate::tantivy_ingest::{IndexerConfig, IncrementalIndexer};

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
            data_path: storage_config.data_path.clone(),
            consumer_id: "resume-test".to_string(),
            batch_size: 2,
            dedup_on_replay: true,
            max_batches: 1,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let writer1 = lexical_indexer.create_writer_with_memory(15_000_000).unwrap();
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
        let writer2 = lexical_indexer.create_writer_with_memory(15_000_000).unwrap();
        let mut pipeline2 = IncrementalIndexer::new(pipeline_config2, writer2);
        let r2 = pipeline2.run(&storage).await.unwrap();
        assert_eq!(r2.events_indexed, 2);
        assert!(r2.caught_up);

        // All 4 docs should be in the index
        assert_eq!(lexical_indexer.doc_count().unwrap(), 4);
    }

    // =========================================================================
    // Error types
    // =========================================================================

    #[test]
    fn error_display() {
        let e1 = LexicalIngestError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        assert!(e1.to_string().contains("I/O error"));

        let e2 = LexicalIngestError::SchemaFingerprintMismatch {
            expected: "abc".to_string(),
            found: "xyz".to_string(),
        };
        assert!(e2.to_string().contains("fingerprint mismatch"));
    }

    #[test]
    fn read_stored_fingerprint_missing() {
        let dir = tempdir().unwrap();
        assert!(read_stored_fingerprint(dir.path()).is_none());
    }

    #[test]
    fn read_stored_fingerprint_present() {
        let dir = tempdir().unwrap();
        let fp_path = dir.path().join(FINGERPRINT_FILENAME);
        std::fs::write(&fp_path, "abcdef123456").unwrap();
        assert_eq!(
            read_stored_fingerprint(dir.path()),
            Some("abcdef123456".to_string())
        );
    }

    // =========================================================================
    // Default config
    // =========================================================================

    #[test]
    fn default_config_values() {
        let cfg = LexicalIndexerConfig::default();
        assert_eq!(cfg.writer_memory_bytes, 50_000_000);
        assert!(cfg.index_dir.to_str().unwrap().contains("tantivy-lexical"));
    }
}
