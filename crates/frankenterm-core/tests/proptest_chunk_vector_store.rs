//! Property-based tests for `search::chunk_vector_store` module.
//!
//! Covers: generation lifecycle state machine, embedding upsert/search invariants,
//! cosine similarity properties, pruning, drift reporting, and encoding round-trips.

use std::collections::BTreeSet;

use proptest::prelude::*;
use tempfile::tempdir;

use frankenterm_core::search::{
    ChunkDirection, ChunkEmbeddingUpsert, ChunkSourceOffset, ChunkVectorStore, SemanticChunk,
    SemanticGenerationStatus,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn open_store() -> (ChunkVectorStore, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let store = ChunkVectorStore::open(dir.path().join("test.db")).expect("open");
    (store, dir)
}

fn make_normalized_vec(dim: usize) -> Vec<f32> {
    if dim == 0 {
        return vec![];
    }
    let val = 1.0 / (dim as f32).sqrt();
    vec![val; dim]
}

/// Build a normalized vector with a specific direction bias in dimension `bias_dim`.
fn make_biased_vec(dim: usize, bias_dim: usize, bias: f32) -> Vec<f32> {
    if dim == 0 {
        return vec![];
    }
    let mut v = vec![0.0f32; dim];
    v[bias_dim % dim] = bias;
    // Normalize to unit length.
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn make_chunk(
    chunk_id: &str,
    start_ordinal: u64,
    end_ordinal: u64,
    direction: ChunkDirection,
) -> SemanticChunk {
    SemanticChunk {
        chunk_id: chunk_id.to_string(),
        policy_version: "ft.recorder.chunking.v1".to_string(),
        pane_id: 1,
        session_id: Some("sess".to_string()),
        direction,
        start_offset: ChunkSourceOffset {
            segment_id: 0,
            ordinal: start_ordinal,
            byte_offset: start_ordinal * 100,
        },
        end_offset: ChunkSourceOffset {
            segment_id: 0,
            ordinal: end_ordinal,
            byte_offset: end_ordinal * 100,
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

fn make_upsert(
    profile: &str,
    generation: &str,
    chunk_id: &str,
    start_ord: u64,
    end_ord: u64,
    dim: usize,
) -> ChunkEmbeddingUpsert {
    ChunkEmbeddingUpsert {
        profile_id: profile.to_string(),
        generation_id: generation.to_string(),
        chunk: make_chunk(chunk_id, start_ord, end_ord, ChunkDirection::Egress),
        embedding: make_normalized_vec(dim),
    }
}

fn direction_strategy() -> impl Strategy<Value = ChunkDirection> {
    prop_oneof![
        Just(ChunkDirection::Ingress),
        Just(ChunkDirection::Egress),
        Just(ChunkDirection::MixedGlued),
    ]
}

// ── Generation lifecycle invariants ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Registering a generation always starts in Building status.
    #[test]
    fn generation_starts_as_building(
        profile in "[a-z]{3,8}",
        gen_id in "[a-z]{3,8}",
    ) {
        let (store, _dir) = open_store();
        store
            .register_generation(&profile, &gen_id, "v1", "lex-v1")
            .expect("register");

        let sg = store.generation(&profile, &gen_id).expect("fetch").expect("exists");
        prop_assert_eq!(sg.status, SemanticGenerationStatus::Building);
        prop_assert!(sg.activated_at.is_none());
        prop_assert!(sg.retired_at.is_none());
    }

    /// Re-registering same generation updates metadata without error.
    #[test]
    fn generation_register_is_idempotent(
        profile in "[a-z]{3,8}",
        gen_id in "[a-z]{3,8}",
    ) {
        let (store, _dir) = open_store();
        store
            .register_generation(&profile, &gen_id, "v1", "lex-v1")
            .expect("first");
        store
            .register_generation(&profile, &gen_id, "v2", "lex-v2")
            .expect("second");

        let sg = store.generation(&profile, &gen_id).expect("fetch").expect("exists");
        prop_assert_eq!(sg.chunk_policy_version, "v2");
        prop_assert_eq!(sg.lexical_schema_version, "lex-v2");
    }

    /// Activating a generation transitions it to Active.
    #[test]
    fn activation_sets_active_status(
        profile in "[a-z]{3,8}",
        gen_id in "[a-z]{3,8}",
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation(&profile, &gen_id, "v1", "lex-v1")
            .expect("register");
        store.activate_generation(&profile, &gen_id).expect("activate");

        let sg = store.generation(&profile, &gen_id).expect("fetch").expect("exists");
        prop_assert_eq!(sg.status, SemanticGenerationStatus::Active);
        prop_assert!(sg.activated_at.is_some());
    }

    /// Activating a new generation retires the previous active one.
    #[test]
    fn activation_retires_prior_active(
        profile in "[a-z]{3,8}",
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation(&profile, "gen-old", "v1", "lex-v1")
            .expect("register old");
        store.activate_generation(&profile, "gen-old").expect("activate old");

        store
            .register_generation(&profile, "gen-new", "v1", "lex-v1")
            .expect("register new");
        store.activate_generation(&profile, "gen-new").expect("activate new");

        let old = store.generation(&profile, "gen-old").expect("fetch").expect("exists");
        let new = store.generation(&profile, "gen-new").expect("fetch").expect("exists");
        prop_assert_eq!(old.status, SemanticGenerationStatus::Retired);
        prop_assert_eq!(new.status, SemanticGenerationStatus::Active);
    }

    /// active_generation returns the most recently activated one.
    #[test]
    fn active_generation_returns_latest(
        profile in "[a-z]{3,8}",
        n in 2usize..5,
    ) {
        let (mut store, _dir) = open_store();
        let mut last_gen = String::new();
        for i in 0..n {
            let gen_id = format!("gen-{i}");
            store
                .register_generation(&profile, &gen_id, "v1", "lex-v1")
                .expect("register");
            store.activate_generation(&profile, &gen_id).expect("activate");
            last_gen = gen_id;
        }

        let active = store.active_generation(&profile).expect("fetch").expect("exists");
        prop_assert_eq!(active.generation_id, last_gen);
        prop_assert_eq!(active.status, SemanticGenerationStatus::Active);
    }

    /// Activating a non-existent generation returns error.
    #[test]
    fn activate_nonexistent_fails(
        profile in "[a-z]{3,8}",
        gen_id in "[a-z]{3,8}",
    ) {
        let (mut store, _dir) = open_store();
        let result = store.activate_generation(&profile, &gen_id);
        prop_assert!(result.is_err());
    }

    /// Querying a non-existent generation returns None.
    #[test]
    fn nonexistent_generation_is_none(
        profile in "[a-z]{3,8}",
        gen_id in "[a-z]{3,8}",
    ) {
        let (store, _dir) = open_store();
        let result = store.generation(&profile, &gen_id).expect("query");
        prop_assert!(result.is_none());
    }
}

// ── Embedding upsert invariants ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// First upsert reports was_update=false; second reports was_update=true.
    #[test]
    fn upsert_reports_update_correctly(
        dim in 2usize..16,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        let upsert = make_upsert("p1", "g1", "chunk-1", 0, 10, dim);
        let first = store.upsert_chunk_embedding(upsert.clone()).expect("first");
        prop_assert!(!first.was_update, "first upsert should be insert");

        let second = store.upsert_chunk_embedding(upsert).expect("second");
        prop_assert!(second.was_update, "second upsert should be update");
    }

    /// Upsert with wrong policy version fails.
    #[test]
    fn upsert_rejects_policy_mismatch(
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        let mut upsert = make_upsert("p1", "g1", "chunk-1", 0, 10, dim);
        upsert.chunk.policy_version = "wrong-version".to_string();
        let result = store.upsert_chunk_embedding(upsert);
        prop_assert!(result.is_err(), "policy mismatch should fail");
    }

    /// Upsert with non-existent generation fails.
    #[test]
    fn upsert_rejects_missing_generation(
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        let upsert = make_upsert("p1", "g-missing", "chunk-1", 0, 10, dim);
        let result = store.upsert_chunk_embedding(upsert);
        prop_assert!(result.is_err(), "missing generation should fail");
    }

    /// Upsert with empty embedding fails validation.
    #[test]
    fn upsert_rejects_empty_embedding(dummy in 0u8..1) {
        let _ = dummy;
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        let mut upsert = make_upsert("p1", "g1", "chunk-1", 0, 10, 4);
        upsert.embedding = vec![];
        let result = store.upsert_chunk_embedding(upsert);
        prop_assert!(result.is_err(), "empty embedding should fail");
    }

    /// Multiple unique chunks can be inserted into the same generation.
    #[test]
    fn multiple_chunks_per_generation(
        n in 2usize..10,
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        for i in 0..n {
            let upsert = make_upsert(
                "p1", "g1",
                &format!("chunk-{i}"),
                i as u64 * 10,
                (i as u64 + 1) * 10,
                dim,
            );
            store.upsert_chunk_embedding(upsert).expect("upsert");
        }

        // Verify all are searchable.
        let query = make_normalized_vec(dim);
        let hits = store.semantic_search("p1", "g1", &query, 100).expect("search");
        prop_assert_eq!(hits.len(), n, "should find all {} chunks", n);
    }
}

// ── Semantic search invariants ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Search results are capped at the requested limit.
    #[test]
    fn search_respects_limit(
        n in 5usize..15,
        limit in 1usize..5,
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        for i in 0..n {
            let upsert = make_upsert(
                "p1", "g1",
                &format!("chunk-{i}"),
                i as u64 * 10,
                (i as u64 + 1) * 10,
                dim,
            );
            store.upsert_chunk_embedding(upsert).expect("upsert");
        }

        let query = make_normalized_vec(dim);
        let hits = store.semantic_search("p1", "g1", &query, limit).expect("search");
        prop_assert!(hits.len() <= limit, "got {} > limit {}", hits.len(), limit);
    }

    /// Search with empty query vector returns nothing.
    #[test]
    fn search_empty_query_returns_nothing(dummy in 0u8..1) {
        let _ = dummy;
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        let upsert = make_upsert("p1", "g1", "chunk-1", 0, 10, 4);
        store.upsert_chunk_embedding(upsert).expect("upsert");

        let hits = store.semantic_search("p1", "g1", &[], 10).expect("search");
        prop_assert_eq!(hits.len(), 0);
    }

    /// Search results are ordered by descending score.
    #[test]
    fn search_results_ordered_by_score(
        n in 3usize..8,
        dim in 4usize..16,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        // Insert chunks with different embeddings.
        for i in 0..n {
            let mut upsert = make_upsert(
                "p1", "g1",
                &format!("chunk-{i}"),
                i as u64 * 10,
                (i as u64 + 1) * 10,
                dim,
            );
            // Make each chunk's embedding slightly different.
            upsert.embedding = make_biased_vec(dim, i % dim, 1.0);
            store.upsert_chunk_embedding(upsert).expect("upsert");
        }

        let query = make_biased_vec(dim, 0, 1.0);
        let hits = store.semantic_search("p1", "g1", &query, n).expect("search");

        for window in hits.windows(2) {
            prop_assert!(
                window[0].score >= window[1].score,
                "results should be ordered by descending score: {} < {}",
                window[0].score, window[1].score
            );
        }
    }

    /// Search scores are in [-1, 1] range (cosine similarity bounds).
    #[test]
    fn search_scores_in_valid_range(
        n in 2usize..6,
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        for i in 0..n {
            let mut upsert = make_upsert(
                "p1", "g1",
                &format!("chunk-{i}"),
                i as u64 * 10,
                (i as u64 + 1) * 10,
                dim,
            );
            upsert.embedding = make_biased_vec(dim, i % dim, 1.0);
            store.upsert_chunk_embedding(upsert).expect("upsert");
        }

        let query = make_normalized_vec(dim);
        let hits = store.semantic_search("p1", "g1", &query, 100).expect("search");

        for hit in &hits {
            prop_assert!(
                hit.score >= -1.0 - 1e-6 && hit.score <= 1.0 + 1e-6,
                "cosine score {} out of [-1, 1] range", hit.score
            );
        }
    }

    /// Search across different profiles returns only matching profile's chunks.
    #[test]
    fn search_is_profile_scoped(
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register p1");
        store
            .register_generation("p2", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register p2");

        let u1 = make_upsert("p1", "g1", "chunk-1", 0, 10, dim);
        let u2 = make_upsert("p2", "g1", "chunk-2", 0, 10, dim);
        store.upsert_chunk_embedding(u1).expect("upsert p1");
        store.upsert_chunk_embedding(u2).expect("upsert p2");

        let query = make_normalized_vec(dim);
        let hits_p1 = store.semantic_search("p1", "g1", &query, 10).expect("search p1");
        let hits_p2 = store.semantic_search("p2", "g1", &query, 10).expect("search p2");

        prop_assert_eq!(hits_p1.len(), 1);
        prop_assert_eq!(hits_p2.len(), 1);
        prop_assert!(hits_p1.iter().all(|h| h.profile_id == "p1"));
        prop_assert!(hits_p2.iter().all(|h| h.profile_id == "p2"));
    }

    /// Chunk IDs in results are unique.
    #[test]
    fn search_chunk_ids_unique(
        n in 3usize..10,
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        for i in 0..n {
            let upsert = make_upsert(
                "p1", "g1",
                &format!("chunk-{i}"),
                i as u64 * 10,
                (i as u64 + 1) * 10,
                dim,
            );
            store.upsert_chunk_embedding(upsert).expect("upsert");
        }

        let query = make_normalized_vec(dim);
        let hits = store.semantic_search("p1", "g1", &query, 100).expect("search");
        let ids: BTreeSet<&str> = hits.iter().map(|h| h.chunk_id.as_str()).collect();
        prop_assert_eq!(ids.len(), hits.len(), "chunk IDs should be unique");
    }
}

// ── Pruning invariants ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Pruning removes chunks whose end_ordinal <= cutoff.
    #[test]
    fn prune_removes_chunks_through_cutoff(
        n in 3usize..10,
        cutoff_idx in 0usize..3,
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        for i in 0..n {
            let upsert = make_upsert(
                "p1", "g1",
                &format!("chunk-{i}"),
                i as u64 * 10,
                (i as u64 + 1) * 10,
                dim,
            );
            store.upsert_chunk_embedding(upsert).expect("upsert");
        }

        let cutoff_ordinal = (cutoff_idx as u64 + 1) * 10;
        let deleted = store
            .prune_chunks_through_ordinal("p1", "g1", cutoff_ordinal)
            .expect("prune");

        let query = make_normalized_vec(dim);
        let remaining = store.semantic_search("p1", "g1", &query, 100).expect("search");

        // All remaining chunks should have end_ordinal > cutoff.
        for hit in &remaining {
            prop_assert!(
                hit.end_offset.ordinal > cutoff_ordinal,
                "chunk {} has end_ordinal {} <= cutoff {}",
                hit.chunk_id, hit.end_offset.ordinal, cutoff_ordinal
            );
        }

        prop_assert!(deleted <= n, "can't delete more than inserted");
        prop_assert_eq!(
            deleted + remaining.len(), n,
            "deleted + remaining should equal total"
        );
    }

    /// Pruning with cutoff 0 removes only chunks with end_ordinal <= 0.
    #[test]
    fn prune_cutoff_zero_removes_nothing_if_ordinals_positive(
        n in 1usize..5,
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        for i in 0..n {
            let upsert = make_upsert(
                "p1", "g1",
                &format!("chunk-{i}"),
                (i as u64 + 1) * 10, // start > 0
                (i as u64 + 2) * 10, // end > 0
                dim,
            );
            store.upsert_chunk_embedding(upsert).expect("upsert");
        }

        let deleted = store
            .prune_chunks_through_ordinal("p1", "g1", 0)
            .expect("prune");
        prop_assert_eq!(deleted, 0, "nothing should be pruned at cutoff 0");
    }
}

// ── Drift report invariants ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Drift report total_chunks matches insertion count.
    #[test]
    fn drift_report_total_chunks(
        n in 1usize..8,
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        for i in 0..n {
            let upsert = make_upsert(
                "p1", "g1",
                &format!("chunk-{i}"),
                i as u64 * 10,
                (i as u64 + 1) * 10,
                dim,
            );
            store.upsert_chunk_embedding(upsert).expect("upsert");
        }

        let report = store
            .drift_report("p1", "g1", "lex-v1", None)
            .expect("drift");
        prop_assert_eq!(
            report.total_chunks, n as u64,
            "total_chunks should match insertions"
        );
    }

    /// Schema mismatch is detected when expected != generation's version.
    #[test]
    fn drift_report_detects_schema_mismatch(
        lex_version in "[a-z]{3,6}",
    ) {
        let (store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        let report = store
            .drift_report("p1", "g1", &lex_version, None)
            .expect("drift");
        let expected_mismatch = lex_version != "lex-v1";
        prop_assert_eq!(
            report.lexical_schema_mismatch, expected_mismatch,
            "mismatch flag for expected='{}' vs actual='lex-v1'",
            lex_version
        );
    }

    /// Non-normalized embeddings are detected in drift report.
    /// (Note: validate_embedding_vector prevents non-normalized upserts,
    ///  so non_normalized_chunks should always be 0 for valid data.)
    #[test]
    fn drift_report_non_normalized_zero_for_valid(
        n in 1usize..5,
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        for i in 0..n {
            let upsert = make_upsert(
                "p1", "g1",
                &format!("chunk-{i}"),
                i as u64 * 10,
                (i as u64 + 1) * 10,
                dim,
            );
            store.upsert_chunk_embedding(upsert).expect("upsert");
        }

        let report = store
            .drift_report("p1", "g1", "lex-v1", None)
            .expect("drift");
        prop_assert_eq!(
            report.non_normalized_chunks, 0,
            "all embeddings should pass L2 norm check"
        );
    }

    /// Drift report for non-existent generation fails.
    #[test]
    fn drift_report_missing_generation_fails(
        profile in "[a-z]{3,8}",
        gen_id in "[a-z]{3,8}",
    ) {
        let (store, _dir) = open_store();
        let result = store.drift_report(&profile, &gen_id, "lex-v1", None);
        prop_assert!(result.is_err());
    }
}

// ── Direction round-trip ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Direction is preserved through upsert+search round-trip.
    #[test]
    fn direction_round_trip_through_store(
        direction in direction_strategy(),
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        let upsert = ChunkEmbeddingUpsert {
            profile_id: "p1".to_string(),
            generation_id: "g1".to_string(),
            chunk: make_chunk("chunk-dir", 0, 10, direction),
            embedding: make_normalized_vec(dim),
        };
        store.upsert_chunk_embedding(upsert).expect("upsert");

        let query = make_normalized_vec(dim);
        let hits = store.semantic_search("p1", "g1", &query, 10).expect("search");
        prop_assert_eq!(hits.len(), 1);
        prop_assert_eq!(
            hits[0].direction, direction,
            "direction should survive round-trip"
        );
    }

    /// Content hash is preserved through upsert+search round-trip.
    #[test]
    fn content_hash_round_trip(
        hash_suffix in "[a-z0-9]{4,12}",
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        let mut chunk = make_chunk("chunk-hash", 0, 10, ChunkDirection::Egress);
        chunk.content_hash = format!("hash-{hash_suffix}");

        let upsert = ChunkEmbeddingUpsert {
            profile_id: "p1".to_string(),
            generation_id: "g1".to_string(),
            chunk,
            embedding: make_normalized_vec(dim),
        };
        store.upsert_chunk_embedding(upsert).expect("upsert");

        let query = make_normalized_vec(dim);
        let hits = store.semantic_search("p1", "g1", &query, 10).expect("search");
        prop_assert_eq!(hits.len(), 1);
        prop_assert_eq!(
            &hits[0].content_hash,
            &format!("hash-{hash_suffix}"),
            "content hash should survive round-trip"
        );
    }
}

// ── Persistence across open/close ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// Store data survives close and reopen.
    #[test]
    fn store_persistence(
        n in 1usize..5,
        dim in 2usize..8,
    ) {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("persist.db");

        {
            let mut store = ChunkVectorStore::open(&db_path).expect("open write");
            store
                .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
                .expect("register");

            for i in 0..n {
                let upsert = make_upsert(
                    "p1", "g1",
                    &format!("chunk-{i}"),
                    i as u64 * 10,
                    (i as u64 + 1) * 10,
                    dim,
                );
                store.upsert_chunk_embedding(upsert).expect("upsert");
            }
        }

        let store = ChunkVectorStore::open(&db_path).expect("reopen");
        let sg = store.generation("p1", "g1").expect("fetch").expect("exists");
        prop_assert_eq!(sg.status, SemanticGenerationStatus::Building);

        let query = make_normalized_vec(dim);
        let hits = store.semantic_search("p1", "g1", &query, 100).expect("search");
        prop_assert_eq!(hits.len(), n, "all chunks should persist");
    }
}

// ── SemanticGenerationStatus serde round-trip ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// Status enum round-trips through serde.
    #[test]
    fn status_serde_round_trip(
        idx in 0usize..4,
    ) {
        let statuses = [
            SemanticGenerationStatus::Building,
            SemanticGenerationStatus::Active,
            SemanticGenerationStatus::Retired,
            SemanticGenerationStatus::Failed,
        ];
        let status = statuses[idx];
        let json = serde_json::to_string(&status).expect("serialize");
        let parsed: SemanticGenerationStatus = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(status, parsed);
    }

    /// Identical query vector and stored embedding produce score ~= 1.0.
    #[test]
    fn identical_vectors_score_near_one(
        dim in 2usize..16,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        let embedding = make_normalized_vec(dim);
        let upsert = ChunkEmbeddingUpsert {
            profile_id: "p1".to_string(),
            generation_id: "g1".to_string(),
            chunk: make_chunk("chunk-id", 0, 10, ChunkDirection::Egress),
            embedding: embedding.clone(),
        };
        store.upsert_chunk_embedding(upsert).expect("upsert");

        let hits = store.semantic_search("p1", "g1", &embedding, 1).expect("search");
        prop_assert_eq!(hits.len(), 1);
        prop_assert!(
            (hits[0].score - 1.0).abs() < 1e-4,
            "identical vectors should have score ~1.0, got {}", hits[0].score
        );
    }

    /// Offset fields survive upsert+search round-trip.
    #[test]
    fn offset_fields_round_trip(
        start_ord in 0u64..1000,
        end_ord in 1000u64..2000,
        dim in 2usize..8,
    ) {
        let (mut store, _dir) = open_store();
        store
            .register_generation("p1", "g1", "ft.recorder.chunking.v1", "lex-v1")
            .expect("register");

        let upsert = make_upsert("p1", "g1", "chunk-off", start_ord, end_ord, dim);
        store.upsert_chunk_embedding(upsert).expect("upsert");

        let query = make_normalized_vec(dim);
        let hits = store.semantic_search("p1", "g1", &query, 10).expect("search");
        prop_assert_eq!(hits.len(), 1);
        prop_assert_eq!(hits[0].start_offset.ordinal, start_ord);
        prop_assert_eq!(hits[0].end_offset.ordinal, end_ord);
    }
}
