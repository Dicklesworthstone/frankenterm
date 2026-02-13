//! Property-based tests for the `tantivy_ingest` module.
//!
//! Covers `IndexDocumentFields` serde roundtrips — the canonical flat
//! representation of a Tantivy document used for indexing and search.

use frankenterm_core::tantivy_ingest::IndexDocumentFields;
use proptest::prelude::*;

// =========================================================================
// Strategy — split into two groups to stay within 12-tuple limit
// =========================================================================

/// First half of the document fields.
fn arb_identity_fields() -> impl Strategy<Value = (
    String, String, String, u64,
    Option<String>, Option<String>, Option<String>,
    Option<String>, Option<String>, Option<String>,
)> {
    (
        "[a-z.]{3,10}",         // schema_version
        "[a-z.]{3,10}",         // lexical_schema_version
        "[a-f0-9]{8,16}",       // event_id
        0_u64..100_000,         // pane_id
        proptest::option::of("[a-f0-9]{8,16}"),  // session_id
        proptest::option::of("[a-f0-9]{8,16}"),  // workflow_id
        proptest::option::of("[a-f0-9]{8,16}"),  // correlation_id
        proptest::option::of("[a-f0-9]{8,16}"),  // parent_event_id
        proptest::option::of("[a-f0-9]{8,16}"),  // trigger_event_id
        proptest::option::of("[a-f0-9]{8,16}"),  // root_event_id
    )
}

/// Second half of the document fields.
fn arb_content_fields() -> impl Strategy<Value = (
    String, String,
    Option<String>, Option<String>, Option<String>, Option<String>,
    bool, Option<String>,
    i64, i64, u64, u64,
)> {
    (
        "[a-z_]{3,15}",         // source
        "[a-z_]{3,15}",         // event_type
        proptest::option::of("[a-z_]{3,10}"),  // ingress_kind
        proptest::option::of("[a-z_]{3,10}"),  // segment_kind
        proptest::option::of("[a-z_]{3,10}"),  // control_marker_type
        proptest::option::of("[a-z_]{3,10}"),  // lifecycle_phase
        any::<bool>(),          // is_gap
        proptest::option::of("[a-z_]{3,10}"),  // redaction
        0_i64..2_000_000_000_000,  // occurred_at_ms
        0_i64..2_000_000_000_000,  // recorded_at_ms
        0_u64..100_000,         // sequence
        0_u64..100_000,         // log_offset
    )
}

fn arb_index_document() -> impl Strategy<Value = IndexDocumentFields> {
    (arb_identity_fields(), arb_content_fields())
        .prop_map(|(identity, content)| {
            let (schema_version, lexical_schema_version, event_id, pane_id,
                 session_id, workflow_id, correlation_id,
                 parent_event_id, trigger_event_id, root_event_id) = identity;
            let (source, event_type, ingress_kind, segment_kind,
                 control_marker_type, lifecycle_phase, is_gap, redaction,
                 occurred_at_ms, recorded_at_ms, sequence, log_offset) = content;
            IndexDocumentFields {
                schema_version,
                lexical_schema_version,
                event_id,
                pane_id,
                session_id,
                workflow_id,
                correlation_id,
                parent_event_id,
                trigger_event_id,
                root_event_id,
                source,
                event_type,
                ingress_kind,
                segment_kind,
                control_marker_type,
                lifecycle_phase,
                is_gap,
                redaction,
                occurred_at_ms,
                recorded_at_ms,
                sequence,
                log_offset,
                text: String::new(),
                text_symbols: String::new(),
                details_json: "{}".to_string(),
            }
        })
}

// =========================================================================
// IndexDocumentFields — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// IndexDocumentFields serde roundtrip preserves all fields.
    #[test]
    fn prop_document_serde(doc in arb_index_document()) {
        let json = serde_json::to_string(&doc).unwrap();
        let back: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, doc);
    }

    /// IndexDocumentFields serde is deterministic.
    #[test]
    fn prop_document_deterministic(doc in arb_index_document()) {
        let j1 = serde_json::to_string(&doc).unwrap();
        let j2 = serde_json::to_string(&doc).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// event_id is always present in serialized JSON.
    #[test]
    fn prop_document_has_event_id(doc in arb_index_document()) {
        let json = serde_json::to_string(&doc).unwrap();
        prop_assert!(json.contains("\"event_id\""));
    }

    /// Optional fields with None roundtrip correctly.
    #[test]
    fn prop_document_none_fields(
        pane_id in 0_u64..100,
        event_id in "[a-f0-9]{8}",
    ) {
        let doc = IndexDocumentFields {
            schema_version: "v1".to_string(),
            lexical_schema_version: "v1".to_string(),
            event_id,
            pane_id,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
            source: "test".to_string(),
            event_type: "test".to_string(),
            ingress_kind: None,
            segment_kind: None,
            control_marker_type: None,
            lifecycle_phase: None,
            is_gap: false,
            redaction: None,
            occurred_at_ms: 0,
            recorded_at_ms: 0,
            sequence: 0,
            log_offset: 0,
            text: String::new(),
            text_symbols: String::new(),
            details_json: "{}".to_string(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        prop_assert!(back.session_id.is_none());
        prop_assert!(back.workflow_id.is_none());
        prop_assert!(back.ingress_kind.is_none());
    }

    /// Document with all Some fields roundtrips.
    #[test]
    fn prop_document_all_some(
        pane_id in 0_u64..100,
        event_id in "[a-f0-9]{8}",
        session_id in "[a-f0-9]{8}",
    ) {
        let doc = IndexDocumentFields {
            schema_version: "v1".to_string(),
            lexical_schema_version: "v1".to_string(),
            event_id,
            pane_id,
            session_id: Some(session_id),
            workflow_id: Some("wf1".into()),
            correlation_id: Some("corr1".into()),
            parent_event_id: Some("parent1".into()),
            trigger_event_id: Some("trigger1".into()),
            root_event_id: Some("root1".into()),
            source: "test".to_string(),
            event_type: "ingress_text".to_string(),
            ingress_kind: Some("send_text".into()),
            segment_kind: Some("delta".into()),
            control_marker_type: Some("prompt".into()),
            lifecycle_phase: Some("running".into()),
            is_gap: true,
            redaction: Some("full".into()),
            occurred_at_ms: 1_700_000_000_000,
            recorded_at_ms: 1_700_000_000_001,
            sequence: 42,
            log_offset: 1024,
            text: "hello world".to_string(),
            text_symbols: "hello.world".to_string(),
            details_json: "{\"key\":\"val\"}".to_string(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, doc);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn minimal_document_roundtrips() {
    let doc = IndexDocumentFields {
        schema_version: "v1".into(),
        lexical_schema_version: "v1".into(),
        event_id: "abc123".into(),
        pane_id: 1,
        session_id: None,
        workflow_id: None,
        correlation_id: None,
        parent_event_id: None,
        trigger_event_id: None,
        root_event_id: None,
        source: "test".into(),
        event_type: "ingress_text".into(),
        ingress_kind: Some("send_text".into()),
        segment_kind: None,
        control_marker_type: None,
        lifecycle_phase: None,
        is_gap: false,
        redaction: None,
        occurred_at_ms: 1_700_000_000_000,
        recorded_at_ms: 1_700_000_000_001,
        sequence: 42,
        log_offset: 1024,
        text: "hello world".into(),
        text_symbols: "hello.world".into(),
        details_json: "{}".into(),
    };
    let json = serde_json::to_string(&doc).unwrap();
    let back: IndexDocumentFields = serde_json::from_str(&json).unwrap();
    assert_eq!(back.event_id, "abc123");
    assert_eq!(back.pane_id, 1);
    assert_eq!(back.text, "hello world");
}
