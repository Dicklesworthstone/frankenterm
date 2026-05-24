//! Persistent chunk-vector lifecycle store for recorder semantic indexing.
//!
//! Bead: `wa-oegrb.5.3`
//!
//! This module implements a deterministic persistence/query layer for semantic
//! chunk embeddings keyed by:
//! - `profile_id` (provider/model/profile compatibility id)
//! - `generation_id` (index generation boundary)
//! - `chunk_id` (policy-versioned chunk identity from `ft.recorder.chunking.v1`)
//!
//! It also exposes:
//! - retention-aware pruning by offset boundary
//! - deterministic nearest-neighbor search
//! - lexical/vector drift reporting against lexical checkpoint progress

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::search::{ChunkDirection, ChunkSourceOffset, SemanticChunk};

/// Result alias for chunk-vector store operations.
pub type Result<T> = std::result::Result<T, ChunkVectorStoreError>;

const SEMANTIC_SCHEMA_VERSION: i64 = 1;

/// Identity of the embedder pipeline that produced the vectors stored in this
/// chunk-vector store. Persisted into `semantic_store_metadata` and compared
/// against on every open; a mismatch triggers atomic invalidation
/// (`ensure_semantic_store_metadata`, this file) — embeddings are dropped and
/// active generations are marked `failed` so the recorder can rebuild from a
/// clean slate under the new embedder.
///
/// The identity is intentionally just "name + dimension" to keep the
/// invalidation gate cheap and feature-flag-agnostic. Callers with richer
/// metadata (model revision, tokenizer hash, normalization variant) are
/// expected to fold those into `embedder_version` so two genuinely
/// different pipelines hash to two distinct identity strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticEmbedderIdentity {
    /// Human-readable name of the embedder + any disambiguating metadata
    /// (model revision, tokenizer hash, etc.) that distinguishes it from
    /// every other pipeline whose vectors are NOT interchangeable with it.
    pub embedder_version: String,
    /// Output dimension of the embedder. A change in dimension always
    /// requires invalidation because previously-stored vectors cannot be
    /// compared against new ones.
    pub embedding_dimension: usize,
}

impl SemanticEmbedderIdentity {
    /// Identity used by callers that have not yet wired a real embedder
    /// through (the historical default — preserved so existing callers
    /// continue to compile and behave as before). New code that DOES have
    /// a real fastembed pipeline should pass a populated identity via
    /// `from_fastembed_pipeline` or build one directly.
    #[must_use]
    pub fn unversioned() -> Self {
        Self {
            embedder_version: "unversioned".to_string(),
            embedding_dimension: 0,
        }
    }

    /// Build an identity from a fastembed-style pipeline description. The
    /// name is taken verbatim (callers should already have folded
    /// model-revision / tokenizer disambiguation into it via
    /// `fastembed_embedder::model_display_name` or similar) and the
    /// dimension is the embedder's output dim. Kept feature-flag-agnostic
    /// — this struct lives in the core crate and never imports fastembed
    /// directly.
    #[must_use]
    pub fn from_fastembed_pipeline(model_name: impl Into<String>, dimension: usize) -> Self {
        Self {
            embedder_version: model_name.into(),
            embedding_dimension: dimension,
        }
    }
}

impl Default for SemanticEmbedderIdentity {
    fn default() -> Self {
        Self::unversioned()
    }
}

/// Semantic generation lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticGenerationStatus {
    Building,
    Active,
    Retired,
    Failed,
}

impl SemanticGenerationStatus {
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "building" => Ok(Self::Building),
            "active" => Ok(Self::Active),
            "retired" => Ok(Self::Retired),
            "failed" => Ok(Self::Failed),
            _ => Err(ChunkVectorStoreError::InvalidDbValue(format!(
                "unknown semantic generation status: {value}"
            ))),
        }
    }
}

/// Registered semantic generation metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticGeneration {
    pub profile_id: String,
    pub generation_id: String,
    pub chunk_policy_version: String,
    pub lexical_schema_version: String,
    pub status: SemanticGenerationStatus,
    pub created_at: i64,
    pub activated_at: Option<i64>,
    pub retired_at: Option<i64>,
}

/// Upsert payload for a chunk embedding row.
#[derive(Debug, Clone)]
pub struct ChunkEmbeddingUpsert {
    pub profile_id: String,
    pub generation_id: String,
    pub chunk: SemanticChunk,
    pub embedding: Vec<f32>,
}

/// Upsert result metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEmbeddingUpsertOutcome {
    pub profile_id: String,
    pub generation_id: String,
    pub chunk_id: String,
    pub was_update: bool,
}

/// Semantic search hit for chunk vectors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkVectorHit {
    pub profile_id: String,
    pub generation_id: String,
    pub chunk_id: String,
    pub score: f64,
    pub direction: ChunkDirection,
    pub start_offset: ChunkSourceOffset,
    pub end_offset: ChunkSourceOffset,
    pub content_hash: String,
}

/// Drift report comparing vector lifecycle state with lexical progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkVectorDriftReport {
    pub profile_id: String,
    pub generation_id: String,
    pub chunk_policy_version: String,
    pub generation_status: SemanticGenerationStatus,
    pub generation_lexical_schema_version: String,
    pub expected_lexical_schema_version: String,
    pub lexical_schema_mismatch: bool,
    pub lexical_upto_ordinal: Option<u64>,
    pub total_chunks: u64,
    pub max_vector_ordinal: Option<u64>,
    pub chunks_beyond_lexical: u64,
    pub non_normalized_chunks: u64,
}

/// Errors for chunk-vector store lifecycle/query operations.
#[derive(Debug, Error)]
pub enum ChunkVectorStoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("generation not found: profile={profile_id}, generation={generation_id}")]
    GenerationNotFound {
        profile_id: String,
        generation_id: String,
    },
    #[error(
        "chunk policy mismatch for profile={profile_id}, generation={generation_id}: expected={expected}, got={actual}"
    )]
    ChunkPolicyMismatch {
        profile_id: String,
        generation_id: String,
        expected: String,
        actual: String,
    },
    #[error("invalid embedding vector: {0}")]
    InvalidVector(String),
    #[error("integer conversion overflow for field: {0}")]
    IntegerOverflow(&'static str),
    #[error("invalid database value: {0}")]
    InvalidDbValue(String),
}

/// Persistent lifecycle store for chunk embeddings.
///
/// Do not hold a `ChunkVectorStore` connection concurrently with a main
/// [`crate::storage::StorageHandle`] connection in the same async task.
/// The stores usually point at different SQLite files, but both use a
/// five-second `busy_timeout`; a future call path that opens both inside
/// one blocking section can turn writer contention into a visible
/// `SQLITE_BUSY` on whichever side reaches its timeout first.
pub struct ChunkVectorStore {
    conn: Connection,
}

impl ChunkVectorStore {
    /// Open or create the store at the provided sqlite path. Uses the
    /// historical `SemanticEmbedderIdentity::unversioned()` placeholder
    /// identity — preserved for callers that have not yet wired a real
    /// embedder pipeline through. Real callers should prefer
    /// [`Self::open_with_identity`] so the metadata-invalidation gate
    /// at [`ensure_semantic_store_metadata`] actually fires when the
    /// embedder behind the store changes.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_identity(path, SemanticEmbedderIdentity::unversioned())
    }

    /// Open or create the store at the provided sqlite path, recording
    /// the supplied embedder identity in `semantic_store_metadata`.
    ///
    /// br-ft-6utac: this is a SECOND, independent SQLite connection
    /// — it does NOT route through [`crate::storage::StorageHandle`]'s
    /// writer thread. Both stores apply `busy_timeout=5s` per
    /// connection. Holding a `ChunkVectorStore` connection
    /// concurrently with a [`crate::storage::PooledReadConn`] (or
    /// any other `StorageHandle::*_with_cx` async closure) inside
    /// the same blocking task can pin the slower side's
    /// busy_timeout window and surface `SQLITE_BUSY` to whichever
    /// caller blinks first. WAL mode isolates the per-DB-file
    /// writer state, so this is contention rather than a deadlock,
    /// but the call-site discipline is the same: open, do the
    /// work, drop the handle before re-entering the main store.
    ///
    /// If the persisted identity differs from `identity` the metadata
    /// invalidation gate fires atomically: stored embeddings are
    /// dropped, in-flight generations are marked `failed`, and
    /// metadata is updated to match the new identity.
    pub fn open_with_identity(
        path: impl AsRef<Path>,
        identity: SemanticEmbedderIdentity,
    ) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", 1)?;
        // Long-lived store; allow SQLite to retry under writer contention
        // instead of surfacing SQLITE_BUSY immediately to higher layers.
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        conn.execute_batch(SCHEMA_SQL)?;
        ensure_semantic_store_metadata(&mut conn, &identity)?;
        Ok(Self { conn })
    }

    /// Register (or refresh metadata for) a semantic generation.
    pub fn register_generation(
        &self,
        profile_id: &str,
        generation_id: &str,
        chunk_policy_version: &str,
        lexical_schema_version: &str,
    ) -> Result<()> {
        let now = now_epoch_seconds()?;
        self.conn.execute(
            "INSERT INTO semantic_generations (
                profile_id, generation_id, chunk_policy_version, lexical_schema_version,
                status, created_at, activated_at, retired_at
             ) VALUES (?1, ?2, ?3, ?4, 'building', ?5, NULL, NULL)
             ON CONFLICT(profile_id, generation_id) DO UPDATE SET
                chunk_policy_version = excluded.chunk_policy_version,
                lexical_schema_version = excluded.lexical_schema_version",
            params![
                profile_id,
                generation_id,
                chunk_policy_version,
                lexical_schema_version,
                now
            ],
        )?;
        Ok(())
    }

    /// Activate a generation for the profile and retire any prior active generation.
    pub fn activate_generation(&mut self, profile_id: &str, generation_id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let now = now_epoch_seconds()?;

        tx.execute(
            "UPDATE semantic_generations
             SET status = 'retired', retired_at = ?2
             WHERE profile_id = ?1 AND status = 'active' AND generation_id != ?3",
            params![profile_id, now, generation_id],
        )?;

        let updated = tx.execute(
            "UPDATE semantic_generations
             SET status = 'active',
                 activated_at = COALESCE(activated_at, ?3),
                 retired_at = NULL
             WHERE profile_id = ?1 AND generation_id = ?2",
            params![profile_id, generation_id, now],
        )?;

        if updated == 0 {
            return Err(ChunkVectorStoreError::GenerationNotFound {
                profile_id: profile_id.to_string(),
                generation_id: generation_id.to_string(),
            });
        }

        tx.commit()?;
        Ok(())
    }

    /// Fetch generation metadata for a profile+generation pair.
    pub fn generation(
        &self,
        profile_id: &str,
        generation_id: &str,
    ) -> Result<Option<SemanticGeneration>> {
        self.conn
            .query_row(
                "SELECT profile_id, generation_id, chunk_policy_version, lexical_schema_version,
                        status, created_at, activated_at, retired_at
                 FROM semantic_generations
                 WHERE profile_id = ?1 AND generation_id = ?2",
                params![profile_id, generation_id],
                decode_generation_row,
            )
            .optional()
            .map_err(ChunkVectorStoreError::from)
    }

    /// Fetch the currently active generation for a profile.
    pub fn active_generation(&self, profile_id: &str) -> Result<Option<SemanticGeneration>> {
        self.conn
            .query_row(
                "SELECT profile_id, generation_id, chunk_policy_version, lexical_schema_version,
                        status, created_at, activated_at, retired_at
                 FROM semantic_generations
                 WHERE profile_id = ?1 AND status = 'active'
                 ORDER BY activated_at DESC, created_at DESC
                 LIMIT 1",
                params![profile_id],
                decode_generation_row,
            )
            .optional()
            .map_err(ChunkVectorStoreError::from)
    }

    /// Upsert a chunk embedding for a profile generation.
    ///
    /// Invariants enforced:
    /// - generation must exist
    /// - chunk policy version must match generation policy version
    /// - embedding values must be finite and L2-normalized
    pub fn upsert_chunk_embedding(
        &mut self,
        payload: ChunkEmbeddingUpsert,
    ) -> Result<ChunkEmbeddingUpsertOutcome> {
        validate_embedding_vector(&payload.embedding)?;
        let generation = self
            .generation(&payload.profile_id, &payload.generation_id)?
            .ok_or_else(|| ChunkVectorStoreError::GenerationNotFound {
                profile_id: payload.profile_id.clone(),
                generation_id: payload.generation_id.clone(),
            })?;

        if generation.chunk_policy_version != payload.chunk.policy_version {
            return Err(ChunkVectorStoreError::ChunkPolicyMismatch {
                profile_id: payload.profile_id.clone(),
                generation_id: payload.generation_id.clone(),
                expected: generation.chunk_policy_version,
                actual: payload.chunk.policy_version.clone(),
            });
        }

        let embedding_dimension = usize_to_i64(payload.embedding.len(), "embedding_dimension")?;
        let embedding_blob = encode_f32_embedding_blob(&payload.embedding);
        let pane_id = u64_to_i64(payload.chunk.pane_id, "pane_id")?;
        let start_segment_id =
            u64_to_i64(payload.chunk.start_offset.segment_id, "start_segment_id")?;
        let start_ordinal = u64_to_i64(payload.chunk.start_offset.ordinal, "start_ordinal")?;
        let start_byte_offset =
            u64_to_i64(payload.chunk.start_offset.byte_offset, "start_byte_offset")?;
        let end_segment_id = u64_to_i64(payload.chunk.end_offset.segment_id, "end_segment_id")?;
        let end_ordinal = u64_to_i64(payload.chunk.end_offset.ordinal, "end_ordinal")?;
        let end_byte_offset = u64_to_i64(payload.chunk.end_offset.byte_offset, "end_byte_offset")?;
        let event_count = usize_to_i64(payload.chunk.event_count, "event_count")?;
        let text_chars = usize_to_i64(payload.chunk.text_chars, "text_chars")?;
        let direction = direction_to_str(payload.chunk.direction);
        let now = now_epoch_seconds()?;

        let tx = self.conn.transaction()?;

        let exists = tx.query_row(
            "SELECT EXISTS(
                SELECT 1
                FROM semantic_chunk_embeddings
                WHERE profile_id = ?1 AND generation_id = ?2 AND chunk_id = ?3
            )",
            params![
                payload.profile_id,
                payload.generation_id,
                payload.chunk.chunk_id
            ],
            |row| row.get::<_, i64>(0),
        )? == 1;

        tx.execute(
            "INSERT INTO semantic_chunk_embeddings (
                profile_id, generation_id, chunk_id, chunk_policy_version,
                pane_id, session_id, direction,
                start_segment_id, start_ordinal, start_byte_offset,
                end_segment_id, end_ordinal, end_byte_offset,
                event_count, text_chars, content_hash,
                embedding_dimension, embedding_vector, inserted_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7,
                ?8, ?9, ?10,
                ?11, ?12, ?13,
                ?14, ?15, ?16,
                ?17, ?18, ?19, ?19
            )
            ON CONFLICT(profile_id, generation_id, chunk_id) DO UPDATE SET
                chunk_policy_version = excluded.chunk_policy_version,
                pane_id = excluded.pane_id,
                session_id = excluded.session_id,
                direction = excluded.direction,
                start_segment_id = excluded.start_segment_id,
                start_ordinal = excluded.start_ordinal,
                start_byte_offset = excluded.start_byte_offset,
                end_segment_id = excluded.end_segment_id,
                end_ordinal = excluded.end_ordinal,
                end_byte_offset = excluded.end_byte_offset,
                event_count = excluded.event_count,
                text_chars = excluded.text_chars,
                content_hash = excluded.content_hash,
                embedding_dimension = excluded.embedding_dimension,
                embedding_vector = excluded.embedding_vector,
                updated_at = excluded.updated_at",
            params![
                payload.profile_id,
                payload.generation_id,
                payload.chunk.chunk_id,
                payload.chunk.policy_version,
                pane_id,
                payload.chunk.session_id,
                direction,
                start_segment_id,
                start_ordinal,
                start_byte_offset,
                end_segment_id,
                end_ordinal,
                end_byte_offset,
                event_count,
                text_chars,
                payload.chunk.content_hash,
                embedding_dimension,
                embedding_blob,
                now
            ],
        )?;

        tx.commit()?;
        Ok(ChunkEmbeddingUpsertOutcome {
            profile_id: payload.profile_id,
            generation_id: payload.generation_id,
            chunk_id: payload.chunk.chunk_id,
            was_update: exists,
        })
    }

    /// Retention-aware prune: delete chunks whose end ordinal is <= cutoff.
    pub fn prune_chunks_through_ordinal(
        &self,
        profile_id: &str,
        generation_id: &str,
        cutoff_end_ordinal: u64,
    ) -> Result<usize> {
        let cutoff = u64_to_i64(cutoff_end_ordinal, "cutoff_end_ordinal")?;
        let deleted = self.conn.execute(
            "DELETE FROM semantic_chunk_embeddings
             WHERE profile_id = ?1
               AND generation_id = ?2
               AND end_ordinal <= ?3",
            params![profile_id, generation_id, cutoff],
        )?;
        Ok(deleted)
    }

    /// Deterministic semantic retrieval for a profile generation.
    pub fn semantic_search(
        &self,
        profile_id: &str,
        generation_id: &str,
        query_vector: &[f32],
        limit: usize,
    ) -> Result<Vec<ChunkVectorHit>> {
        // ft-kt3yw: reject empty query vectors with a structured error so
        // direct semantic search callers can distinguish a broken
        // embedder/query pipeline from a valid no-hit result. This matches the
        // segment-embedding backend (`search_semantic_backend_with_scan_limit`)
        // and the adjacent non-finite guard below.
        if query_vector.is_empty() {
            return Err(ChunkVectorStoreError::InvalidVector(
                "query vector is empty".to_string(),
            ));
        }
        if query_vector.iter().any(|v| !v.is_finite()) {
            return Err(ChunkVectorStoreError::InvalidVector(
                "query vector contains non-finite values".to_string(),
            ));
        }

        let dimension = usize_to_i64(query_vector.len(), "query_vector dimension")?;
        let mut stmt = self.conn.prepare(
            "SELECT chunk_id, direction,
                    start_segment_id, start_ordinal, start_byte_offset,
                    end_segment_id, end_ordinal, end_byte_offset,
                    content_hash, embedding_vector
             FROM semantic_chunk_embeddings
             WHERE profile_id = ?1
               AND generation_id = ?2
               AND embedding_dimension = ?3
             ORDER BY chunk_id ASC",
        )?;

        let rows = stmt.query_map(params![profile_id, generation_id, dimension], |row| {
            let chunk_id: String = row.get(0)?;
            let direction_raw: String = row.get(1)?;
            let start_segment_id: i64 = row.get(2)?;
            let start_ordinal: i64 = row.get(3)?;
            let start_byte_offset: i64 = row.get(4)?;
            let end_segment_id: i64 = row.get(5)?;
            let end_ordinal: i64 = row.get(6)?;
            let end_byte_offset: i64 = row.get(7)?;
            let start_offset = ChunkSourceOffset {
                segment_id: u64::try_from(start_segment_id)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, start_segment_id))?,
                ordinal: u64::try_from(start_ordinal)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, start_ordinal))?,
                byte_offset: u64::try_from(start_byte_offset)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(4, start_byte_offset))?,
            };
            let end_offset = ChunkSourceOffset {
                segment_id: u64::try_from(end_segment_id)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(5, end_segment_id))?,
                ordinal: u64::try_from(end_ordinal)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(6, end_ordinal))?,
                byte_offset: u64::try_from(end_byte_offset)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(7, end_byte_offset))?,
            };
            let content_hash: String = row.get(8)?;
            let vector_blob: Vec<u8> = row.get(9)?;
            Ok((
                chunk_id,
                direction_raw,
                start_offset,
                end_offset,
                content_hash,
                vector_blob,
            ))
        })?;

        let mut hits = Vec::new();
        for row in rows {
            let (chunk_id, direction_raw, start_offset, end_offset, content_hash, vector_blob) =
                row?;
            let candidate = decode_f32_embedding_blob(&vector_blob, query_vector.len())?;
            let Some(score) = cosine_similarity(query_vector, &candidate) else {
                continue;
            };

            hits.push(ChunkVectorHit {
                profile_id: profile_id.to_string(),
                generation_id: generation_id.to_string(),
                chunk_id,
                score,
                direction: direction_from_str(&direction_raw)?,
                start_offset,
                end_offset,
                content_hash,
            });
        }

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.chunk_id.cmp(&b.chunk_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// Build a lexical/vector consistency report for a generation.
    pub fn drift_report(
        &self,
        profile_id: &str,
        generation_id: &str,
        expected_lexical_schema_version: &str,
        lexical_upto_ordinal: Option<u64>,
    ) -> Result<ChunkVectorDriftReport> {
        let generation = self.generation(profile_id, generation_id)?.ok_or_else(|| {
            ChunkVectorStoreError::GenerationNotFound {
                profile_id: profile_id.to_string(),
                generation_id: generation_id.to_string(),
            }
        })?;

        let (total_chunks_i64, max_end_ordinal_i64): (i64, Option<i64>) = self.conn.query_row(
            "SELECT COUNT(*), MAX(end_ordinal)
             FROM semantic_chunk_embeddings
             WHERE profile_id = ?1 AND generation_id = ?2",
            params![profile_id, generation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let chunks_beyond_lexical = if let Some(ordinal) = lexical_upto_ordinal {
            let ordinal_i64 = u64_to_i64(ordinal, "lexical_upto_ordinal")?;
            let count: i64 = self.conn.query_row(
                "SELECT COUNT(*)
                 FROM semantic_chunk_embeddings
                 WHERE profile_id = ?1
                   AND generation_id = ?2
                   AND end_ordinal > ?3",
                params![profile_id, generation_id, ordinal_i64],
                |row| row.get(0),
            )?;
            i64_to_u64(count, "chunks_beyond_lexical")?
        } else {
            0
        };

        let mut non_normalized_chunks = 0u64;
        let mut stmt = self.conn.prepare(
            "SELECT embedding_dimension, embedding_vector
             FROM semantic_chunk_embeddings
             WHERE profile_id = ?1 AND generation_id = ?2",
        )?;
        let rows = stmt.query_map(params![profile_id, generation_id], |row| {
            let dim_i64: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((dim_i64, blob))
        })?;

        for row in rows {
            let (dim_i64, blob) = row?;
            let dim = i64_to_usize(dim_i64, "embedding_dimension")?;
            let vec = decode_f32_embedding_blob(&blob, dim)?;
            if !is_l2_normalized(&vec) {
                non_normalized_chunks += 1;
            }
        }

        Ok(ChunkVectorDriftReport {
            profile_id: profile_id.to_string(),
            generation_id: generation_id.to_string(),
            chunk_policy_version: generation.chunk_policy_version,
            generation_status: generation.status,
            generation_lexical_schema_version: generation.lexical_schema_version.clone(),
            expected_lexical_schema_version: expected_lexical_schema_version.to_string(),
            lexical_schema_mismatch: generation.lexical_schema_version
                != expected_lexical_schema_version,
            lexical_upto_ordinal,
            total_chunks: i64_to_u64(total_chunks_i64, "total_chunks")?,
            max_vector_ordinal: max_end_ordinal_i64
                .map(|v| i64_to_u64(v, "max_vector_ordinal"))
                .transpose()?,
            chunks_beyond_lexical,
            non_normalized_chunks,
        })
    }
}

const SCHEMA_SQL: &str = r"
CREATE TABLE IF NOT EXISTS semantic_generations (
    profile_id TEXT NOT NULL,
    generation_id TEXT NOT NULL,
    chunk_policy_version TEXT NOT NULL,
    lexical_schema_version TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('building', 'active', 'retired', 'failed')),
    created_at INTEGER NOT NULL,
    activated_at INTEGER,
    retired_at INTEGER,
    PRIMARY KEY(profile_id, generation_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_semantic_generations_active_profile
    ON semantic_generations(profile_id)
    WHERE status = 'active';

CREATE TABLE IF NOT EXISTS semantic_store_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS semantic_chunk_embeddings (
    profile_id TEXT NOT NULL,
    generation_id TEXT NOT NULL,
    chunk_id TEXT NOT NULL,
    chunk_policy_version TEXT NOT NULL,
    pane_id INTEGER NOT NULL,
    session_id TEXT,
    direction TEXT NOT NULL CHECK(direction IN ('ingress', 'egress', 'mixed_glued')),
    start_segment_id INTEGER NOT NULL,
    start_ordinal INTEGER NOT NULL,
    start_byte_offset INTEGER NOT NULL,
    end_segment_id INTEGER NOT NULL,
    end_ordinal INTEGER NOT NULL,
    end_byte_offset INTEGER NOT NULL,
    event_count INTEGER NOT NULL,
    text_chars INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    embedding_dimension INTEGER NOT NULL,
    embedding_vector BLOB NOT NULL,
    inserted_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(profile_id, generation_id, chunk_id),
    FOREIGN KEY(profile_id, generation_id)
        REFERENCES semantic_generations(profile_id, generation_id)
        ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_semantic_chunk_span_identity
    ON semantic_chunk_embeddings (
        profile_id,
        generation_id,
        start_segment_id,
        start_ordinal,
        end_segment_id,
        end_ordinal,
        content_hash
    );

CREATE INDEX IF NOT EXISTS idx_semantic_chunk_generation_ordinals
    ON semantic_chunk_embeddings(profile_id, generation_id, start_ordinal, end_ordinal);
";

fn ensure_semantic_store_metadata(
    conn: &mut Connection,
    identity: &SemanticEmbedderIdentity,
) -> Result<()> {
    let expected = [
        (
            "semantic_schema_version",
            SEMANTIC_SCHEMA_VERSION.to_string(),
        ),
        (
            "semantic_embedder_version",
            identity.embedder_version.clone(),
        ),
        (
            "semantic_embedding_dimension",
            identity.embedding_dimension.to_string(),
        ),
    ];
    // Wrap the read → reset → metadata-update sequence in a single
    // transaction. Without it, a crash between DELETE FROM
    // semantic_chunk_embeddings and the metadata UPSERT leaves embeddings
    // gone but the stored version mismatched on next open — the same
    // invalidation triggers a second time, possibly racing with new
    // generations that began under the old version. BEGIN IMMEDIATE
    // matches the SQLite Class-4 recipe for read-then-write transactions.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut requires_reset = false;
    for (key, value) in &expected {
        let stored: Option<String> = tx
            .query_row(
                "SELECT value FROM semantic_store_metadata WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        if stored.as_deref() != Some(value.as_str()) {
            requires_reset = true;
        }
    }
    if requires_reset {
        tx.execute("DELETE FROM semantic_chunk_embeddings", [])?;
        tx.execute(
            "UPDATE semantic_generations
             SET status = 'failed'
             WHERE status IN ('building', 'active', 'retired')",
            [],
        )?;
    }
    for (key, value) in &expected {
        tx.execute(
            "INSERT INTO semantic_store_metadata(key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn decode_generation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SemanticGeneration> {
    let status_raw: String = row.get(4)?;
    let status = SemanticGenerationStatus::from_str(&status_raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(SemanticGeneration {
        profile_id: row.get(0)?,
        generation_id: row.get(1)?,
        chunk_policy_version: row.get(2)?,
        lexical_schema_version: row.get(3)?,
        status,
        created_at: row.get(5)?,
        activated_at: row.get(6)?,
        retired_at: row.get(7)?,
    })
}

fn direction_to_str(direction: ChunkDirection) -> &'static str {
    match direction {
        ChunkDirection::Ingress => "ingress",
        ChunkDirection::Egress => "egress",
        ChunkDirection::MixedGlued => "mixed_glued",
    }
}

fn direction_from_str(value: &str) -> Result<ChunkDirection> {
    match value {
        "ingress" => Ok(ChunkDirection::Ingress),
        "egress" => Ok(ChunkDirection::Egress),
        "mixed_glued" => Ok(ChunkDirection::MixedGlued),
        _ => Err(ChunkVectorStoreError::InvalidDbValue(format!(
            "unknown chunk direction: {value}"
        ))),
    }
}

fn encode_f32_embedding_blob(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(vector));
    for &value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_f32_embedding_blob(blob: &[u8], dimension: usize) -> Result<Vec<f32>> {
    let expected_len = dimension.checked_mul(std::mem::size_of::<f32>()).ok_or(
        ChunkVectorStoreError::IntegerOverflow("embedding blob length"),
    )?;
    if blob.len() != expected_len {
        return Err(ChunkVectorStoreError::InvalidDbValue(format!(
            "invalid embedding byte length: expected {expected_len}, got {}",
            blob.len()
        )));
    }

    let mut out = Vec::with_capacity(dimension);
    for chunk in blob.chunks_exact(4) {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return Err(ChunkVectorStoreError::InvalidDbValue(
                "embedding contains non-finite values".to_string(),
            ));
        }
        out.push(value);
    }
    Ok(out)
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }

    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let x64 = f64::from(x);
        let y64 = f64::from(y);
        dot = x64.mul_add(y64, dot);
        norm_a = x64.mul_add(x64, norm_a);
        norm_b = y64.mul_add(y64, norm_b);
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom <= f64::EPSILON {
        return None;
    }
    Some(dot / denom)
}

fn is_l2_normalized(vector: &[f32]) -> bool {
    if vector.is_empty() {
        return false;
    }
    let norm = vector
        .iter()
        .map(|v| f64::from(*v) * f64::from(*v))
        .sum::<f64>()
        .sqrt();
    (norm - 1.0).abs() <= 1e-3
}

fn validate_embedding_vector(vector: &[f32]) -> Result<()> {
    if vector.is_empty() {
        return Err(ChunkVectorStoreError::InvalidVector(
            "vector is empty".to_string(),
        ));
    }
    if vector.iter().any(|v| !v.is_finite()) {
        return Err(ChunkVectorStoreError::InvalidVector(
            "vector contains non-finite values".to_string(),
        ));
    }
    if !is_l2_normalized(vector) {
        return Err(ChunkVectorStoreError::InvalidVector(
            "vector must be L2-normalized".to_string(),
        ));
    }
    Ok(())
}

fn now_epoch_seconds() -> Result<i64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| ChunkVectorStoreError::InvalidDbValue(err.to_string()))?;
    u64_to_i64(now.as_secs(), "now_epoch_seconds")
}

fn usize_to_i64(value: usize, field: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| ChunkVectorStoreError::IntegerOverflow(field))
}

fn u64_to_i64(value: u64, field: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| ChunkVectorStoreError::IntegerOverflow(field))
}

fn i64_to_u64(value: i64, field: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| ChunkVectorStoreError::IntegerOverflow(field))
}

fn i64_to_usize(value: i64, field: &'static str) -> Result<usize> {
    usize::try_from(value).map_err(|_| ChunkVectorStoreError::IntegerOverflow(field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    // ── Helper functions ──────────────────────────────────────────────────

    fn open_in_memory() -> ChunkVectorStore {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", 1).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        ensure_semantic_store_metadata(&mut conn, &SemanticEmbedderIdentity::unversioned())
            .unwrap();
        ChunkVectorStore { conn }
    }

    fn make_normalized_vec(dim: usize) -> Vec<f32> {
        let val = 1.0 / (dim as f32).sqrt();
        vec![val; dim]
    }

    fn make_chunk(
        chunk_id: &str,
        pane_id: u64,
        direction: ChunkDirection,
        start_ordinal: u64,
        end_ordinal: u64,
    ) -> SemanticChunk {
        SemanticChunk {
            chunk_id: chunk_id.to_string(),
            policy_version: "ft.recorder.chunking.v1".to_string(),
            pane_id,
            session_id: Some("sess-test".to_string()),
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
            event_ids: vec!["evt-1".to_string()],
            event_count: 1,
            occurred_at_start_ms: 1000,
            occurred_at_end_ms: 1100,
            text_chars: 50,
            content_hash: format!("hash-{chunk_id}"),
            text: format!("content of {chunk_id}"),
            overlap: None,
        }
    }

    fn setup_generation(store: &ChunkVectorStore) {
        store
            .register_generation("prof-1", "gen-1", "ft.recorder.chunking.v1", "lex-v1")
            .unwrap();
    }

    fn make_upsert(
        chunk_id: &str,
        start_ordinal: u64,
        end_ordinal: u64,
        dim: usize,
    ) -> ChunkEmbeddingUpsert {
        ChunkEmbeddingUpsert {
            profile_id: "prof-1".to_string(),
            generation_id: "gen-1".to_string(),
            chunk: make_chunk(
                chunk_id,
                1,
                ChunkDirection::Egress,
                start_ordinal,
                end_ordinal,
            ),
            embedding: make_normalized_vec(dim),
        }
    }

    // ── SemanticGenerationStatus tests ────────────────────────────────────

    #[test]
    fn generation_status_from_str_valid() {
        assert_eq!(
            SemanticGenerationStatus::from_str("building").unwrap(),
            SemanticGenerationStatus::Building
        );
        assert_eq!(
            SemanticGenerationStatus::from_str("active").unwrap(),
            SemanticGenerationStatus::Active
        );
        assert_eq!(
            SemanticGenerationStatus::from_str("retired").unwrap(),
            SemanticGenerationStatus::Retired
        );
        assert_eq!(
            SemanticGenerationStatus::from_str("failed").unwrap(),
            SemanticGenerationStatus::Failed
        );
    }

    #[test]
    fn generation_status_from_str_invalid() {
        assert!(SemanticGenerationStatus::from_str("unknown").is_err());
        assert!(SemanticGenerationStatus::from_str("").is_err());
    }

    #[test]
    fn generation_status_serde_roundtrip() {
        for status in [
            SemanticGenerationStatus::Building,
            SemanticGenerationStatus::Active,
            SemanticGenerationStatus::Retired,
            SemanticGenerationStatus::Failed,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: SemanticGenerationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, parsed);
        }
    }

    // ── direction_to_str / direction_from_str tests ───────────────────────

    #[test]
    fn direction_str_roundtrip() {
        for dir in [
            ChunkDirection::Ingress,
            ChunkDirection::Egress,
            ChunkDirection::MixedGlued,
        ] {
            let s = direction_to_str(dir);
            let parsed = direction_from_str(s).unwrap();
            assert_eq!(dir, parsed);
        }
    }

    #[test]
    fn direction_from_str_invalid() {
        assert!(direction_from_str("unknown").is_err());
    }

    // ── encode/decode embedding blob tests ────────────────────────────────

    #[test]
    fn embedding_blob_roundtrip() {
        let original = vec![1.0f32, -0.5, 0.25, 2.75];
        let blob = encode_f32_embedding_blob(&original);
        let decoded = decode_f32_embedding_blob(&blob, original.len()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn embedding_blob_empty_roundtrip() {
        let original: Vec<f32> = vec![];
        let blob = encode_f32_embedding_blob(&original);
        let decoded = decode_f32_embedding_blob(&blob, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn decode_blob_wrong_length_fails() {
        let blob = vec![0u8; 12]; // 3 floats
        let result = decode_f32_embedding_blob(&blob, 4); // expects 4 floats (16 bytes)
        assert!(result.is_err());
    }

    #[test]
    fn decode_blob_rejects_nan() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&f32::NAN.to_le_bytes());
        let result = decode_f32_embedding_blob(&blob, 1);
        assert!(result.is_err());
    }

    #[test]
    fn decode_blob_rejects_infinity() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&f32::INFINITY.to_le_bytes());
        let result = decode_f32_embedding_blob(&blob, 1);
        assert!(result.is_err());
    }

    // ── cosine_similarity tests ───────────────────────────────────────────

    #[test]
    fn cosine_similarity_identical_vectors() {
        let v = vec![1.0f32, 0.0, 0.0];
        let sim = cosine_similarity(&v, &v).unwrap();
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_opposite_vectors() {
        let a = vec![1.0f32, 0.0];
        let b = vec![-1.0f32, 0.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!((sim - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_different_lengths_returns_none() {
        let a = vec![1.0f32, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        assert!(cosine_similarity(&a, &b).is_none());
    }

    #[test]
    fn cosine_similarity_empty_returns_none() {
        let empty: Vec<f32> = vec![];
        assert!(cosine_similarity(&empty, &empty).is_none());
    }

    #[test]
    fn cosine_similarity_zero_vector_returns_none() {
        let zero = vec![0.0f32, 0.0, 0.0];
        let other = vec![1.0f32, 0.0, 0.0];
        assert!(cosine_similarity(&zero, &other).is_none());
    }

    // ── is_l2_normalized tests ────────────────────────────────────────────

    #[test]
    fn is_l2_normalized_unit_vector() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!(is_l2_normalized(&v));
    }

    #[test]
    fn is_l2_normalized_uniform() {
        let dim = 4;
        let val = 1.0 / (dim as f32).sqrt();
        let v = vec![val; dim];
        assert!(is_l2_normalized(&v));
    }

    #[test]
    fn is_l2_normalized_unnormalized_fails() {
        let v = vec![2.0f32, 0.0, 0.0];
        assert!(!is_l2_normalized(&v));
    }

    #[test]
    fn is_l2_normalized_empty_fails() {
        assert!(!is_l2_normalized(&[]));
    }

    // ── validate_embedding_vector tests ───────────────────────────────────

    #[test]
    fn validate_empty_vector_fails() {
        let result = validate_embedding_vector(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn validate_non_finite_fails() {
        let result = validate_embedding_vector(&[f32::NAN]);
        assert!(result.is_err());
    }

    #[test]
    fn validate_unnormalized_fails() {
        let result = validate_embedding_vector(&[2.0, 0.0, 0.0]);
        assert!(result.is_err());
    }

    #[test]
    fn validate_normalized_succeeds() {
        let v = make_normalized_vec(3);
        assert!(validate_embedding_vector(&v).is_ok());
    }

    // ── Integer conversion tests ──────────────────────────────────────────

    #[test]
    fn u64_to_i64_valid() {
        assert_eq!(u64_to_i64(42, "test").unwrap(), 42i64);
    }

    #[test]
    fn u64_to_i64_overflow() {
        assert!(u64_to_i64(u64::MAX, "test").is_err());
    }

    #[test]
    fn i64_to_u64_valid() {
        assert_eq!(i64_to_u64(42, "test").unwrap(), 42u64);
    }

    #[test]
    fn i64_to_u64_negative_fails() {
        assert!(i64_to_u64(-1, "test").is_err());
    }

    #[test]
    fn usize_to_i64_valid() {
        assert_eq!(usize_to_i64(100, "test").unwrap(), 100i64);
    }

    // ── Store: register_generation tests ──────────────────────────────────

    #[test]
    fn register_generation_creates_building_status() {
        let store = open_in_memory();
        store
            .register_generation("p1", "g1", "chunk-v1", "lex-v1")
            .unwrap();

        let generation = store.generation("p1", "g1").unwrap().unwrap();
        assert_eq!(generation.profile_id, "p1");
        assert_eq!(generation.generation_id, "g1");
        assert_eq!(generation.status, SemanticGenerationStatus::Building);
        assert!(generation.activated_at.is_none());
    }

    #[test]
    fn register_generation_upsert_updates_versions() {
        let store = open_in_memory();
        store
            .register_generation("p1", "g1", "chunk-v1", "lex-v1")
            .unwrap();
        store
            .register_generation("p1", "g1", "chunk-v2", "lex-v2")
            .unwrap();

        let generation = store.generation("p1", "g1").unwrap().unwrap();
        assert_eq!(generation.chunk_policy_version, "chunk-v2");
        assert_eq!(generation.lexical_schema_version, "lex-v2");
    }

    // ── Store: activate_generation tests ──────────────────────────────────

    #[test]
    fn activate_generation_sets_active() {
        let mut store = open_in_memory();
        store
            .register_generation("p1", "g1", "chunk-v1", "lex-v1")
            .unwrap();
        store.activate_generation("p1", "g1").unwrap();

        let generation = store.generation("p1", "g1").unwrap().unwrap();
        assert_eq!(generation.status, SemanticGenerationStatus::Active);
        assert!(generation.activated_at.is_some());
    }

    #[test]
    fn activate_generation_retires_previous() {
        let mut store = open_in_memory();
        store
            .register_generation("p1", "g1", "chunk-v1", "lex-v1")
            .unwrap();
        store.activate_generation("p1", "g1").unwrap();

        store
            .register_generation("p1", "g2", "chunk-v1", "lex-v1")
            .unwrap();
        store.activate_generation("p1", "g2").unwrap();

        let g1 = store.generation("p1", "g1").unwrap().unwrap();
        assert_eq!(g1.status, SemanticGenerationStatus::Retired);
        assert!(g1.retired_at.is_some());

        let g2 = store.generation("p1", "g2").unwrap().unwrap();
        assert_eq!(g2.status, SemanticGenerationStatus::Active);
    }

    #[test]
    fn activate_nonexistent_generation_fails() {
        let mut store = open_in_memory();
        let result = store.activate_generation("p1", "missing");
        assert!(result.is_err());
    }

    // ── Store: active_generation tests ────────────────────────────────────

    #[test]
    fn active_generation_returns_none_when_none_active() {
        let store = open_in_memory();
        store
            .register_generation("p1", "g1", "chunk-v1", "lex-v1")
            .unwrap();
        assert!(store.active_generation("p1").unwrap().is_none());
    }

    #[test]
    fn active_generation_returns_active() {
        let mut store = open_in_memory();
        store
            .register_generation("p1", "g1", "chunk-v1", "lex-v1")
            .unwrap();
        store.activate_generation("p1", "g1").unwrap();

        let generation = store.active_generation("p1").unwrap().unwrap();
        assert_eq!(generation.generation_id, "g1");
    }

    // ── Store: upsert_chunk_embedding tests ───────────────────────────────

    #[test]
    fn upsert_new_embedding_returns_was_update_false() {
        let mut store = open_in_memory();
        setup_generation(&store);

        let outcome = store
            .upsert_chunk_embedding(make_upsert("c1", 0, 5, 4))
            .unwrap();
        assert!(!outcome.was_update);
        assert_eq!(outcome.chunk_id, "c1");
    }

    #[test]
    fn upsert_existing_embedding_returns_was_update_true() {
        let mut store = open_in_memory();
        setup_generation(&store);

        store
            .upsert_chunk_embedding(make_upsert("c1", 0, 5, 4))
            .unwrap();
        let outcome = store
            .upsert_chunk_embedding(make_upsert("c1", 0, 5, 4))
            .unwrap();
        assert!(outcome.was_update);
    }

    #[test]
    fn upsert_without_generation_fails() {
        let mut store = open_in_memory();
        let result = store.upsert_chunk_embedding(make_upsert("c1", 0, 5, 4));
        assert!(result.is_err());
    }

    #[test]
    fn upsert_with_wrong_policy_version_fails() {
        let mut store = open_in_memory();
        setup_generation(&store);

        let mut payload = make_upsert("c1", 0, 5, 4);
        payload.chunk.policy_version = "wrong-version".to_string();
        let result = store.upsert_chunk_embedding(payload);
        assert!(result.is_err());
    }

    #[test]
    fn upsert_with_empty_embedding_fails() {
        let mut store = open_in_memory();
        setup_generation(&store);

        let mut payload = make_upsert("c1", 0, 5, 4);
        payload.embedding = vec![];
        let result = store.upsert_chunk_embedding(payload);
        assert!(result.is_err());
    }

    #[test]
    fn upsert_with_nan_embedding_fails() {
        let mut store = open_in_memory();
        setup_generation(&store);

        let mut payload = make_upsert("c1", 0, 5, 4);
        payload.embedding = vec![f32::NAN; 4];
        let result = store.upsert_chunk_embedding(payload);
        assert!(result.is_err());
    }

    // ── Store: prune_chunks_through_ordinal tests ─────────────────────────

    #[test]
    fn prune_deletes_chunks_up_to_cutoff() {
        let mut store = open_in_memory();
        setup_generation(&store);

        store
            .upsert_chunk_embedding(make_upsert("c1", 0, 5, 4))
            .unwrap();
        store
            .upsert_chunk_embedding(make_upsert("c2", 6, 10, 4))
            .unwrap();
        store
            .upsert_chunk_embedding(make_upsert("c3", 11, 15, 4))
            .unwrap();

        let deleted = store
            .prune_chunks_through_ordinal("prof-1", "gen-1", 10)
            .unwrap();
        assert_eq!(deleted, 2); // c1 (end=5) and c2 (end=10) deleted

        // c3 should remain
        let hits = store
            .semantic_search("prof-1", "gen-1", &make_normalized_vec(4), 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id, "c3");
    }

    #[test]
    fn prune_returns_zero_when_nothing_to_delete() {
        let store = open_in_memory();
        setup_generation(&store);

        let deleted = store
            .prune_chunks_through_ordinal("prof-1", "gen-1", 100)
            .unwrap();
        assert_eq!(deleted, 0);
    }

    // ── Store: semantic_search tests ──────────────────────────────────────

    #[test]
    fn semantic_search_empty_store_returns_empty() {
        let store = open_in_memory();
        setup_generation(&store);

        let hits = store
            .semantic_search("prof-1", "gen-1", &make_normalized_vec(4), 10)
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn semantic_search_empty_query_fails() {
        // ft-kt3yw: an empty query vector is an invalid query, not a valid
        // no-hit result. It must be rejected with a structured error, matching
        // the non-finite guard and the segment-embedding backend.
        let store = open_in_memory();
        let result = store.semantic_search("prof-1", "gen-1", &[], 10);
        let rejected = matches!(result, Err(ChunkVectorStoreError::InvalidVector(_)));
        assert!(
            rejected,
            "empty query vector must be rejected with InvalidVector, got {result:?}"
        );
    }

    #[test]
    fn semantic_search_non_finite_query_fails() {
        let store = open_in_memory();
        let result = store.semantic_search("prof-1", "gen-1", &[f32::NAN], 10);
        assert!(result.is_err());
    }

    #[test]
    fn semantic_search_returns_results_sorted_by_score() {
        let mut store = open_in_memory();
        setup_generation(&store);

        // Insert two chunks with different embeddings
        let mut p1 = make_upsert("c1", 0, 5, 3);
        p1.embedding = normalize_vec(&[1.0, 0.0, 0.0]);
        store.upsert_chunk_embedding(p1).unwrap();

        let mut p2 = make_upsert("c2", 6, 10, 3);
        p2.embedding = normalize_vec(&[0.9, 0.1, 0.0]);
        store.upsert_chunk_embedding(p2).unwrap();

        // Query with [1, 0, 0] => c1 should be highest score
        let query = normalize_vec(&[1.0, 0.0, 0.0]);
        let hits = store
            .semantic_search("prof-1", "gen-1", &query, 10)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].score >= hits[1].score);
        assert_eq!(hits[0].chunk_id, "c1");
    }

    #[test]
    fn semantic_search_respects_limit() {
        let mut store = open_in_memory();
        setup_generation(&store);

        for i in 0..5 {
            store
                .upsert_chunk_embedding(make_upsert(&format!("c{i}"), i * 10, i * 10 + 5, 4))
                .unwrap();
        }

        let hits = store
            .semantic_search("prof-1", "gen-1", &make_normalized_vec(4), 2)
            .unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn semantic_search_dimension_mismatch_returns_no_hits() {
        let mut store = open_in_memory();
        setup_generation(&store);

        store
            .upsert_chunk_embedding(make_upsert("c1", 0, 5, 4))
            .unwrap();

        // Query with different dimension
        let hits = store
            .semantic_search("prof-1", "gen-1", &make_normalized_vec(8), 10)
            .unwrap();
        assert!(hits.is_empty());
    }

    // ── Store: drift_report tests ─────────────────────────────────────────

    #[test]
    fn drift_report_empty_generation() {
        let store = open_in_memory();
        setup_generation(&store);

        let report = store
            .drift_report("prof-1", "gen-1", "lex-v1", None)
            .unwrap();
        assert_eq!(report.total_chunks, 0);
        assert!(!report.lexical_schema_mismatch);
        assert_eq!(report.chunks_beyond_lexical, 0);
    }

    #[test]
    fn drift_report_detects_schema_mismatch() {
        let store = open_in_memory();
        setup_generation(&store);

        let report = store
            .drift_report("prof-1", "gen-1", "lex-v999", None)
            .unwrap();
        assert!(report.lexical_schema_mismatch);
        assert_eq!(report.expected_lexical_schema_version, "lex-v999");
        assert_eq!(report.generation_lexical_schema_version, "lex-v1");
    }

    #[test]
    fn drift_report_counts_chunks_beyond_lexical() {
        let mut store = open_in_memory();
        setup_generation(&store);

        store
            .upsert_chunk_embedding(make_upsert("c1", 0, 5, 4))
            .unwrap();
        store
            .upsert_chunk_embedding(make_upsert("c2", 6, 15, 4))
            .unwrap();

        let report = store
            .drift_report("prof-1", "gen-1", "lex-v1", Some(10))
            .unwrap();
        assert_eq!(report.total_chunks, 2);
        assert_eq!(report.chunks_beyond_lexical, 1); // c2 end_ordinal=15 > 10
    }

    #[test]
    fn drift_report_nonexistent_generation_fails() {
        let store = open_in_memory();
        let result = store.drift_report("missing", "missing", "lex-v1", None);
        assert!(result.is_err());
    }

    #[test]
    fn drift_report_detects_non_normalized_chunks() {
        let mut store = open_in_memory();
        setup_generation(&store);

        // Insert a chunk with normalized vector
        store
            .upsert_chunk_embedding(make_upsert("c1", 0, 5, 4))
            .unwrap();

        // Directly insert a non-normalized vector via SQL
        let blob = encode_f32_embedding_blob(&[2.0, 0.0, 0.0, 0.0]);
        store
            .conn
            .execute(
                "INSERT INTO semantic_chunk_embeddings (
                    profile_id, generation_id, chunk_id, chunk_policy_version,
                    pane_id, session_id, direction,
                    start_segment_id, start_ordinal, start_byte_offset,
                    end_segment_id, end_ordinal, end_byte_offset,
                    event_count, text_chars, content_hash,
                    embedding_dimension, embedding_vector, inserted_at, updated_at
                ) VALUES (
                    'prof-1', 'gen-1', 'c-bad', 'ft.recorder.chunking.v1',
                    1, NULL, 'egress',
                    0, 10, 1000,
                    0, 15, 1500,
                    1, 50, 'hash-bad',
                    4, ?1, 1000, 1000
                )",
                params![blob],
            )
            .unwrap();

        let report = store
            .drift_report("prof-1", "gen-1", "lex-v1", None)
            .unwrap();
        assert_eq!(report.non_normalized_chunks, 1);
    }

    #[test]
    fn semantic_store_metadata_mismatch_invalidates_embeddings() {
        let db = NamedTempFile::new().unwrap();
        let path = db.path().to_path_buf();

        {
            let mut store = ChunkVectorStore::open(&path).unwrap();
            setup_generation(&store);
            store
                .upsert_chunk_embedding(make_upsert("c1", 0, 5, 4))
                .unwrap();
            assert_eq!(
                store
                    .semantic_search("prof-1", "gen-1", &make_normalized_vec(4), 10)
                    .unwrap()
                    .len(),
                1
            );
            store
                .conn
                .execute(
                    "UPDATE semantic_store_metadata
                     SET value = '999'
                     WHERE key = 'semantic_schema_version'",
                    [],
                )
                .unwrap();
        }

        let reopened = ChunkVectorStore::open(&path).unwrap();
        assert!(
            reopened
                .semantic_search("prof-1", "gen-1", &make_normalized_vec(4), 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened
                .generation("prof-1", "gen-1")
                .unwrap()
                .unwrap()
                .status,
            SemanticGenerationStatus::Failed
        );
    }

    #[test]
    fn semantic_store_missing_metadata_invalidates_embeddings() {
        let db = NamedTempFile::new().unwrap();
        let path = db.path().to_path_buf();

        {
            let mut store = ChunkVectorStore::open(&path).unwrap();
            setup_generation(&store);
            store
                .upsert_chunk_embedding(make_upsert("c1", 0, 5, 4))
                .unwrap();
            assert_eq!(
                store
                    .semantic_search("prof-1", "gen-1", &make_normalized_vec(4), 10)
                    .unwrap()
                    .len(),
                1
            );
            store
                .conn
                .execute(
                    "DELETE FROM semantic_store_metadata
                     WHERE key = 'semantic_schema_version'",
                    [],
                )
                .unwrap();
        }

        let reopened = ChunkVectorStore::open(&path).unwrap();
        assert!(
            reopened
                .semantic_search("prof-1", "gen-1", &make_normalized_vec(4), 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened
                .generation("prof-1", "gen-1")
                .unwrap()
                .unwrap()
                .status,
            SemanticGenerationStatus::Failed
        );
    }

    // ── ChunkVectorHit serde roundtrip ────────────────────────────────────

    #[test]
    fn chunk_vector_hit_serde_roundtrip() {
        let hit = ChunkVectorHit {
            profile_id: "p1".to_string(),
            generation_id: "g1".to_string(),
            chunk_id: "c1".to_string(),
            score: 0.95,
            direction: ChunkDirection::Egress,
            start_offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 0,
                byte_offset: 0,
            },
            end_offset: ChunkSourceOffset {
                segment_id: 0,
                ordinal: 5,
                byte_offset: 500,
            },
            content_hash: "hash123".to_string(),
        };

        let json = serde_json::to_string(&hit).unwrap();
        let parsed: ChunkVectorHit = serde_json::from_str(&json).unwrap();
        assert_eq!(hit.chunk_id, parsed.chunk_id);
        assert!((hit.score - parsed.score).abs() < f64::EPSILON);
    }

    // ── ChunkVectorDriftReport serde roundtrip ────────────────────────────

    #[test]
    fn drift_report_serde_roundtrip() {
        let report = ChunkVectorDriftReport {
            profile_id: "p1".to_string(),
            generation_id: "g1".to_string(),
            chunk_policy_version: "chunk-v1".to_string(),
            generation_status: SemanticGenerationStatus::Active,
            generation_lexical_schema_version: "lex-v1".to_string(),
            expected_lexical_schema_version: "lex-v1".to_string(),
            lexical_schema_mismatch: false,
            lexical_upto_ordinal: Some(100),
            total_chunks: 50,
            max_vector_ordinal: Some(95),
            chunks_beyond_lexical: 3,
            non_normalized_chunks: 0,
        };

        let json = serde_json::to_string(&report).unwrap();
        let parsed: ChunkVectorDriftReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, parsed);
    }

    // ── Helper for normalizing a vector ───────────────────────────────────

    fn normalize_vec(v: &[f32]) -> Vec<f32> {
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm == 0.0 {
            return v.to_vec();
        }
        v.iter().map(|x| x / norm).collect()
    }

    // ── ensure_semantic_store_metadata atomicity tests (ft-xphjm) ─────────

    /// Open the schema directly without invoking the metadata-invalidation
    /// pass. This lets a test pre-stage embeddings + a stale metadata row
    /// and then call ensure_semantic_store_metadata explicitly to observe
    /// its atomicity behaviour from the outside.
    fn open_schema_only(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "foreign_keys", 1).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn
    }

    fn count_rows(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn metadata_value(conn: &Connection, key: &str) -> Option<String> {
        conn.query_row(
            "SELECT value FROM semantic_store_metadata WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap()
    }

    #[test]
    fn ensure_metadata_invalidation_is_atomic_on_stale_schema_version() {
        // Stage a DB whose semantic_store_metadata claims an obsolete
        // schema version while embeddings + an active generation are
        // already present. ensure_semantic_store_metadata must, in a
        // SINGLE atomic transaction:
        //   * DELETE the embeddings (they're stale w.r.t. the new schema)
        //   * UPDATE the active generation to status='failed'
        //   * UPSERT the metadata to the current version
        // After return, both halves must be visible together.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();

        // Stage 1: write the schema, insert an embedding and a generation,
        // and stamp metadata with a deliberately stale schema version.
        {
            let conn = open_schema_only(&path);
            conn.execute(
                "INSERT INTO semantic_generations (
                    profile_id, generation_id, chunk_policy_version,
                    lexical_schema_version, status, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    "prof-stale",
                    "gen-stale",
                    "ft.recorder.chunking.v1",
                    "lex-v1",
                    "active",
                    1_700_000_000_i64
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO semantic_chunk_embeddings (
                    profile_id, generation_id, chunk_id, chunk_policy_version, pane_id,
                    direction, start_segment_id, start_ordinal, start_byte_offset,
                    end_segment_id, end_ordinal, end_byte_offset,
                    event_count, text_chars, content_hash, embedding_dimension,
                    embedding_vector, inserted_at, updated_at
                ) VALUES (
                    'prof-stale', 'gen-stale', 'chunk-1', 'ft.recorder.chunking.v1', 1,
                    'egress', 0, 0, 0, 0, 1, 100, 1, 50, 'hash-1', 4,
                    X'00000000000000000000000000000000', 1, 1
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO semantic_store_metadata(key, value)
                 VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params!["semantic_schema_version", "999999"],
            )
            .unwrap();
            assert_eq!(count_rows(&conn, "semantic_chunk_embeddings"), 1);
            assert_eq!(
                metadata_value(&conn, "semantic_schema_version").as_deref(),
                Some("999999")
            );
        }

        // Stage 2: invoke the production invalidation pass.
        {
            let mut conn = open_schema_only(&path);
            ensure_semantic_store_metadata(&mut conn, &SemanticEmbedderIdentity::unversioned())
                .unwrap();

            // Both halves of the invalidation tx must be visible.
            assert_eq!(
                count_rows(&conn, "semantic_chunk_embeddings"),
                0,
                "embeddings must be cleared when schema version is stale"
            );
            assert_eq!(
                metadata_value(&conn, "semantic_schema_version").as_deref(),
                Some(SEMANTIC_SCHEMA_VERSION.to_string().as_str()),
                "metadata must advance to the current schema version"
            );
            // Generation should be marked failed (was active).
            let status: String = conn
                .query_row(
                    "SELECT status FROM semantic_generations WHERE generation_id = 'gen-stale'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                status, "failed",
                "active generation must transition to failed under invalidation"
            );
        }

        // Stage 3: re-running invalidation on the now-current DB must
        // be a no-op — the matching predicates short-circuit and no new
        // DELETE / UPDATE fires (pinned via row counts staying at 0/0).
        {
            let mut conn = open_schema_only(&path);
            ensure_semantic_store_metadata(&mut conn, &SemanticEmbedderIdentity::unversioned())
                .unwrap();
            assert_eq!(count_rows(&conn, "semantic_chunk_embeddings"), 0);
            assert_eq!(
                metadata_value(&conn, "semantic_schema_version").as_deref(),
                Some(SEMANTIC_SCHEMA_VERSION.to_string().as_str()),
            );
        }
    }

    #[test]
    fn ensure_metadata_invalidation_rolls_back_on_simulated_panic_between_steps() {
        // Fault injection: the production code wraps DELETE → UPDATE →
        // UPSERT in a single BEGIN IMMEDIATE transaction. If a crash
        // landed between any two of those steps, none of them must
        // persist — the next open must see the original (pre-tx) state
        // and run a clean invalidation.
        //
        // We can't actually crash inside `ensure_semantic_store_metadata`
        // from a test, but we CAN demonstrate the rollback semantics it
        // relies on: open a transaction with the same shape, perform the
        // DELETE, then drop the Transaction without commit. rusqlite's
        // Drop impl issues ROLLBACK; the row must reappear.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();

        // Stage embeddings + stale metadata exactly as in the prior test.
        {
            let conn = open_schema_only(&path);
            conn.execute(
                "INSERT INTO semantic_generations (
                    profile_id, generation_id, chunk_policy_version,
                    lexical_schema_version, status, created_at
                ) VALUES ('prof-stale', 'gen-stale', 'ft.recorder.chunking.v1', 'lex-v1', 'active', 1700000000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO semantic_chunk_embeddings (
                    profile_id, generation_id, chunk_id, chunk_policy_version, pane_id,
                    direction, start_segment_id, start_ordinal, start_byte_offset,
                    end_segment_id, end_ordinal, end_byte_offset,
                    event_count, text_chars, content_hash, embedding_dimension,
                    embedding_vector, inserted_at, updated_at
                ) VALUES (
                    'prof-stale', 'gen-stale', 'chunk-1', 'ft.recorder.chunking.v1', 1,
                    'egress', 0, 0, 0, 0, 1, 100, 1, 50, 'hash-1', 4,
                    X'00000000000000000000000000000000', 1, 1
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO semantic_store_metadata(key, value) VALUES ('semantic_schema_version', '999999')",
                [],
            )
            .unwrap();
        }

        // Simulate "crash mid-transaction": BEGIN IMMEDIATE, DELETE the
        // embeddings, then drop the Transaction without committing. The
        // production function uses the exact same begin-behaviour, so
        // this exercises the same rollback path the production code
        // would hit under panic unwind.
        {
            let mut conn = open_schema_only(&path);
            {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .unwrap();
                tx.execute("DELETE FROM semantic_chunk_embeddings", [])
                    .unwrap();
                // Verify mid-tx the DELETE is observed by THIS connection.
                assert_eq!(count_rows(&tx, "semantic_chunk_embeddings"), 0);
                // Drop without commit — rusqlite::Transaction::drop issues
                // ROLLBACK. This is the panic-unwind contract.
            }
            // After rollback the embedding row must be back, and the
            // stale metadata must be unchanged — the failed tx left no
            // partial state behind.
            assert_eq!(
                count_rows(&conn, "semantic_chunk_embeddings"),
                1,
                "rolled-back DELETE must leave the embedding row intact"
            );
            assert_eq!(
                metadata_value(&conn, "semantic_schema_version").as_deref(),
                Some("999999"),
                "rolled-back tx must not touch metadata either"
            );
        }

        // Now run the real production invalidation. It should observe
        // the still-stale state and atomically clean up both halves —
        // proof that a prior aborted attempt did not corrupt the state
        // such that the second attempt sees mismatched intermediates.
        {
            let mut conn = open_schema_only(&path);
            ensure_semantic_store_metadata(&mut conn, &SemanticEmbedderIdentity::unversioned())
                .unwrap();
            assert_eq!(count_rows(&conn, "semantic_chunk_embeddings"), 0);
            assert_eq!(
                metadata_value(&conn, "semantic_schema_version").as_deref(),
                Some(SEMANTIC_SCHEMA_VERSION.to_string().as_str()),
            );
        }
    }

    // ── Embedder identity invalidation tests (ft-0etp3) ────────────────────

    /// Insert one fake embedding row + one active generation against the
    /// supplied identity. Used by the embedder-flip tests below to stage
    /// pre-flip state without going through the (feature-gated) real
    /// upsert API.
    fn stage_embeddings_for_identity(path: &std::path::Path, identity: &SemanticEmbedderIdentity) {
        let mut conn = open_schema_only(path);
        ensure_semantic_store_metadata(&mut conn, identity).unwrap();
        conn.execute(
            "INSERT INTO semantic_generations (
                profile_id, generation_id, chunk_policy_version,
                lexical_schema_version, status, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "prof-flip",
                "gen-flip",
                "ft.recorder.chunking.v1",
                "lex-v1",
                "active",
                1_700_000_000_i64,
            ],
        )
        .unwrap();
        let dim = identity.embedding_dimension.max(4) as i64;
        // Tiny vector so the stored bytes are deterministic regardless of dim.
        let blob = vec![0u8; (dim * 4) as usize];
        conn.execute(
            "INSERT INTO semantic_chunk_embeddings (
                profile_id, generation_id, chunk_id, chunk_policy_version, pane_id,
                direction, start_segment_id, start_ordinal, start_byte_offset,
                end_segment_id, end_ordinal, end_byte_offset,
                event_count, text_chars, content_hash, embedding_dimension,
                embedding_vector, inserted_at, updated_at
            ) VALUES (
                'prof-flip', 'gen-flip', 'chunk-1', 'ft.recorder.chunking.v1', 1,
                'egress', 0, 0, 0, 0, 1, 100, 1, 50, 'hash-1', ?1,
                ?2, 1, 1
            )",
            params![dim, blob],
        )
        .unwrap();
    }

    #[test]
    fn flipping_embedder_version_invalidates_existing_embeddings() {
        // Two pipelines with the same dimension but different model
        // names produce vectors that are NOT mutually comparable
        // (different training, different normalization). Opening the
        // store with identity B after staging under identity A must
        // atomically drop the stale embeddings.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();

        let identity_a =
            SemanticEmbedderIdentity::from_fastembed_pipeline("fastembed-bge-small@v1.5", 384);
        let identity_b =
            SemanticEmbedderIdentity::from_fastembed_pipeline("fastembed-bge-base@v1.5", 384);

        stage_embeddings_for_identity(&path, &identity_a);

        // Sanity: under identity A the row is there.
        {
            let conn = open_schema_only(&path);
            assert_eq!(count_rows(&conn, "semantic_chunk_embeddings"), 1);
            assert_eq!(
                metadata_value(&conn, "semantic_embedder_version").as_deref(),
                Some("fastembed-bge-small@v1.5")
            );
        }

        // Open under identity B → invalidation tx fires.
        let _store = ChunkVectorStore::open_with_identity(&path, identity_b.clone()).unwrap();

        let conn = open_schema_only(&path);
        assert_eq!(
            count_rows(&conn, "semantic_chunk_embeddings"),
            0,
            "embedder rename must drop the prior pipeline's vectors"
        );
        assert_eq!(
            metadata_value(&conn, "semantic_embedder_version").as_deref(),
            Some("fastembed-bge-base@v1.5"),
            "metadata must record the new embedder identity"
        );
        let status: String = conn
            .query_row(
                "SELECT status FROM semantic_generations WHERE generation_id = 'gen-flip'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");
    }

    #[test]
    fn flipping_embedding_dimension_invalidates_existing_embeddings() {
        // Same model name, different output dim — for example a model
        // upgrade from a 384-dim variant to a 768-dim variant. Old
        // vectors cannot be compared against new ones, so the flip
        // must invalidate.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();

        let identity_384 =
            SemanticEmbedderIdentity::from_fastembed_pipeline("fastembed-multimodel", 384);
        let identity_768 =
            SemanticEmbedderIdentity::from_fastembed_pipeline("fastembed-multimodel", 768);

        stage_embeddings_for_identity(&path, &identity_384);

        let _store = ChunkVectorStore::open_with_identity(&path, identity_768.clone()).unwrap();

        let conn = open_schema_only(&path);
        assert_eq!(count_rows(&conn, "semantic_chunk_embeddings"), 0);
        assert_eq!(
            metadata_value(&conn, "semantic_embedding_dimension").as_deref(),
            Some("768"),
        );
    }

    #[test]
    fn reopening_with_same_identity_is_a_noop() {
        // Stability check: opening the same store with the same identity
        // a second time must NOT invalidate. Otherwise every restart
        // would burn the entire embedding cache.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();

        let identity =
            SemanticEmbedderIdentity::from_fastembed_pipeline("fastembed-bge-small@v1.5", 384);

        stage_embeddings_for_identity(&path, &identity);
        // Re-open — same identity, embeddings must survive.
        let _store = ChunkVectorStore::open_with_identity(&path, identity.clone()).unwrap();

        let conn = open_schema_only(&path);
        assert_eq!(
            count_rows(&conn, "semantic_chunk_embeddings"),
            1,
            "same-identity reopen must NOT invalidate the cache"
        );
        assert_eq!(
            metadata_value(&conn, "semantic_embedder_version").as_deref(),
            Some("fastembed-bge-small@v1.5"),
        );
    }

    #[test]
    fn legacy_open_uses_unversioned_identity() {
        // The historical `ChunkVectorStore::open(path)` shape is
        // preserved as a thin wrapper that uses the unversioned
        // placeholder. This test pins that behavior so a callers
        // upgrading to identity-aware open don't get a silent
        // semantic shift on the legacy path.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();

        let _store = ChunkVectorStore::open(&path).unwrap();

        let conn = open_schema_only(&path);
        assert_eq!(
            metadata_value(&conn, "semantic_embedder_version").as_deref(),
            Some("unversioned"),
        );
        assert_eq!(
            metadata_value(&conn, "semantic_embedding_dimension").as_deref(),
            Some("0"),
        );
    }

    #[test]
    fn semantic_embedder_identity_default_is_unversioned() {
        let default = SemanticEmbedderIdentity::default();
        assert_eq!(default, SemanticEmbedderIdentity::unversioned());
        assert_eq!(default.embedder_version, "unversioned");
        assert_eq!(default.embedding_dimension, 0);
    }
}
