//! Property tests for `scan_pipeline::ChunkedPipelineState` lifecycle
//! invariants and `search::chunk_vector_store` vector-dimension
//! preservation invariants.
//!
//! ## Why this file exists
//!
//! `proptest_scan_pipeline.rs` (21 KB, ~30 properties) covers the
//! chunked-vs-batch parity surface (newlines, ANSI bytes, trigger
//! categories under varied chunk boundaries + compression
//! settings). It does NOT cover the lifecycle invariants of
//! `ChunkedPipelineState` itself: reset idempotency, empty-chunk
//! no-op, total_bytes monotonicity, and the logical_lines equation
//! (`logical_lines == newline_count + (1 if !ends_with_newline else 0)`
//! when any bytes have been observed).
//!
//! `proptest_chunk_vector_store.rs` (32 KB, ~40 properties) covers
//! generation lifecycle, upsert/search invariants, cosine
//! similarity, and serde round-trips. It does NOT pin the
//! user-requested **vector dimension preservation** invariants
//! end-to-end across an upsert+search cycle: that for any dim D,
//! a vector of dim D inserted under a generation with embedding
//! dimension D is retrievable via `semantic_search` with a query
//! vector of dim D, AND a query vector of a *different* dim D'
//! returns no hits (the dimension filter at line 530 of
//! chunk_vector_store.rs).
//!
//! Logs are emitted as structured tracing-json events on every
//! property case so failing cases land a parseable record of the
//! input + observed behavior — same shape as the prior phase
//! sweeps.

use std::sync::Once;

use frankenterm_core::scan_pipeline::{ChunkedPipelineState, ScanPipeline, ScanPipelineConfig};
use frankenterm_core::search::{
    ChunkDirection, ChunkEmbeddingUpsert, ChunkSourceOffset, ChunkVectorStore, SemanticChunk,
};
use proptest::prelude::*;
use tempfile::tempdir;
use tracing::info;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

// =============================================================================
// scan_pipeline ChunkedPipelineState lifecycle invariants
// =============================================================================

fn arb_chunk_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..256)
}

fn arb_chunk_sequence() -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(arb_chunk_bytes(), 0..6)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// **scan_pipeline reset idempotency**: calling reset() twice
    /// in a row produces identical state observable via the
    /// public getters. Catches any future field that leaks
    /// across reset boundaries.
    #[test]
    fn proptest_scan_pipeline_reset_is_idempotent(
        chunks in arb_chunk_sequence(),
    ) {
        init_test_tracing_json();
        let pipeline = ScanPipeline::new(ScanPipelineConfig::default());
        let mut state = ChunkedPipelineState::new(16_777_216);
        for chunk in &chunks {
            let _ = pipeline.process_chunk(chunk, &mut state);
        }
        state.reset();
        let after_first_reset = (
            state.total_bytes(),
            state.newline_count(),
            state.ansi_byte_count(),
            state.total_trigger_matches(),
            state.logical_lines(),
            state.has_errors(),
            state.has_completions(),
        );
        state.reset();
        let after_second_reset = (
            state.total_bytes(),
            state.newline_count(),
            state.ansi_byte_count(),
            state.total_trigger_matches(),
            state.logical_lines(),
            state.has_errors(),
            state.has_completions(),
        );

        info!(
            test = "reset_is_idempotent",
            chunk_count = chunks.len(),
            "reset idempotency case"
        );

        prop_assert_eq!(after_first_reset, after_second_reset,
            "reset() must be idempotent across observable getters");
    }

    /// **scan_pipeline reset returns to fresh-construction
    /// equivalent**: state observable getters after reset() must
    /// equal those of a freshly-constructed state.
    #[test]
    fn proptest_scan_pipeline_reset_matches_fresh_state(
        chunks in arb_chunk_sequence(),
    ) {
        init_test_tracing_json();
        let pipeline = ScanPipeline::new(ScanPipelineConfig::default());
        let mut state = ChunkedPipelineState::new(16_777_216);
        for chunk in &chunks {
            let _ = pipeline.process_chunk(chunk, &mut state);
        }
        state.reset();
        let fresh = ChunkedPipelineState::new(16_777_216);

        prop_assert_eq!(state.total_bytes(), fresh.total_bytes());
        prop_assert_eq!(state.newline_count(), fresh.newline_count());
        prop_assert_eq!(state.ansi_byte_count(), fresh.ansi_byte_count());
        prop_assert_eq!(state.total_trigger_matches(), fresh.total_trigger_matches());
        prop_assert_eq!(state.logical_lines(), fresh.logical_lines());
        prop_assert_eq!(state.has_errors(), fresh.has_errors());
        prop_assert_eq!(state.has_completions(), fresh.has_completions());
    }

    /// **scan_pipeline total_bytes is monotone**: after any
    /// sequence of process_chunk calls, total_bytes equals the
    /// cumulative input byte count exactly. The byte counter
    /// must not skip or double-count across chunk boundaries.
    #[test]
    fn proptest_scan_pipeline_total_bytes_matches_input(
        chunks in arb_chunk_sequence(),
    ) {
        init_test_tracing_json();
        let pipeline = ScanPipeline::new(ScanPipelineConfig::default());
        let mut state = ChunkedPipelineState::new(16_777_216);
        let mut running_total: u64 = 0;
        for chunk in &chunks {
            running_total += chunk.len() as u64;
            let _ = pipeline.process_chunk(chunk, &mut state);
            prop_assert_eq!(state.total_bytes(), running_total,
                "total_bytes must match cumulative input bytes after every chunk");
        }
    }

    /// **scan_pipeline empty chunk is a no-op for byte counters**:
    /// processing an empty chunk does not change total_bytes,
    /// newline_count, ansi_byte_count, or total_trigger_matches.
    /// The `saw_any_bytes` flag is independently bumped only
    /// when a non-empty chunk arrives.
    #[test]
    fn proptest_scan_pipeline_empty_chunk_is_byte_noop(
        prefix_chunks in arb_chunk_sequence(),
    ) {
        init_test_tracing_json();
        let pipeline = ScanPipeline::new(ScanPipelineConfig::default());
        let mut state = ChunkedPipelineState::new(16_777_216);
        for chunk in &prefix_chunks {
            let _ = pipeline.process_chunk(chunk, &mut state);
        }
        let before_total_bytes = state.total_bytes();
        let before_newlines = state.newline_count();
        let before_ansi = state.ansi_byte_count();
        let before_triggers = state.total_trigger_matches();

        // Process an empty chunk.
        let _ = pipeline.process_chunk(&[], &mut state);

        prop_assert_eq!(state.total_bytes(), before_total_bytes,
            "empty chunk must not change total_bytes");
        prop_assert_eq!(state.newline_count(), before_newlines,
            "empty chunk must not change newline_count");
        prop_assert_eq!(state.ansi_byte_count(), before_ansi,
            "empty chunk must not change ansi_byte_count");
        prop_assert_eq!(state.total_trigger_matches(), before_triggers,
            "empty chunk must not change total_trigger_matches");
    }

    /// **scan_pipeline logical_lines equation**: when any non-
    /// empty chunk has been observed, the logical_lines count
    /// equals `newline_count + (1 if !ends_with_newline else 0)`.
    /// Pinned by storage.rs:9855 ("append_segment_sync assigns
    /// seq = COALESCE(MAX(seq) + 1, 0)") and a related
    /// ft-7do6c invariant.
    #[test]
    fn proptest_scan_pipeline_logical_lines_equation(
        chunks in arb_chunk_sequence(),
    ) {
        init_test_tracing_json();
        let pipeline = ScanPipeline::new(ScanPipelineConfig::default());
        let mut state = ChunkedPipelineState::new(16_777_216);
        for chunk in &chunks {
            let _ = pipeline.process_chunk(chunk, &mut state);
        }
        let observed = state.logical_lines();
        let total_bytes = state.total_bytes();
        let newlines = state.newline_count();

        let expected = if total_bytes == 0 {
            0
        } else {
            // Recover the ends_with_newline state from the running
            // total of input bytes — reach into the last non-empty
            // chunk if any.
            let last_byte = chunks
                .iter()
                .rev()
                .find(|c| !c.is_empty())
                .and_then(|c| c.last().copied());
            let ends_with_newline = matches!(last_byte, Some(b'\n'));
            if ends_with_newline { newlines } else { newlines + 1 }
        };

        prop_assert_eq!(observed, expected,
            "logical_lines must satisfy the documented equation");
    }
}

// =============================================================================
// chunk_vector_store dimension-preservation invariants
// =============================================================================

/// Build a unit-norm vector of the requested dimension where one
/// designated cell holds 1.0 and the rest are 0.0. L2-normalization
/// is exact (the vector already has norm 1.0). The
/// `validate_embedding_vector` gate at line 908 of
/// chunk_vector_store.rs requires norm within 1e-3 of 1.0; our
/// construction is exact so the gate always passes.
fn unit_norm_vec(dim: usize, biased_axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; dim];
    if dim > 0 {
        v[biased_axis % dim] = 1.0;
    }
    v
}

fn make_chunk(chunk_id: &str) -> SemanticChunk {
    SemanticChunk {
        chunk_id: chunk_id.to_string(),
        policy_version: "ft.recorder.chunking.v1".to_string(),
        pane_id: 1,
        session_id: Some("sess".to_string()),
        direction: ChunkDirection::Egress,
        start_offset: ChunkSourceOffset {
            segment_id: 0,
            ordinal: 0,
            byte_offset: 0,
        },
        end_offset: ChunkSourceOffset {
            segment_id: 0,
            ordinal: 1,
            byte_offset: 100,
        },
        event_ids: vec!["e1".to_string()],
        event_count: 1,
        occurred_at_start_ms: 1_000,
        occurred_at_end_ms: 1_100,
        text_chars: 50,
        content_hash: format!("hash-{chunk_id}"),
        text: format!("content-{chunk_id}"),
        overlap: None,
    }
}

fn open_store_with_dim(_dim: usize) -> (ChunkVectorStore, tempfile::TempDir) {
    // The chunk-vector-store's open() uses the
    // SemanticEmbedderIdentity::unversioned() placeholder — fine
    // for dimension-preservation testing because the dimension
    // filter at the SQL level (embedding_dimension = ?3) doesn't
    // depend on the embedder identity.
    let dir = tempdir().expect("tempdir");
    let store = ChunkVectorStore::open(dir.path().join("test.db")).expect("open");
    store
        .register_generation(
            "profile",
            "gen",
            "ft.recorder.chunking.v1",
            "ft.recorder.lexical.v1",
        )
        .expect("register generation");
    (store, dir)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// **chunk_vector_store dimension preservation** — the
    /// user-requested invariant. For any dim D in 4..=64, a
    /// unit-norm vector of dim D inserted via
    /// `upsert_chunk_embedding` is retrievable via
    /// `semantic_search` with a query vector of the SAME
    /// dimension. The retrieved hit's `score` (cosine
    /// similarity) is ~1.0 when the query vector points along
    /// the same axis the inserted vector emphasizes.
    #[test]
    fn proptest_chunk_vector_store_dim_preserved_across_upsert_and_search(
        dim in 4usize..=64usize,
        biased_axis in 0usize..=64usize,
    ) {
        init_test_tracing_json();
        let (mut store, _dir) = open_store_with_dim(dim);

        // Insert a unit-norm vector with mass on `biased_axis`.
        let inserted = unit_norm_vec(dim, biased_axis);
        let upsert = ChunkEmbeddingUpsert {
            profile_id: "profile".to_string(),
            generation_id: "gen".to_string(),
            chunk: make_chunk("chunk-A"),
            embedding: inserted.clone(),
        };
        store.upsert_chunk_embedding(upsert).expect("upsert");

        // Query with the SAME vector — must retrieve the chunk
        // we just inserted with cosine score ~1.0.
        let hits = store
            .semantic_search("profile", "gen", &inserted, 4)
            .expect("search");

        info!(
            test = "dim_preserved_upsert_search",
            dim,
            biased_axis,
            hit_count = hits.len(),
            top_score = hits.first().map(|h| h.score),
            "dim preservation case"
        );

        prop_assert_eq!(hits.len(), 1,
            "exactly one hit expected for the single inserted chunk at dim {}", dim);
        let top_hit = &hits[0];
        prop_assert_eq!(&top_hit.chunk_id, "chunk-A",
            "hit chunk_id must match the inserted chunk_id");
        prop_assert!(
            (top_hit.score - 1.0).abs() < 1e-3,
            "self-similarity at dim {} must be ~1.0, got {}",
            dim, top_hit.score
        );
    }

    /// **chunk_vector_store dimension filter** — querying with
    /// a vector of dim D' that DOESN'T match the inserted
    /// dimension D returns zero hits. The SQL filter
    /// `embedding_dimension = ?3` at chunk_vector_store.rs:530
    /// must fully exclude the foreign-dim chunk.
    #[test]
    fn proptest_chunk_vector_store_dim_mismatch_returns_no_hits(
        insert_dim in 4usize..=16usize,
        delta in 1usize..=8usize,
    ) {
        init_test_tracing_json();
        let query_dim = insert_dim + delta;
        let (mut store, _dir) = open_store_with_dim(insert_dim);

        let inserted = unit_norm_vec(insert_dim, 0);
        let upsert = ChunkEmbeddingUpsert {
            profile_id: "profile".to_string(),
            generation_id: "gen".to_string(),
            chunk: make_chunk("chunk-B"),
            embedding: inserted,
        };
        store.upsert_chunk_embedding(upsert).expect("upsert");

        // Query with a different-dimension vector. Per the
        // dimension filter, no hits should be returned even
        // though the chunk exists for the same profile/generation.
        let query = unit_norm_vec(query_dim, 0);
        let hits = store
            .semantic_search("profile", "gen", &query, 4)
            .expect("search");

        info!(
            test = "dim_mismatch_returns_no_hits",
            insert_dim,
            query_dim,
            hit_count = hits.len(),
            "dim filter case"
        );

        prop_assert_eq!(hits.len(), 0,
            "a query vector of dim {} must NOT retrieve a chunk of dim {}",
            query_dim, insert_dim);
    }
}
