use frankenterm_core::search::{
    ChunkDirection, ChunkEmbeddingUpsert, ChunkVectorStore, RECORDER_CHUNKING_POLICY_V1,
    SemanticChunk,
};
use frankenterm_core::search::{ChunkSourceOffset, SemanticGenerationStatus};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn normalized(values: &[f32]) -> Vec<f32> {
    let norm = values
        .iter()
        .map(|v| f64::from(*v) * f64::from(*v))
        .sum::<f64>()
        .sqrt();
    if norm <= f64::EPSILON {
        return vec![0.0; values.len()];
    }
    values
        .iter()
        .map(|v| (f64::from(*v) / norm) as f32)
        .collect()
}

fn make_chunk(
    chunk_id: &str,
    start_ordinal: u64,
    end_ordinal: u64,
    direction: ChunkDirection,
    text: &str,
) -> SemanticChunk {
    SemanticChunk {
        chunk_id: chunk_id.to_string(),
        policy_version: RECORDER_CHUNKING_POLICY_V1.to_string(),
        pane_id: 7,
        session_id: Some("sess-7".to_string()),
        direction,
        start_offset: ChunkSourceOffset {
            segment_id: 1,
            ordinal: start_ordinal,
            byte_offset: start_ordinal * 100,
        },
        end_offset: ChunkSourceOffset {
            segment_id: 1,
            ordinal: end_ordinal,
            byte_offset: end_ordinal * 100 + 42,
        },
        event_ids: vec![format!("evt-{start_ordinal}")],
        event_count: 1,
        occurred_at_start_ms: start_ordinal * 10,
        occurred_at_end_ms: end_ordinal * 10,
        text_chars: text.chars().count(),
        content_hash: sha256_hex(text.as_bytes()),
        text: text.to_string(),
        overlap: None,
    }
}

#[test]
fn lifecycle_register_activate_upsert_and_search() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("chunk-vectors.db");
    let mut store = ChunkVectorStore::open(db).unwrap();

    store
        .register_generation(
            "profile-a",
            "gen-1",
            RECORDER_CHUNKING_POLICY_V1,
            "ft.recorder.lexical.v1",
        )
        .unwrap();
    store.activate_generation("profile-a", "gen-1").unwrap();

    let active = store.active_generation("profile-a").unwrap().unwrap();
    assert_eq!(active.generation_id, "gen-1");
    assert_eq!(active.status, SemanticGenerationStatus::Active);

    let chunk_error = make_chunk(
        "chunk-error",
        10,
        12,
        ChunkDirection::Egress,
        "[OUT] error: compile failed",
    );
    let chunk_success = make_chunk(
        "chunk-success",
        13,
        14,
        ChunkDirection::Egress,
        "[OUT] build succeeded",
    );

    let out1 = store
        .upsert_chunk_embedding(ChunkEmbeddingUpsert {
            profile_id: "profile-a".to_string(),
            generation_id: "gen-1".to_string(),
            chunk: chunk_error.clone(),
            embedding: normalized(&[1.0, 0.0, 0.0]),
        })
        .unwrap();
    assert!(!out1.was_update);

    let out2 = store
        .upsert_chunk_embedding(ChunkEmbeddingUpsert {
            profile_id: "profile-a".to_string(),
            generation_id: "gen-1".to_string(),
            chunk: chunk_success.clone(),
            embedding: normalized(&[0.0, 1.0, 0.0]),
        })
        .unwrap();
    assert!(!out2.was_update);

    let hits = store
        .semantic_search("profile-a", "gen-1", &normalized(&[1.0, 0.0, 0.0]), 5)
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].chunk_id, "chunk-error");
    assert!(hits[0].score >= hits[1].score);

    // Update the same chunk identity with a new vector.
    let out3 = store
        .upsert_chunk_embedding(ChunkEmbeddingUpsert {
            profile_id: "profile-a".to_string(),
            generation_id: "gen-1".to_string(),
            chunk: chunk_error,
            embedding: normalized(&[0.0, 1.0, 0.0]),
        })
        .unwrap();
    assert!(out3.was_update);

    let reranked = store
        .semantic_search("profile-a", "gen-1", &normalized(&[1.0, 0.0, 0.0]), 5)
        .unwrap();
    assert_eq!(reranked[0].chunk_id, "chunk-success");
}

#[test]
fn rejects_policy_mismatch_for_generation() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("chunk-vectors.db");
    let mut store = ChunkVectorStore::open(db).unwrap();

    store
        .register_generation(
            "profile-a",
            "gen-1",
            RECORDER_CHUNKING_POLICY_V1,
            "ft.recorder.lexical.v1",
        )
        .unwrap();

    let mut chunk = make_chunk("chunk-1", 1, 2, ChunkDirection::Egress, "[OUT] hello world");
    chunk.policy_version = "ft.recorder.chunking.v2".to_string();

    let err = store
        .upsert_chunk_embedding(ChunkEmbeddingUpsert {
            profile_id: "profile-a".to_string(),
            generation_id: "gen-1".to_string(),
            chunk,
            embedding: normalized(&[1.0, 0.0]),
        })
        .unwrap_err();

    let text = err.to_string();
    assert!(text.contains("chunk policy mismatch"));
}

#[test]
fn retention_prune_by_end_ordinal() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("chunk-vectors.db");
    let mut store = ChunkVectorStore::open(db).unwrap();

    store
        .register_generation(
            "profile-a",
            "gen-1",
            RECORDER_CHUNKING_POLICY_V1,
            "ft.recorder.lexical.v1",
        )
        .unwrap();

    for (id, start, end) in [("a", 1, 5), ("b", 6, 15), ("c", 16, 30)] {
        let chunk = make_chunk(
            id,
            start,
            end,
            ChunkDirection::Egress,
            &format!("[OUT] {id}"),
        );
        store
            .upsert_chunk_embedding(ChunkEmbeddingUpsert {
                profile_id: "profile-a".to_string(),
                generation_id: "gen-1".to_string(),
                chunk,
                embedding: normalized(&[1.0, 0.0, 0.0]),
            })
            .unwrap();
    }

    let deleted = store
        .prune_chunks_through_ordinal("profile-a", "gen-1", 15)
        .unwrap();
    assert_eq!(deleted, 2);

    let remaining = store
        .semantic_search("profile-a", "gen-1", &normalized(&[1.0, 0.0, 0.0]), 10)
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].chunk_id, "c");
}

#[test]
fn drift_report_flags_lexical_lag_and_schema_mismatch() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("chunk-vectors.db");
    let mut store = ChunkVectorStore::open(db).unwrap();

    store
        .register_generation(
            "profile-a",
            "gen-1",
            RECORDER_CHUNKING_POLICY_V1,
            "ft.recorder.lexical.v1",
        )
        .unwrap();
    store.activate_generation("profile-a", "gen-1").unwrap();

    let c1 = make_chunk("lag-1", 10, 20, ChunkDirection::Egress, "[OUT] first");
    let c2 = make_chunk("lag-2", 21, 40, ChunkDirection::Egress, "[OUT] second");

    store
        .upsert_chunk_embedding(ChunkEmbeddingUpsert {
            profile_id: "profile-a".to_string(),
            generation_id: "gen-1".to_string(),
            chunk: c1,
            embedding: normalized(&[1.0, 0.0, 0.0]),
        })
        .unwrap();
    store
        .upsert_chunk_embedding(ChunkEmbeddingUpsert {
            profile_id: "profile-a".to_string(),
            generation_id: "gen-1".to_string(),
            chunk: c2,
            embedding: normalized(&[0.0, 1.0, 0.0]),
        })
        .unwrap();

    let report = store
        .drift_report("profile-a", "gen-1", "ft.recorder.lexical.v2", Some(25))
        .unwrap();

    assert_eq!(report.total_chunks, 2);
    assert_eq!(report.max_vector_ordinal, Some(40));
    assert_eq!(report.chunks_beyond_lexical, 1);
    assert!(report.lexical_schema_mismatch);
    assert_eq!(
        report.generation_lexical_schema_version,
        "ft.recorder.lexical.v1"
    );
    assert_eq!(report.non_normalized_chunks, 0);
}
