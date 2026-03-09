//! Property-based tests for the lexical backend bridge module.
//!
//! Covers IngestLifecyclePolicy, LexicalSchemaVersion, DocumentSource enum
//! parse/display roundtrips, LexicalBackendConfig serde roundtrips with defaults,
//! LexicalBackendMetrics rate computation bounds, and BridgeDocument serde.

use frankenterm_core::search::lexical_backend_bridge::{
    compute_churn_rate, compute_query_error_rate, compute_rejection_rate, BridgeDocument,
    DocumentSource, IngestLifecyclePolicy, LexicalBackendConfig, LexicalBackendMetrics,
    LexicalSchemaVersion,
};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_lifecycle_policy() -> impl Strategy<Value = IngestLifecyclePolicy> {
    prop_oneof![
        Just(IngestLifecyclePolicy::Checkpoint),
        Just(IngestLifecyclePolicy::TtlLru),
    ]
}

fn arb_schema_version() -> impl Strategy<Value = LexicalSchemaVersion> {
    prop_oneof![
        Just(LexicalSchemaVersion::RecorderV1),
        Just(LexicalSchemaVersion::FrankenSearchV1),
    ]
}

fn arb_document_source() -> impl Strategy<Value = DocumentSource> {
    prop_oneof![
        Just(DocumentSource::Scrollback),
        Just(DocumentSource::Command),
        Just(DocumentSource::AgentArtifact),
        Just(DocumentSource::PaneMetadata),
        Just(DocumentSource::Cass),
        Just(DocumentSource::RecorderEvent),
    ]
}

fn arb_config() -> impl Strategy<Value = LexicalBackendConfig> {
    (
        arb_lifecycle_policy(),
        arb_schema_version(),
        1..1000usize,     // flush_batch_size
        1..3600u64,       // flush_interval_secs
        0..365u32,        // ttl_days
        0..1_000_000u64,  // max_index_size_bytes
        1..100_000_000usize, // writer_heap_bytes
        any::<bool>(),    // terminal_tokenizers
        0..10000u32,      // max_docs_per_second
    )
        .prop_map(
            |(lp, sv, fbs, fis, ttl, mis, whb, tt, mdps)| LexicalBackendConfig {
                lifecycle_policy: lp,
                schema_version: sv,
                flush_batch_size: fbs,
                flush_interval_secs: fis,
                ttl_days: ttl,
                max_index_size_bytes: mis,
                writer_heap_bytes: whb,
                terminal_tokenizers: tt,
                max_docs_per_second: mdps,
            },
        )
}

fn arb_metrics() -> impl Strategy<Value = LexicalBackendMetrics> {
    (
        0..100_000u64, // docs_ingested
        0..100_000u64, // docs_expired
        0..100_000u64, // docs_active
        0..10_000u64,  // flush_count
        0..100_000u64, // docs_rejected
        0..100_000u64, // queries_executed
        0..100_000u64, // query_errors
        0..1_000_000u64, // index_size_bytes
        0..100u32,     // segment_count
    )
        .prop_map(
            |(di, de, da, fc, dr, qe, qerr, isb, sc)| LexicalBackendMetrics {
                docs_ingested: di,
                docs_expired: de,
                docs_active: da,
                flush_count: fc,
                docs_rejected: dr,
                queries_executed: qe,
                query_errors: qerr,
                index_size_bytes: isb,
                segment_count: sc,
                schema_version: LexicalSchemaVersion::default().as_str().to_string(),
                lifecycle_policy: IngestLifecyclePolicy::default().as_str().to_string(),
            },
        )
}

fn arb_bridge_document() -> impl Strategy<Value = BridgeDocument> {
    (
        "[a-z0-9]{1,20}",       // doc_id
        "[a-zA-Z0-9 ]{0,100}",  // text
        arb_document_source(),
        any::<u64>(),            // captured_at_ms
        proptest::option::of(any::<u64>()), // pane_id
        proptest::option::of("[a-z]{1,10}".prop_map(String::from)), // session_id
        proptest::option::of("[a-f0-9]{8}".prop_map(String::from)), // content_hash
    )
        .prop_map(
            |(doc_id, text, source, captured_at_ms, pane_id, session_id, content_hash)| {
                BridgeDocument {
                    doc_id,
                    text,
                    source,
                    captured_at_ms,
                    pane_id,
                    session_id,
                    content_hash,
                    metadata: vec![],
                }
            },
        )
}

// =============================================================================
// IngestLifecyclePolicy properties
// =============================================================================

proptest! {
    /// parse(as_str()) roundtrip for IngestLifecyclePolicy.
    #[test]
    fn lifecycle_policy_parse_as_str_roundtrip(policy in arb_lifecycle_policy()) {
        let s = policy.as_str();
        let parsed = IngestLifecyclePolicy::parse(s);
        prop_assert_eq!(parsed, policy);
    }

    /// Display matches as_str.
    #[test]
    fn lifecycle_policy_display_matches_as_str(policy in arb_lifecycle_policy()) {
        prop_assert_eq!(format!("{policy}"), policy.as_str());
    }

    /// Serde JSON roundtrip.
    #[test]
    fn lifecycle_policy_serde_roundtrip(policy in arb_lifecycle_policy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let rt: IngestLifecyclePolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, policy);
    }

    /// Unknown input parses to default (TtlLru).
    #[test]
    fn lifecycle_policy_unknown_defaults(input in "[A-Z]{3,10}") {
        let parsed = IngestLifecyclePolicy::parse(&input);
        prop_assert_eq!(parsed, IngestLifecyclePolicy::TtlLru);
    }
}

// =============================================================================
// LexicalSchemaVersion properties
// =============================================================================

proptest! {
    /// parse(as_str()) roundtrip for LexicalSchemaVersion.
    #[test]
    fn schema_version_parse_as_str_roundtrip(version in arb_schema_version()) {
        let s = version.as_str();
        let parsed = LexicalSchemaVersion::parse(s);
        prop_assert_eq!(parsed, version);
    }

    /// Display matches as_str.
    #[test]
    fn schema_version_display_matches_as_str(version in arb_schema_version()) {
        prop_assert_eq!(format!("{version}"), version.as_str());
    }

    /// Serde JSON roundtrip.
    #[test]
    fn schema_version_serde_roundtrip(version in arb_schema_version()) {
        let json = serde_json::to_string(&version).unwrap();
        let rt: LexicalSchemaVersion = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, version);
    }

    /// Unknown input parses to default (FrankenSearchV1).
    #[test]
    fn schema_version_unknown_defaults(input in "[A-Z]{3,10}") {
        let parsed = LexicalSchemaVersion::parse(&input);
        prop_assert_eq!(parsed, LexicalSchemaVersion::FrankenSearchV1);
    }

    /// RecorderV1 uses terminal tokenizers, FrankenSearchV1 does not.
    #[test]
    fn schema_version_tokenizer_consistency(version in arb_schema_version()) {
        let uses_tokenizers = version.uses_terminal_tokenizers();
        let check = matches!(version, LexicalSchemaVersion::RecorderV1);
        prop_assert_eq!(uses_tokenizers, check);
    }
}

// =============================================================================
// DocumentSource properties
// =============================================================================

proptest! {
    /// parse(as_str()) roundtrip for DocumentSource.
    #[test]
    fn document_source_parse_as_str_roundtrip(source in arb_document_source()) {
        let s = source.as_str();
        let parsed = DocumentSource::parse(s);
        prop_assert_eq!(parsed, source);
    }

    /// Display matches as_str.
    #[test]
    fn document_source_display_matches_as_str(source in arb_document_source()) {
        prop_assert_eq!(format!("{source}"), source.as_str());
    }

    /// Serde JSON roundtrip.
    #[test]
    fn document_source_serde_roundtrip(source in arb_document_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let rt: DocumentSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, source);
    }

    /// Unknown input parses to Scrollback (default).
    #[test]
    fn document_source_unknown_defaults(input in "[A-Z]{3,10}") {
        let parsed = DocumentSource::parse(&input);
        prop_assert_eq!(parsed, DocumentSource::Scrollback);
    }

    /// All as_str() outputs are unique.
    #[test]
    fn document_source_as_str_unique(a in arb_document_source(), b in arb_document_source()) {
        if a == b {
            prop_assert_eq!(a.as_str(), b.as_str());
        } else {
            let check = a.as_str() != b.as_str();
            prop_assert!(check, "different sources should have different as_str()");
        }
    }
}

// =============================================================================
// LexicalBackendConfig properties
// =============================================================================

proptest! {
    /// Config survives JSON roundtrip.
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let rt: LexicalBackendConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, config);
    }

    /// Empty JSON object produces default config.
    #[test]
    fn config_empty_json_defaults(_dummy in 0..10u8) {
        let cfg: LexicalBackendConfig = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(cfg, LexicalBackendConfig::default());
    }

    /// recorder_defaults() is a valid recorder path config.
    #[test]
    fn config_recorder_defaults_is_recorder_path(_dummy in 0..10u8) {
        let cfg = LexicalBackendConfig::recorder_defaults();
        prop_assert!(cfg.is_recorder_path());
        prop_assert!(!cfg.is_frankensearch_path());
    }

    /// frankensearch_defaults() is a valid frankensearch path config.
    #[test]
    fn config_frankensearch_defaults_is_frankensearch_path(_dummy in 0..10u8) {
        let cfg = LexicalBackendConfig::frankensearch_defaults();
        prop_assert!(cfg.is_frankensearch_path());
        prop_assert!(!cfg.is_recorder_path());
    }

    /// is_recorder_path and is_frankensearch_path are mutually consistent.
    #[test]
    fn config_path_detection_consistent(config in arb_config()) {
        // A config can be recorder path, frankensearch path, or neither
        // (e.g., Checkpoint + FrankenSearchV1) but never both.
        if config.is_recorder_path() {
            prop_assert!(!config.is_frankensearch_path());
        }
    }
}

// =============================================================================
// LexicalBackendMetrics rate functions
// =============================================================================

proptest! {
    /// compute_churn_rate is in [0.0, ...] (can exceed 1.0 if docs_expired > docs_ingested).
    #[test]
    fn churn_rate_non_negative(metrics in arb_metrics()) {
        let rate = compute_churn_rate(&metrics);
        prop_assert!(rate >= 0.0);
    }

    /// compute_churn_rate is 0 when docs_ingested is 0.
    #[test]
    fn churn_rate_zero_when_no_ingestion(docs_expired in 0..100_000u64) {
        let metrics = LexicalBackendMetrics {
            docs_expired,
            docs_ingested: 0,
            ..Default::default()
        };
        let rate = compute_churn_rate(&metrics);
        prop_assert!((rate - 0.0).abs() < f64::EPSILON);
    }

    /// compute_query_error_rate is in [0.0, 1.0].
    #[test]
    fn query_error_rate_bounded(
        queries in 0..100_000u64,
        errors in 0..100_000u64,
    ) {
        let capped_errors = errors.min(queries);
        let metrics = LexicalBackendMetrics {
            queries_executed: queries,
            query_errors: capped_errors,
            ..Default::default()
        };
        let rate = compute_query_error_rate(&metrics);
        prop_assert!(rate >= 0.0);
        prop_assert!(rate <= 1.0);
    }

    /// compute_query_error_rate is 0 when no queries.
    #[test]
    fn query_error_rate_zero_when_no_queries(errors in 0..100_000u64) {
        let metrics = LexicalBackendMetrics {
            queries_executed: 0,
            query_errors: errors,
            ..Default::default()
        };
        let rate = compute_query_error_rate(&metrics);
        prop_assert!((rate - 0.0).abs() < f64::EPSILON);
    }

    /// compute_rejection_rate is in [0.0, 1.0].
    #[test]
    fn rejection_rate_bounded(
        ingested in 0..100_000u64,
        rejected in 0..100_000u64,
    ) {
        let metrics = LexicalBackendMetrics {
            docs_ingested: ingested,
            docs_rejected: rejected,
            ..Default::default()
        };
        let rate = compute_rejection_rate(&metrics);
        prop_assert!(rate >= 0.0);
        prop_assert!(rate <= 1.0);
    }

    /// compute_rejection_rate is 0 when no ingest attempts.
    #[test]
    fn rejection_rate_zero_when_no_attempts(_dummy in 0..10u8) {
        let metrics = LexicalBackendMetrics {
            docs_ingested: 0,
            docs_rejected: 0,
            ..Default::default()
        };
        let rate = compute_rejection_rate(&metrics);
        prop_assert!((rate - 0.0).abs() < f64::EPSILON);
    }

    /// Metrics survive JSON roundtrip.
    #[test]
    fn metrics_serde_roundtrip(metrics in arb_metrics()) {
        let json = serde_json::to_string(&metrics).unwrap();
        let rt: LexicalBackendMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, metrics);
    }
}

// =============================================================================
// BridgeDocument properties
// =============================================================================

proptest! {
    /// BridgeDocument survives JSON roundtrip.
    #[test]
    fn bridge_document_serde_roundtrip(doc in arb_bridge_document()) {
        let json = serde_json::to_string(&doc).unwrap();
        let rt: BridgeDocument = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.doc_id, doc.doc_id);
        prop_assert_eq!(rt.text, doc.text);
        prop_assert_eq!(rt.source, doc.source);
        prop_assert_eq!(rt.captured_at_ms, doc.captured_at_ms);
        prop_assert_eq!(rt.pane_id, doc.pane_id);
        prop_assert_eq!(rt.session_id, doc.session_id);
        prop_assert_eq!(rt.content_hash, doc.content_hash);
    }

    /// BridgeDocument with metadata roundtrips (metadata is Vec<(String,String)>).
    #[test]
    fn bridge_document_metadata_roundtrip(
        doc_id in "[a-z]{1,10}",
        key in "[a-z]{1,5}",
        value in "[a-z]{1,10}",
    ) {
        let doc = BridgeDocument {
            doc_id,
            text: "test".to_string(),
            source: DocumentSource::Scrollback,
            captured_at_ms: 0,
            pane_id: None,
            session_id: None,
            content_hash: None,
            metadata: vec![(key.clone(), value.clone())],
        };
        let json = serde_json::to_string(&doc).unwrap();
        let rt: BridgeDocument = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.metadata.len(), 1);
        prop_assert_eq!(&rt.metadata[0].0, &key);
        prop_assert_eq!(&rt.metadata[0].1, &value);
    }
}
