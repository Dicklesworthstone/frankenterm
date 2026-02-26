//! FSVI (FrankenSearch Vector Index) bridge adapter for the B4 migration.
//!
//! Wraps `frankensearch::index::VectorIndex` and provides a compatible search
//! API that integrates with the existing `SearchOrchestrator` pipeline.
//!
//! # ID mapping
//!
//! FTVI uses numeric `u64` IDs; FSVI uses string `doc_id`s. This bridge
//! maps between them using decimal string representation (e.g., `42_u64` ↔ `"42"`).
//!
//! # Feature gate
//!
//! This module requires the `frankensearch` feature.

use frankensearch::index::VectorIndex;
use std::path::{Path, PathBuf};

use crate::Error;

/// Vector index backend selector.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VectorIndexBackend {
    /// Legacy in-memory FTVI index.
    #[default]
    Ftvi,
    /// FrankenSearch FSVI file-backed index.
    Fsvi,
}

/// A search hit from the FSVI adapter.
#[derive(Debug, Clone)]
pub struct FsviHit {
    /// Numeric document ID (parsed from string doc_id).
    pub id: u64,
    /// Similarity score (dot product or cosine).
    pub score: f32,
}

/// Adapter that wraps a frankensearch `VectorIndex` for use in frankenterm.
///
/// Manages the u64 ↔ string doc_id mapping and provides a search API
/// compatible with `FtviIndex::search()`.
pub struct FsviAdapter {
    index: VectorIndex,
    path: PathBuf,
}

impl FsviAdapter {
    /// Open an existing FSVI index from disk.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let index = VectorIndex::open(path).map_err(|e| {
            Error::Runtime(format!(
                "failed to open FSVI index at {}: {e}",
                path.display()
            ))
        })?;
        Ok(Self {
            index,
            path: path.to_path_buf(),
        })
    }

    /// Create a new empty FSVI index.
    pub fn create(path: &Path, embedder_id: &str, dimension: usize) -> Result<Self, Error> {
        let writer = VectorIndex::create(path, embedder_id, dimension).map_err(|e| {
            Error::Runtime(format!(
                "failed to create FSVI index at {}: {e}",
                path.display()
            ))
        })?;
        writer.finish().map_err(|e| {
            Error::Runtime(format!(
                "failed to finish FSVI writer at {}: {e}",
                path.display()
            ))
        })?;
        Self::open(path)
    }

    /// Get the vector dimension.
    #[must_use]
    pub fn dimension(&self) -> usize {
        self.index.dimension()
    }

    /// Get the record count (excluding tombstones).
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.index.record_count()
    }

    /// Get the embedder ID stored in the index.
    #[must_use]
    pub fn embedder_id(&self) -> &str {
        self.index.embedder_id()
    }

    /// Whether the index needs compaction (high tombstone ratio).
    #[must_use]
    pub fn needs_compaction(&self) -> bool {
        self.index.needs_compaction()
    }

    /// The file path of this index.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a vector with a u64 ID (converted to string doc_id).
    pub fn append(&mut self, id: u64, vector: &[f32]) -> Result<(), Error> {
        let doc_id = id.to_string();
        self.index
            .append(&doc_id, vector)
            .map_err(|e| Error::Runtime(format!("FSVI append failed for id {id}: {e}")))
    }

    /// Append a batch of (id, vector) pairs.
    pub fn append_batch(&mut self, entries: &[(u64, Vec<f32>)]) -> Result<(), Error> {
        let string_entries: Vec<(String, Vec<f32>)> = entries
            .iter()
            .map(|(id, vec)| (id.to_string(), vec.clone()))
            .collect();
        self.index
            .append_batch(&string_entries)
            .map_err(|e| Error::Runtime(format!("FSVI batch append failed: {e}")))
    }

    /// Soft-delete a document by u64 ID.
    pub fn soft_delete(&mut self, id: u64) -> Result<bool, Error> {
        let doc_id = id.to_string();
        self.index
            .soft_delete(&doc_id)
            .map_err(|e| Error::Runtime(format!("FSVI soft_delete failed for id {id}: {e}")))
    }

    /// Compact the index (merge WAL, vacuum tombstones).
    pub fn compact(&mut self) -> Result<(), Error> {
        self.index
            .compact()
            .map_err(|e| Error::Runtime(format!("FSVI compact failed: {e}")))?;
        Ok(())
    }

    /// Search for top-k nearest neighbors by dot product similarity.
    ///
    /// Returns `(u64_id, score)` pairs in descending score order, matching
    /// the `FtviIndex::search()` API.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
        if query.len() != self.dimension() || k == 0 {
            return Vec::new();
        }

        // Use frankensearch's search_top_k (brute-force cosine similarity).
        let results = match self.index.search_top_k(query, k, None) {
            Ok(hits) => hits,
            Err(_) => return Vec::new(),
        };

        results
            .iter()
            .filter_map(|hit| {
                let id: u64 = hit.doc_id.parse().ok()?;
                Some((id, hit.score))
            })
            .collect()
    }
}

impl std::fmt::Debug for FsviAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsviAdapter")
            .field("path", &self.path)
            .field("dimension", &self.index.dimension())
            .field("record_count", &self.index.record_count())
            .field("embedder_id", &self.index.embedder_id())
            .finish()
    }
}

/// Convert FTVI binary data to FSVI format at the given output path.
///
/// Maps u64 record IDs to string doc_ids via decimal representation.
pub fn convert_ftvi_to_fsvi(
    ftvi_data: &[u8],
    output_path: &Path,
    embedder_id: &str,
) -> Result<usize, Error> {
    use super::vector_index::FtviIndex;

    let ftvi = FtviIndex::from_bytes(ftvi_data)
        .map_err(|e| Error::Runtime(format!("failed to parse FTVI data: {e}")))?;

    if ftvi.is_empty() {
        // Create an empty FSVI index
        let writer = VectorIndex::create(output_path, embedder_id, ftvi.dimension())
            .map_err(|e| Error::Runtime(format!("failed to create FSVI index: {e}")))?;
        writer
            .finish()
            .map_err(|e| Error::Runtime(format!("failed to finish FSVI writer: {e}")))?;
        return Ok(0);
    }

    let mut writer = VectorIndex::create(output_path, embedder_id, ftvi.dimension())
        .map_err(|e| Error::Runtime(format!("failed to create FSVI writer: {e}")))?;

    let count = ftvi.len();
    for i in 0..count {
        let id = ftvi.id_at(i);
        let vector = ftvi.vector_at(i);
        let doc_id = id.to_string();
        writer
            .write_record(&doc_id, vector)
            .map_err(|e| Error::Runtime(format!("FSVI write_record failed for id {id}: {e}")))?;
    }

    writer
        .finish()
        .map_err(|e| Error::Runtime(format!("failed to finish FSVI writer: {e}")))?;

    Ok(count)
}

/// Summary of an FSVI index for diagnostics.
#[derive(Debug, Clone)]
pub struct FsviIndexInfo {
    pub path: PathBuf,
    pub dimension: usize,
    pub record_count: usize,
    pub embedder_id: String,
    pub embedder_revision: String,
    pub quantization: String,
    pub needs_compaction: bool,
    pub tombstone_count: usize,
}

impl FsviAdapter {
    /// Produce a diagnostic summary of this index.
    #[must_use]
    pub fn info(&self) -> FsviIndexInfo {
        FsviIndexInfo {
            path: self.path.clone(),
            dimension: self.index.dimension(),
            record_count: self.index.record_count(),
            embedder_id: self.index.embedder_id().to_string(),
            embedder_revision: self.index.embedder_revision().to_string(),
            quantization: format!("{:?}", self.index.metadata().quantization),
            needs_compaction: self.index.needs_compaction(),
            tombstone_count: self.index.tombstone_count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::vector_index::{FtviIndex, write_ftvi_vec};
    use super::*;
    use tempfile::TempDir;

    fn temp_fsvi_path(dir: &TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    // ── VectorIndexBackend ────────────────────────────────────────────

    #[test]
    fn backend_default_is_ftvi() {
        assert_eq!(VectorIndexBackend::default(), VectorIndexBackend::Ftvi);
    }

    #[test]
    fn backend_eq_and_debug() {
        assert_eq!(VectorIndexBackend::Ftvi, VectorIndexBackend::Ftvi);
        assert_ne!(VectorIndexBackend::Ftvi, VectorIndexBackend::Fsvi);
        assert!(format!("{:?}", VectorIndexBackend::Fsvi).contains("Fsvi"));
    }

    // ── FsviAdapter lifecycle ─────────────────────────────────────────

    #[test]
    fn create_and_open_empty_index() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "empty.fsvi");
        let adapter = FsviAdapter::create(&path, "test-embedder", 64).unwrap();
        assert_eq!(adapter.dimension(), 64);
        assert_eq!(adapter.record_count(), 0);
        assert_eq!(adapter.embedder_id(), "test-embedder");
    }

    #[test]
    fn append_and_search() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "test.fsvi");

        // Create with a simple embedder
        {
            let writer = VectorIndex::create(&path, "test", 4).unwrap();
            writer.finish().unwrap();
        }

        let mut adapter = FsviAdapter::open(&path).unwrap();
        adapter.append(1, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        adapter.append(2, &[0.0, 1.0, 0.0, 0.0]).unwrap();
        adapter.append(3, &[0.7, 0.7, 0.0, 0.0]).unwrap();

        let results = adapter.search(&[1.0, 0.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        // Top result should be id=1 (exact match)
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn search_empty_index() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "empty_search.fsvi");
        let adapter = FsviAdapter::create(&path, "test", 4).unwrap();
        let results = adapter.search(&[1.0, 0.0, 0.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn search_dimension_mismatch_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "dim_mismatch.fsvi");
        let adapter = FsviAdapter::create(&path, "test", 4).unwrap();
        // Query has wrong dimension
        let results = adapter.search(&[1.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn search_k_zero_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "k_zero.fsvi");
        let mut adapter = FsviAdapter::create(&path, "test", 4).unwrap();
        adapter.append(1, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        let results = adapter.search(&[1.0, 0.0, 0.0, 0.0], 0);
        assert!(results.is_empty());
    }

    #[test]
    fn append_batch() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "batch.fsvi");
        let mut adapter = FsviAdapter::create(&path, "test", 2).unwrap();

        let entries = vec![
            (10, vec![1.0, 0.0]),
            (20, vec![0.0, 1.0]),
            (30, vec![0.5, 0.5]),
        ];
        adapter.append_batch(&entries).unwrap();
        // Records go to WAL first; need compact to merge into main index
        adapter.compact().unwrap();
        assert_eq!(adapter.record_count(), 3);
    }

    #[test]
    fn soft_delete() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "delete.fsvi");
        let mut adapter = FsviAdapter::create(&path, "test", 2).unwrap();

        adapter.append(1, &[1.0, 0.0]).unwrap();
        adapter.append(2, &[0.0, 1.0]).unwrap();
        // Compact WAL into main index before asserting record count
        adapter.compact().unwrap();
        assert_eq!(adapter.record_count(), 2);

        let deleted = adapter.soft_delete(1).unwrap();
        assert!(deleted);

        // Tombstone doesn't reduce record_count, but search should skip it
    }

    #[test]
    fn soft_delete_nonexistent_returns_false() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "delete_miss.fsvi");
        let mut adapter = FsviAdapter::create(&path, "test", 2).unwrap();
        adapter.append(1, &[1.0, 0.0]).unwrap();

        let deleted = adapter.soft_delete(999).unwrap();
        assert!(!deleted);
    }

    // ── FTVI → FSVI conversion ────────────────────────────────────────

    #[test]
    fn convert_empty_ftvi() {
        let dir = TempDir::new().unwrap();
        let ftvi_data = write_ftvi_vec(4, &[]).unwrap();
        let out = temp_fsvi_path(&dir, "converted_empty.fsvi");
        let count = convert_ftvi_to_fsvi(&ftvi_data, &out, "hash-128").unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn convert_ftvi_to_fsvi_roundtrip() {
        let dir = TempDir::new().unwrap();

        // Build FTVI data
        let records: Vec<(u64, &[f32])> = vec![
            (1, &[1.0, 0.0, 0.0, 0.0]),
            (2, &[0.0, 1.0, 0.0, 0.0]),
            (3, &[0.5, 0.5, 0.0, 0.0]),
        ];
        let ftvi_data = write_ftvi_vec(4, &records).unwrap();

        // Convert to FSVI
        let out = temp_fsvi_path(&dir, "converted.fsvi");
        let count = convert_ftvi_to_fsvi(&ftvi_data, &out, "test-embedder").unwrap();
        assert_eq!(count, 3);

        // Open FSVI and search
        let adapter = FsviAdapter::open(&out).unwrap();
        assert_eq!(adapter.dimension(), 4);
        assert_eq!(adapter.record_count(), 3);
        assert_eq!(adapter.embedder_id(), "test-embedder");

        // Search should find id=1 as best match for [1,0,0,0]
        let results = adapter.search(&[1.0, 0.0, 0.0, 0.0], 2);
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 1);
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn convert_preserves_search_ordering() {
        let dir = TempDir::new().unwrap();

        let records: Vec<(u64, &[f32])> =
            vec![(10, &[1.0, 0.0]), (20, &[0.0, 1.0]), (30, &[0.9, 0.1])];
        let ftvi_data = write_ftvi_vec(2, &records).unwrap();

        // Search in FTVI
        let ftvi = FtviIndex::from_bytes(&ftvi_data).unwrap();
        let ftvi_results = ftvi.search(&[1.0, 0.0], 3);

        // Convert and search in FSVI
        let out = temp_fsvi_path(&dir, "ordering.fsvi");
        convert_ftvi_to_fsvi(&ftvi_data, &out, "test").unwrap();
        let adapter = FsviAdapter::open(&out).unwrap();
        let fsvi_results = adapter.search(&[1.0, 0.0], 3);

        // Both should have the same top result
        assert_eq!(ftvi_results[0].0, fsvi_results[0].0);
        // Score ordering should match (exact scores may differ due to f16 quantization differences)
        let ftvi_ids: Vec<u64> = ftvi_results.iter().map(|r| r.0).collect();
        let fsvi_ids: Vec<u64> = fsvi_results.iter().map(|r| r.0).collect();
        assert_eq!(ftvi_ids, fsvi_ids, "search ordering should match");
    }

    // ── FsviIndexInfo ─────────────────────────────────────────────────

    #[test]
    fn info_diagnostic() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "info.fsvi");
        let adapter = FsviAdapter::create(&path, "diag-embedder", 128).unwrap();
        let info = adapter.info();
        assert_eq!(info.dimension, 128);
        assert_eq!(info.embedder_id, "diag-embedder");
        assert_eq!(info.record_count, 0);
        assert!(!info.needs_compaction);
        assert_eq!(info.tombstone_count, 0);
    }

    // ── Debug impl ────────────────────────────────────────────────────

    #[test]
    fn adapter_debug() {
        let dir = TempDir::new().unwrap();
        let path = temp_fsvi_path(&dir, "debug.fsvi");
        let adapter = FsviAdapter::create(&path, "test", 32).unwrap();
        let dbg = format!("{:?}", adapter);
        assert!(dbg.contains("FsviAdapter"));
        assert!(dbg.contains("dimension"));
    }

    // ── FsviHit ───────────────────────────────────────────────────────

    #[test]
    fn fsvi_hit_clone_debug() {
        let hit = FsviHit {
            id: 42,
            score: 0.95,
        };
        let hit2 = hit.clone();
        assert_eq!(hit2.id, 42);
        assert!((hit2.score - 0.95).abs() < f32::EPSILON);
        let dbg = format!("{:?}", hit);
        assert!(dbg.contains("42"));
    }

    // ── Error handling ────────────────────────────────────────────────

    #[test]
    fn open_nonexistent_path_errors() {
        let result = FsviAdapter::open(Path::new("/tmp/nonexistent_fsvi_path_12345.fsvi"));
        assert!(result.is_err());
    }

    #[test]
    fn convert_bad_ftvi_data_errors() {
        let dir = TempDir::new().unwrap();
        let out = temp_fsvi_path(&dir, "bad.fsvi");
        let result = convert_ftvi_to_fsvi(b"BADDATA", &out, "test");
        assert!(result.is_err());
    }
}
