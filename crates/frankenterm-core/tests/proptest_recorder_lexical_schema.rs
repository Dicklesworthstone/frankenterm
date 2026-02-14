#![cfg(feature = "recorder-lexical")]

//! Property-based tests for the `recorder_lexical_schema` module.
//!
//! Tests schema construction invariants, field handle correctness,
//! fingerprint stability/determinism, tokenizer registration,
//! and document conversion properties.

use frankenterm_core::recorder_lexical_schema::{
    LexicalFieldHandles, TOKENIZER_TERMINAL_SYMBOLS, TOKENIZER_TERMINAL_TEXT,
    build_lexical_schema_v1, fields_to_document, register_tokenizers, schema_fingerprint,
};
use frankenterm_core::tantivy_ingest::{IndexDocumentFields, LEXICAL_SCHEMA_VERSION};
use proptest::prelude::*;
use tantivy::{Document, Index};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_event_id() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z0-9\\-]{5,40}",
        Just("ev-test-1".to_string()),
        Just("".to_string()),
    ]
}

fn arb_optional_string() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        "[a-z0-9\\-]{1,30}".prop_map(Some),
    ]
}

fn arb_source() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("wezterm_mux".to_string()),
        Just("robot_mode".to_string()),
        Just("workflow_engine".to_string()),
        Just("operator_action".to_string()),
        Just("recovery_flow".to_string()),
    ]
}

fn arb_event_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("ingress_text".to_string()),
        Just("egress_output".to_string()),
        Just("control_marker".to_string()),
        Just("lifecycle_marker".to_string()),
    ]
}

fn arb_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[a-zA-Z0-9 _./:\\-]{0,200}",
        Just("echo hello world".to_string()),
        Just("cargo test --release".to_string()),
    ]
}

fn arb_document_fields() -> impl Strategy<Value = IndexDocumentFields> {
    (
        arb_event_id(),
        any::<u64>(),           // pane_id
        arb_optional_string(),  // session_id
        arb_optional_string(),  // workflow_id
        arb_optional_string(),  // correlation_id
        arb_source(),
        arb_event_type(),
        arb_optional_string(),  // parent_event_id
        arb_optional_string(),  // trigger_event_id
        arb_optional_string(),  // root_event_id
    )
        .prop_flat_map(
            |(
                event_id,
                pane_id,
                session_id,
                workflow_id,
                correlation_id,
                source,
                event_type,
                parent_event_id,
                trigger_event_id,
                root_event_id,
            )| {
                (
                    Just(event_id),
                    Just(pane_id),
                    Just(session_id),
                    Just(workflow_id),
                    Just(correlation_id),
                    Just(source),
                    Just(event_type),
                    Just(parent_event_id),
                    Just(trigger_event_id),
                    Just(root_event_id),
                    arb_optional_string(), // ingress_kind
                    arb_optional_string(), // segment_kind
                    arb_optional_string(), // control_marker_type
                    arb_optional_string(), // lifecycle_phase
                    any::<bool>(),         // is_gap
                    arb_optional_string(), // redaction
                    any::<i64>(),          // occurred_at_ms
                    any::<i64>(),          // recorded_at_ms
                    any::<u64>(),          // sequence
                    any::<u64>(),          // log_offset
                    arb_text(),            // text
                    arb_text(),            // text_symbols
                    Just("{}".to_string()), // details_json
                )
            },
        )
        .prop_map(
            |(
                event_id,
                pane_id,
                session_id,
                workflow_id,
                correlation_id,
                source,
                event_type,
                parent_event_id,
                trigger_event_id,
                root_event_id,
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
                text,
                text_symbols,
                details_json,
            )| {
                IndexDocumentFields {
                    schema_version: "ft.recorder.v1".to_string(),
                    lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
                    event_id,
                    pane_id,
                    session_id,
                    workflow_id,
                    correlation_id,
                    source,
                    event_type,
                    parent_event_id,
                    trigger_event_id,
                    root_event_id,
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
                    text,
                    text_symbols,
                    details_json,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// Schema structure properties
// ---------------------------------------------------------------------------

proptest! {
    /// Schema construction is deterministic: multiple builds produce identical schemas.
    #[test]
    fn schema_construction_deterministic(_seed in any::<u64>()) {
        let (schema1, handles1) = build_lexical_schema_v1();
        let (schema2, handles2) = build_lexical_schema_v1();

        // Field count must always be 25
        let fields1: Vec<_> = schema1.fields().collect();
        let fields2: Vec<_> = schema2.fields().collect();
        prop_assert_eq!(fields1.len(), 25);
        prop_assert_eq!(fields2.len(), 25);
        prop_assert_eq!(fields1.len(), fields2.len());

        // Handles must be identical
        prop_assert_eq!(handles1.event_id, handles2.event_id);
        prop_assert_eq!(handles1.pane_id, handles2.pane_id);
        prop_assert_eq!(handles1.text, handles2.text);
        prop_assert_eq!(handles1.details_json, handles2.details_json);
    }

    /// Field handles all resolve to valid fields in the schema.
    #[test]
    fn all_handles_resolve_to_fields(_seed in any::<u64>()) {
        let (schema, handles) = build_lexical_schema_v1();

        // Verify each named field exists in the schema
        let names = [
            "schema_version", "lexical_schema_version", "event_id",
            "pane_id", "session_id", "workflow_id", "correlation_id",
            "parent_event_id", "trigger_event_id", "root_event_id",
            "source", "event_type", "ingress_kind", "segment_kind",
            "control_marker_type", "lifecycle_phase", "is_gap", "redaction",
            "occurred_at_ms", "recorded_at_ms", "sequence", "log_offset",
            "text", "text_symbols", "details_json",
        ];

        for name in &names {
            let field = schema.get_field(name);
            prop_assert!(field.is_ok(), "field '{}' must exist", name);
        }

        // Verify handle fields match schema lookups
        prop_assert_eq!(handles.event_id, schema.get_field("event_id").unwrap());
        prop_assert_eq!(handles.pane_id, schema.get_field("pane_id").unwrap());
        prop_assert_eq!(handles.text, schema.get_field("text").unwrap());
        prop_assert_eq!(handles.is_gap, schema.get_field("is_gap").unwrap());
        prop_assert_eq!(handles.sequence, schema.get_field("sequence").unwrap());
    }
}

// ---------------------------------------------------------------------------
// Fingerprint properties
// ---------------------------------------------------------------------------

proptest! {
    /// Fingerprint is deterministic across repeated calls.
    #[test]
    fn fingerprint_deterministic(n in 2usize..=10) {
        let fingerprints: Vec<String> = (0..n)
            .map(|_| {
                let (schema, _) = build_lexical_schema_v1();
                schema_fingerprint(&schema)
            })
            .collect();

        for fp in &fingerprints[1..] {
            prop_assert_eq!(fp, &fingerprints[0]);
        }
    }

    /// Fingerprint is always a 64-character hex string (SHA-256).
    #[test]
    fn fingerprint_format(_seed in any::<u64>()) {
        let (schema, _) = build_lexical_schema_v1();
        let fp = schema_fingerprint(&schema);

        prop_assert_eq!(fp.len(), 64, "SHA-256 hex must be 64 chars, got {}", fp.len());
        prop_assert!(
            fp.chars().all(|c| c.is_ascii_hexdigit()),
            "fingerprint must be hex: {}", fp
        );
    }

    /// Fingerprint is lowercase hex.
    #[test]
    fn fingerprint_lowercase(_seed in any::<u64>()) {
        let (schema, _) = build_lexical_schema_v1();
        let fp = schema_fingerprint(&schema);
        prop_assert_eq!(fp, fp.to_lowercase());
    }
}

// ---------------------------------------------------------------------------
// Document conversion properties
// ---------------------------------------------------------------------------

proptest! {
    /// Any valid IndexDocumentFields can be converted to a TantivyDocument without panic.
    #[test]
    fn document_conversion_never_panics(fields in arb_document_fields()) {
        let (_schema, handles) = build_lexical_schema_v1();
        let _doc = fields_to_document(&fields, &handles);
        // Just verifying no panic
    }

    /// Converted documents contain the event_id field.
    #[test]
    fn document_contains_event_id(fields in arb_document_fields()) {
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);

        if !fields.event_id.is_empty() {
            prop_assert!(
                json.contains(&fields.event_id),
                "document JSON should contain event_id '{}': {}",
                fields.event_id,
                &json[..json.len().min(200)]
            );
        }
    }

    /// Converted documents preserve the pane_id field.
    #[test]
    fn document_preserves_pane_id(fields in arb_document_fields()) {
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);
        let parsed: serde_json::Value = serde_json::from_str(&json)
            .expect("document JSON should be valid");

        // pane_id is a u64 field, should appear in the JSON
        let pane_ids: Vec<_> = parsed["pane_id"].as_array()
            .expect("pane_id should be array")
            .iter()
            .filter_map(|v| v.as_u64())
            .collect();
        prop_assert!(pane_ids.contains(&fields.pane_id),
            "document should contain pane_id {}", fields.pane_id);
    }

    /// Converted documents preserve the is_gap boolean field.
    #[test]
    fn document_preserves_is_gap(fields in arb_document_fields()) {
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);
        let parsed: serde_json::Value = serde_json::from_str(&json)
            .expect("document JSON should be valid");

        let gap_values: Vec<_> = parsed["is_gap"].as_array()
            .expect("is_gap should be array")
            .iter()
            .filter_map(|v| v.as_bool())
            .collect();
        prop_assert!(gap_values.contains(&fields.is_gap),
            "document should contain is_gap={}", fields.is_gap);
    }

    /// Optional fields: when None, they should not appear in the document.
    #[test]
    fn optional_none_fields_omitted(
        pane_id in any::<u64>(),
        occurred_at_ms in any::<i64>(),
        recorded_at_ms in any::<i64>(),
        sequence in any::<u64>(),
        log_offset in any::<u64>(),
        is_gap in any::<bool>(),
    ) {
        let fields = IndexDocumentFields {
            schema_version: "ft.recorder.v1".to_string(),
            lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
            event_id: "ev-none-test".to_string(),
            pane_id,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: "test".to_string(),
            event_type: "test".to_string(),
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
            ingress_kind: None,
            segment_kind: None,
            control_marker_type: None,
            lifecycle_phase: None,
            is_gap,
            redaction: None,
            occurred_at_ms,
            recorded_at_ms,
            sequence,
            log_offset,
            text: "test".to_string(),
            text_symbols: "test".to_string(),
            details_json: "{}".to_string(),
        };

        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);

        // The required fields should still be present
        prop_assert!(json.contains("ev-none-test"));

        // Optional None fields should not add extra values
        // (Tantivy uses multi-valued fields so absent fields just have empty arrays)
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // session_id was None, so it should be an empty array
        let session_arr = parsed["session_id"].as_array();
        if let Some(arr) = session_arr {
            prop_assert!(arr.is_empty(), "session_id should be empty when None");
        }
    }
}

// ---------------------------------------------------------------------------
// Tokenizer registration properties
// ---------------------------------------------------------------------------

proptest! {
    /// Tokenizers can be registered without error on any fresh in-RAM index.
    #[test]
    fn tokenizer_registration_always_succeeds(_seed in any::<u64>()) {
        let (schema, _) = build_lexical_schema_v1();
        let index = Index::create_in_ram(schema);
        register_tokenizers(&index);

        let mgr = index.tokenizers();
        prop_assert!(mgr.get(TOKENIZER_TERMINAL_TEXT).is_some(),
            "terminal text tokenizer should be registered");
        prop_assert!(mgr.get(TOKENIZER_TERMINAL_SYMBOLS).is_some(),
            "terminal symbols tokenizer should be registered");
    }

    /// Re-registering tokenizers on the same index doesn't cause errors.
    #[test]
    fn tokenizer_reregistration_idempotent(n in 1usize..=5) {
        let (schema, _) = build_lexical_schema_v1();
        let index = Index::create_in_ram(schema);

        for _ in 0..n {
            register_tokenizers(&index);
        }

        let mgr = index.tokenizers();
        prop_assert!(mgr.get(TOKENIZER_TERMINAL_TEXT).is_some());
        prop_assert!(mgr.get(TOKENIZER_TERMINAL_SYMBOLS).is_some());
    }
}

// ---------------------------------------------------------------------------
// Index roundtrip properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// Documents can be indexed and the index doc count matches.
    #[test]
    fn index_roundtrip_preserves_doc_count(
        fields_list in prop::collection::vec(arb_document_fields(), 1..=5)
    ) {
        let (schema, handles) = build_lexical_schema_v1();
        let index = Index::create_in_ram(schema);
        register_tokenizers(&index);

        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();

        let expected_count = fields_list.len();
        for fields in &fields_list {
            let doc = fields_to_document(fields, &handles);
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        prop_assert_eq!(
            searcher.num_docs() as usize,
            expected_count,
            "indexed doc count should match input"
        );
    }
}

// ---------------------------------------------------------------------------
// Schema version constant properties
// ---------------------------------------------------------------------------

proptest! {
    /// LEXICAL_SCHEMA_VERSION is non-empty.
    #[test]
    fn schema_version_non_empty(_seed in any::<u64>()) {
        prop_assert!(!LEXICAL_SCHEMA_VERSION.is_empty());
    }

    /// Tokenizer names are non-empty and contain version suffixes.
    #[test]
    fn tokenizer_names_have_versions(_seed in any::<u64>()) {
        prop_assert!(TOKENIZER_TERMINAL_TEXT.contains("v1"));
        prop_assert!(TOKENIZER_TERMINAL_SYMBOLS.contains("v1"));
        prop_assert!(TOKENIZER_TERMINAL_TEXT.starts_with("ft_"));
        prop_assert!(TOKENIZER_TERMINAL_SYMBOLS.starts_with("ft_"));
    }
}
