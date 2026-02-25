//! Property-based tests for the chunking adapter (ft-dr6zv.1.3.6 / B5).
//!
//! Validates round-trip fidelity, metadata invariants, determinism, and batch
//! statistics for the SemanticChunk → ChunkDocument conversion layer.

use frankenterm_core::search::{
    ChunkDirection, ChunkOverlap, ChunkSourceOffset, SemanticChunk, batch_stats, chunk_to_document,
    chunks_to_documents, document_to_partial_chunk, extract_direction, extract_end_offset,
    extract_event_ids, extract_pane_id, extract_session_id, extract_start_offset,
    terminal_metadata_count,
};
use proptest::prelude::*;
use std::collections::HashSet;

// ── Strategies ─────────────────────────────────────────────────────────

fn arb_direction() -> impl Strategy<Value = ChunkDirection> {
    prop_oneof![
        Just(ChunkDirection::Ingress),
        Just(ChunkDirection::Egress),
        Just(ChunkDirection::MixedGlued),
    ]
}

fn arb_offset() -> impl Strategy<Value = ChunkSourceOffset> {
    (any::<u64>(), any::<u64>(), any::<u64>()).prop_map(|(seg, ord, byte)| ChunkSourceOffset {
        segment_id: seg,
        ordinal: ord,
        byte_offset: byte,
    })
}

fn arb_overlap() -> impl Strategy<Value = Option<ChunkOverlap>> {
    prop_oneof![
        3 => Just(None),
        1 => (
            "[a-z0-9\\-]{1,40}",
            arb_offset(),
            1..500_usize,
            "[a-zA-Z0-9 ]{1,100}",
        ).prop_map(|(from_id, offset, chars, text)| Some(ChunkOverlap {
            from_chunk_id: from_id,
            source_end_offset: offset,
            chars,
            text,
        })),
    ]
}

fn arb_chunk() -> impl Strategy<Value = SemanticChunk> {
    // Split into two groups to avoid > 12-tuple limit
    let core = (
        "[a-z0-9\\-]{1,50}",                       // chunk_id
        1..=u64::MAX,                              // pane_id
        proptest::option::of("[a-z0-9\\-]{1,30}"), // session_id
        arb_direction(),
        arb_offset(), // start_offset
        arb_offset(), // end_offset
    );
    let extra = (
        prop::collection::vec("[a-z0-9]{1,20}".prop_map(|s: String| s), 0..10usize), // event_ids
        any::<u64>(),                             // occurred_at_start_ms
        any::<u64>(),                             // occurred_at_end_ms
        "[a-f0-9]{8,64}".prop_map(|s: String| s), // content_hash
        "[^\x00]{0,500}".prop_map(|s: String| s), // text
        arb_overlap(),
    );
    (core, extra).prop_map(
        |(
            (chunk_id, pane_id, session_id, direction, start_offset, end_offset),
            (event_ids, start_ms, end_ms, content_hash, text, overlap),
        )| {
            let text_chars = text.chars().count();
            let event_count = event_ids.len();
            SemanticChunk {
                chunk_id,
                policy_version: "ft.recorder.chunking.v1".to_string(),
                pane_id,
                session_id,
                direction,
                start_offset,
                end_offset,
                event_ids,
                event_count,
                occurred_at_start_ms: start_ms,
                occurred_at_end_ms: end_ms,
                text_chars,
                content_hash,
                text,
                overlap,
            }
        },
    )
}

fn arb_chunk_batch() -> impl Strategy<Value = Vec<SemanticChunk>> {
    prop::collection::vec(arb_chunk(), 0..10)
}

// ── Property tests ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Round-trip: chunk → document → partial chunk preserves core fields
    #[test]
    fn roundtrip_preserves_core_fields(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);
        let reconstructed = document_to_partial_chunk(&doc);

        prop_assert!(reconstructed.is_some(), "round-trip must succeed for valid chunk");
        let recon = reconstructed.unwrap();

        prop_assert_eq!(&recon.chunk_id, &chunk.chunk_id);
        prop_assert_eq!(recon.pane_id, chunk.pane_id);
        prop_assert_eq!(&recon.session_id, &chunk.session_id);
        prop_assert_eq!(recon.direction, chunk.direction);
        prop_assert_eq!(&recon.policy_version, &chunk.policy_version);
        prop_assert_eq!(&recon.text, &chunk.text);
        prop_assert_eq!(recon.event_count, chunk.event_count);
        prop_assert_eq!(recon.text_chars, chunk.text_chars);
        prop_assert_eq!(&recon.content_hash, &chunk.content_hash);
    }

    // Round-trip preserves offsets exactly
    #[test]
    fn roundtrip_preserves_offsets(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);
        let recon = document_to_partial_chunk(&doc).unwrap();

        prop_assert_eq!(recon.start_offset, chunk.start_offset);
        prop_assert_eq!(recon.end_offset, chunk.end_offset);
    }

    // Round-trip preserves timestamps
    #[test]
    fn roundtrip_preserves_timestamps(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);
        let recon = document_to_partial_chunk(&doc).unwrap();

        prop_assert_eq!(recon.occurred_at_start_ms, chunk.occurred_at_start_ms);
        prop_assert_eq!(recon.occurred_at_end_ms, chunk.occurred_at_end_ms);
    }

    // Round-trip preserves event IDs
    #[test]
    fn roundtrip_preserves_event_ids(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);
        let recon = document_to_partial_chunk(&doc).unwrap();
        prop_assert_eq!(&recon.event_ids, &chunk.event_ids);
    }

    // Round-trip preserves overlap metadata (from_chunk_id + chars)
    #[test]
    fn roundtrip_preserves_overlap_metadata(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);
        let recon = document_to_partial_chunk(&doc).unwrap();

        match (&chunk.overlap, &recon.overlap) {
            (None, None) => {} // ok
            (Some(orig), Some(recon_ov)) => {
                prop_assert_eq!(&recon_ov.from_chunk_id, &orig.from_chunk_id);
                prop_assert_eq!(recon_ov.chars, orig.chars);
            }
            (orig, recon_ov) => {
                prop_assert!(false, "overlap mismatch: orig={:?}, recon={:?}", orig.is_some(), recon_ov.is_some());
            }
        }
    }

    // All metadata keys have ft. prefix
    #[test]
    fn all_metadata_keys_have_ft_prefix(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);
        for key in doc.metadata.keys() {
            prop_assert!(key.starts_with("ft."), "key {} missing ft. prefix", key);
        }
    }

    // Metadata extraction helpers are consistent
    #[test]
    fn extraction_helpers_consistent(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);

        prop_assert_eq!(extract_pane_id(&doc.metadata), Some(chunk.pane_id));
        prop_assert_eq!(extract_session_id(&doc.metadata), chunk.session_id.clone());
        prop_assert_eq!(extract_direction(&doc.metadata), Some(chunk.direction));
        prop_assert_eq!(extract_start_offset(&doc.metadata), Some(chunk.start_offset.clone()));
        prop_assert_eq!(extract_end_offset(&doc.metadata), Some(chunk.end_offset.clone()));

        let expected_events = if chunk.event_ids.is_empty() {
            vec![]
        } else {
            chunk.event_ids.clone()
        };
        prop_assert_eq!(extract_event_ids(&doc.metadata), expected_events);
    }

    // Conversion is deterministic (same input → same output)
    #[test]
    fn conversion_is_deterministic(chunk in arb_chunk()) {
        let doc1 = chunk_to_document(&chunk);
        let doc2 = chunk_to_document(&chunk);

        prop_assert_eq!(&doc1.id, &doc2.id);
        prop_assert_eq!(&doc1.content, &doc2.content);
        prop_assert_eq!(&doc1.title, &doc2.title);
        prop_assert_eq!(&doc1.metadata, &doc2.metadata);
    }

    // Document content matches chunk text
    #[test]
    fn document_content_matches_text(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);
        prop_assert_eq!(&doc.content, &chunk.text);
    }

    // Document ID matches chunk ID
    #[test]
    fn document_id_matches_chunk_id(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);
        prop_assert_eq!(&doc.id, &chunk.chunk_id);
    }

    // Batch conversion preserves order and count
    #[test]
    fn batch_preserves_order_and_count(chunks in arb_chunk_batch()) {
        let docs = chunks_to_documents(&chunks);

        prop_assert_eq!(docs.len(), chunks.len());

        let chunk_ids: Vec<&str> = chunks.iter().map(|c| c.chunk_id.as_str()).collect();
        let doc_ids: Vec<&str> = docs.iter().map(|d| d.id.as_str()).collect();
        prop_assert_eq!(chunk_ids, doc_ids);
    }

    // Batch stats are consistent with docs
    #[test]
    fn batch_stats_consistent(chunks in arb_chunk_batch()) {
        let docs = chunks_to_documents(&chunks);
        let stats = batch_stats(&docs);

        prop_assert_eq!(stats.total_chunks, chunks.len());

        let expected_panes: HashSet<u64> = chunks.iter().map(|c| c.pane_id).collect();
        prop_assert_eq!(stats.distinct_panes, expected_panes.len());

        let expected_sessions: HashSet<&str> = chunks.iter()
            .filter_map(|c| c.session_id.as_deref())
            .collect();
        prop_assert_eq!(stats.distinct_sessions, expected_sessions.len());

        let expected_overlaps = chunks.iter().filter(|c| c.overlap.is_some()).count();
        prop_assert_eq!(stats.chunks_with_overlap, expected_overlaps);
    }

    // Terminal metadata count is reasonable
    #[test]
    fn metadata_count_bounded(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);
        let count = terminal_metadata_count(&doc.metadata);

        // Base metadata: 14-16 keys (depending on session_id and event_ids)
        // Plus 0-2 for overlap
        // Total: 14-18
        prop_assert!(count >= 14, "too few metadata keys: {}", count);
        prop_assert!(count <= 18, "too many metadata keys: {}", count);
    }

    // Title is first line or None for empty/whitespace text
    #[test]
    fn title_is_first_line_or_none(chunk in arb_chunk()) {
        let doc = chunk_to_document(&chunk);

        let first_line = chunk.text.lines().next().unwrap_or("").trim();
        if first_line.is_empty() {
            prop_assert!(doc.title.is_none());
        } else {
            prop_assert!(doc.title.is_some());
            let title = doc.title.as_ref().unwrap();
            // Title is at most 120 chars (not bytes) and is a prefix of first line
            let title_chars = title.chars().count();
            prop_assert!(title_chars <= 120, "title has {} chars", title_chars);
            prop_assert!(first_line.starts_with(title.as_str()) || title_chars == 120);
        }
    }
}
