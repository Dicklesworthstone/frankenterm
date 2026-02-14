//! Property-based tests for the `tantivy_ingest` module.
//!
//! Covers `IndexDocumentFields` serde roundtrips — the canonical flat
//! representation of a Tantivy document used for indexing and search.
//! Also covers `map_event_to_document` mapping correctness.

use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel, RecorderSegmentKind,
    RecorderTextEncoding,
};
use frankenterm_core::tantivy_ingest::{
    IndexDocumentFields, LEXICAL_SCHEMA_VERSION, map_event_to_document,
};
use proptest::prelude::*;

// =========================================================================
// Strategy — split into two groups to stay within 12-tuple limit
// =========================================================================

/// Identity fields tuple: (schema_version, lexical_schema_version, event_id,
/// pane_id, session_id, workflow_id, correlation_id, parent_event_id,
/// trigger_event_id, root_event_id).
type IdentityFields = (
    String,
    String,
    String,
    u64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Content fields tuple: (source, event_type, ingress_kind, segment_kind,
/// control_marker_type, lifecycle_phase, is_gap, redaction, occurred_at_ms,
/// recorded_at_ms, sequence, log_offset).
type ContentFields = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
    i64,
    i64,
    u64,
    u64,
);

/// First half of the document fields.
fn arb_identity_fields() -> impl Strategy<Value = IdentityFields> {
    (
        "[a-z.]{3,10}",                         // schema_version
        "[a-z.]{3,10}",                         // lexical_schema_version
        "[a-f0-9]{8,16}",                       // event_id
        0_u64..100_000,                         // pane_id
        proptest::option::of("[a-f0-9]{8,16}"), // session_id
        proptest::option::of("[a-f0-9]{8,16}"), // workflow_id
        proptest::option::of("[a-f0-9]{8,16}"), // correlation_id
        proptest::option::of("[a-f0-9]{8,16}"), // parent_event_id
        proptest::option::of("[a-f0-9]{8,16}"), // trigger_event_id
        proptest::option::of("[a-f0-9]{8,16}"), // root_event_id
    )
}

/// Second half of the document fields.
fn arb_content_fields() -> impl Strategy<Value = ContentFields> {
    (
        "[a-z_]{3,15}",                       // source
        "[a-z_]{3,15}",                       // event_type
        proptest::option::of("[a-z_]{3,10}"), // ingress_kind
        proptest::option::of("[a-z_]{3,10}"), // segment_kind
        proptest::option::of("[a-z_]{3,10}"), // control_marker_type
        proptest::option::of("[a-z_]{3,10}"), // lifecycle_phase
        any::<bool>(),                        // is_gap
        proptest::option::of("[a-z_]{3,10}"), // redaction
        0_i64..2_000_000_000_000,             // occurred_at_ms
        0_i64..2_000_000_000_000,             // recorded_at_ms
        0_u64..100_000,                       // sequence
        0_u64..100_000,                       // log_offset
    )
}

fn arb_index_document() -> impl Strategy<Value = IndexDocumentFields> {
    (arb_identity_fields(), arb_content_fields()).prop_map(|(identity, content)| {
        let (
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
        ) = identity;
        let (
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
        ) = content;
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
// Helper: build a RecorderEvent for map_event_to_document tests
// =========================================================================

fn make_recorder_event(
    pane_id: u64,
    sequence: u64,
    ts: u64,
    source: RecorderEventSource,
    payload: RecorderEventPayload,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("prop-{}-{}", pane_id, sequence),
        pane_id,
        session_id: Some("proptest-session".into()),
        workflow_id: None,
        correlation_id: None,
        source,
        occurred_at_ms: ts,
        recorded_at_ms: ts + 1,
        sequence,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload,
    }
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
// IndexDocumentFields — pretty-print & structural
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Pretty-printed JSON also roundtrips correctly.
    #[test]
    fn prop_document_pretty_serde(doc in arb_index_document()) {
        let json = serde_json::to_string_pretty(&doc).unwrap();
        let back: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, doc);
    }

    /// All required field names appear in serialized JSON.
    #[test]
    fn prop_document_required_fields_present(doc in arb_index_document()) {
        let json = serde_json::to_string(&doc).unwrap();
        prop_assert!(json.contains("\"schema_version\""), "missing schema_version");
        prop_assert!(json.contains("\"lexical_schema_version\""), "missing lexical_schema_version");
        prop_assert!(json.contains("\"event_id\""), "missing event_id");
        prop_assert!(json.contains("\"pane_id\""), "missing pane_id");
        prop_assert!(json.contains("\"source\""), "missing source");
        prop_assert!(json.contains("\"event_type\""), "missing event_type");
        prop_assert!(json.contains("\"is_gap\""), "missing is_gap");
        prop_assert!(json.contains("\"occurred_at_ms\""), "missing occurred_at_ms");
        prop_assert!(json.contains("\"recorded_at_ms\""), "missing recorded_at_ms");
        prop_assert!(json.contains("\"sequence\""), "missing sequence");
        prop_assert!(json.contains("\"log_offset\""), "missing log_offset");
        prop_assert!(json.contains("\"text\""), "missing text");
        prop_assert!(json.contains("\"details_json\""), "missing details_json");
    }

    /// Clone produces an equal document.
    #[test]
    fn prop_document_clone_eq(doc in arb_index_document()) {
        let cloned = doc.clone();
        prop_assert_eq!(cloned, doc);
    }

    /// Negative i64 timestamps roundtrip correctly.
    #[test]
    fn prop_document_negative_timestamps(
        occurred in -2_000_000_000_000_i64..0,
        recorded in -2_000_000_000_000_i64..0,
    ) {
        let doc = IndexDocumentFields {
            schema_version: "v1".into(),
            lexical_schema_version: "v1".into(),
            event_id: "neg-ts".into(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
            source: "test".into(),
            event_type: "test".into(),
            ingress_kind: None,
            segment_kind: None,
            control_marker_type: None,
            lifecycle_phase: None,
            is_gap: false,
            redaction: None,
            occurred_at_ms: occurred,
            recorded_at_ms: recorded,
            sequence: 0,
            log_offset: 0,
            text: String::new(),
            text_symbols: String::new(),
            details_json: "{}".into(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.occurred_at_ms, occurred);
        prop_assert_eq!(back.recorded_at_ms, recorded);
    }

    /// Documents with empty string fields roundtrip correctly.
    #[test]
    fn prop_document_empty_strings(pane_id in 0_u64..100) {
        let doc = IndexDocumentFields {
            schema_version: String::new(),
            lexical_schema_version: String::new(),
            event_id: String::new(),
            pane_id,
            session_id: Some(String::new()),
            workflow_id: None,
            correlation_id: None,
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
            source: String::new(),
            event_type: String::new(),
            ingress_kind: Some(String::new()),
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
            details_json: String::new(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, doc);
    }

    /// Large pane_id values (near u64::MAX) roundtrip.
    #[test]
    fn prop_document_large_pane_id(offset in 0_u64..1000) {
        let pane_id = u64::MAX - offset;
        let doc = IndexDocumentFields {
            schema_version: "v1".into(),
            lexical_schema_version: "v1".into(),
            event_id: "large-pane".into(),
            pane_id,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
            source: "test".into(),
            event_type: "test".into(),
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
            details_json: "{}".into(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, pane_id);
    }
}

// =========================================================================
// map_event_to_document — mapping correctness
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// map_event_to_document for IngressText sets correct event_type.
    #[test]
    fn prop_map_ingress_text(
        pane_id in 0_u64..1000,
        seq in 0_u64..1000,
        ts in 1_000_000_u64..2_000_000,
        text in "[a-zA-Z0-9 ]{0,30}",
        offset in 0_u64..100_000,
    ) {
        let event = make_recorder_event(
            pane_id, seq, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: text.clone(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let doc = map_event_to_document(&event, offset);
        prop_assert_eq!(&doc.event_type, "ingress_text");
        prop_assert_eq!(&doc.text, &text);
        prop_assert_eq!(&doc.text_symbols, &text, "text_symbols should mirror text");
        prop_assert!(doc.ingress_kind.is_some());
        prop_assert!(doc.segment_kind.is_none());
        prop_assert!(!doc.is_gap);
    }

    /// map_event_to_document for EgressOutput sets correct event_type and gap flag.
    #[test]
    fn prop_map_egress_output(
        pane_id in 0_u64..1000,
        seq in 0_u64..1000,
        ts in 1_000_000_u64..2_000_000,
        text in "[a-zA-Z0-9 ]{0,30}",
        is_gap in any::<bool>(),
        offset in 0_u64..100_000,
    ) {
        let event = make_recorder_event(
            pane_id, seq, ts,
            RecorderEventSource::WeztermMux,
            RecorderEventPayload::EgressOutput {
                text: text.clone(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap,
            },
        );
        let doc = map_event_to_document(&event, offset);
        prop_assert_eq!(&doc.event_type, "egress_output");
        prop_assert_eq!(doc.is_gap, is_gap);
        prop_assert!(doc.segment_kind.is_some());
        prop_assert!(doc.ingress_kind.is_none());
    }

    /// map_event_to_document always sets lexical_schema_version to LEXICAL_SCHEMA_VERSION.
    #[test]
    fn prop_map_sets_lexical_schema(
        pane_id in 0_u64..100,
        ts in 1_000_000_u64..2_000_000,
        offset in 0_u64..100_000,
    ) {
        let event = make_recorder_event(
            pane_id, 0, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "schema-test".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let doc = map_event_to_document(&event, offset);
        prop_assert_eq!(&doc.lexical_schema_version, LEXICAL_SCHEMA_VERSION);
    }

    /// map_event_to_document preserves pane_id, sequence, and log_offset.
    #[test]
    fn prop_map_preserves_identity(
        pane_id in 0_u64..100_000,
        seq in 0_u64..100_000,
        ts in 1_000_000_u64..2_000_000,
        offset in 0_u64..100_000,
    ) {
        let event = make_recorder_event(
            pane_id, seq, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "identity".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let doc = map_event_to_document(&event, offset);
        prop_assert_eq!(doc.pane_id, pane_id);
        prop_assert_eq!(doc.sequence, seq);
        prop_assert_eq!(doc.log_offset, offset);
        prop_assert_eq!(&doc.event_id, &event.event_id);
        prop_assert_eq!(doc.occurred_at_ms, ts as i64);
        prop_assert_eq!(doc.recorded_at_ms, (ts + 1) as i64);
    }

    /// map_event_to_document with Full redaction produces empty text.
    #[test]
    fn prop_map_full_redaction_clears_text(
        pane_id in 0_u64..100,
        ts in 1_000_000_u64..2_000_000,
        text in "[a-z]{5,20}",
    ) {
        let event = make_recorder_event(
            pane_id, 0, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text,
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::Full,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let doc = map_event_to_document(&event, 0);
        prop_assert!(doc.text.is_empty(), "Full redaction should clear text, got: {}", doc.text);
    }

    /// map_event_to_document with Partial redaction replaces text with [REDACTED].
    #[test]
    fn prop_map_partial_redaction(
        pane_id in 0_u64..100,
        ts in 1_000_000_u64..2_000_000,
        text in "[a-z]{5,20}",
    ) {
        let event = make_recorder_event(
            pane_id, 0, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text,
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::Partial,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let doc = map_event_to_document(&event, 0);
        prop_assert_eq!(&doc.text, "[REDACTED]");
    }

    /// Mapped document preserves causality fields from the source event.
    #[test]
    fn prop_map_preserves_causality(
        pane_id in 0_u64..100,
        ts in 1_000_000_u64..2_000_000,
        parent in "[a-f0-9]{8}",
        trigger in "[a-f0-9]{8}",
    ) {
        let mut event = make_recorder_event(
            pane_id, 0, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "causality".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        event.causality.parent_event_id = Some(parent.clone());
        event.causality.trigger_event_id = Some(trigger.clone());
        event.causality.root_event_id = None;

        let doc = map_event_to_document(&event, 0);
        prop_assert_eq!(doc.parent_event_id.as_deref(), Some(parent.as_str()));
        prop_assert_eq!(doc.trigger_event_id.as_deref(), Some(trigger.as_str()));
        prop_assert!(doc.root_event_id.is_none());
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
