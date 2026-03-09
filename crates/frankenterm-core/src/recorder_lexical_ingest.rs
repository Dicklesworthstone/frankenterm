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
    LexicalFieldHandles, build_lexical_schema_v1, fields_to_document, register_tokenizers,
    schema_fingerprint,
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
        self.writer
            .add_document(tantivy_doc)
            .map_err(|e| IndexWriteError::Rejected {
                reason: e.to_string(),
            })?;
        self.docs_added += 1;
        Ok(())
    }

    fn commit(&mut self) -> Result<IndexCommitStats, IndexWriteError> {
        self.writer
            .commit()
            .map_err(|e| IndexWriteError::CommitFailed {
                reason: e.to_string(),
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
        let writer = self.index.writer_with_num_threads(1, memory_bytes)?;
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
        RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality,
        RecorderEventPayload, RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel,
        RecorderSegmentKind, RecorderTextEncoding,
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

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;

        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build recorder_lexical_ingest test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
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
        if let Err(LexicalIngestError::SchemaFingerprintMismatch { expected, found }) = result {
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

    #[test]
    fn full_pipeline_with_tantivy_writer() {
        use crate::recorder_storage::{
            AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, DurabilityLevel,
            RecorderStorage,
        };
        use crate::tantivy_ingest::{IncrementalIndexer, IndexerConfig, LEXICAL_INDEXER_CONSUMER};

        run_async_test(async {
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
                source: crate::recorder_storage::RecorderSourceDescriptor::AppendLog {
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
        use crate::recorder_storage::{
            AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, DurabilityLevel,
            RecorderStorage,
        };
        use crate::tantivy_ingest::{IncrementalIndexer, IndexerConfig};

        run_async_test(async {
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
                source: crate::recorder_storage::RecorderSourceDescriptor::AppendLog {
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

    // =========================================================================
    // Constants
    // =========================================================================

    #[test]
    fn constants_valid() {
        assert_eq!(DEFAULT_WRITER_MEMORY_BYTES, 50_000_000);
        assert_eq!(FINGERPRINT_FILENAME, ".ft_schema_fingerprint");
        assert!(FINGERPRINT_FILENAME.starts_with('.'));
    }

    // =========================================================================
    // LexicalIndexerConfig — traits
    // =========================================================================

    #[test]
    fn config_clone_preserves_fields() {
        let cfg = LexicalIndexerConfig {
            index_dir: PathBuf::from("/custom/path"),
            writer_memory_bytes: 100_000,
        };
        let cfg2 = cfg.clone();
        assert_eq!(cfg2.index_dir, PathBuf::from("/custom/path"));
        assert_eq!(cfg2.writer_memory_bytes, 100_000);
    }

    #[test]
    fn config_debug_formatting() {
        let cfg = LexicalIndexerConfig::default();
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("LexicalIndexerConfig"));
        assert!(dbg.contains("writer_memory_bytes"));
    }

    // =========================================================================
    // LexicalIngestError — comprehensive coverage
    // =========================================================================

    #[test]
    fn error_display_io_permission() {
        let e = LexicalIngestError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        ));
        let msg = e.to_string();
        assert!(msg.contains("I/O error"));
        assert!(msg.contains("access denied"));
    }

    #[test]
    fn error_display_fingerprint_exact_format() {
        let e = LexicalIngestError::SchemaFingerprintMismatch {
            expected: "abc123".to_string(),
            found: "xyz789".to_string(),
        };
        let msg = e.to_string();
        assert!(msg.contains("expected=abc123"));
        assert!(msg.contains("found=xyz789"));
    }

    #[test]
    fn error_is_error_trait() {
        let e: Box<dyn std::error::Error> =
            Box::new(LexicalIngestError::Io(std::io::Error::other("test")));
        assert!(e.to_string().contains("I/O error"));
    }

    #[test]
    fn error_from_io_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let err: LexicalIngestError = io_err.into();
        match err {
            LexicalIngestError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
            other => panic!("expected Io variant, got {other:?}"),
        }
    }

    #[test]
    fn error_debug_formatting() {
        let e = LexicalIngestError::SchemaFingerprintMismatch {
            expected: "a".into(),
            found: "b".into(),
        };
        let dbg = format!("{e:?}");
        assert!(dbg.contains("SchemaFingerprintMismatch"));
    }

    // =========================================================================
    // TantivyIndexWriterAdapter — counter behaviors
    // =========================================================================

    #[test]
    fn writer_empty_commit_returns_zeroed_stats() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();
        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_added, 0);
        assert_eq!(stats.docs_deleted, 0);
        assert_eq!(stats.segment_count, 0);
    }

    #[test]
    fn writer_delete_increments_counter() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // Add a document, then delete it
        let ev = sample_event("ev-1", "hello");
        writer.add_document(&map_event_to_document(&ev, 0)).unwrap();
        writer.delete_by_event_id("ev-1").unwrap();

        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_added, 1);
        assert_eq!(stats.docs_deleted, 1);
    }

    #[test]
    fn writer_interleaved_add_delete() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // Add 3, delete 1
        for i in 0..3 {
            let ev = sample_event(&format!("ev-{i}"), &format!("text-{i}"));
            writer.add_document(&map_event_to_document(&ev, i)).unwrap();
        }
        writer.delete_by_event_id("ev-0").unwrap();

        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_added, 3);
        assert_eq!(stats.docs_deleted, 1);
    }

    #[test]
    fn writer_multiple_deletes() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // Delete 3 events (even if they don't exist, counter still increments)
        writer.delete_by_event_id("ev-a").unwrap();
        writer.delete_by_event_id("ev-b").unwrap();
        writer.delete_by_event_id("ev-c").unwrap();

        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_added, 0);
        assert_eq!(stats.docs_deleted, 3);
    }

    #[test]
    fn writer_counters_reset_each_commit() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // Commit 1: 2 adds, 1 delete
        let ev1 = sample_event("ev-1", "a");
        let ev2 = sample_event("ev-2", "b");
        writer
            .add_document(&map_event_to_document(&ev1, 0))
            .unwrap();
        writer
            .add_document(&map_event_to_document(&ev2, 1))
            .unwrap();
        writer.delete_by_event_id("ev-1").unwrap();
        let s1 = writer.commit().unwrap();
        assert_eq!(s1.docs_added, 2);
        assert_eq!(s1.docs_deleted, 1);

        // Commit 2: counters should be reset
        let ev3 = sample_event("ev-3", "c");
        writer
            .add_document(&map_event_to_document(&ev3, 2))
            .unwrap();
        let s2 = writer.commit().unwrap();
        assert_eq!(s2.docs_added, 1);
        assert_eq!(s2.docs_deleted, 0);
    }

    // =========================================================================
    // LexicalIndexer — accessors and lifecycle
    // =========================================================================

    #[test]
    fn indexer_fingerprint_deterministic() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();

        let idx1 = LexicalIndexer::open(test_config(dir1.path())).unwrap();
        let idx2 = LexicalIndexer::open(test_config(dir2.path())).unwrap();

        // Same schema → same fingerprint
        assert_eq!(idx1.fingerprint(), idx2.fingerprint());
    }

    #[test]
    fn indexer_handles_returns_valid_fields() {
        let dir = tempdir().unwrap();
        let indexer = LexicalIndexer::open(test_config(dir.path())).unwrap();

        let handles = indexer.handles();
        // Verify field handles are distinct (event_id != pane_id)
        assert_ne!(handles.event_id, handles.pane_id);
        assert_ne!(handles.event_id, handles.occurred_at_ms);
    }

    #[test]
    fn indexer_index_reference() {
        let dir = tempdir().unwrap();
        let indexer = LexicalIndexer::open(test_config(dir.path())).unwrap();

        let index = indexer.index();
        // Index should have a schema with the expected fields
        let schema = index.schema();
        assert!(schema.get_field("event_id").is_ok());
        assert!(schema.get_field("pane_id").is_ok());
    }

    #[test]
    fn indexer_create_writer_default() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        // create_writer uses the config's memory budget
        let mut writer = indexer.create_writer().unwrap();
        // Verify it works by adding a doc
        let ev = sample_event("ev-default", "test");
        writer.add_document(&map_event_to_document(&ev, 0)).unwrap();
        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_added, 1);
    }

    #[test]
    fn indexer_doc_count_tracks_operations() {
        let dir = tempdir().unwrap();
        let indexer = LexicalIndexer::open(test_config(dir.path())).unwrap();

        assert_eq!(indexer.doc_count().unwrap(), 0);

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();
        for i in 0..3 {
            let ev = sample_event(&format!("ev-{i}"), &format!("text-{i}"));
            writer.add_document(&map_event_to_document(&ev, i)).unwrap();
        }
        writer.commit().unwrap();

        assert_eq!(indexer.doc_count().unwrap(), 3);
    }

    // =========================================================================
    // read_stored_fingerprint — edge cases
    // =========================================================================

    #[test]
    fn read_stored_fingerprint_trims_whitespace() {
        let dir = tempdir().unwrap();
        let fp_path = dir.path().join(FINGERPRINT_FILENAME);
        std::fs::write(&fp_path, "  abc123  \n").unwrap();
        assert_eq!(
            read_stored_fingerprint(dir.path()),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn read_stored_fingerprint_trims_newlines() {
        let dir = tempdir().unwrap();
        let fp_path = dir.path().join(FINGERPRINT_FILENAME);
        std::fs::write(&fp_path, "fp-value\n\n").unwrap();
        assert_eq!(
            read_stored_fingerprint(dir.path()),
            Some("fp-value".to_string())
        );
    }

    #[test]
    fn read_stored_fingerprint_empty_file() {
        let dir = tempdir().unwrap();
        let fp_path = dir.path().join(FINGERPRINT_FILENAME);
        std::fs::write(&fp_path, "").unwrap();
        assert_eq!(read_stored_fingerprint(dir.path()), Some(String::new()));
    }

    #[test]
    fn read_stored_fingerprint_nonexistent_dir() {
        assert!(read_stored_fingerprint(Path::new("/nonexistent/dir/99999")).is_none());
    }

    // =========================================================================
    // Index lifecycle — directory creation
    // =========================================================================

    #[test]
    fn open_creates_missing_directory() {
        let dir = tempdir().unwrap();
        let index_dir = dir.path().join("deep/nested/index");
        assert!(!index_dir.exists());

        let config = LexicalIndexerConfig {
            index_dir: index_dir.clone(),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        assert!(index_dir.exists());
        assert_eq!(indexer.doc_count().unwrap(), 0);
    }

    #[test]
    fn open_persists_fingerprint_for_new_index() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config.clone()).unwrap();

        let fp_path = config.index_dir.join(FINGERPRINT_FILENAME);
        let stored = std::fs::read_to_string(&fp_path).unwrap();
        assert_eq!(stored.trim(), indexer.fingerprint());
    }

    #[test]
    fn open_without_fingerprint_file_succeeds() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());

        // Create index
        let indexer = LexicalIndexer::open(config.clone()).unwrap();
        drop(indexer);

        // Remove the fingerprint file
        let fp_path = config.index_dir.join(FINGERPRINT_FILENAME);
        std::fs::remove_file(&fp_path).unwrap();

        // Reopen should succeed (fingerprint check is skipped if file absent)
        let indexer2 = LexicalIndexer::open(config).unwrap();
        assert!(!indexer2.fingerprint().is_empty());
    }

    // =========================================================================
    // Delete semantics
    // =========================================================================

    #[test]
    fn delete_nonexistent_event_succeeds() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // Deleting a non-existent event should not error
        writer.delete_by_event_id("no-such-event").unwrap();
        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_deleted, 1); // counter increments regardless
        assert_eq!(indexer.doc_count().unwrap(), 0);
    }

    #[test]
    fn delete_same_event_twice() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        let ev = sample_event("ev-dup", "text");
        writer.add_document(&map_event_to_document(&ev, 0)).unwrap();
        writer.commit().unwrap();
        assert_eq!(indexer.doc_count().unwrap(), 1);

        // Delete same event twice
        writer.delete_by_event_id("ev-dup").unwrap();
        writer.delete_by_event_id("ev-dup").unwrap();
        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_deleted, 2); // counter tracks calls, not unique docs
        assert_eq!(indexer.doc_count().unwrap(), 0);
    }

    // =========================================================================
    // Large batch
    // =========================================================================

    #[test]
    fn large_batch_commit() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        let count = 100;
        for i in 0..count {
            let ev = sample_event(&format!("ev-{i}"), &format!("command number {i}"));
            writer.add_document(&map_event_to_document(&ev, i)).unwrap();
        }
        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_added, count);
        assert_eq!(indexer.doc_count().unwrap(), count);
    }

    // =========================================================================
    // Egress-only indexing
    // =========================================================================

    #[test]
    fn index_egress_only_events() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let indexer = LexicalIndexer::open(config).unwrap();

        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        for i in 0..3 {
            let ev = egress_event(&format!("eg-{i}"), &format!("output line {i}"));
            writer.add_document(&map_event_to_document(&ev, i)).unwrap();
        }
        let stats = writer.commit().unwrap();
        assert_eq!(stats.docs_added, 3);
        assert_eq!(indexer.doc_count().unwrap(), 3);
    }

    // =========================================================================
    // Index reopen preserves documents
    // =========================================================================

    #[test]
    fn reopen_index_preserves_documents() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());

        // Create index and add documents
        let indexer = LexicalIndexer::open(config.clone()).unwrap();
        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();
        let ev = sample_event("ev-persist", "persisted data");
        writer.add_document(&map_event_to_document(&ev, 0)).unwrap();
        writer.commit().unwrap();
        assert_eq!(indexer.doc_count().unwrap(), 1);
        drop(writer);
        drop(indexer);

        // Reopen and verify
        let indexer2 = LexicalIndexer::open(config).unwrap();
        assert_eq!(indexer2.doc_count().unwrap(), 1);
    }
}
