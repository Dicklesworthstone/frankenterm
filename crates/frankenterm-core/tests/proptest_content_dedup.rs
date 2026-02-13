//! Property-based tests for content_dedup module.
//!
//! Verifies content-addressable deduplication invariants:
//! - Hash determinism: same bytes → same hash
//! - Hash collision resistance: different bytes → different hash (probabilistic)
//! - Hash format: always 64 lowercase hex characters
//! - Config monotonicity: should_dedup(n) ⇒ should_dedup(n + k)
//! - DedupStats finalize: dedup_ratio = logical/unique, bytes_saved = logical - unique
//! - DedupStats finalize is idempotent
//! - Counters conservation: processed = deduplicated + inserted + inline
//! - Engine: identical content → Deduplicated on re-store
//! - Engine: content retrieval roundtrip integrity
//! - Engine: ref_count matches store count
//! - Engine: gc only removes unreferenced blocks
//! - Engine: stats bounds (unique_blocks ≤ processed, logical ≥ unique)
//! - Serde roundtrips for all serializable types
//!
//! Bead: wa-g9i4

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::content_dedup::{
    ContentBlock, ContentStore, DedupConfig, DedupEngine, DedupStats, EngineCounters, StoreResult,
    content_hash,
};

// ────────────────────────────────────────────────────────────────────
// In-memory ContentStore (copied from unit tests since mod tests is private)
// ────────────────────────────────────────────────────────────────────

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

fn engine_with_config(config: DedupConfig) -> DedupEngine<MemoryStore> {
    DedupEngine::new(config, MemoryStore::default())
}

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_content(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..max_len)
}

fn arb_dedup_content() -> impl Strategy<Value = Vec<u8>> {
    // Content that's large enough to be deduplicated (>= 32 bytes default)
    prop::collection::vec(any::<u8>(), 32..256)
}

fn arb_config() -> impl Strategy<Value = DedupConfig> {
    (1usize..=128, 1usize..=1024).prop_map(|(min, max_inline)| DedupConfig {
        min_dedup_size: min,
        max_inline_size: max_inline,
    })
}

fn arb_dedup_stats() -> impl Strategy<Value = DedupStats> {
    (
        1u64..=1000,    // unique_blocks
        1u64..=100_000, // unique_bytes
        1u64..=10,      // multiplier for logical_bytes
    )
        .prop_map(|(unique_blocks, unique_bytes, mult)| {
            let logical_bytes = unique_bytes * mult;
            let total_references = unique_blocks * mult;
            DedupStats {
                total_references,
                unique_blocks,
                unique_bytes,
                logical_bytes,
                dedup_ratio: 0.0,
                bytes_saved: 0,
            }
        })
}

// ────────────────────────────────────────────────────────────────────
// Hash: determinism
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Same bytes always produce the same hash.
    #[test]
    fn prop_hash_deterministic(data in arb_content(512)) {
        let h1 = content_hash(&data);
        let h2 = content_hash(&data);
        prop_assert_eq!(h1, h2, "hash must be deterministic");
    }

    /// Hash is always a 64-character lowercase hex string.
    #[test]
    fn prop_hash_format(data in arb_content(512)) {
        let h = content_hash(&data);
        prop_assert_eq!(h.len(), 64, "SHA-256 hex digest must be 64 chars");
        prop_assert!(
            h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "hash must be lowercase hex"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Hash: collision resistance
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Two distinct byte sequences produce different hashes (probabilistic).
    #[test]
    fn prop_hash_collision_resistance(
        a in arb_content(256),
        b in arb_content(256),
    ) {
        if a != b {
            let ha = content_hash(&a);
            let hb = content_hash(&b);
            prop_assert_ne!(ha, hb, "SHA-256 collision on different inputs");
        }
    }

    /// Appending a single byte always changes the hash.
    #[test]
    fn prop_hash_avalanche_append(data in arb_content(128)) {
        let h1 = content_hash(&data);
        let mut extended = data.clone();
        extended.push(0x42);
        let h2 = content_hash(&extended);
        prop_assert_ne!(h1, h2, "appending a byte must change hash");
    }
}

// ────────────────────────────────────────────────────────────────────
// Config: should_dedup monotonicity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// should_dedup is monotone: if true for n, then true for n + k.
    #[test]
    fn prop_should_dedup_monotone(
        min_size in 1usize..=256,
        n in 0usize..=512,
        k in 0usize..=256,
    ) {
        let config = DedupConfig {
            min_dedup_size: min_size,
            max_inline_size: 256,
        };
        if config.should_dedup(n) {
            prop_assert!(
                config.should_dedup(n + k),
                "should_dedup({}) but not should_dedup({})", n, n + k
            );
        }
    }

    /// should_dedup threshold is exact: true iff content_len >= min_dedup_size.
    #[test]
    fn prop_should_dedup_threshold(
        min_size in 1usize..=256,
        content_len in 0usize..=512,
    ) {
        let config = DedupConfig {
            min_dedup_size: min_size,
            max_inline_size: 256,
        };
        prop_assert_eq!(
            config.should_dedup(content_len),
            content_len >= min_size,
            "should_dedup({}) with min_size={}", content_len, min_size
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Config: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DedupConfig survives JSON roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: DedupConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.min_dedup_size, back.min_dedup_size);
        prop_assert_eq!(config.max_inline_size, back.max_inline_size);
    }
}

// ────────────────────────────────────────────────────────────────────
// DedupStats: finalize computes correct derived fields
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// finalize sets dedup_ratio = logical_bytes / unique_bytes.
    #[test]
    fn prop_stats_finalize_ratio(stats in arb_dedup_stats()) {
        let finalized = stats.clone().finalize();
        let expected_ratio = if stats.unique_bytes > 0 {
            stats.logical_bytes as f64 / stats.unique_bytes as f64
        } else {
            1.0
        };
        prop_assert!(
            (finalized.dedup_ratio - expected_ratio).abs() < 1e-9,
            "ratio {} != expected {}", finalized.dedup_ratio, expected_ratio
        );
    }

    /// finalize sets bytes_saved = logical_bytes - unique_bytes (saturating).
    #[test]
    fn prop_stats_finalize_bytes_saved(stats in arb_dedup_stats()) {
        let finalized = stats.clone().finalize();
        let expected_saved = stats.logical_bytes.saturating_sub(stats.unique_bytes);
        prop_assert_eq!(
            finalized.bytes_saved, expected_saved,
            "bytes_saved mismatch"
        );
    }

    /// finalize is idempotent: finalize(finalize(x)) == finalize(x).
    #[test]
    fn prop_stats_finalize_idempotent(stats in arb_dedup_stats()) {
        let once = stats.finalize();
        let twice = once.clone().finalize();
        prop_assert!(
            (once.dedup_ratio - twice.dedup_ratio).abs() < 1e-12,
            "dedup_ratio changed on second finalize"
        );
        prop_assert_eq!(once.bytes_saved, twice.bytes_saved, "bytes_saved changed");
    }

    /// With zero unique_bytes, dedup_ratio is 1.0 (no division by zero).
    #[test]
    fn prop_stats_finalize_zero_unique(
        logical in 0u64..=1000,
        refs in 0u64..=100,
    ) {
        let stats = DedupStats {
            total_references: refs,
            unique_blocks: 0,
            unique_bytes: 0,
            logical_bytes: logical,
            dedup_ratio: 0.0,
            bytes_saved: 0,
        };
        let finalized = stats.finalize();
        prop_assert!(
            (finalized.dedup_ratio - 1.0).abs() < 1e-12,
            "zero unique bytes should give ratio 1.0"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// DedupStats: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DedupStats survives JSON roundtrip after finalize.
    #[test]
    fn prop_stats_serde_roundtrip(stats in arb_dedup_stats()) {
        let finalized = stats.finalize();
        let json = serde_json::to_string(&finalized).unwrap();
        let back: DedupStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(finalized.total_references, back.total_references);
        prop_assert_eq!(finalized.unique_blocks, back.unique_blocks);
        prop_assert_eq!(finalized.unique_bytes, back.unique_bytes);
        prop_assert_eq!(finalized.logical_bytes, back.logical_bytes);
        prop_assert_eq!(finalized.bytes_saved, back.bytes_saved);
        prop_assert!(
            (finalized.dedup_ratio - back.dedup_ratio).abs() < 1e-9,
            "dedup_ratio drift in serde"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// EngineCounters: conservation law
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// dedup_rate is in [0, 1].
    #[test]
    fn prop_dedup_rate_bounded(
        deduped in 0u64..=100,
        inserted in 0u64..=100,
        inline in 0u64..=100,
    ) {
        let c = EngineCounters {
            total_processed: deduped + inserted + inline,
            total_deduplicated: deduped,
            total_inserted: inserted,
            total_inline: inline,
        };
        let rate = c.dedup_rate();
        prop_assert!(rate >= 0.0, "dedup_rate must be >= 0, got {}", rate);
        prop_assert!(rate <= 1.0, "dedup_rate must be <= 1, got {}", rate);
    }

    /// dedup_rate == 0 when no non-inline segments.
    #[test]
    fn prop_dedup_rate_zero_no_non_inline(inline in 0u64..=100) {
        let c = EngineCounters {
            total_processed: inline,
            total_deduplicated: 0,
            total_inserted: 0,
            total_inline: inline,
        };
        prop_assert!(
            c.dedup_rate().abs() < 1e-12,
            "rate should be 0 with no non-inline"
        );
    }

    /// EngineCounters survives serde roundtrip.
    #[test]
    fn prop_counters_serde_roundtrip(
        processed in 0u64..=1000,
        deduped in 0u64..=500,
        inserted in 0u64..=500,
        inline in 0u64..=500,
    ) {
        let c = EngineCounters {
            total_processed: processed,
            total_deduplicated: deduped,
            total_inserted: inserted,
            total_inline: inline,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: EngineCounters = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c.total_processed, back.total_processed);
        prop_assert_eq!(c.total_deduplicated, back.total_deduplicated);
        prop_assert_eq!(c.total_inserted, back.total_inserted);
        prop_assert_eq!(c.total_inline, back.total_inline);
    }
}

// ────────────────────────────────────────────────────────────────────
// Engine: counters conservation law
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// total_processed == total_deduplicated + total_inserted + total_inline.
    #[test]
    fn prop_engine_counter_conservation(
        contents in prop::collection::vec(arb_content(200), 1..=50),
    ) {
        let mut eng = engine();
        for (i, content) in contents.iter().enumerate() {
            eng.process_segment(content, i as u64).unwrap();
        }

        let c = eng.counters();
        prop_assert_eq!(
            c.total_processed,
            c.total_deduplicated + c.total_inserted + c.total_inline,
            "counter conservation violated"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Engine: identical content → dedup on re-store
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Storing the same content N times: first Insert, rest Deduplicated.
    #[test]
    fn prop_identical_content_dedup(
        content in arb_dedup_content(),
        n in 2usize..=20,
    ) {
        let mut eng = engine();
        let r1 = eng.process_segment(&content, 0).unwrap();
        prop_assert_eq!(r1.outcome, StoreResult::Inserted, "first store must Insert");
        let expected_hash = r1.hash;

        for i in 1..n {
            let r = eng.process_segment(&content, i as u64).unwrap();
            prop_assert_eq!(
                r.outcome,
                StoreResult::Deduplicated,
                "store #{} must Deduplicate", i
            );
            prop_assert_eq!(r.hash, expected_hash.clone(), "hash must be stable");
        }
    }

    /// N stores of same content → unique_blocks == 1, total_references == N.
    #[test]
    fn prop_identical_content_stats(
        content in arb_dedup_content(),
        n in 1usize..=20,
    ) {
        let mut eng = engine();
        for i in 0..n {
            eng.process_segment(&content, i as u64).unwrap();
        }

        let stats = eng.stats().unwrap();
        prop_assert_eq!(stats.unique_blocks, 1, "should have 1 unique block");
        prop_assert_eq!(stats.total_references, n as u64, "ref count should be N");
    }
}

// ────────────────────────────────────────────────────────────────────
// Engine: content roundtrip integrity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Stored content retrieves identically.
    #[test]
    fn prop_content_roundtrip(content in arb_dedup_content()) {
        let mut eng = engine();
        let result = eng.process_segment(&content, 1000).unwrap();

        if !result.stored_inline {
            let retrieved = eng.get_content(&result.hash).unwrap().unwrap();
            prop_assert_eq!(
                retrieved, content,
                "roundtrip integrity failed"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Engine: inline / dedup threshold
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Content below min_dedup_size is stored inline.
    #[test]
    fn prop_small_content_inline(
        min_size in 16usize..=128,
        content_len in 0usize..=127,
    ) {
        let min_size = min_size.max(content_len + 1); // ensure content < min_size
        let config = DedupConfig {
            min_dedup_size: min_size,
            max_inline_size: 256,
        };
        let mut eng = engine_with_config(config);
        let content: Vec<u8> = (0..content_len).map(|i| i as u8).collect();

        let result = eng.process_segment(&content, 0).unwrap();
        prop_assert!(result.stored_inline, "content below threshold must be inline");
    }

    /// Content at or above min_dedup_size goes to the store (not inline).
    #[test]
    fn prop_large_content_not_inline(
        min_size in 1usize..=64,
        extra in 0usize..=128,
    ) {
        let content_len = min_size + extra;
        let config = DedupConfig {
            min_dedup_size: min_size,
            max_inline_size: 256,
        };
        let mut eng = engine_with_config(config);
        let content: Vec<u8> = (0..content_len).map(|i| i as u8).collect();

        let result = eng.process_segment(&content, 0).unwrap();
        prop_assert!(!result.stored_inline, "content above threshold must not be inline");
    }
}

// ────────────────────────────────────────────────────────────────────
// Engine: ref counting and GC
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// N stores then N releases → ref_count 0; gc removes the block.
    #[test]
    fn prop_release_all_then_gc(
        content in arb_dedup_content(),
        n in 1usize..=10,
    ) {
        let mut eng = engine();
        for i in 0..n {
            eng.process_segment(&content, i as u64).unwrap();
        }

        let hash = content_hash(&content);

        // Release all N references
        for i in 0..n {
            let remaining = eng.release(&hash).unwrap();
            prop_assert_eq!(
                remaining, (n - i - 1) as u64,
                "ref_count after release #{}", i
            );
        }

        // GC should remove the block
        let removed = eng.gc().unwrap();
        prop_assert_eq!(removed, 1, "gc should remove the unreferenced block");

        let stats = eng.stats().unwrap();
        prop_assert_eq!(stats.unique_blocks, 0, "store should be empty after gc");
    }

    /// Partial release: releasing k of N refs leaves N-k; gc removes nothing.
    #[test]
    fn prop_partial_release_gc_preserves(
        content in arb_dedup_content(),
        n in 2usize..=10,
        k_frac in 0.1..=0.9_f64,
    ) {
        let k = ((n as f64 * k_frac).floor() as usize).max(1).min(n - 1);
        let mut eng = engine();
        for i in 0..n {
            eng.process_segment(&content, i as u64).unwrap();
        }

        let hash = content_hash(&content);
        for _ in 0..k {
            eng.release(&hash).unwrap();
        }

        // GC should not remove the block (still has refs)
        let removed = eng.gc().unwrap();
        prop_assert_eq!(removed, 0, "gc should not remove referenced block");

        let stats = eng.stats().unwrap();
        prop_assert_eq!(stats.unique_blocks, 1, "block should still exist");
        prop_assert_eq!(
            stats.total_references, (n - k) as u64,
            "ref_count should be N - k"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Engine: stats bounds
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// unique_blocks <= total non-inline processed segments.
    #[test]
    fn prop_stats_unique_blocks_bounded(
        contents in prop::collection::vec(arb_dedup_content(), 1..=30),
    ) {
        let mut eng = engine();
        for (i, content) in contents.iter().enumerate() {
            eng.process_segment(content, i as u64).unwrap();
        }

        let stats = eng.stats().unwrap();
        let c = eng.counters();
        prop_assert!(
            stats.unique_blocks <= (c.total_inserted + c.total_deduplicated),
            "unique_blocks {} > non-inline {}", stats.unique_blocks, c.total_inserted + c.total_deduplicated
        );
    }

    /// logical_bytes >= unique_bytes (since each block has ref_count >= 1).
    #[test]
    fn prop_stats_logical_ge_unique(
        contents in prop::collection::vec(arb_dedup_content(), 1..=30),
    ) {
        let mut eng = engine();
        for (i, content) in contents.iter().enumerate() {
            eng.process_segment(content, i as u64).unwrap();
        }

        let stats = eng.stats().unwrap();
        prop_assert!(
            stats.logical_bytes >= stats.unique_bytes,
            "logical {} < unique {}", stats.logical_bytes, stats.unique_bytes
        );
    }

    /// dedup_ratio >= 1.0 when there are unique blocks.
    #[test]
    fn prop_stats_dedup_ratio_ge_one(
        contents in prop::collection::vec(arb_dedup_content(), 1..=30),
    ) {
        let mut eng = engine();
        for (i, content) in contents.iter().enumerate() {
            eng.process_segment(content, i as u64).unwrap();
        }

        let stats = eng.stats().unwrap();
        if stats.unique_blocks > 0 {
            prop_assert!(
                stats.dedup_ratio >= 1.0,
                "dedup_ratio {} < 1.0", stats.dedup_ratio
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Engine: mixed content (unique + duplicated)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// K unique contents each stored M times: unique_blocks == K, total_refs == K*M.
    #[test]
    fn prop_mixed_dedup_stats(
        k in 1usize..=10,
        m in 1usize..=10,
    ) {
        let mut eng = engine();
        for content_id in 0..k {
            // Each "unique" content: fill with content_id repeated
            let content: Vec<u8> = vec![content_id as u8; 64];
            for rep in 0..m {
                eng.process_segment(&content, (content_id * m + rep) as u64).unwrap();
            }
        }

        let stats = eng.stats().unwrap();
        prop_assert_eq!(stats.unique_blocks, k as u64, "should have K unique blocks");
        prop_assert_eq!(
            stats.total_references, (k * m) as u64,
            "should have K*M total references"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// ContentBlock: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// ContentBlock survives JSON roundtrip.
    #[test]
    fn prop_content_block_serde(
        byte_size in 0usize..=100_000,
        ref_count in 0u64..=10_000,
        first_seen in 0u64..=u64::MAX / 2,
        last_seen_offset in 0u64..=1_000_000,
    ) {
        let block = ContentBlock {
            hash: content_hash(&byte_size.to_le_bytes()),
            byte_size,
            ref_count,
            first_seen_ms: first_seen,
            last_seen_ms: first_seen + last_seen_offset,
        };

        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(block.hash, back.hash);
        prop_assert_eq!(block.byte_size, back.byte_size);
        prop_assert_eq!(block.ref_count, back.ref_count);
        prop_assert_eq!(block.first_seen_ms, back.first_seen_ms);
        prop_assert_eq!(block.last_seen_ms, back.last_seen_ms);
    }
}

// ────────────────────────────────────────────────────────────────────
// Engine: process_segment result consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// process_segment result hash matches content_hash of input.
    #[test]
    fn prop_process_segment_hash_matches(content in arb_content(256)) {
        let mut eng = engine();
        let result = eng.process_segment(&content, 0).unwrap();
        let expected_hash = content_hash(&content);
        prop_assert_eq!(result.hash, expected_hash, "result hash must match content_hash");
    }

    /// process_segment content_len matches input length.
    #[test]
    fn prop_process_segment_len_matches(content in arb_content(256)) {
        let mut eng = engine();
        let result = eng.process_segment(&content, 0).unwrap();
        prop_assert_eq!(result.content_len, content.len(), "content_len mismatch");
    }
}
