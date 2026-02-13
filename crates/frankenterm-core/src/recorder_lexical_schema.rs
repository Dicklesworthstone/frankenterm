//! Tantivy schema definition for the `ft.recorder.lexical.v1` index.
//!
//! Bead: wa-oegrb.4.2
//!
//! Provides the concrete Tantivy schema, compile-time field handles,
//! custom tokenizer registration, document conversion from
//! [`IndexDocumentFields`], and a deterministic schema fingerprint for
//! rebuild detection.

use sha2::{Digest, Sha256};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED, STORED,
    STRING,
};
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, RegexTokenizer, RemoveLongFilter, TextAnalyzer};
use tantivy::{Index, TantivyDocument};

use crate::tantivy_ingest::{IndexDocumentFields, LEXICAL_SCHEMA_VERSION};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Custom tokenizer for terminal text: splits on word-like runs, lowercases,
/// folds accented characters, and removes excessively long tokens.
pub const TOKENIZER_TERMINAL_TEXT: &str = "ft_terminal_text_v1";

/// Custom tokenizer for terminal symbols: same regex split + lowercase,
/// but no ASCII folding or length filter.
pub const TOKENIZER_TERMINAL_SYMBOLS: &str = "ft_terminal_symbols_v1";

/// Regex pattern shared by both terminal tokenizers.
/// Matches runs of alphanumeric characters plus common path/URL punctuation.
const TERMINAL_TOKEN_PATTERN: &str = r"[A-Za-z0-9_./:\-]+";

/// Maximum token length (bytes) for the text tokenizer.
const MAX_TOKEN_LENGTH: usize = 256;

// ---------------------------------------------------------------------------
// LexicalFieldHandles — compile-time-safe field accessors
// ---------------------------------------------------------------------------

/// All 25 Tantivy [`Field`] handles for the `ft.recorder.lexical.v1` schema.
///
/// [`Field`] is `Copy`, so this struct is cheap to clone and pass around.
#[derive(Debug, Clone, Copy)]
pub struct LexicalFieldHandles {
    // --- identity (3 STRING) ---
    pub schema_version: Field,
    pub lexical_schema_version: Field,
    pub event_id: Field,

    // --- pane/session (4: 1 u64 + 3 STRING) ---
    pub pane_id: Field,
    pub session_id: Field,
    pub workflow_id: Field,
    pub correlation_id: Field,

    // --- causality (3 STRING) ---
    pub parent_event_id: Field,
    pub trigger_event_id: Field,
    pub root_event_id: Field,

    // --- source/type (2 STRING) ---
    pub source: Field,
    pub event_type: Field,

    // --- variant-specific (5 STRING + 1 bool) ---
    pub ingress_kind: Field,
    pub segment_kind: Field,
    pub control_marker_type: Field,
    pub lifecycle_phase: Field,
    pub is_gap: Field,
    pub redaction: Field,

    // --- timestamps/ordering (4 numeric) ---
    pub occurred_at_ms: Field,
    pub recorded_at_ms: Field,
    pub sequence: Field,
    pub log_offset: Field,

    // --- text (2 TEXT) ---
    pub text: Field,
    pub text_symbols: Field,

    // --- details (1 stored-only) ---
    pub details_json: Field,
}

// ---------------------------------------------------------------------------
// Schema builder
// ---------------------------------------------------------------------------

/// Construct the `ft.recorder.lexical.v1` Tantivy schema and field handles.
///
/// Field layout:
/// - 16 STRING exact-term fields (raw tokenizer, indexed + stored)
/// - 5 numeric fields (INDEXED | STORED | FAST)
/// - 1 bool field (INDEXED | STORED | FAST)
/// - 2 TEXT fields with custom tokenizers
/// - 1 stored-only TEXT field (details_json)
///
/// Total: 25 fields.
pub fn build_lexical_schema_v1() -> (Schema, LexicalFieldHandles) {
    let mut b = Schema::builder();

    // -- 16 STRING exact-term fields --
    let schema_version = b.add_text_field("schema_version", STRING | STORED);
    let lexical_schema_version = b.add_text_field("lexical_schema_version", STRING | STORED);
    let event_id = b.add_text_field("event_id", STRING | STORED);
    let session_id = b.add_text_field("session_id", STRING | STORED);
    let workflow_id = b.add_text_field("workflow_id", STRING | STORED);
    let correlation_id = b.add_text_field("correlation_id", STRING | STORED);
    let parent_event_id = b.add_text_field("parent_event_id", STRING | STORED);
    let trigger_event_id = b.add_text_field("trigger_event_id", STRING | STORED);
    let root_event_id = b.add_text_field("root_event_id", STRING | STORED);
    let source = b.add_text_field("source", STRING | STORED);
    let event_type = b.add_text_field("event_type", STRING | STORED);
    let ingress_kind = b.add_text_field("ingress_kind", STRING | STORED);
    let segment_kind = b.add_text_field("segment_kind", STRING | STORED);
    let control_marker_type = b.add_text_field("control_marker_type", STRING | STORED);
    let lifecycle_phase = b.add_text_field("lifecycle_phase", STRING | STORED);
    let redaction = b.add_text_field("redaction", STRING | STORED);

    // -- 5 numeric fields --
    let pane_id = b.add_u64_field("pane_id", INDEXED | STORED | FAST);
    let occurred_at_ms = b.add_i64_field("occurred_at_ms", INDEXED | STORED | FAST);
    let recorded_at_ms = b.add_i64_field("recorded_at_ms", INDEXED | STORED | FAST);
    let sequence = b.add_u64_field("sequence", INDEXED | STORED | FAST);
    let log_offset = b.add_u64_field("log_offset", INDEXED | STORED | FAST);

    // -- 1 bool field --
    let is_gap = b.add_bool_field("is_gap", INDEXED | STORED | FAST);

    // -- 2 TEXT fields with custom tokenizers --
    let text_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER_TERMINAL_TEXT)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let text = b.add_text_field("text", text_options);

    let symbols_options = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER_TERMINAL_SYMBOLS)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    // text_symbols is NOT stored (per schema contract)
    let text_symbols = b.add_text_field("text_symbols", symbols_options);

    // -- 1 stored-only TEXT field --
    let details_json = b.add_text_field("details_json", STORED);

    let schema = b.build();
    let handles = LexicalFieldHandles {
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
        text,
        text_symbols,
        details_json,
    };

    (schema, handles)
}

// ---------------------------------------------------------------------------
// Tokenizer registration
// ---------------------------------------------------------------------------

/// Register the custom terminal tokenizers on the given [`Index`].
///
/// Must be called after opening or creating the index and before any
/// indexing or querying operations.
pub fn register_tokenizers(index: &Index) {
    // ft_terminal_text_v1: regex split → lowercase → ascii fold → remove long
    let text_analyzer = TextAnalyzer::builder(
        RegexTokenizer::new(TERMINAL_TOKEN_PATTERN).expect("hardcoded regex is valid"),
    )
    .filter(LowerCaser)
    .filter(AsciiFoldingFilter)
    .filter(RemoveLongFilter::limit(MAX_TOKEN_LENGTH))
    .build();

    index
        .tokenizers()
        .register(TOKENIZER_TERMINAL_TEXT, text_analyzer);

    // ft_terminal_symbols_v1: regex split → lowercase (no folding or length limit)
    let symbols_analyzer = TextAnalyzer::builder(
        RegexTokenizer::new(TERMINAL_TOKEN_PATTERN).expect("hardcoded regex is valid"),
    )
    .filter(LowerCaser)
    .build();

    index
        .tokenizers()
        .register(TOKENIZER_TERMINAL_SYMBOLS, symbols_analyzer);
}

// ---------------------------------------------------------------------------
// Document conversion
// ---------------------------------------------------------------------------

/// Convert an [`IndexDocumentFields`] struct into a Tantivy [`TantivyDocument`].
///
/// Optional fields (`Option<String>`) are only added when `Some`, so queries
/// on absent fields simply won't match (correct exact-term semantics).
pub fn fields_to_document(
    fields: &IndexDocumentFields,
    handles: &LexicalFieldHandles,
) -> TantivyDocument {
    let mut doc = TantivyDocument::default();

    // Identity
    doc.add_text(handles.schema_version, &fields.schema_version);
    doc.add_text(
        handles.lexical_schema_version,
        &fields.lexical_schema_version,
    );
    doc.add_text(handles.event_id, &fields.event_id);

    // Pane/session
    doc.add_u64(handles.pane_id, fields.pane_id);
    if let Some(ref v) = fields.session_id {
        doc.add_text(handles.session_id, v);
    }
    if let Some(ref v) = fields.workflow_id {
        doc.add_text(handles.workflow_id, v);
    }
    if let Some(ref v) = fields.correlation_id {
        doc.add_text(handles.correlation_id, v);
    }

    // Causality
    if let Some(ref v) = fields.parent_event_id {
        doc.add_text(handles.parent_event_id, v);
    }
    if let Some(ref v) = fields.trigger_event_id {
        doc.add_text(handles.trigger_event_id, v);
    }
    if let Some(ref v) = fields.root_event_id {
        doc.add_text(handles.root_event_id, v);
    }

    // Source/type
    doc.add_text(handles.source, &fields.source);
    doc.add_text(handles.event_type, &fields.event_type);

    // Variant-specific
    if let Some(ref v) = fields.ingress_kind {
        doc.add_text(handles.ingress_kind, v);
    }
    if let Some(ref v) = fields.segment_kind {
        doc.add_text(handles.segment_kind, v);
    }
    if let Some(ref v) = fields.control_marker_type {
        doc.add_text(handles.control_marker_type, v);
    }
    if let Some(ref v) = fields.lifecycle_phase {
        doc.add_text(handles.lifecycle_phase, v);
    }
    doc.add_bool(handles.is_gap, fields.is_gap);
    if let Some(ref v) = fields.redaction {
        doc.add_text(handles.redaction, v);
    }

    // Timestamps/ordering
    doc.add_i64(handles.occurred_at_ms, fields.occurred_at_ms);
    doc.add_i64(handles.recorded_at_ms, fields.recorded_at_ms);
    doc.add_u64(handles.sequence, fields.sequence);
    doc.add_u64(handles.log_offset, fields.log_offset);

    // Text
    doc.add_text(handles.text, &fields.text);
    doc.add_text(handles.text_symbols, &fields.text_symbols);

    // Details
    doc.add_text(handles.details_json, &fields.details_json);

    doc
}

// ---------------------------------------------------------------------------
// Schema fingerprint
// ---------------------------------------------------------------------------

/// Compute a deterministic SHA-256 fingerprint over the schema and tokenizer
/// configuration strings.
///
/// Used for rebuild detection: if the fingerprint changes between runs, a
/// full reindex is needed.
pub fn schema_fingerprint(schema: &Schema) -> String {
    let schema_json = serde_json::to_string(schema).unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(schema_json.as_bytes());
    // Include tokenizer config so changes to tokenizer pipelines also trigger reindex.
    hasher.update(LEXICAL_SCHEMA_VERSION.as_bytes());
    hasher.update(TOKENIZER_TERMINAL_TEXT.as_bytes());
    hasher.update(TERMINAL_TOKEN_PATTERN.as_bytes());
    hasher.update(b"LowerCaser|AsciiFoldingFilter|RemoveLongFilter(256)");
    hasher.update(TOKENIZER_TERMINAL_SYMBOLS.as_bytes());
    hasher.update(TERMINAL_TOKEN_PATTERN.as_bytes());
    hasher.update(b"LowerCaser");

    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::Document;
    use crate::recording::{
        RecorderControlMarkerType, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
        RecorderEventSource, RecorderIngressKind, RecorderLifecyclePhase, RecorderRedactionLevel,
        RecorderSegmentKind, RecorderTextEncoding, RECORDER_EVENT_SCHEMA_VERSION_V1,
    };
    use crate::tantivy_ingest::map_event_to_document;

    fn sample_ingress_event() -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "ev-ingress-1".to_string(),
            pane_id: 42,
            session_id: Some("sess-1".to_string()),
            workflow_id: Some("wf-1".to_string()),
            correlation_id: Some("corr-1".to_string()),
            source: RecorderEventSource::RobotMode,
            occurred_at_ms: 1_700_000_000_000,
            recorded_at_ms: 1_700_000_000_001,
            sequence: 7,
            causality: RecorderEventCausality {
                parent_event_id: Some("parent-1".to_string()),
                trigger_event_id: None,
                root_event_id: Some("root-1".to_string()),
            },
            payload: RecorderEventPayload::IngressText {
                text: "echo hello world".to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        }
    }

    fn sample_egress_event() -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "ev-egress-1".to_string(),
            pane_id: 10,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1_700_000_000_100,
            recorded_at_ms: 1_700_000_000_101,
            sequence: 3,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::EgressOutput {
                text: "/usr/local/bin/cargo test --release".to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        }
    }

    fn sample_control_event() -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "ev-ctrl-1".to_string(),
            pane_id: 5,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1_700_000_000_200,
            recorded_at_ms: 1_700_000_000_201,
            sequence: 10,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PromptBoundary,
                details: serde_json::json!({"cols": 80, "rows": 24}),
            },
        }
    }

    fn sample_lifecycle_event() -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "ev-lc-1".to_string(),
            pane_id: 8,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1_700_000_000_300,
            recorded_at_ms: 1_700_000_000_301,
            sequence: 20,
            causality: RecorderEventCausality {
                parent_event_id: Some("parent-lc".to_string()),
                trigger_event_id: None,
                root_event_id: Some("root-lc".to_string()),
            },
            payload: RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: RecorderLifecyclePhase::PaneOpened,
                reason: Some("user action".to_string()),
                details: serde_json::json!({}),
            },
        }
    }

    // =========================================================================
    // Schema structure tests
    // =========================================================================

    #[test]
    fn schema_has_25_fields() {
        let (schema, _handles) = build_lexical_schema_v1();
        let fields: Vec<_> = schema.fields().collect();
        assert_eq!(fields.len(), 25, "schema must have exactly 25 fields");
    }

    #[test]
    fn schema_field_names_present() {
        let (schema, _) = build_lexical_schema_v1();
        let expected_names = [
            "schema_version",
            "lexical_schema_version",
            "event_id",
            "pane_id",
            "session_id",
            "workflow_id",
            "correlation_id",
            "parent_event_id",
            "trigger_event_id",
            "root_event_id",
            "source",
            "event_type",
            "ingress_kind",
            "segment_kind",
            "control_marker_type",
            "lifecycle_phase",
            "is_gap",
            "redaction",
            "occurred_at_ms",
            "recorded_at_ms",
            "sequence",
            "log_offset",
            "text",
            "text_symbols",
            "details_json",
        ];
        for name in &expected_names {
            assert!(
                schema.get_field(name).is_ok(),
                "field '{}' must exist in schema",
                name
            );
        }
    }

    #[test]
    fn field_handles_match_schema_lookup() {
        let (schema, handles) = build_lexical_schema_v1();
        assert_eq!(handles.event_id, schema.get_field("event_id").unwrap());
        assert_eq!(handles.pane_id, schema.get_field("pane_id").unwrap());
        assert_eq!(handles.text, schema.get_field("text").unwrap());
        assert_eq!(
            handles.details_json,
            schema.get_field("details_json").unwrap()
        );
        assert_eq!(handles.is_gap, schema.get_field("is_gap").unwrap());
    }

    // =========================================================================
    // Document conversion tests — all 4 payload variants
    // =========================================================================

    #[test]
    fn convert_ingress_event_to_document() {
        let event = sample_ingress_event();
        let doc_fields = map_event_to_document(&event, 100);
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&doc_fields, &handles);

        // Use Tantivy's to_json (TantivyDocument doesn't impl serde::Serialize)
        let json = doc.to_json(&schema);
        assert!(json.contains("ev-ingress-1"));
        assert!(json.contains("echo hello world"));
    }

    #[test]
    fn convert_egress_event_to_document() {
        let event = sample_egress_event();
        let doc_fields = map_event_to_document(&event, 200);
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&doc_fields, &handles);

        let json = doc.to_json(&schema);
        assert!(json.contains("ev-egress-1"));
        assert!(json.contains("cargo test"));
    }

    #[test]
    fn convert_control_event_to_document() {
        let event = sample_control_event();
        let doc_fields = map_event_to_document(&event, 300);
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&doc_fields, &handles);

        let json = doc.to_json(&schema);
        assert!(json.contains("ev-ctrl-1"));
        assert!(json.contains("cols"));
    }

    #[test]
    fn convert_lifecycle_event_to_document() {
        let event = sample_lifecycle_event();
        let doc_fields = map_event_to_document(&event, 400);
        let (schema, handles) = build_lexical_schema_v1();
        let doc = fields_to_document(&doc_fields, &handles);

        let json = doc.to_json(&schema);
        assert!(json.contains("ev-lc-1"));
        assert!(json.contains("user action"));
    }

    // =========================================================================
    // Optional field omission
    // =========================================================================

    #[test]
    fn optional_fields_omitted_when_none() {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "ev-minimal".to_string(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::OperatorAction,
            occurred_at_ms: 1000,
            recorded_at_ms: 1001,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: "test".to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        };

        let doc_fields = map_event_to_document(&event, 0);
        assert!(doc_fields.session_id.is_none());
        assert!(doc_fields.workflow_id.is_none());
        assert!(doc_fields.correlation_id.is_none());
        assert!(doc_fields.parent_event_id.is_none());
        assert!(doc_fields.trigger_event_id.is_none());
        assert!(doc_fields.root_event_id.is_none());

        // Document should still be created without errors
        let (_schema, handles) = build_lexical_schema_v1();
        let _doc = fields_to_document(&doc_fields, &handles);
    }

    // =========================================================================
    // Fingerprint stability
    // =========================================================================

    #[test]
    fn fingerprint_is_deterministic() {
        let (schema1, _) = build_lexical_schema_v1();
        let (schema2, _) = build_lexical_schema_v1();
        assert_eq!(schema_fingerprint(&schema1), schema_fingerprint(&schema2));
    }

    #[test]
    fn fingerprint_is_hex_sha256() {
        let (schema, _) = build_lexical_schema_v1();
        let fp = schema_fingerprint(&schema);
        // SHA-256 produces 32 bytes = 64 hex chars
        assert_eq!(fp.len(), 64);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // =========================================================================
    // Tokenizer registration (requires temp index)
    // =========================================================================

    #[test]
    fn tokenizers_register_without_error() {
        let (schema, _) = build_lexical_schema_v1();
        let index = Index::create_in_ram(schema);
        register_tokenizers(&index);
        // Verify tokenizers are registered by name
        let tokenizer_mgr = index.tokenizers();
        assert!(tokenizer_mgr.get(TOKENIZER_TERMINAL_TEXT).is_some());
        assert!(tokenizer_mgr.get(TOKENIZER_TERMINAL_SYMBOLS).is_some());
    }

    // =========================================================================
    // End-to-end: index a document and read it back
    // =========================================================================

    #[test]
    fn roundtrip_index_and_read_document() {
        let (schema, handles) = build_lexical_schema_v1();
        let index = Index::create_in_ram(schema.clone());
        register_tokenizers(&index);

        let event = sample_ingress_event();
        let doc_fields = map_event_to_document(&event, 42);
        let doc = fields_to_document(&doc_fields, &handles);

        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 1);
    }

    #[test]
    fn enum_coverage_all_sources() {
        // Verify format_source in tantivy_ingest covers all variants
        // by constructing events with each source and converting them.
        let (_schema, handles) = build_lexical_schema_v1();
        let sources = [
            RecorderEventSource::WeztermMux,
            RecorderEventSource::RobotMode,
            RecorderEventSource::WorkflowEngine,
            RecorderEventSource::OperatorAction,
            RecorderEventSource::RecoveryFlow,
        ];
        for source in sources {
            let event = RecorderEvent {
                schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                event_id: format!("ev-{:?}", source),
                pane_id: 1,
                session_id: None,
                workflow_id: None,
                correlation_id: None,
                source,
                occurred_at_ms: 1000,
                recorded_at_ms: 1001,
                sequence: 0,
                causality: RecorderEventCausality {
                    parent_event_id: None,
                    trigger_event_id: None,
                    root_event_id: None,
                },
                payload: RecorderEventPayload::IngressText {
                    text: "t".to_string(),
                    encoding: RecorderTextEncoding::Utf8,
                    redaction: RecorderRedactionLevel::None,
                    ingress_kind: RecorderIngressKind::SendText,
                },
            };
            let doc_fields = map_event_to_document(&event, 0);
            let _doc = fields_to_document(&doc_fields, &handles);
        }
    }
}
