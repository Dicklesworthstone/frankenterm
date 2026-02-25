//! Chunking adapter for the frankensearch migration (B5).
//!
//! FrankenTerm's `SemanticChunk` carries terminal-specific metadata (pane ID,
//! session, direction, source offsets, overlap) that has no equivalent in
//! frankensearch's flat `IndexableDocument` schema. This adapter converts
//! between the two representations while preserving all terminal metadata in
//! the document's `metadata: HashMap<String, String>` field.
//!
//! # Metadata key conventions
//!
//! All terminal-specific metadata uses the `ft.` prefix:
//!
//! | Key                       | Description                            |
//! |---------------------------|----------------------------------------|
//! | `ft.pane_id`              | Numeric pane identifier                |
//! | `ft.session_id`           | Optional session identifier            |
//! | `ft.direction`            | Chunk direction (ingress/egress/mixed) |
//! | `ft.policy_version`       | Chunking policy version string         |
//! | `ft.event_count`          | Number of events in this chunk         |
//! | `ft.text_chars`           | Character count of chunk text          |
//! | `ft.content_hash`         | SHA-256 content hash                   |
//! | `ft.start_segment_id`     | Start offset segment                   |
//! | `ft.start_ordinal`        | Start offset ordinal                   |
//! | `ft.start_byte_offset`    | Start offset byte                      |
//! | `ft.end_segment_id`       | End offset segment                     |
//! | `ft.end_ordinal`          | End offset ordinal                     |
//! | `ft.end_byte_offset`      | End offset byte                        |
//! | `ft.occurred_at_start_ms` | First event timestamp (ms)             |
//! | `ft.occurred_at_end_ms`   | Last event timestamp (ms)              |
//! | `ft.overlap_from`         | Source chunk ID of overlap prefix       |
//! | `ft.overlap_chars`        | Number of overlap characters           |
//! | `ft.event_ids`            | Comma-separated event IDs              |
//!
//! # Feature gate
//!
//! This module requires the `frankensearch` feature.

use std::collections::HashMap;

use super::chunking::{ChunkDirection, ChunkOverlap, ChunkSourceOffset, SemanticChunk};

/// Frankensearch-compatible document produced from a `SemanticChunk`.
///
/// This is a lightweight struct mirroring `frankensearch::IndexableDocument`
/// so the adapter works without depending on frankensearch types at the
/// type level (the feature gate only controls downstream usage).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkDocument {
    /// Document ID (maps from `SemanticChunk::chunk_id`).
    pub id: String,
    /// Main searchable content (maps from `SemanticChunk::text`).
    pub content: String,
    /// Optional title (first line of chunk text, truncated).
    pub title: Option<String>,
    /// Terminal-specific metadata preserved from the semantic chunk.
    pub metadata: HashMap<String, String>,
}

/// Metadata key prefix for all FrankenTerm terminal-specific fields.
pub const METADATA_PREFIX: &str = "ft.";

// ── Metadata key constants ──────────────────────────────────────────────

pub const KEY_PANE_ID: &str = "ft.pane_id";
pub const KEY_SESSION_ID: &str = "ft.session_id";
pub const KEY_DIRECTION: &str = "ft.direction";
pub const KEY_POLICY_VERSION: &str = "ft.policy_version";
pub const KEY_EVENT_COUNT: &str = "ft.event_count";
pub const KEY_TEXT_CHARS: &str = "ft.text_chars";
pub const KEY_CONTENT_HASH: &str = "ft.content_hash";
pub const KEY_START_SEGMENT_ID: &str = "ft.start_segment_id";
pub const KEY_START_ORDINAL: &str = "ft.start_ordinal";
pub const KEY_START_BYTE_OFFSET: &str = "ft.start_byte_offset";
pub const KEY_END_SEGMENT_ID: &str = "ft.end_segment_id";
pub const KEY_END_ORDINAL: &str = "ft.end_ordinal";
pub const KEY_END_BYTE_OFFSET: &str = "ft.end_byte_offset";
pub const KEY_OCCURRED_AT_START_MS: &str = "ft.occurred_at_start_ms";
pub const KEY_OCCURRED_AT_END_MS: &str = "ft.occurred_at_end_ms";
pub const KEY_OVERLAP_FROM: &str = "ft.overlap_from";
pub const KEY_OVERLAP_CHARS: &str = "ft.overlap_chars";
pub const KEY_EVENT_IDS: &str = "ft.event_ids";

/// Maximum title length (characters) extracted from chunk text.
const MAX_TITLE_LEN: usize = 120;

// ── SemanticChunk → ChunkDocument ───────────────────────────────────────

/// Convert a single `SemanticChunk` into a `ChunkDocument`.
///
/// All terminal metadata is packed into `metadata` using `ft.` prefixed keys.
/// The conversion is deterministic: identical input always produces identical output.
pub fn chunk_to_document(chunk: &SemanticChunk) -> ChunkDocument {
    let mut metadata = HashMap::new();

    // Core identity
    metadata.insert(KEY_PANE_ID.to_string(), chunk.pane_id.to_string());
    if let Some(ref sid) = chunk.session_id {
        metadata.insert(KEY_SESSION_ID.to_string(), sid.clone());
    }
    metadata.insert(
        KEY_DIRECTION.to_string(),
        direction_to_str(chunk.direction).to_string(),
    );
    metadata.insert(KEY_POLICY_VERSION.to_string(), chunk.policy_version.clone());

    // Counts
    metadata.insert(KEY_EVENT_COUNT.to_string(), chunk.event_count.to_string());
    metadata.insert(KEY_TEXT_CHARS.to_string(), chunk.text_chars.to_string());
    metadata.insert(KEY_CONTENT_HASH.to_string(), chunk.content_hash.clone());

    // Source offsets
    pack_offset(&mut metadata, "start", &chunk.start_offset);
    pack_offset(&mut metadata, "end", &chunk.end_offset);

    // Timestamps
    metadata.insert(
        KEY_OCCURRED_AT_START_MS.to_string(),
        chunk.occurred_at_start_ms.to_string(),
    );
    metadata.insert(
        KEY_OCCURRED_AT_END_MS.to_string(),
        chunk.occurred_at_end_ms.to_string(),
    );

    // Overlap
    if let Some(ref overlap) = chunk.overlap {
        metadata.insert(KEY_OVERLAP_FROM.to_string(), overlap.from_chunk_id.clone());
        metadata.insert(KEY_OVERLAP_CHARS.to_string(), overlap.chars.to_string());
    }

    // Event IDs (comma-separated)
    if !chunk.event_ids.is_empty() {
        metadata.insert(KEY_EVENT_IDS.to_string(), chunk.event_ids.join(","));
    }

    // Title: first line, truncated
    let title = extract_title(&chunk.text);

    ChunkDocument {
        id: chunk.chunk_id.clone(),
        content: chunk.text.clone(),
        title,
        metadata,
    }
}

/// Convert a batch of `SemanticChunk`s into `ChunkDocument`s.
///
/// Preserves ordering. The conversion is independent per chunk (no cross-chunk state).
pub fn chunks_to_documents(chunks: &[SemanticChunk]) -> Vec<ChunkDocument> {
    chunks.iter().map(chunk_to_document).collect()
}

// ── ChunkDocument → metadata extraction helpers ─────────────────────────

/// Extract the pane ID from document metadata.
#[allow(clippy::implicit_hasher)]
pub fn extract_pane_id(metadata: &HashMap<String, String>) -> Option<u64> {
    metadata.get(KEY_PANE_ID).and_then(|v| v.parse().ok())
}

/// Extract the session ID from document metadata.
#[allow(clippy::implicit_hasher)]
pub fn extract_session_id(metadata: &HashMap<String, String>) -> Option<String> {
    metadata.get(KEY_SESSION_ID).cloned()
}

/// Extract the chunk direction from document metadata.
#[allow(clippy::implicit_hasher)]
pub fn extract_direction(metadata: &HashMap<String, String>) -> Option<ChunkDirection> {
    metadata
        .get(KEY_DIRECTION)
        .and_then(|v| str_to_direction(v))
}

/// Extract the policy version from document metadata.
#[allow(clippy::implicit_hasher)]
pub fn extract_policy_version(metadata: &HashMap<String, String>) -> Option<String> {
    metadata.get(KEY_POLICY_VERSION).cloned()
}

/// Extract source offset from document metadata.
#[allow(clippy::implicit_hasher)]
pub fn extract_start_offset(metadata: &HashMap<String, String>) -> Option<ChunkSourceOffset> {
    unpack_offset(metadata, "start")
}

/// Extract end offset from document metadata.
#[allow(clippy::implicit_hasher)]
pub fn extract_end_offset(metadata: &HashMap<String, String>) -> Option<ChunkSourceOffset> {
    unpack_offset(metadata, "end")
}

/// Extract overlap metadata from document metadata.
#[allow(clippy::implicit_hasher)]
pub fn extract_overlap(metadata: &HashMap<String, String>) -> Option<(String, usize)> {
    let from = metadata.get(KEY_OVERLAP_FROM)?;
    let chars = metadata.get(KEY_OVERLAP_CHARS)?.parse().ok()?;
    Some((from.clone(), chars))
}

/// Extract event IDs from document metadata.
#[allow(clippy::implicit_hasher)]
pub fn extract_event_ids(metadata: &HashMap<String, String>) -> Vec<String> {
    metadata
        .get(KEY_EVENT_IDS)
        .map(|v| v.split(',').map(String::from).collect())
        .unwrap_or_default()
}

/// Count how many `ft.` prefixed keys exist in the metadata.
#[allow(clippy::implicit_hasher)]
pub fn terminal_metadata_count(metadata: &HashMap<String, String>) -> usize {
    metadata
        .keys()
        .filter(|k| k.starts_with(METADATA_PREFIX))
        .count()
}

// ── Reconstruction: ChunkDocument → partial SemanticChunk ───────────────

/// Partial reconstruction of a `SemanticChunk` from a `ChunkDocument`.
///
/// This recovers all metadata that was packed into the document. Fields that
/// cannot be recovered (e.g., full `ChunkOverlap.text`, `ChunkOverlap.source_end_offset`)
/// are set to default/empty values. The `event_ids` list is recovered from
/// the comma-separated metadata field.
///
/// Returns `None` if required metadata (pane_id, direction) is missing.
pub fn document_to_partial_chunk(doc: &ChunkDocument) -> Option<SemanticChunk> {
    let pane_id = extract_pane_id(&doc.metadata)?;
    let direction = extract_direction(&doc.metadata)?;
    let session_id = extract_session_id(&doc.metadata);
    let policy_version =
        extract_policy_version(&doc.metadata).unwrap_or_else(|| "unknown".to_string());

    let start_offset = extract_start_offset(&doc.metadata).unwrap_or(ChunkSourceOffset {
        segment_id: 0,
        ordinal: 0,
        byte_offset: 0,
    });
    let end_offset = extract_end_offset(&doc.metadata).unwrap_or(ChunkSourceOffset {
        segment_id: 0,
        ordinal: 0,
        byte_offset: 0,
    });

    let event_count: usize = doc
        .metadata
        .get(KEY_EVENT_COUNT)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let text_chars: usize = doc
        .metadata
        .get(KEY_TEXT_CHARS)
        .and_then(|v| v.parse().ok())
        .unwrap_or(doc.content.len());
    let content_hash = doc
        .metadata
        .get(KEY_CONTENT_HASH)
        .cloned()
        .unwrap_or_default();

    let occurred_at_start_ms: u64 = doc
        .metadata
        .get(KEY_OCCURRED_AT_START_MS)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let occurred_at_end_ms: u64 = doc
        .metadata
        .get(KEY_OCCURRED_AT_END_MS)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let event_ids = extract_event_ids(&doc.metadata);

    let overlap = extract_overlap(&doc.metadata).map(|(from_chunk_id, chars)| ChunkOverlap {
        from_chunk_id,
        source_end_offset: ChunkSourceOffset {
            segment_id: 0,
            ordinal: 0,
            byte_offset: 0,
        },
        chars,
        text: String::new(), // cannot recover overlap text from metadata alone
    });

    Some(SemanticChunk {
        chunk_id: doc.id.clone(),
        policy_version,
        pane_id,
        session_id,
        direction,
        start_offset,
        end_offset,
        event_ids,
        event_count,
        occurred_at_start_ms,
        occurred_at_end_ms,
        text_chars,
        content_hash,
        text: doc.content.clone(),
        overlap,
    })
}

// ── Batch adapter stats ─────────────────────────────────────────────────

/// Summary statistics for a batch conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkAdapterStats {
    /// Number of chunks converted.
    pub total_chunks: usize,
    /// Number of chunks with overlap metadata.
    pub chunks_with_overlap: usize,
    /// Number of distinct pane IDs.
    pub distinct_panes: usize,
    /// Number of distinct session IDs.
    pub distinct_sessions: usize,
    /// Total character count across all chunks.
    pub total_chars: usize,
    /// Total metadata entries across all documents.
    pub total_metadata_entries: usize,
}

/// Compute summary statistics for a batch of converted documents.
pub fn batch_stats(docs: &[ChunkDocument]) -> ChunkAdapterStats {
    let mut panes = std::collections::HashSet::new();
    let mut sessions = std::collections::HashSet::new();
    let mut chunks_with_overlap = 0usize;
    let mut total_chars = 0usize;
    let mut total_metadata_entries = 0usize;

    for doc in docs {
        if let Some(pane) = doc.metadata.get(KEY_PANE_ID) {
            panes.insert(pane.clone());
        }
        if let Some(session) = doc.metadata.get(KEY_SESSION_ID) {
            sessions.insert(session.clone());
        }
        if doc.metadata.contains_key(KEY_OVERLAP_FROM) {
            chunks_with_overlap += 1;
        }
        total_chars += doc.content.len();
        total_metadata_entries += terminal_metadata_count(&doc.metadata);
    }

    ChunkAdapterStats {
        total_chunks: docs.len(),
        chunks_with_overlap,
        distinct_panes: panes.len(),
        distinct_sessions: sessions.len(),
        total_chars,
        total_metadata_entries,
    }
}

// ── Internal helpers ────────────────────────────────────────────────────

fn direction_to_str(d: ChunkDirection) -> &'static str {
    match d {
        ChunkDirection::Ingress => "ingress",
        ChunkDirection::Egress => "egress",
        ChunkDirection::MixedGlued => "mixed_glued",
    }
}

fn str_to_direction(s: &str) -> Option<ChunkDirection> {
    match s {
        "ingress" => Some(ChunkDirection::Ingress),
        "egress" => Some(ChunkDirection::Egress),
        "mixed_glued" => Some(ChunkDirection::MixedGlued),
        _ => None,
    }
}

fn pack_offset(metadata: &mut HashMap<String, String>, prefix: &str, offset: &ChunkSourceOffset) {
    metadata.insert(
        format!("ft.{prefix}_segment_id"),
        offset.segment_id.to_string(),
    );
    metadata.insert(format!("ft.{prefix}_ordinal"), offset.ordinal.to_string());
    metadata.insert(
        format!("ft.{prefix}_byte_offset"),
        offset.byte_offset.to_string(),
    );
}

fn unpack_offset(metadata: &HashMap<String, String>, prefix: &str) -> Option<ChunkSourceOffset> {
    let segment_id = metadata
        .get(&format!("ft.{prefix}_segment_id"))?
        .parse()
        .ok()?;
    let ordinal = metadata
        .get(&format!("ft.{prefix}_ordinal"))?
        .parse()
        .ok()?;
    let byte_offset = metadata
        .get(&format!("ft.{prefix}_byte_offset"))?
        .parse()
        .ok()?;
    Some(ChunkSourceOffset {
        segment_id,
        ordinal,
        byte_offset,
    })
}

fn extract_title(text: &str) -> Option<String> {
    let first_line = text.lines().next()?;
    let trimmed = first_line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= MAX_TITLE_LEN {
        Some(trimmed.to_string())
    } else {
        // Truncate at char boundary
        let truncated: String = trimmed.chars().take(MAX_TITLE_LEN).collect();
        Some(truncated)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::chunking::RECORDER_CHUNKING_POLICY_V1;
    use super::*;

    fn sample_chunk() -> SemanticChunk {
        SemanticChunk {
            chunk_id: "chunk-001".to_string(),
            policy_version: RECORDER_CHUNKING_POLICY_V1.to_string(),
            pane_id: 42,
            session_id: Some("sess-abc".to_string()),
            direction: ChunkDirection::Ingress,
            start_offset: ChunkSourceOffset {
                segment_id: 1,
                ordinal: 10,
                byte_offset: 100,
            },
            end_offset: ChunkSourceOffset {
                segment_id: 1,
                ordinal: 25,
                byte_offset: 2500,
            },
            event_ids: vec!["ev-1".to_string(), "ev-2".to_string(), "ev-3".to_string()],
            event_count: 3,
            occurred_at_start_ms: 1000,
            occurred_at_end_ms: 5000,
            text_chars: 42,
            content_hash: "abc123def456".to_string(),
            text: "ls -la /home\ntotal 0\ndrwxr-xr-x 3 user user 96 Jan 1 00:00 .".to_string(),
            overlap: None,
        }
    }

    fn sample_chunk_with_overlap() -> SemanticChunk {
        let mut chunk = sample_chunk();
        chunk.chunk_id = "chunk-002".to_string();
        chunk.overlap = Some(ChunkOverlap {
            from_chunk_id: "chunk-001".to_string(),
            source_end_offset: ChunkSourceOffset {
                segment_id: 1,
                ordinal: 25,
                byte_offset: 2500,
            },
            chars: 15,
            text: "drwxr-xr-x 3 u".to_string(),
        });
        chunk
    }

    // ── Basic conversion ────────────────────────────────────────────────

    #[test]
    fn chunk_to_doc_preserves_id_and_content() {
        let chunk = sample_chunk();
        let doc = chunk_to_document(&chunk);
        assert_eq!(doc.id, "chunk-001");
        assert_eq!(doc.content, chunk.text);
    }

    #[test]
    fn chunk_to_doc_extracts_title() {
        let chunk = sample_chunk();
        let doc = chunk_to_document(&chunk);
        assert_eq!(doc.title.as_deref(), Some("ls -la /home"));
    }

    #[test]
    fn chunk_to_doc_empty_text_no_title() {
        let mut chunk = sample_chunk();
        chunk.text = String::new();
        let doc = chunk_to_document(&chunk);
        assert_eq!(doc.title, None);
    }

    #[test]
    fn chunk_to_doc_whitespace_only_no_title() {
        let mut chunk = sample_chunk();
        chunk.text = "   \n\n  ".to_string();
        let doc = chunk_to_document(&chunk);
        assert_eq!(doc.title, None);
    }

    #[test]
    fn chunk_to_doc_long_first_line_truncates_title() {
        let mut chunk = sample_chunk();
        chunk.text = "x".repeat(200);
        let doc = chunk_to_document(&chunk);
        assert_eq!(doc.title.as_ref().map(|t| t.len()), Some(MAX_TITLE_LEN));
    }

    // ── Metadata preservation ───────────────────────────────────────────

    #[test]
    fn chunk_to_doc_pane_id_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_PANE_ID).unwrap(), "42");
    }

    #[test]
    fn chunk_to_doc_session_id_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_SESSION_ID).unwrap(), "sess-abc");
    }

    #[test]
    fn chunk_to_doc_no_session_id_omitted() {
        let mut chunk = sample_chunk();
        chunk.session_id = None;
        let doc = chunk_to_document(&chunk);
        assert!(!doc.metadata.contains_key(KEY_SESSION_ID));
    }

    #[test]
    fn chunk_to_doc_direction_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_DIRECTION).unwrap(), "ingress");
    }

    #[test]
    fn chunk_to_doc_egress_direction() {
        let mut chunk = sample_chunk();
        chunk.direction = ChunkDirection::Egress;
        let doc = chunk_to_document(&chunk);
        assert_eq!(doc.metadata.get(KEY_DIRECTION).unwrap(), "egress");
    }

    #[test]
    fn chunk_to_doc_mixed_direction() {
        let mut chunk = sample_chunk();
        chunk.direction = ChunkDirection::MixedGlued;
        let doc = chunk_to_document(&chunk);
        assert_eq!(doc.metadata.get(KEY_DIRECTION).unwrap(), "mixed_glued");
    }

    #[test]
    fn chunk_to_doc_policy_version_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(
            doc.metadata.get(KEY_POLICY_VERSION).unwrap(),
            RECORDER_CHUNKING_POLICY_V1
        );
    }

    #[test]
    fn chunk_to_doc_event_count_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_EVENT_COUNT).unwrap(), "3");
    }

    #[test]
    fn chunk_to_doc_text_chars_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_TEXT_CHARS).unwrap(), "42");
    }

    #[test]
    fn chunk_to_doc_content_hash_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_CONTENT_HASH).unwrap(), "abc123def456");
    }

    #[test]
    fn chunk_to_doc_start_offset_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_START_SEGMENT_ID).unwrap(), "1");
        assert_eq!(doc.metadata.get(KEY_START_ORDINAL).unwrap(), "10");
        assert_eq!(doc.metadata.get(KEY_START_BYTE_OFFSET).unwrap(), "100");
    }

    #[test]
    fn chunk_to_doc_end_offset_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_END_SEGMENT_ID).unwrap(), "1");
        assert_eq!(doc.metadata.get(KEY_END_ORDINAL).unwrap(), "25");
        assert_eq!(doc.metadata.get(KEY_END_BYTE_OFFSET).unwrap(), "2500");
    }

    #[test]
    fn chunk_to_doc_timestamps_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_OCCURRED_AT_START_MS).unwrap(), "1000");
        assert_eq!(doc.metadata.get(KEY_OCCURRED_AT_END_MS).unwrap(), "5000");
    }

    #[test]
    fn chunk_to_doc_event_ids_in_metadata() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(doc.metadata.get(KEY_EVENT_IDS).unwrap(), "ev-1,ev-2,ev-3");
    }

    #[test]
    fn chunk_to_doc_no_event_ids_omitted() {
        let mut chunk = sample_chunk();
        chunk.event_ids.clear();
        let doc = chunk_to_document(&chunk);
        assert!(!doc.metadata.contains_key(KEY_EVENT_IDS));
    }

    // ── Overlap metadata ────────────────────────────────────────────────

    #[test]
    fn chunk_to_doc_no_overlap_keys_absent() {
        let doc = chunk_to_document(&sample_chunk());
        assert!(!doc.metadata.contains_key(KEY_OVERLAP_FROM));
        assert!(!doc.metadata.contains_key(KEY_OVERLAP_CHARS));
    }

    #[test]
    fn chunk_to_doc_overlap_preserved() {
        let doc = chunk_to_document(&sample_chunk_with_overlap());
        assert_eq!(doc.metadata.get(KEY_OVERLAP_FROM).unwrap(), "chunk-001");
        assert_eq!(doc.metadata.get(KEY_OVERLAP_CHARS).unwrap(), "15");
    }

    // ── Determinism ─────────────────────────────────────────────────────

    #[test]
    fn conversion_is_deterministic() {
        let chunk = sample_chunk();
        let doc1 = chunk_to_document(&chunk);
        let doc2 = chunk_to_document(&chunk);
        assert_eq!(doc1.id, doc2.id);
        assert_eq!(doc1.content, doc2.content);
        assert_eq!(doc1.title, doc2.title);
        assert_eq!(doc1.metadata, doc2.metadata);
    }

    #[test]
    fn batch_conversion_preserves_order() {
        let chunks = vec![
            {
                let mut c = sample_chunk();
                c.chunk_id = "a".to_string();
                c
            },
            {
                let mut c = sample_chunk();
                c.chunk_id = "b".to_string();
                c
            },
            {
                let mut c = sample_chunk();
                c.chunk_id = "c".to_string();
                c
            },
        ];
        let docs = chunks_to_documents(&chunks);
        let ids: Vec<&str> = docs.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    // ── Metadata extraction helpers ─────────────────────────────────────

    #[test]
    fn extract_pane_id_works() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(extract_pane_id(&doc.metadata), Some(42));
    }

    #[test]
    fn extract_pane_id_missing() {
        let metadata = HashMap::new();
        assert_eq!(extract_pane_id(&metadata), None);
    }

    #[test]
    fn extract_session_id_works() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(
            extract_session_id(&doc.metadata),
            Some("sess-abc".to_string())
        );
    }

    #[test]
    fn extract_direction_all_variants() {
        for (s, expected) in [
            ("ingress", ChunkDirection::Ingress),
            ("egress", ChunkDirection::Egress),
            ("mixed_glued", ChunkDirection::MixedGlued),
        ] {
            assert_eq!(str_to_direction(s), Some(expected));
        }
        assert_eq!(str_to_direction("invalid"), None);
    }

    #[test]
    fn extract_start_offset_works() {
        let doc = chunk_to_document(&sample_chunk());
        let offset = extract_start_offset(&doc.metadata).unwrap();
        assert_eq!(offset.segment_id, 1);
        assert_eq!(offset.ordinal, 10);
        assert_eq!(offset.byte_offset, 100);
    }

    #[test]
    fn extract_end_offset_works() {
        let doc = chunk_to_document(&sample_chunk());
        let offset = extract_end_offset(&doc.metadata).unwrap();
        assert_eq!(offset.segment_id, 1);
        assert_eq!(offset.ordinal, 25);
        assert_eq!(offset.byte_offset, 2500);
    }

    #[test]
    fn extract_overlap_works() {
        let doc = chunk_to_document(&sample_chunk_with_overlap());
        let (from, chars) = extract_overlap(&doc.metadata).unwrap();
        assert_eq!(from, "chunk-001");
        assert_eq!(chars, 15);
    }

    #[test]
    fn extract_overlap_absent() {
        let doc = chunk_to_document(&sample_chunk());
        assert!(extract_overlap(&doc.metadata).is_none());
    }

    #[test]
    fn extract_event_ids_works() {
        let doc = chunk_to_document(&sample_chunk());
        assert_eq!(
            extract_event_ids(&doc.metadata),
            vec!["ev-1", "ev-2", "ev-3"]
        );
    }

    #[test]
    fn extract_event_ids_empty() {
        let metadata = HashMap::new();
        assert!(extract_event_ids(&metadata).is_empty());
    }

    // ── terminal_metadata_count ─────────────────────────────────────────

    #[test]
    fn terminal_metadata_count_basic() {
        let doc = chunk_to_document(&sample_chunk());
        // pane_id, session_id, direction, policy_version, event_count,
        // text_chars, content_hash, start_segment/ordinal/byte (3),
        // end_segment/ordinal/byte (3), occurred_at_start, occurred_at_end,
        // event_ids = 16
        assert_eq!(terminal_metadata_count(&doc.metadata), 16);
    }

    #[test]
    fn terminal_metadata_count_with_overlap() {
        let doc = chunk_to_document(&sample_chunk_with_overlap());
        // 16 base + 2 overlap (from + chars) = 18
        assert_eq!(terminal_metadata_count(&doc.metadata), 18);
    }

    #[test]
    fn terminal_metadata_count_no_session_no_events() {
        let mut chunk = sample_chunk();
        chunk.session_id = None;
        chunk.event_ids.clear();
        let doc = chunk_to_document(&chunk);
        // 16 - session_id - event_ids = 14
        assert_eq!(terminal_metadata_count(&doc.metadata), 14);
    }

    // ── Round-trip: chunk → document → partial chunk ────────────────────

    #[test]
    fn roundtrip_basic_fields() {
        let original = sample_chunk();
        let doc = chunk_to_document(&original);
        let reconstructed = document_to_partial_chunk(&doc).unwrap();

        assert_eq!(reconstructed.chunk_id, original.chunk_id);
        assert_eq!(reconstructed.pane_id, original.pane_id);
        assert_eq!(reconstructed.session_id, original.session_id);
        assert_eq!(reconstructed.direction, original.direction);
        assert_eq!(reconstructed.policy_version, original.policy_version);
        assert_eq!(reconstructed.text, original.text);
        assert_eq!(reconstructed.event_count, original.event_count);
        assert_eq!(reconstructed.text_chars, original.text_chars);
        assert_eq!(reconstructed.content_hash, original.content_hash);
    }

    #[test]
    fn roundtrip_offsets() {
        let original = sample_chunk();
        let doc = chunk_to_document(&original);
        let reconstructed = document_to_partial_chunk(&doc).unwrap();

        assert_eq!(reconstructed.start_offset, original.start_offset);
        assert_eq!(reconstructed.end_offset, original.end_offset);
    }

    #[test]
    fn roundtrip_timestamps() {
        let original = sample_chunk();
        let doc = chunk_to_document(&original);
        let reconstructed = document_to_partial_chunk(&doc).unwrap();

        assert_eq!(
            reconstructed.occurred_at_start_ms,
            original.occurred_at_start_ms
        );
        assert_eq!(
            reconstructed.occurred_at_end_ms,
            original.occurred_at_end_ms
        );
    }

    #[test]
    fn roundtrip_event_ids() {
        let original = sample_chunk();
        let doc = chunk_to_document(&original);
        let reconstructed = document_to_partial_chunk(&doc).unwrap();
        assert_eq!(reconstructed.event_ids, original.event_ids);
    }

    #[test]
    fn roundtrip_overlap_metadata() {
        let original = sample_chunk_with_overlap();
        let doc = chunk_to_document(&original);
        let reconstructed = document_to_partial_chunk(&doc).unwrap();

        let orig_overlap = original.overlap.unwrap();
        let recon_overlap = reconstructed.overlap.unwrap();
        assert_eq!(recon_overlap.from_chunk_id, orig_overlap.from_chunk_id);
        assert_eq!(recon_overlap.chars, orig_overlap.chars);
        // overlap text and source_end_offset cannot be recovered
    }

    #[test]
    fn roundtrip_no_overlap() {
        let original = sample_chunk();
        let doc = chunk_to_document(&original);
        let reconstructed = document_to_partial_chunk(&doc).unwrap();
        assert!(reconstructed.overlap.is_none());
    }

    #[test]
    fn roundtrip_missing_required_metadata_returns_none() {
        let doc = ChunkDocument {
            id: "orphan".to_string(),
            content: "some text".to_string(),
            title: None,
            metadata: HashMap::new(), // no ft.pane_id, ft.direction
        };
        assert!(document_to_partial_chunk(&doc).is_none());
    }

    #[test]
    fn roundtrip_missing_direction_returns_none() {
        let mut metadata = HashMap::new();
        metadata.insert(KEY_PANE_ID.to_string(), "42".to_string());
        // no direction
        let doc = ChunkDocument {
            id: "no-dir".to_string(),
            content: "text".to_string(),
            title: None,
            metadata,
        };
        assert!(document_to_partial_chunk(&doc).is_none());
    }

    // ── Batch stats ─────────────────────────────────────────────────────

    #[test]
    fn batch_stats_empty() {
        let stats = batch_stats(&[]);
        assert_eq!(stats.total_chunks, 0);
        assert_eq!(stats.distinct_panes, 0);
        assert_eq!(stats.distinct_sessions, 0);
    }

    #[test]
    fn batch_stats_single() {
        let docs = chunks_to_documents(&[sample_chunk()]);
        let stats = batch_stats(&docs);
        assert_eq!(stats.total_chunks, 1);
        assert_eq!(stats.distinct_panes, 1);
        assert_eq!(stats.distinct_sessions, 1);
        assert_eq!(stats.chunks_with_overlap, 0);
    }

    #[test]
    fn batch_stats_mixed() {
        let mut c1 = sample_chunk();
        c1.pane_id = 1;
        c1.session_id = Some("sess-1".to_string());

        let mut c2 = sample_chunk();
        c2.chunk_id = "chunk-002".to_string();
        c2.pane_id = 2;
        c2.session_id = Some("sess-2".to_string());
        c2.overlap = Some(ChunkOverlap {
            from_chunk_id: "chunk-001".to_string(),
            source_end_offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 0,
                byte_offset: 0,
            },
            chars: 10,
            text: "overlap".to_string(),
        });

        let mut c3 = sample_chunk();
        c3.chunk_id = "chunk-003".to_string();
        c3.pane_id = 1; // same pane as c1
        c3.session_id = Some("sess-1".to_string()); // same session as c1

        let docs = chunks_to_documents(&[c1, c2, c3]);
        let stats = batch_stats(&docs);
        assert_eq!(stats.total_chunks, 3);
        assert_eq!(stats.distinct_panes, 2);
        assert_eq!(stats.distinct_sessions, 2);
        assert_eq!(stats.chunks_with_overlap, 1);
        assert!(stats.total_chars > 0);
        assert!(stats.total_metadata_entries > 0);
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn chunk_with_unicode_text() {
        let mut chunk = sample_chunk();
        chunk.text = "日本語テキスト\n🎉 emoji line\nアダプター".to_string();
        chunk.text_chars = chunk.text.chars().count();
        let doc = chunk_to_document(&chunk);
        assert_eq!(doc.content, chunk.text);
        assert_eq!(doc.title.as_deref(), Some("日本語テキスト"));

        let reconstructed = document_to_partial_chunk(&doc).unwrap();
        assert_eq!(reconstructed.text, chunk.text);
    }

    #[test]
    fn chunk_with_large_pane_id() {
        let mut chunk = sample_chunk();
        chunk.pane_id = u64::MAX;
        let doc = chunk_to_document(&chunk);
        assert_eq!(extract_pane_id(&doc.metadata), Some(u64::MAX));
    }

    #[test]
    fn chunk_with_zero_timestamps() {
        let mut chunk = sample_chunk();
        chunk.occurred_at_start_ms = 0;
        chunk.occurred_at_end_ms = 0;
        let doc = chunk_to_document(&chunk);
        let reconstructed = document_to_partial_chunk(&doc).unwrap();
        assert_eq!(reconstructed.occurred_at_start_ms, 0);
        assert_eq!(reconstructed.occurred_at_end_ms, 0);
    }

    #[test]
    fn all_directions_roundtrip() {
        for direction in [
            ChunkDirection::Ingress,
            ChunkDirection::Egress,
            ChunkDirection::MixedGlued,
        ] {
            let mut chunk = sample_chunk();
            chunk.direction = direction;
            let doc = chunk_to_document(&chunk);
            let reconstructed = document_to_partial_chunk(&doc).unwrap();
            assert_eq!(reconstructed.direction, direction);
        }
    }

    #[test]
    fn metadata_all_keys_have_ft_prefix() {
        let doc = chunk_to_document(&sample_chunk_with_overlap());
        for key in doc.metadata.keys() {
            assert!(
                key.starts_with("ft."),
                "metadata key {key} missing ft. prefix"
            );
        }
    }
}
