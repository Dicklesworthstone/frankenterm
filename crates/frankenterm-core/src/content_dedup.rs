//! Content-addressable deduplication for captured pane output.
//!
//! Exploits the high repetition in AI agent terminal output by hashing each
//! captured segment and storing identical content only once.  Cross-pane dedup
//! is automatic: the same output in N panes produces 1 content block with
//! `ref_count = N`.
//!
//! # Architecture
//!
//! ```text
//! capture cycle ──► hash(content) ──► content_store (unique blocks)
//!                       │                    ▲
//!                       └── output_segments ─┘  (ref by hash)
//! ```
//!
//! The module provides:
//! - SHA-256 content hashing
//! - Reference-counted content blocks
//! - A [`ContentStore`] trait for pluggable storage backends
//! - Dedup statistics and reporting

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// =============================================================================
// Content Hashing
// =============================================================================

/// Compute the SHA-256 hex digest of content bytes.
#[must_use]
pub fn content_hash(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// A reference-counted content block in the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    /// SHA-256 hex hash of the content.
    pub hash: String,
    /// Size of the content in bytes.
    pub byte_size: usize,
    /// Number of segments referencing this content.
    pub ref_count: u64,
    /// Epoch ms when first stored.
    pub first_seen_ms: u64,
    /// Epoch ms of most recent reference.
    pub last_seen_ms: u64,
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the dedup engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedupConfig {
    /// Minimum content size (bytes) to bother deduplicating.
    /// Very small segments (< 32 bytes) have hash overhead exceeding savings.
    pub min_dedup_size: usize,
    /// Maximum content size (bytes) for inline storage.
    /// Content larger than this is always stored in the content store.
    pub max_inline_size: usize,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            min_dedup_size: 32,
            max_inline_size: 256,
        }
    }
}

impl DedupConfig {
    /// Whether a segment of the given size should be deduplicated.
    #[must_use]
    pub fn should_dedup(&self, content_len: usize) -> bool {
        content_len >= self.min_dedup_size
    }
}

// =============================================================================
// Dedup Statistics
// =============================================================================

/// Statistics about the dedup store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DedupStats {
    /// Total number of segment references (including duplicates).
    pub total_references: u64,
    /// Number of unique content blocks in the store.
    pub unique_blocks: u64,
    /// Total bytes stored (unique content only).
    pub unique_bytes: u64,
    /// Total logical bytes (if all references were stored independently).
    pub logical_bytes: u64,
    /// Deduplication ratio (logical_bytes / unique_bytes).
    /// Higher = more savings. 1.0 = no dedup benefit.
    pub dedup_ratio: f64,
    /// Bytes saved by deduplication.
    pub bytes_saved: u64,
}

impl DedupStats {
    /// Compute derived fields from raw counts.
    #[must_use]
    pub fn finalize(mut self) -> Self {
        if self.unique_bytes > 0 {
            self.dedup_ratio = self.logical_bytes as f64 / self.unique_bytes as f64;
        } else {
            self.dedup_ratio = 1.0;
        }
        self.bytes_saved = self.logical_bytes.saturating_sub(self.unique_bytes);
        self
    }
}

// =============================================================================
// Content Store Trait
// =============================================================================

/// Result of a store operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreResult {
    /// Content was new — inserted into the store.
    Inserted,
    /// Content already existed — ref_count incremented.
    Deduplicated,
}

/// Trait for content-addressable storage backends.
///
/// Implementations handle the actual persistence (SQLite, in-memory, etc.).
/// The trait enables testing with mocks.
pub trait ContentStore: Send + Sync {
    /// Store content by hash, incrementing ref_count if it already exists.
    ///
    /// Returns whether the content was new or deduplicated.
    fn store(
        &mut self,
        hash: &str,
        content: &[u8],
        timestamp_ms: u64,
    ) -> Result<StoreResult, String>;

    /// Retrieve content by hash.
    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String>;

    /// Decrement ref_count for a hash. Returns the new ref_count.
    /// If ref_count reaches 0, the content can be garbage collected.
    fn decrement_ref(&mut self, hash: &str) -> Result<u64, String>;

    /// Remove content blocks with ref_count == 0.
    /// Returns the number of blocks removed.
    fn gc(&mut self) -> Result<usize, String>;

    /// Get current dedup statistics.
    fn stats(&self) -> Result<DedupStats, String>;

    /// Check whether a hash exists in the store.
    fn contains(&self, hash: &str) -> Result<bool, String>;
}

// =============================================================================
// Dedup Engine
// =============================================================================

/// Result of processing a segment through the dedup engine.
#[derive(Debug, Clone)]
pub struct DedupResult {
    /// The content hash.
    pub hash: String,
    /// Whether the content was new or already existed.
    pub outcome: StoreResult,
    /// Size of the content in bytes.
    pub content_len: usize,
    /// Whether the content was below the dedup threshold (stored inline).
    pub stored_inline: bool,
}

/// The dedup engine processes output segments and manages content-addressed storage.
pub struct DedupEngine<S: ContentStore> {
    config: DedupConfig,
    store: S,
    /// Counters for reporting.
    total_processed: u64,
    total_deduplicated: u64,
    total_inserted: u64,
    total_inline: u64,
}

impl<S: ContentStore> DedupEngine<S> {
    /// Create a new dedup engine.
    pub fn new(config: DedupConfig, store: S) -> Self {
        Self {
            config,
            store,
            total_processed: 0,
            total_deduplicated: 0,
            total_inserted: 0,
            total_inline: 0,
        }
    }

    /// Process a captured segment, storing or deduplicating as appropriate.
    pub fn process_segment(
        &mut self,
        content: &[u8],
        timestamp_ms: u64,
    ) -> Result<DedupResult, String> {
        self.total_processed += 1;

        let hash = content_hash(content);

        // Small content: skip dedup, store inline
        if !self.config.should_dedup(content.len()) {
            self.total_inline += 1;
            return Ok(DedupResult {
                hash,
                outcome: StoreResult::Inserted,
                content_len: content.len(),
                stored_inline: true,
            });
        }

        let outcome = self.store.store(&hash, content, timestamp_ms)?;

        match outcome {
            StoreResult::Inserted => self.total_inserted += 1,
            StoreResult::Deduplicated => self.total_deduplicated += 1,
        }

        Ok(DedupResult {
            hash,
            outcome,
            content_len: content.len(),
            stored_inline: false,
        })
    }

    /// Retrieve content by hash from the underlying store.
    pub fn get_content(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.store.get(hash)
    }

    /// Release a reference to content (e.g., when a segment is deleted).
    pub fn release(&mut self, hash: &str) -> Result<u64, String> {
        self.store.decrement_ref(hash)
    }

    /// Run garbage collection to remove unreferenced content.
    pub fn gc(&mut self) -> Result<usize, String> {
        self.store.gc()
    }

    /// Get dedup statistics from the store.
    pub fn stats(&self) -> Result<DedupStats, String> {
        self.store.stats()
    }

    /// Engine-level processing counters.
    #[must_use]
    pub fn counters(&self) -> EngineCounters {
        EngineCounters {
            total_processed: self.total_processed,
            total_deduplicated: self.total_deduplicated,
            total_inserted: self.total_inserted,
            total_inline: self.total_inline,
        }
    }

    /// Get the config.
    #[must_use]
    pub fn config(&self) -> &DedupConfig {
        &self.config
    }
}

/// Engine processing counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineCounters {
    pub total_processed: u64,
    pub total_deduplicated: u64,
    pub total_inserted: u64,
    pub total_inline: u64,
}

impl EngineCounters {
    /// Dedup hit rate (0.0 - 1.0).
    #[must_use]
    pub fn dedup_rate(&self) -> f64 {
        let non_inline = self.total_deduplicated + self.total_inserted;
        if non_inline == 0 {
            0.0
        } else {
            self.total_deduplicated as f64 / non_inline as f64
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ── In-memory content store ───────────────────────────────────────

    #[derive(Debug, Default)]
    struct MemoryStore {
        blocks: HashMap<String, (Vec<u8>, ContentBlock)>,
    }

    impl ContentStore for MemoryStore {
        fn store(
            &mut self,
            hash: &str,
            content: &[u8],
            timestamp_ms: u64,
        ) -> Result<StoreResult, String> {
            if let Some((_data, block)) = self.blocks.get_mut(hash) {
                block.ref_count += 1;
                block.last_seen_ms = timestamp_ms;
                Ok(StoreResult::Deduplicated)
            } else {
                self.blocks.insert(
                    hash.to_string(),
                    (
                        content.to_vec(),
                        ContentBlock {
                            hash: hash.to_string(),
                            byte_size: content.len(),
                            ref_count: 1,
                            first_seen_ms: timestamp_ms,
                            last_seen_ms: timestamp_ms,
                        },
                    ),
                );
                Ok(StoreResult::Inserted)
            }
        }

        fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self.blocks.get(hash).map(|(data, _)| data.clone()))
        }

        fn decrement_ref(&mut self, hash: &str) -> Result<u64, String> {
            if let Some((_data, block)) = self.blocks.get_mut(hash) {
                block.ref_count = block.ref_count.saturating_sub(1);
                Ok(block.ref_count)
            } else {
                Err(format!("hash not found: {hash}"))
            }
        }

        fn gc(&mut self) -> Result<usize, String> {
            let before = self.blocks.len();
            self.blocks.retain(|_, (_, block)| block.ref_count > 0);
            Ok(before - self.blocks.len())
        }

        fn stats(&self) -> Result<DedupStats, String> {
            let unique_blocks = self.blocks.len() as u64;
            let unique_bytes: u64 = self.blocks.values().map(|(_, b)| b.byte_size as u64).sum();
            let total_references: u64 = self.blocks.values().map(|(_, b)| b.ref_count).sum();
            let logical_bytes: u64 = self
                .blocks
                .values()
                .map(|(_, b)| b.byte_size as u64 * b.ref_count)
                .sum();

            Ok(DedupStats {
                total_references,
                unique_blocks,
                unique_bytes,
                logical_bytes,
                ..Default::default()
            }
            .finalize())
        }

        fn contains(&self, hash: &str) -> Result<bool, String> {
            Ok(self.blocks.contains_key(hash))
        }
    }

    fn engine() -> DedupEngine<MemoryStore> {
        DedupEngine::new(DedupConfig::default(), MemoryStore::default())
    }

    // ── Hashing tests ─────────────────────────────────────────────────

    #[test]
    fn hash_deterministic() {
        let data = b"hello world";
        let h1 = content_hash(data);
        let h2 = content_hash(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_different_for_different_content() {
        let h1 = content_hash(b"hello");
        let h2 = content_hash(b"world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_is_hex_string() {
        let h = content_hash(b"test");
        assert_eq!(h.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_empty_content() {
        let h = content_hash(b"");
        assert_eq!(h.len(), 64);
        // SHA-256 of empty = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert!(h.starts_with("e3b0c442"));
    }

    // ── Config tests ──────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let c = DedupConfig::default();
        assert_eq!(c.min_dedup_size, 32);
        assert_eq!(c.max_inline_size, 256);
    }

    #[test]
    fn config_should_dedup() {
        let c = DedupConfig {
            min_dedup_size: 64,
            ..Default::default()
        };
        assert!(!c.should_dedup(32));
        assert!(!c.should_dedup(63));
        assert!(c.should_dedup(64));
        assert!(c.should_dedup(1000));
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = DedupConfig {
            min_dedup_size: 128,
            max_inline_size: 512,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: DedupConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.min_dedup_size, 128);
        assert_eq!(parsed.max_inline_size, 512);
    }

    // ── Engine: basic store/retrieve ──────────────────────────────────

    #[test]
    fn store_new_content() {
        let mut eng = engine();
        let content = vec![0u8; 100]; // Above min_dedup_size
        let result = eng.process_segment(&content, 1000).unwrap();

        assert_eq!(result.outcome, StoreResult::Inserted);
        assert!(!result.stored_inline);
        assert_eq!(result.content_len, 100);
    }

    #[test]
    fn store_duplicate_content() {
        let mut eng = engine();
        let content = vec![42u8; 100];

        let r1 = eng.process_segment(&content, 1000).unwrap();
        assert_eq!(r1.outcome, StoreResult::Inserted);

        let r2 = eng.process_segment(&content, 2000).unwrap();
        assert_eq!(r2.outcome, StoreResult::Deduplicated);
        assert_eq!(r1.hash, r2.hash);
    }

    #[test]
    fn retrieve_stored_content() {
        let mut eng = engine();
        let content = b"hello world, this is a long enough string for dedup";

        let result = eng.process_segment(content, 1000).unwrap();
        let retrieved = eng.get_content(&result.hash).unwrap().unwrap();

        assert_eq!(retrieved, content);
    }

    #[test]
    fn small_content_stored_inline() {
        let mut eng = engine();
        let content = b"hi"; // 2 bytes, below min_dedup_size (32)

        let result = eng.process_segment(content, 1000).unwrap();
        assert!(result.stored_inline);
    }

    // ── Engine: cross-pane dedup ──────────────────────────────────────

    #[test]
    fn cross_pane_dedup() {
        let mut eng = engine();
        let content = vec![0xABu8; 200]; // Same content from "different panes"

        for i in 0..5 {
            let result = eng.process_segment(&content, 1000 + i).unwrap();
            if i == 0 {
                assert_eq!(result.outcome, StoreResult::Inserted);
            } else {
                assert_eq!(result.outcome, StoreResult::Deduplicated);
            }
        }

        // Stats should show 1 unique block, 5 references
        let stats = eng.stats().unwrap();
        assert_eq!(stats.unique_blocks, 1);
        assert_eq!(stats.total_references, 5);
    }

    // ── Engine: ref counting & GC ─────────────────────────────────────

    #[test]
    fn release_decrements_ref_count() {
        let mut eng = engine();
        let content = vec![0u8; 100];

        eng.process_segment(&content, 1000).unwrap();
        eng.process_segment(&content, 2000).unwrap();

        let hash = content_hash(&content);
        let new_count = eng.release(&hash).unwrap();
        assert_eq!(new_count, 1);
    }

    #[test]
    fn gc_removes_unreferenced_blocks() {
        let mut eng = engine();
        let content = vec![0u8; 100];

        eng.process_segment(&content, 1000).unwrap();

        let hash = content_hash(&content);
        eng.release(&hash).unwrap(); // ref_count = 0

        let removed = eng.gc().unwrap();
        assert_eq!(removed, 1);

        // Content should be gone
        let retrieved = eng.get_content(&hash).unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn gc_preserves_referenced_blocks() {
        let mut eng = engine();
        let content = vec![0u8; 100];

        eng.process_segment(&content, 1000).unwrap();
        eng.process_segment(&content, 2000).unwrap(); // ref_count = 2

        let hash = content_hash(&content);
        eng.release(&hash).unwrap(); // ref_count = 1

        let removed = eng.gc().unwrap();
        assert_eq!(removed, 0);

        let retrieved = eng.get_content(&hash).unwrap();
        assert!(retrieved.is_some());
    }

    // ── Engine: counters ──────────────────────────────────────────────

    #[test]
    fn counters_track_processing() {
        let mut eng = engine();
        let big = vec![0u8; 100];
        let small = b"x"; // Below min_dedup_size

        eng.process_segment(&big, 1000).unwrap(); // Inserted
        eng.process_segment(&big, 2000).unwrap(); // Deduplicated
        eng.process_segment(small, 3000).unwrap(); // Inline

        let c = eng.counters();
        assert_eq!(c.total_processed, 3);
        assert_eq!(c.total_inserted, 1);
        assert_eq!(c.total_deduplicated, 1);
        assert_eq!(c.total_inline, 1);
    }

    #[test]
    fn dedup_rate_computation() {
        let mut eng = engine();
        let content = vec![0u8; 100];

        // Insert once, dedup 4 times
        for i in 0..5 {
            eng.process_segment(&content, i).unwrap();
        }

        let c = eng.counters();
        assert!((c.dedup_rate() - 0.8).abs() < f64::EPSILON); // 4/5 = 0.8
    }

    #[test]
    fn dedup_rate_zero_when_no_non_inline() {
        let eng = engine();
        assert!((eng.counters().dedup_rate() - 0.0).abs() < f64::EPSILON);
    }

    // ── Stats ─────────────────────────────────────────────────────────

    #[test]
    fn stats_empty_store() {
        let eng = engine();
        let stats = eng.stats().unwrap();
        assert_eq!(stats.unique_blocks, 0);
        assert_eq!(stats.total_references, 0);
        assert_eq!(stats.unique_bytes, 0);
        assert_eq!(stats.bytes_saved, 0);
        assert!((stats.dedup_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_with_dedup() {
        let mut eng = engine();
        let content = vec![0u8; 100];

        for i in 0..10 {
            eng.process_segment(&content, i).unwrap();
        }

        let stats = eng.stats().unwrap();
        assert_eq!(stats.unique_blocks, 1);
        assert_eq!(stats.total_references, 10);
        assert_eq!(stats.unique_bytes, 100);
        assert_eq!(stats.logical_bytes, 1000); // 100 * 10
        assert!((stats.dedup_ratio - 10.0).abs() < f64::EPSILON);
        assert_eq!(stats.bytes_saved, 900);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let stats = DedupStats {
            total_references: 100,
            unique_blocks: 20,
            unique_bytes: 5000,
            logical_bytes: 25000,
            dedup_ratio: 5.0,
            bytes_saved: 20000,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: DedupStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_references, 100);
        assert!((parsed.dedup_ratio - 5.0).abs() < f64::EPSILON);
    }

    // ── Content block serde ───────────────────────────────────────────

    #[test]
    fn content_block_serde() {
        let block = ContentBlock {
            hash: "abc123".to_string(),
            byte_size: 1024,
            ref_count: 3,
            first_seen_ms: 1000,
            last_seen_ms: 3000,
        };
        let json = serde_json::to_string(&block).unwrap();
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ref_count, 3);
        assert_eq!(parsed.byte_size, 1024);
    }

    // ── Property-based invariants ─────────────────────────────────────

    #[test]
    fn content_address_invariant() {
        // Same content always produces same hash
        for size in [0, 1, 32, 64, 128, 256, 1024, 4096] {
            let content: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
            let h1 = content_hash(&content);
            let h2 = content_hash(&content);
            assert_eq!(h1, h2, "hash must be deterministic for size {size}");
        }
    }

    #[test]
    fn dedup_correctness_n_identical_segments() {
        for n in [1, 2, 5, 10, 50, 100] {
            let mut eng = engine();
            let content = vec![0xFFu8; 200];

            for i in 0..n {
                eng.process_segment(&content, i).unwrap();
            }

            let stats = eng.stats().unwrap();
            assert_eq!(
                stats.unique_blocks, 1,
                "n={n}: should have exactly 1 unique block"
            );
            assert_eq!(stats.total_references, n, "n={n}: ref_count should be {n}");
        }
    }

    #[test]
    fn roundtrip_integrity() {
        let test_contents: Vec<Vec<u8>> = vec![
            vec![0u8; 100],
            (0..200).collect(),
            b"hello world, this is a dedup test with enough bytes".to_vec(),
            vec![0xFF; 1000],
        ];

        let mut eng = engine();

        for content in &test_contents {
            let result = eng.process_segment(content, 1000).unwrap();
            if !result.stored_inline {
                let retrieved = eng.get_content(&result.hash).unwrap().unwrap();
                assert_eq!(
                    &retrieved,
                    content,
                    "roundtrip failed for content of len {}",
                    content.len()
                );
            }
        }
    }

    #[test]
    fn ref_count_only_decreases_via_release() {
        let mut eng = engine();
        let content = vec![0u8; 100];

        // Store 5 times
        for i in 0..5 {
            eng.process_segment(&content, i).unwrap();
        }

        let hash = content_hash(&content);

        // Release should monotonically decrease
        let mut prev_count = 5u64;
        for _ in 0..5 {
            let new_count = eng.release(&hash).unwrap();
            assert!(
                new_count < prev_count,
                "ref_count should decrease: {new_count} >= {prev_count}"
            );
            prev_count = new_count;
        }

        assert_eq!(prev_count, 0);
    }

    #[test]
    fn different_content_gets_different_hashes() {
        let mut eng = engine();
        let mut hashes = std::collections::HashSet::new();

        for i in 0u8..50 {
            let content: Vec<u8> = (0..100).map(|j| i.wrapping_add(j)).collect();
            let result = eng.process_segment(&content, i as u64).unwrap();
            hashes.insert(result.hash);
        }

        assert_eq!(
            hashes.len(),
            50,
            "50 different contents should produce 50 different hashes"
        );
    }

    #[test]
    fn mixed_dedup_and_unique_content() {
        let mut eng = engine();

        // 3 unique contents, each stored 3 times = 9 segments, 3 unique blocks
        let contents: Vec<Vec<u8>> = vec![vec![0u8; 100], vec![1u8; 100], vec![2u8; 100]];

        for content in &contents {
            for i in 0..3 {
                eng.process_segment(content, i).unwrap();
            }
        }

        let stats = eng.stats().unwrap();
        assert_eq!(stats.unique_blocks, 3);
        assert_eq!(stats.total_references, 9);
        assert!((stats.dedup_ratio - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn gc_after_partial_release() {
        let mut eng = engine();
        let c1 = vec![1u8; 100];
        let c2 = vec![2u8; 100];

        // Store c1 twice, c2 once
        eng.process_segment(&c1, 1).unwrap();
        eng.process_segment(&c1, 2).unwrap();
        eng.process_segment(&c2, 3).unwrap();

        // Release all refs to c2
        let h2 = content_hash(&c2);
        eng.release(&h2).unwrap();

        // GC should remove c2 only
        let removed = eng.gc().unwrap();
        assert_eq!(removed, 1);

        let stats = eng.stats().unwrap();
        assert_eq!(stats.unique_blocks, 1);
        assert_eq!(stats.total_references, 2);
    }

    #[test]
    fn engine_counters_serde() {
        let c = EngineCounters {
            total_processed: 100,
            total_deduplicated: 60,
            total_inserted: 30,
            total_inline: 10,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: EngineCounters = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_processed, 100);
        assert!((parsed.dedup_rate() - (60.0 / 90.0)).abs() < 1e-10);
    }
}
