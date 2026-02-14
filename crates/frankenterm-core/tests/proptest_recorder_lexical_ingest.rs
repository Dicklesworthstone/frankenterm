#![cfg(feature = "recorder-lexical")]

//! Property-based tests for the `recorder_lexical_ingest` module.
//!
//! Tests TantivyIndexWriterAdapter counting semantics, LexicalIndexerConfig
//! defaults, error type Display, fingerprint persistence roundtrips, and
//! index lifecycle properties.

use frankenterm_core::recorder_lexical_ingest::{
    LexicalIndexer, LexicalIndexerConfig, LexicalIngestError, read_stored_fingerprint,
};
use frankenterm_core::recorder_lexical_schema::{
    build_lexical_schema_v1, fields_to_document, register_tokenizers,
};
use frankenterm_core::tantivy_ingest::{IndexDocumentFields, IndexWriter, LEXICAL_SCHEMA_VERSION};
use proptest::prelude::*;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_event_id() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z0-9\\-]{5,40}",
        Just("ev-test-1".to_string()),
    ]
}

fn arb_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[a-zA-Z0-9 _./:\\-]{0,100}",
        Just("echo hello world".to_string()),
        Just("cargo test --release".to_string()),
    ]
}

fn arb_document_fields() -> impl Strategy<Value = IndexDocumentFields> {
    (arb_event_id(), any::<u64>(), arb_text(), arb_text(), any::<bool>())
        .prop_map(|(event_id, pane_id, text, text_symbols, is_gap)| {
            IndexDocumentFields {
                schema_version: "ft.recorder.v1".to_string(),
                lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
                event_id,
                pane_id,
                session_id: None,
                workflow_id: None,
                correlation_id: None,
                source: "test".to_string(),
                event_type: "ingress_text".to_string(),
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
                ingress_kind: Some("send_text".to_string()),
                segment_kind: None,
                control_marker_type: None,
                lifecycle_phase: None,
                is_gap,
                redaction: None,
                occurred_at_ms: 1_700_000_000_000,
                recorded_at_ms: 1_700_000_000_001,
                sequence: 0,
                log_offset: 0,
                text,
                text_symbols,
                details_json: "{}".to_string(),
            }
        })
}

fn arb_writer_memory() -> impl Strategy<Value = usize> {
    prop_oneof![
        Just(15_000_000usize),
        Just(50_000_000usize),
        15_000_000usize..=100_000_000usize,
    ]
}

// ---------------------------------------------------------------------------
// Config default properties
// ---------------------------------------------------------------------------

proptest! {
    /// Default config always has positive writer memory.
    #[test]
    fn default_config_writer_memory_positive(_seed in any::<u64>()) {
        let cfg = LexicalIndexerConfig::default();
        prop_assert!(cfg.writer_memory_bytes > 0,
            "default writer memory should be positive, got {}", cfg.writer_memory_bytes);
    }

    /// Default config path contains the expected directory name.
    #[test]
    fn default_config_path_contains_tantivy(_seed in any::<u64>()) {
        let cfg = LexicalIndexerConfig::default();
        let path_str = cfg.index_dir.to_string_lossy();
        prop_assert!(path_str.contains("tantivy"),
            "default path should reference tantivy: {}", path_str);
    }

    /// Config Clone produces identical values.
    #[test]
    fn config_clone_identical(memory in arb_writer_memory()) {
        let cfg = LexicalIndexerConfig {
            index_dir: std::path::PathBuf::from("/tmp/test-idx"),
            writer_memory_bytes: memory,
        };
        let cloned = cfg.clone();
        prop_assert_eq!(cfg.writer_memory_bytes, cloned.writer_memory_bytes);
        prop_assert_eq!(cfg.index_dir, cloned.index_dir);
    }
}

// ---------------------------------------------------------------------------
// Error type properties
// ---------------------------------------------------------------------------

proptest! {
    /// LexicalIngestError::SchemaFingerprintMismatch Display contains both fingerprints.
    #[test]
    fn error_display_contains_fingerprints(
        expected in "[a-f0-9]{10,64}",
        found in "[a-f0-9]{10,64}",
    ) {
        let err = LexicalIngestError::SchemaFingerprintMismatch {
            expected: expected.clone(),
            found: found.clone(),
        };
        let display = err.to_string();
        prop_assert!(display.contains(&expected),
            "display should contain expected fp '{}': {}", expected, display);
        prop_assert!(display.contains(&found),
            "display should contain found fp '{}': {}", found, display);
        prop_assert!(display.contains("mismatch"),
            "display should mention 'mismatch': {}", display);
    }

    /// LexicalIngestError::Io Display contains 'I/O'.
    #[test]
    fn error_io_display_contains_prefix(
        msg in "[a-zA-Z0-9 ]{1,50}",
    ) {
        let err = LexicalIngestError::Io(
            std::io::Error::new(std::io::ErrorKind::Other, msg.clone())
        );
        let display = err.to_string();
        prop_assert!(display.contains("I/O"),
            "IO error display should contain 'I/O': {}", display);
    }
}

// ---------------------------------------------------------------------------
// Index creation properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Opening an indexer in a fresh tempdir always succeeds.
    #[test]
    fn open_fresh_always_succeeds(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config);
        prop_assert!(indexer.is_ok(), "open should succeed: {:?}", indexer.err());
    }

    /// Fingerprint is always non-empty after creation.
    #[test]
    fn fingerprint_nonempty_after_create(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        prop_assert!(!indexer.fingerprint().is_empty());
    }

    /// Doc count is zero on a freshly created index.
    #[test]
    fn fresh_index_has_zero_docs(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        prop_assert_eq!(indexer.doc_count().unwrap(), 0);
    }

    /// Reopening an index produces the same fingerprint.
    #[test]
    fn reopen_preserves_fingerprint(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };

        let fp1 = {
            let indexer = LexicalIndexer::open(config.clone()).unwrap();
            indexer.fingerprint().to_string()
        };

        let fp2 = {
            let indexer = LexicalIndexer::open(config).unwrap();
            indexer.fingerprint().to_string()
        };

        prop_assert_eq!(fp1, fp2);
    }
}

// ---------------------------------------------------------------------------
// Fingerprint persistence properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// read_stored_fingerprint returns None for non-existent directory.
    #[test]
    fn read_fingerprint_missing_dir(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let result = read_stored_fingerprint(&dir.path().join("nonexistent"));
        prop_assert!(result.is_none());
    }

    /// After creating an index, read_stored_fingerprint returns the correct value.
    #[test]
    fn read_fingerprint_after_create(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let idx_dir = dir.path().join("idx");
        let config = LexicalIndexerConfig {
            index_dir: idx_dir.clone(),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        let expected = indexer.fingerprint().to_string();

        let stored = read_stored_fingerprint(&idx_dir);
        prop_assert_eq!(stored, Some(expected));
    }

    /// Tampered fingerprint causes SchemaFingerprintMismatch on reopen.
    #[test]
    fn tampered_fingerprint_detected(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let idx_dir = dir.path().join("idx");
        let config = LexicalIndexerConfig {
            index_dir: idx_dir.clone(),
            writer_memory_bytes: 15_000_000,
        };

        // Create
        let _indexer = LexicalIndexer::open(config.clone()).unwrap();
        drop(_indexer);

        // Tamper
        let fp_path = idx_dir.join(".ft_schema_fingerprint");
        std::fs::write(&fp_path, "tampered_value").unwrap();

        // Reopen should fail
        let result = LexicalIndexer::open(config);
        prop_assert!(result.is_err());
    }
}

// ---------------------------------------------------------------------------
// Writer counting properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Adding N documents and committing yields docs_added == N.
    #[test]
    fn writer_add_count_matches(
        doc_count in 1usize..=8,
    ) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        for i in 0..doc_count {
            let fields = IndexDocumentFields {
                schema_version: "ft.recorder.v1".to_string(),
                lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
                event_id: format!("ev-{}", i),
                pane_id: 1,
                session_id: None,
                workflow_id: None,
                correlation_id: None,
                source: "test".to_string(),
                event_type: "ingress_text".to_string(),
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
                ingress_kind: None,
                segment_kind: None,
                control_marker_type: None,
                lifecycle_phase: None,
                is_gap: false,
                redaction: None,
                occurred_at_ms: 1_700_000_000_000,
                recorded_at_ms: 1_700_000_000_001,
                sequence: i as u64,
                log_offset: 0,
                text: "test".to_string(),
                text_symbols: "test".to_string(),
                details_json: "{}".to_string(),
            };
            writer.add_document(&fields).unwrap();
        }

        let stats = writer.commit().unwrap();
        prop_assert_eq!(stats.docs_added as usize, doc_count,
            "docs_added should be {}, got {}", doc_count, stats.docs_added);
    }

    /// Commit resets the stats counter to zero.
    #[test]
    fn writer_commit_resets_stats(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // Add 3 docs, commit
        for i in 0..3 {
            let fields = IndexDocumentFields {
                schema_version: "ft.recorder.v1".to_string(),
                lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
                event_id: format!("ev-a{}", i),
                pane_id: 1,
                session_id: None,
                workflow_id: None,
                correlation_id: None,
                source: "test".to_string(),
                event_type: "ingress_text".to_string(),
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
                ingress_kind: None,
                segment_kind: None,
                control_marker_type: None,
                lifecycle_phase: None,
                is_gap: false,
                redaction: None,
                occurred_at_ms: 1_700_000_000_000,
                recorded_at_ms: 1_700_000_000_001,
                sequence: i,
                log_offset: 0,
                text: "test".to_string(),
                text_symbols: "test".to_string(),
                details_json: "{}".to_string(),
            };
            writer.add_document(&fields).unwrap();
        }
        let stats1 = writer.commit().unwrap();
        prop_assert_eq!(stats1.docs_added, 3);

        // Second commit with 1 doc
        let fields = IndexDocumentFields {
            schema_version: "ft.recorder.v1".to_string(),
            lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
            event_id: "ev-b0".to_string(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: "test".to_string(),
            event_type: "ingress_text".to_string(),
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
            ingress_kind: None,
            segment_kind: None,
            control_marker_type: None,
            lifecycle_phase: None,
            is_gap: false,
            redaction: None,
            occurred_at_ms: 1_700_000_000_000,
            recorded_at_ms: 1_700_000_000_001,
            sequence: 10,
            log_offset: 0,
            text: "test".to_string(),
            text_symbols: "test".to_string(),
            details_json: "{}".to_string(),
        };
        writer.add_document(&fields).unwrap();
        let stats2 = writer.commit().unwrap();
        prop_assert_eq!(stats2.docs_added, 1,
            "after reset, should be 1, got {}", stats2.docs_added);
    }

    /// doc_count matches total documents after multiple commits.
    #[test]
    fn doc_count_accumulates(
        batch1 in 1usize..=4,
        batch2 in 1usize..=4,
    ) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // First batch
        for i in 0..batch1 {
            let fields = IndexDocumentFields {
                schema_version: "ft.recorder.v1".to_string(),
                lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
                event_id: format!("ev-1-{}", i),
                pane_id: 1,
                session_id: None,
                workflow_id: None,
                correlation_id: None,
                source: "test".to_string(),
                event_type: "ingress_text".to_string(),
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
                ingress_kind: None,
                segment_kind: None,
                control_marker_type: None,
                lifecycle_phase: None,
                is_gap: false,
                redaction: None,
                occurred_at_ms: 1_700_000_000_000,
                recorded_at_ms: 1_700_000_000_001,
                sequence: i as u64,
                log_offset: 0,
                text: "test".to_string(),
                text_symbols: "test".to_string(),
                details_json: "{}".to_string(),
            };
            writer.add_document(&fields).unwrap();
        }
        writer.commit().unwrap();

        // Second batch
        for i in 0..batch2 {
            let fields = IndexDocumentFields {
                schema_version: "ft.recorder.v1".to_string(),
                lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
                event_id: format!("ev-2-{}", i),
                pane_id: 1,
                session_id: None,
                workflow_id: None,
                correlation_id: None,
                source: "test".to_string(),
                event_type: "ingress_text".to_string(),
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
                ingress_kind: None,
                segment_kind: None,
                control_marker_type: None,
                lifecycle_phase: None,
                is_gap: false,
                redaction: None,
                occurred_at_ms: 1_700_000_000_000,
                recorded_at_ms: 1_700_000_000_001,
                sequence: (batch1 + i) as u64,
                log_offset: 0,
                text: "test".to_string(),
                text_symbols: "test".to_string(),
                details_json: "{}".to_string(),
            };
            writer.add_document(&fields).unwrap();
        }
        writer.commit().unwrap();

        let total = indexer.doc_count().unwrap() as usize;
        prop_assert_eq!(total, batch1 + batch2,
            "total docs should be {} + {} = {}, got {}", batch1, batch2, batch1 + batch2, total);
    }
}

// ---------------------------------------------------------------------------
// Delete counting properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Deleting by event_id increments docs_deleted in commit stats.
    #[test]
    fn delete_count_tracked(n_delete in 1usize..=3) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        let mut writer = indexer.create_writer_with_memory(15_000_000).unwrap();

        // Add some docs first
        for i in 0..5 {
            let fields = IndexDocumentFields {
                schema_version: "ft.recorder.v1".to_string(),
                lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
                event_id: format!("ev-{}", i),
                pane_id: 1,
                session_id: None,
                workflow_id: None,
                correlation_id: None,
                source: "test".to_string(),
                event_type: "ingress_text".to_string(),
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
                ingress_kind: None,
                segment_kind: None,
                control_marker_type: None,
                lifecycle_phase: None,
                is_gap: false,
                redaction: None,
                occurred_at_ms: 1_700_000_000_000,
                recorded_at_ms: 1_700_000_000_001,
                sequence: i as u64,
                log_offset: 0,
                text: "test".to_string(),
                text_symbols: "test".to_string(),
                details_json: "{}".to_string(),
            };
            writer.add_document(&fields).unwrap();
        }
        writer.commit().unwrap();

        // Delete n_delete docs
        for i in 0..n_delete {
            writer.delete_by_event_id(&format!("ev-{}", i)).unwrap();
        }
        let stats = writer.commit().unwrap();

        prop_assert_eq!(stats.docs_deleted as usize, n_delete,
            "should have deleted {}, got {}", n_delete, stats.docs_deleted);
    }
}

// ---------------------------------------------------------------------------
// Accessor properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5))]

    /// index() accessor returns a valid index reference (can create a reader).
    #[test]
    fn index_accessor_valid(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        let reader = indexer.index().reader();
        prop_assert!(reader.is_ok(), "reader creation should succeed");
    }

    /// handles() accessor returns valid handles that match the schema.
    #[test]
    fn handles_accessor_matches_schema(_seed in any::<u64>()) {
        let dir = tempdir().expect("tempdir");
        let config = LexicalIndexerConfig {
            index_dir: dir.path().join("idx"),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        let handles = indexer.handles();

        // Build a fresh schema and verify handles match
        let (_, fresh_handles) = build_lexical_schema_v1();
        prop_assert_eq!(handles.event_id, fresh_handles.event_id);
        prop_assert_eq!(handles.pane_id, fresh_handles.pane_id);
        prop_assert_eq!(handles.text, fresh_handles.text);
        prop_assert_eq!(handles.sequence, fresh_handles.sequence);
    }
}
