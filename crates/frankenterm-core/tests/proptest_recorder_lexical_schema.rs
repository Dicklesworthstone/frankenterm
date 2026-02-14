#![cfg(feature = "recorder-lexical")]

//! Property-based tests for the `recorder_lexical_schema` module.
//!
//! Tests schema construction invariants, field handle correctness,
//! fingerprint stability/determinism, tokenizer registration,
//! and document conversion properties.

use frankenterm_core::recorder_lexical_schema::{
    TOKENIZER_TERMINAL_SYMBOLS, TOKENIZER_TERMINAL_TEXT, build_lexical_schema_v1,
    fields_to_document, register_tokenizers, schema_fingerprint,
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
    prop_oneof![Just(None), "[a-z0-9\\-]{1,30}".prop_map(Some),]
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
    // Split into two groups to stay within proptest's 12-element tuple limit.
    let group_a = (
        arb_event_id(),
        any::<u64>(),          // pane_id
        arb_optional_string(), // session_id
        arb_optional_string(), // workflow_id
        arb_optional_string(), // correlation_id
        arb_source(),
        arb_event_type(),
        arb_optional_string(), // parent_event_id
        arb_optional_string(), // trigger_event_id
        arb_optional_string(), // root_event_id
    );
    let group_b = (
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
    );
    (group_a, group_b).prop_map(
        |(
            (
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
            ),
            (
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
            ),
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
                details_json: "{}".to_string(),
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
        let lower = fp.to_lowercase();
        prop_assert_eq!(fp, lower);
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

// ---------------------------------------------------------------------------
// LexicalFieldHandles: Clone, Copy, Debug
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// LexicalFieldHandles Copy produces identical handles.
    #[test]
    fn field_handles_copy_identical(_seed in any::<u64>()) {
        let (_schema, handles) = build_lexical_schema_v1();
        let copied = handles;
        prop_assert_eq!(handles.event_id, copied.event_id);
        prop_assert_eq!(handles.pane_id, copied.pane_id);
        prop_assert_eq!(handles.text, copied.text);
        prop_assert_eq!(handles.sequence, copied.sequence);
        prop_assert_eq!(handles.is_gap, copied.is_gap);
        prop_assert_eq!(handles.details_json, copied.details_json);
        prop_assert_eq!(handles.source, copied.source);
        prop_assert_eq!(handles.event_type, copied.event_type);
    }

    /// LexicalFieldHandles Clone produces identical handles.
    #[test]
    fn field_handles_clone_identical(_seed in any::<u64>()) {
        let (_schema, handles) = build_lexical_schema_v1();
        let cloned = handles.clone();
        prop_assert_eq!(handles.event_id, cloned.event_id);
        prop_assert_eq!(handles.pane_id, cloned.pane_id);
        prop_assert_eq!(handles.text, cloned.text);
        prop_assert_eq!(handles.occurred_at_ms, cloned.occurred_at_ms);
        prop_assert_eq!(handles.recorded_at_ms, cloned.recorded_at_ms);
    }

    /// LexicalFieldHandles Debug is non-empty.
    #[test]
    fn field_handles_debug_non_empty(_seed in any::<u64>()) {
        let (_schema, handles) = build_lexical_schema_v1();
        let debug = format!("{:?}", handles);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("LexicalFieldHandles"));
    }
}

// ---------------------------------------------------------------------------
// Document conversion: additional field preservation
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Converted documents always produce valid JSON.
    #[test]
    fn document_json_is_valid(fields in arb_document_fields()) {
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&json);
        prop_assert!(parsed.is_ok(), "document JSON should be valid: {}", &json[..json.len().min(200)]);
    }

    /// Converted documents preserve the source field.
    #[test]
    fn document_preserves_source(fields in arb_document_fields()) {
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);
        prop_assert!(
            json.contains(&fields.source),
            "document should contain source '{}': {}",
            fields.source,
            &json[..json.len().min(200)]
        );
    }

    /// Converted documents preserve the event_type field.
    #[test]
    fn document_preserves_event_type(fields in arb_document_fields()) {
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);
        prop_assert!(
            json.contains(&fields.event_type),
            "document should contain event_type '{}': {}",
            fields.event_type,
            &json[..json.len().min(200)]
        );
    }

    /// Converted documents preserve the sequence field.
    #[test]
    fn document_preserves_sequence(fields in arb_document_fields()) {
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let seq_values: Vec<_> = parsed["sequence"].as_array()
            .expect("sequence should be array")
            .iter()
            .filter_map(|v| v.as_u64())
            .collect();
        prop_assert!(seq_values.contains(&fields.sequence),
            "document should contain sequence {}", fields.sequence);
    }

    /// Converted documents preserve the schema_version field.
    #[test]
    fn document_preserves_schema_version(fields in arb_document_fields()) {
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);
        prop_assert!(json.contains(&fields.schema_version),
            "document should contain schema_version");
    }

    /// Documents with all optional fields set are valid.
    #[test]
    fn document_with_all_optional_fields(
        pane_id in any::<u64>(),
        ts in any::<i64>(),
        seq in any::<u64>(),
    ) {
        let fields = IndexDocumentFields {
            schema_version: "ft.recorder.v1".to_string(),
            lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
            event_id: "ev-all-fields".to_string(),
            pane_id,
            session_id: Some("sess-1".to_string()),
            workflow_id: Some("wf-1".to_string()),
            correlation_id: Some("corr-1".to_string()),
            source: "robot_mode".to_string(),
            event_type: "ingress_text".to_string(),
            parent_event_id: Some("ev-parent".to_string()),
            trigger_event_id: Some("ev-trigger".to_string()),
            root_event_id: Some("ev-root".to_string()),
            ingress_kind: Some("send_text".to_string()),
            segment_kind: Some("delta".to_string()),
            control_marker_type: Some("resize".to_string()),
            lifecycle_phase: Some("capture_started".to_string()),
            is_gap: false,
            redaction: Some("none".to_string()),
            occurred_at_ms: ts,
            recorded_at_ms: ts + 1,
            sequence: seq,
            log_offset: 0,
            text: "hello world".to_string(),
            text_symbols: "hello world".to_string(),
            details_json: "{}".to_string(),
        };

        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&fields, &handles);
        let json = doc.to_json(&schema);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // All optional fields should be present as non-empty arrays
        let session_arr = parsed["session_id"].as_array().unwrap();
        prop_assert!(!session_arr.is_empty(), "session_id should have values");
        let workflow_arr = parsed["workflow_id"].as_array().unwrap();
        prop_assert!(!workflow_arr.is_empty(), "workflow_id should have values");
    }
}

// ---------------------------------------------------------------------------
// Schema: field name coverage
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Schema has exactly 25 fields.
    #[test]
    fn schema_has_25_fields(_seed in any::<u64>()) {
        let (schema, _) = build_lexical_schema_v1();
        let field_count = schema.fields().count();
        prop_assert_eq!(field_count, 25, "expected 25 fields, got {}", field_count);
    }

    /// All causality fields exist in the schema.
    #[test]
    fn schema_has_causality_fields(_seed in any::<u64>()) {
        let (schema, handles) = build_lexical_schema_v1();
        prop_assert_eq!(handles.parent_event_id, schema.get_field("parent_event_id").unwrap());
        prop_assert_eq!(handles.trigger_event_id, schema.get_field("trigger_event_id").unwrap());
        prop_assert_eq!(handles.root_event_id, schema.get_field("root_event_id").unwrap());
    }

    /// All timestamp/ordering fields exist in the schema.
    #[test]
    fn schema_has_ordering_fields(_seed in any::<u64>()) {
        let (schema, handles) = build_lexical_schema_v1();
        prop_assert_eq!(handles.occurred_at_ms, schema.get_field("occurred_at_ms").unwrap());
        prop_assert_eq!(handles.recorded_at_ms, schema.get_field("recorded_at_ms").unwrap());
        prop_assert_eq!(handles.sequence, schema.get_field("sequence").unwrap());
        prop_assert_eq!(handles.log_offset, schema.get_field("log_offset").unwrap());
    }

    /// Fingerprint is non-empty.
    #[test]
    fn fingerprint_non_empty(_seed in any::<u64>()) {
        let (schema, _) = build_lexical_schema_v1();
        let fp = schema_fingerprint(&schema);
        prop_assert!(!fp.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Index: multiple document roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Index with varying doc counts has correct count.
    #[test]
    fn index_varying_doc_counts(
        count in 1usize..=10,
    ) {
        let (schema, handles) = build_lexical_schema_v1();
        let index = Index::create_in_ram(schema);
        register_tokenizers(&index);

        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();

        for i in 0..count {
            let fields = IndexDocumentFields {
                schema_version: "ft.recorder.v1".to_string(),
                lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
                event_id: format!("ev-{}", i),
                pane_id: i as u64,
                session_id: None,
                workflow_id: None,
                correlation_id: None,
                source: "robot_mode".to_string(),
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
                occurred_at_ms: 1000 + i as i64,
                recorded_at_ms: 1001 + i as i64,
                sequence: i as u64,
                log_offset: i as u64,
                text: format!("text {}", i),
                text_symbols: format!("sym {}", i),
                details_json: "{}".to_string(),
            };
            let doc = fields_to_document(&fields, &handles);
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        prop_assert_eq!(searcher.num_docs() as usize, count);
    }
}
