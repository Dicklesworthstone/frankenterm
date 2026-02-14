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
pub struct ChunkVectorStore {
    conn: Connection,
}

impl ChunkVectorStore {
    /// Open or create the store at the provided sqlite path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", 1)?;
        conn.execute_batch(SCHEMA_SQL)?;
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
        if query_vector.is_empty() {
            return Ok(Vec::new());
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
        dot += x64 * y64;
        norm_a += x64 * x64;
        norm_b += y64 * y64;
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
