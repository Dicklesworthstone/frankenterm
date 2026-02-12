//! Cross-module integration tests for recently-built data structure modules.
//!
//! These tests verify that independently-developed modules compose correctly
//! when used together in realistic pipeline scenarios:
//!
//! A. Entropy-aware eviction: entropy scoring guides scrollback trimming
//! B. Bloom-filter accelerated dedup: probabilistic fast-path before SHA-256
//! C. Stream integrity with sharded metrics: rolling hash + lock-free counters
//! D. Completion tokens orchestrating multi-step capture pipelines

use frankenterm_core::bloom_filter::BloomFilter;
use frankenterm_core::completion_token::{
    Boundaries, CompletionBoundary, CompletionState, CompletionTracker, CompletionTrackerConfig,
    StepOutcome,
};
use frankenterm_core::content_dedup::{ContentStore, DedupConfig, DedupEngine, StoreResult};
use frankenterm_core::entropy_accounting::{
    EntropyEstimator, EvictionConfig as EntropyEvictionConfig, InformationBudget,
    PaneEntropySummary, compute_entropy, eviction_order, eviction_score, information_cost,
};
use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::pane_tiers::PaneTier;
use frankenterm_core::scrollback_eviction::{
    EvictionConfig, PaneTierSource, ScrollbackEvictor, SegmentStore,
};
use frankenterm_core::sharded_counter::{ShardedCounter, ShardedMax};
use frankenterm_core::stream_hash::{IntegrityChecker, StreamHash};
use std::collections::HashMap;

// =============================================================================
// Mock infrastructure
// =============================================================================

/// In-memory segment store for testing.
struct MockSegmentStore {
    /// pane_id → list of segment contents (oldest first).
    segments: HashMap<u64, Vec<Vec<u8>>>,
}

impl MockSegmentStore {
    fn new() -> Self {
        Self {
            segments: HashMap::new(),
        }
    }

    fn add_segments(&mut self, pane_id: u64, data: Vec<Vec<u8>>) {
        self.segments.insert(pane_id, data);
    }
}

impl SegmentStore for MockSegmentStore {
    fn count_segments(&self, pane_id: u64) -> Result<usize, String> {
        Ok(self.segments.get(&pane_id).map_or(0, |s| s.len()))
    }

    fn delete_oldest_segments(&self, pane_id: u64, count: usize) -> Result<usize, String> {
        // For read-only plan tests, we don't actually delete.
        // Return the count that would be deleted.
        let current = self.segments.get(&pane_id).map_or(0, |s| s.len());
        Ok(count.min(current))
    }

    fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
        Ok(self.segments.keys().copied().collect())
    }
}

/// In-memory tier source backed by a HashMap.
struct MockTierSource {
    tiers: HashMap<u64, PaneTier>,
}

impl MockTierSource {
    fn new(tiers: HashMap<u64, PaneTier>) -> Self {
        Self { tiers }
    }
}

impl PaneTierSource for MockTierSource {
    fn tier_for(&self, pane_id: u64) -> Option<PaneTier> {
        self.tiers.get(&pane_id).copied()
    }
}

/// In-memory content store for dedup testing.
struct MockContentStore {
    blocks: HashMap<String, (Vec<u8>, u64, u64)>, // hash → (content, ref_count, last_seen)
}

impl MockContentStore {
    fn new() -> Self {
        Self {
            blocks: HashMap::new(),
        }
    }
}

impl ContentStore for MockContentStore {
    fn store(
        &mut self,
        hash: &str,
        content: &[u8],
        timestamp_ms: u64,
    ) -> Result<StoreResult, String> {
        if let Some(entry) = self.blocks.get_mut(hash) {
            entry.1 += 1;
            entry.2 = timestamp_ms;
            Ok(StoreResult::Deduplicated)
        } else {
            self.blocks
                .insert(hash.to_string(), (content.to_vec(), 1, timestamp_ms));
            Ok(StoreResult::Inserted)
        }
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.blocks.get(hash).map(|(c, _, _)| c.clone()))
    }

    fn decrement_ref(&mut self, hash: &str) -> Result<u64, String> {
        if let Some(entry) = self.blocks.get_mut(hash) {
            entry.1 = entry.1.saturating_sub(1);
            Ok(entry.1)
        } else {
            Ok(0)
        }
    }

    fn gc(&mut self) -> Result<usize, String> {
        let before = self.blocks.len();
        self.blocks.retain(|_, (_, rc, _)| *rc > 0);
        Ok(before - self.blocks.len())
    }

    fn stats(&self) -> Result<frankenterm_core::content_dedup::DedupStats, String> {
        let mut stats = frankenterm_core::content_dedup::DedupStats::default();
        for (_, (content, rc, _)) in &self.blocks {
            stats.unique_blocks += 1;
            stats.unique_bytes += content.len() as u64;
            stats.total_references += rc;
            stats.logical_bytes += content.len() as u64 * rc;
        }
        Ok(stats.finalize())
    }

    fn contains(&self, hash: &str) -> Result<bool, String> {
        Ok(self.blocks.contains_key(hash))
    }
}

// =============================================================================
// A. Entropy-aware eviction: entropy scoring guides scrollback trimming
// =============================================================================

/// Generate a segment of repeated bytes (low entropy).
fn low_entropy_segments(count: usize, seg_size: usize) -> Vec<Vec<u8>> {
    (0..count).map(|_| vec![b'A'; seg_size]).collect()
}

/// Generate segments with random-ish content (high entropy).
fn high_entropy_segments(count: usize, seg_size: usize, seed: u8) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            (0..seg_size)
                .map(|j| {
                    // Simple deterministic pseudo-random via LCG
                    let val = (i as u8)
                        .wrapping_mul(37)
                        .wrapping_add(j as u8)
                        .wrapping_mul(seed)
                        .wrapping_add(131);
                    val
                })
                .collect()
        })
        .collect()
}

#[test]
fn entropy_guides_eviction_order() {
    // Pane 1: low-entropy (all 'A's) — cheap to lose
    // Pane 2: high-entropy (varied bytes) — expensive to lose
    let low_data: Vec<u8> = vec![b'A'; 10_000];
    let high_data: Vec<u8> = (0..10_000u32)
        .map(|i| (i.wrapping_mul(37) & 0xFF) as u8)
        .collect();

    let low_ent = compute_entropy(&low_data);
    let high_ent = compute_entropy(&high_data);

    let low_cost = information_cost(low_data.len(), low_ent);
    let high_cost = information_cost(high_data.len(), high_ent);

    // Low entropy = low information cost = better eviction candidate
    assert!(
        low_cost < high_cost,
        "low entropy should have lower info cost"
    );

    let config = EntropyEvictionConfig::default();
    let age_ms = 60_000; // 1 minute old

    let low_score = eviction_score(low_cost, age_ms, &config);
    let high_score = eviction_score(high_cost, age_ms, &config);

    // Lower score = evict first
    assert!(
        low_score < high_score,
        "low-entropy pane should have lower eviction score (evict first)"
    );

    // eviction_order should return lowest-scoring first
    let scores = vec![(1, low_score), (2, high_score)];
    let order = eviction_order(&scores);
    assert_eq!(order, vec![1, 2], "low-entropy pane 1 should evict first");
}

#[test]
fn entropy_eviction_interacts_with_tier_based_eviction() {
    // Tier-based: Active panes keep 10k segments, Dormant keep 100
    // Entropy-aware: within the same tier, high-entropy panes should be protected
    let eviction_config = EvictionConfig::default();

    // Two dormant panes, both over limit
    let mut store = MockSegmentStore::new();
    store.add_segments(10, low_entropy_segments(500, 100)); // low entropy, 500 segs
    store.add_segments(20, high_entropy_segments(500, 100, 42)); // high entropy, 500 segs

    let tiers = HashMap::from([(10, PaneTier::Dormant), (20, PaneTier::Dormant)]);
    let tier_source = MockTierSource::new(tiers);

    let evictor = ScrollbackEvictor::new(eviction_config, store, tier_source);
    let plan = evictor.plan(MemoryPressureTier::Green).unwrap();

    // Both panes are over limit (500 > 100 for dormant) so both get targets
    assert_eq!(plan.panes_affected, 2);

    // Now use entropy to decide which to prioritize for eviction
    let pane10_data: Vec<u8> = low_entropy_segments(500, 100).concat();
    let pane20_data: Vec<u8> = high_entropy_segments(500, 100, 42).concat();

    let entropy_config = EntropyEvictionConfig::default();
    let score10 = eviction_score(
        information_cost(pane10_data.len(), compute_entropy(&pane10_data)),
        60_000,
        &entropy_config,
    );
    let score20 = eviction_score(
        information_cost(pane20_data.len(), compute_entropy(&pane20_data)),
        60_000,
        &entropy_config,
    );

    // Low-entropy pane 10 should be evicted first
    let order = eviction_order(&[(10, score10), (20, score20)]);
    assert_eq!(
        order[0], 10,
        "entropy-aware order should evict low-entropy pane first"
    );
}

#[test]
fn information_budget_tracks_entropy_weighted_usage() {
    // Pane 1: 10KB raw but low entropy (constant data) → low info cost
    let data1 = vec![b'X'; 10_000];
    let ent1 = compute_entropy(&data1);
    let cost1 = information_cost(data1.len(), ent1);
    assert!(
        cost1 < 10.0,
        "constant data should have near-zero info cost: got {cost1}"
    );

    // Pane 2: high entropy data → high info cost
    // Use all 256 byte values to maximize entropy
    let data2: Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
    let ent2 = compute_entropy(&data2);
    let cost2 = information_cost(data2.len(), ent2);

    // Set budget between cost1 and cost1+cost2
    let mut budget = InformationBudget::new(cost2 / 2.0);

    budget.add(cost1);
    assert!(
        !budget.is_exceeded(),
        "low-entropy data shouldn't exceed budget"
    );

    budget.add(cost2);
    assert!(
        budget.is_exceeded(),
        "high-entropy data should push over budget (cost={cost2:.1}, budget={})",
        cost2 / 2.0
    );

    // Remove the low-cost pane — still over budget
    budget.remove(cost1);
    assert!(budget.is_exceeded());

    // Remove the high-cost pane — under budget
    budget.remove(cost2);
    assert!(!budget.is_exceeded());
}

#[test]
fn incremental_estimator_matches_batch_entropy_for_eviction() {
    let data: Vec<u8> = b"The quick brown fox jumps over the lazy dog. ".repeat(100);

    // Batch computation
    let batch_entropy = compute_entropy(&data);
    let batch_cost = information_cost(data.len(), batch_entropy);

    // Incremental estimation
    let mut estimator = EntropyEstimator::new(data.len());
    estimator.update_block(&data);
    let est_entropy = estimator.entropy();
    let est_cost = information_cost(data.len(), est_entropy);

    // Should produce similar (not identical due to windowing) results
    assert!(
        (batch_entropy - est_entropy).abs() < 0.1,
        "entropy: batch={batch_entropy:.3} est={est_entropy:.3}"
    );
    assert!(
        (batch_cost - est_cost).abs() / batch_cost < 0.02,
        "info cost: batch={batch_cost:.1} est={est_cost:.1}"
    );
}

// =============================================================================
// B. Bloom-filter accelerated dedup
// =============================================================================

#[test]
fn bloom_prefilter_avoids_sha256_for_new_content() {
    let mut bloom = BloomFilter::with_capacity(1000, 0.01);
    let mut dedup = DedupEngine::new(DedupConfig::default(), MockContentStore::new());

    let segments: Vec<Vec<u8>> = (0..50)
        .map(|i| format!("Agent output line {} with unique content xyz{}", i, i * 37).into_bytes())
        .collect();

    let mut bloom_short_circuits = 0u64;

    for seg in &segments {
        if !bloom.contains(seg) {
            // Definitely new — can skip full dedup check
            bloom.insert(seg);
            bloom_short_circuits += 1;
            // Still need to store it (inline or dedup engine)
            let result = dedup.process_segment(seg, 1000);
            assert!(result.is_ok());
        } else {
            // Might be duplicate — need full SHA-256
            let result = dedup.process_segment(seg, 1000);
            assert!(result.is_ok());
        }
    }

    // All 50 segments are unique, so bloom should short-circuit all of them
    // (bloom false-positive rate is 1%, so maybe 0-1 false positives)
    assert!(
        bloom_short_circuits >= 49,
        "bloom should short-circuit most new content: got {bloom_short_circuits}/50"
    );
}

#[test]
fn bloom_catches_duplicates_for_dedup_engine() {
    let mut bloom = BloomFilter::with_capacity(1000, 0.01);
    let mut dedup = DedupEngine::new(DedupConfig::default(), MockContentStore::new());

    let unique_seg = b"This is a unique segment with enough bytes for dedup threshold.";
    let dup_seg = b"This segment appears multiple times with enough bytes for threshold.";

    // First insertion
    bloom.insert(unique_seg.as_slice());
    let r1 = dedup.process_segment(unique_seg, 100).unwrap();
    assert_eq!(r1.outcome, StoreResult::Inserted);

    bloom.insert(dup_seg.as_slice());
    let r2 = dedup.process_segment(dup_seg, 200).unwrap();
    assert_eq!(r2.outcome, StoreResult::Inserted);

    // Duplicate of dup_seg
    assert!(
        bloom.contains(dup_seg.as_slice()),
        "bloom should detect potential duplicate"
    );
    let r3 = dedup.process_segment(dup_seg, 300).unwrap();
    assert_eq!(
        r3.outcome,
        StoreResult::Deduplicated,
        "dedup engine confirms duplicate"
    );
}

#[test]
fn bloom_no_false_negatives_across_dedup_pipeline() {
    // Property: if bloom says "not present", the content is genuinely new.
    // We verify by inserting everything into both bloom and dedup.
    let mut bloom = BloomFilter::with_capacity(500, 0.05);
    let mut dedup = DedupEngine::new(DedupConfig::default(), MockContentStore::new());

    let segments: Vec<Vec<u8>> = (0..200)
        .map(|i| {
            format!(
                "Segment content number {} with padding for minimum size.",
                i
            )
            .into_bytes()
        })
        .collect();

    // Insert all segments
    for seg in &segments {
        bloom.insert(seg);
        dedup.process_segment(seg, 1000).unwrap();
    }

    // Re-check: bloom must say "probably yes" for every inserted segment
    for seg in &segments {
        assert!(
            bloom.contains(seg),
            "bloom must not produce false negatives for inserted content"
        );
    }
}

// =============================================================================
// C. Stream integrity with sharded metrics
// =============================================================================

#[test]
fn stream_hash_with_sharded_throughput_counters() {
    let mut hasher = StreamHash::new();
    let bytes_counter = ShardedCounter::new();
    let max_segment_size = ShardedMax::new();

    // Simulate capture pipeline: process multiple segments
    let segments = vec![
        b"Output from agent 1: task completed successfully.\n".to_vec(),
        b"Error: rate limit exceeded for model claude-3-opus.\n".to_vec(),
        b"Agent 2: beginning code review of module x.rs\n".to_vec(),
    ];

    for seg in &segments {
        hasher.update(seg);
        bytes_counter.add(seg.len() as u64);
        max_segment_size.observe(seg.len() as u64);
    }

    let total_bytes: usize = segments.iter().map(|s| s.len()).sum();
    assert_eq!(
        bytes_counter.get(),
        total_bytes as u64,
        "sharded counter should track total bytes"
    );
    assert_eq!(
        hasher.bytes_hashed(),
        total_bytes as u64,
        "stream hash byte count should match counter"
    );

    let max_seg: usize = segments.iter().map(|s| s.len()).max().unwrap();
    assert_eq!(
        max_segment_size.get(),
        max_seg as u64,
        "sharded max should track largest segment"
    );
}

#[test]
fn stream_hash_combine_for_parallel_capture() {
    // Two capture threads processing different parts of a stream
    let part_a = b"First half of the terminal output stream.";
    let part_b = b"Second half of the terminal output stream.";

    let mut hash_a = StreamHash::new();
    hash_a.update(part_a);

    let mut hash_b = StreamHash::new();
    hash_b.update(part_b);

    // Combined hash should match a single-pass hash
    let combined = hash_a.combine(&hash_b);

    let mut single_pass = StreamHash::new();
    single_pass.update(part_a);
    single_pass.update(part_b);

    assert_eq!(
        combined.digest(),
        single_pass.digest(),
        "combined hash must equal single-pass hash (homomorphic property)"
    );
}

#[test]
fn integrity_checker_detects_corruption_with_metrics() {
    let bytes_counter = ShardedCounter::new();
    let mismatch_counter = ShardedCounter::new();

    // Producer side
    let mut producer = IntegrityChecker::new();
    let data = b"Hello from the PTY side of the pipeline.";
    producer.update(data);
    bytes_counter.add(data.len() as u64);

    // Consumer side (correct)
    let mut consumer_ok = IntegrityChecker::new();
    consumer_ok.update(data);
    consumer_ok.set_remote_digest(producer.local_digest());
    let result = consumer_ok.check().unwrap();
    assert!(result.matches, "matching data should pass integrity check");

    // Consumer side (corrupted — one byte different)
    let mut corrupted = data.to_vec();
    corrupted[5] = b'X'; // "HelloXfrom..." instead of "Hello from..."
    let mut consumer_bad = IntegrityChecker::new();
    consumer_bad.update(&corrupted);
    consumer_bad.set_remote_digest(producer.local_digest());
    let result = consumer_bad.check().unwrap();
    assert!(
        !result.matches,
        "corrupted data should fail integrity check"
    );
    mismatch_counter.add(1);

    assert_eq!(bytes_counter.get(), data.len() as u64);
    assert_eq!(mismatch_counter.get(), 1, "one corruption detected");
}

// =============================================================================
// D. Completion tokens orchestrating capture pipeline
// =============================================================================

#[test]
fn completion_token_tracks_capture_dedup_eviction_pipeline() {
    let config = CompletionTrackerConfig {
        default_timeout_ms: 0,
        max_active_tokens: 100,
        retention_ms: 60_000,
    };
    let mut tracker = CompletionTracker::new(config);

    // Define the pipeline boundary
    let boundary = CompletionBoundary::new(&["capture", "dedup", "entropy", "eviction"]);

    let token_id = tracker.begin("process_pane_42", boundary).unwrap();

    // Step 1: Capture — state starts as Pending, transitions to InProgress on first advance
    assert_eq!(tracker.state(&token_id), Some(CompletionState::Pending));
    tracker.advance(&token_id, "capture", StepOutcome::Ok, "captured 5000 bytes");

    // Still in progress (3 steps remaining)
    assert_eq!(tracker.state(&token_id), Some(CompletionState::InProgress));
    let pending = tracker.pending_subsystems(&token_id).unwrap();
    assert_eq!(pending.len(), 3);

    // Step 2: Dedup
    tracker.advance(&token_id, "dedup", StepOutcome::Ok, "new content stored");

    // Step 3: Entropy accounting
    tracker.advance(
        &token_id,
        "entropy",
        StepOutcome::Ok,
        "entropy=4.2, cost=2625.0",
    );

    // Step 4: Eviction check
    tracker.advance(&token_id, "eviction", StepOutcome::Skipped, "under budget");

    // All boundary subsystems reported → Completed
    assert_eq!(
        tracker.state(&token_id),
        Some(CompletionState::Completed),
        "token should complete when all boundary subsystems report"
    );

    // Cause chain should have 4 entries
    let chain = tracker.cause_chain(&token_id).unwrap();
    assert_eq!(chain.len(), 4);
    assert!(chain.failed_subsystems().is_empty());
}

#[test]
fn completion_token_captures_failure_in_pipeline() {
    let config = CompletionTrackerConfig {
        default_timeout_ms: 0,
        max_active_tokens: 100,
        retention_ms: 60_000,
    };
    let mut tracker = CompletionTracker::new(config);
    let boundary = CompletionBoundary::new(&["capture", "dedup", "entropy"]);

    let token_id = tracker.begin("process_pane_99", boundary).unwrap();

    // Capture succeeds
    tracker.advance(&token_id, "capture", StepOutcome::Ok, "captured 3000 bytes");

    // Dedup fails
    tracker.advance(
        &token_id,
        "dedup",
        StepOutcome::Error,
        "content store unavailable",
    );

    // After Ok + Error, state is PartialFailure (some steps succeeded, one failed)
    assert_eq!(
        tracker.state(&token_id),
        Some(CompletionState::PartialFailure),
        "pipeline should be partial failure when some steps succeed and one fails"
    );

    // Cause chain preserves the failure context
    let chain = tracker.cause_chain(&token_id).unwrap();
    let failed = chain.failed_subsystems();
    assert_eq!(failed, vec!["dedup"]);
}

#[test]
fn completion_token_with_skip_and_completion() {
    let config = CompletionTrackerConfig {
        default_timeout_ms: 0,
        max_active_tokens: 100,
        retention_ms: 60_000,
    };
    let mut tracker = CompletionTracker::new(config);
    let boundary = Boundaries::capture(); // ["ingest", "storage"]

    let token_id = tracker.begin("quick_capture_pane_7", boundary).unwrap();

    tracker.advance(&token_id, "ingest", StepOutcome::Ok, "200 bytes");
    tracker.advance(
        &token_id,
        "storage",
        StepOutcome::Skipped,
        "dedup: already stored",
    );

    // Skipped counts as completed for the boundary
    assert_eq!(tracker.state(&token_id), Some(CompletionState::Completed));
}

#[test]
fn completion_tracker_manages_multiple_concurrent_tokens() {
    let config = CompletionTrackerConfig {
        default_timeout_ms: 0,
        max_active_tokens: 100,
        retention_ms: 60_000,
    };
    let mut tracker = CompletionTracker::new(config);

    // Three concurrent pane processing operations
    let boundary = CompletionBoundary::new(&["capture", "store"]);
    let t1 = tracker.begin("pane_1", boundary.clone()).unwrap();
    let t2 = tracker.begin("pane_2", boundary.clone()).unwrap();
    let t3 = tracker.begin("pane_3", boundary).unwrap();

    assert_eq!(tracker.active_count(), 3);

    // Complete pane 1
    tracker.advance(&t1, "capture", StepOutcome::Ok, "ok");
    tracker.advance(&t1, "store", StepOutcome::Ok, "ok");
    assert_eq!(tracker.state(&t1), Some(CompletionState::Completed));

    // Fail pane 2
    tracker.advance(&t2, "capture", StepOutcome::Error, "timeout");
    assert_eq!(tracker.state(&t2), Some(CompletionState::Failed));

    // Pane 3 still in progress
    tracker.advance(&t3, "capture", StepOutcome::Ok, "ok");
    assert_eq!(tracker.state(&t3), Some(CompletionState::InProgress));

    // Only t3 is still active (t1=Completed, t2=Failed are terminal)
    assert_eq!(tracker.active_count(), 1);
    // But total_count includes all 3 tokens
    assert_eq!(tracker.total_count(), 3);
}

// =============================================================================
// E. End-to-end: full capture pipeline integration
// =============================================================================

#[test]
fn e2e_capture_pipeline_entropy_bloom_dedup_hash() {
    // Simulate a realistic capture pipeline that uses all modules together:
    // 1. Capture raw bytes from pane
    // 2. Stream hash for integrity
    // 3. Bloom filter fast-path
    // 4. Content dedup
    // 5. Entropy accounting
    // 6. Budget check

    let mut stream_hasher = StreamHash::new();
    let mut bloom = BloomFilter::with_capacity(100, 0.05);
    let mut dedup = DedupEngine::new(DedupConfig::default(), MockContentStore::new());
    let mut estimator = EntropyEstimator::new(100_000);
    let mut budget = InformationBudget::new(50_000.0);
    let bytes_total = ShardedCounter::new();

    // Simulate 10 capture cycles with a mix of unique and repeated content
    let outputs = vec![
        b"Agent 1: Starting task analysis for module refactor.".to_vec(),
        b"Agent 2: ERROR: rate limit exceeded, retrying in 30s.".to_vec(),
        b"Agent 1: Starting task analysis for module refactor.".to_vec(), // duplicate
        b"Agent 3: Code review complete. No issues found in diff.".to_vec(),
        b"Agent 2: ERROR: rate limit exceeded, retrying in 30s.".to_vec(), // duplicate
        vec![0x42; 200],                                                   // low entropy filler
        (0..200u8).collect::<Vec<u8>>(),                                   // high entropy data
        b"Agent 1: Starting task analysis for module refactor.".to_vec(),  // triple
        b"Agent 4: Build succeeded. 147 tests passed, 0 failed.".to_vec(),
        b"Session terminated. Goodbye.".to_vec(), // short, might be inline
    ];

    let mut dedup_count = 0u64;
    let mut bloom_fast_path = 0u64;

    for (i, output) in outputs.iter().enumerate() {
        // 1. Stream hash
        stream_hasher.update(output);
        bytes_total.add(output.len() as u64);

        // 2. Bloom fast-path
        let bloom_hit = bloom.contains(output);
        if !bloom_hit {
            bloom_fast_path += 1;
            bloom.insert(output);
        }

        // 3. Dedup
        let result = dedup.process_segment(output, (i as u64 + 1) * 100);
        assert!(result.is_ok());
        let result = result.unwrap();
        if result.outcome == StoreResult::Deduplicated {
            dedup_count += 1;
        }

        // 4. Entropy
        estimator.update_block(output);
    }

    // 5. Budget check
    let ent = estimator.entropy();
    let total_raw: usize = outputs.iter().map(|o| o.len()).sum();
    let cost = information_cost(total_raw, ent);
    budget.add(cost);

    // Verify pipeline coherence
    let total_from_counter = bytes_total.get();
    assert_eq!(
        total_from_counter, total_raw as u64,
        "sharded counter should match raw total"
    );
    assert_eq!(
        stream_hasher.bytes_hashed(),
        total_raw as u64,
        "stream hash byte count should match"
    );

    // Dedup should have caught the duplicates
    assert!(
        dedup_count >= 2,
        "should detect at least 2 duplicates: got {dedup_count}"
    );

    // Bloom should have fast-pathed some unique content
    assert!(
        bloom_fast_path >= 7,
        "bloom should fast-path most unique content: got {bloom_fast_path}/10"
    );

    // Entropy should be mid-range (mix of constant + varied content)
    assert!(
        ent > 1.0 && ent < 7.0,
        "entropy should be mid-range: {ent:.2}"
    );

    // Budget should not be exceeded for this small dataset
    assert!(!budget.is_exceeded());
}

#[test]
fn e2e_pane_entropy_summaries_for_eviction_ranking() {
    // Build PaneEntropySummary for multiple panes with different content profiles
    let config = EntropyEvictionConfig::default();

    // Pane 1: AI agent log output (moderate entropy, English text)
    let text_data = b"The agent processed 42 files and found 3 errors in the codebase.".repeat(50);
    let text_ent = compute_entropy(&text_data);
    let text_cost = information_cost(text_data.len(), text_ent);

    // Pane 2: Repeated error messages (low entropy — constant repetition)
    let error_data = vec![b'E'; 5000]; // Pure constant: entropy ≈ 0
    let error_ent = compute_entropy(&error_data);
    let error_cost = information_cost(error_data.len(), error_ent);

    // Pane 3: Binary/random output (high entropy)
    let binary_data: Vec<u8> = (0..5000u32)
        .map(|i| (i.wrapping_mul(97) & 0xFF) as u8)
        .collect();
    let binary_ent = compute_entropy(&binary_data);
    let binary_cost = information_cost(binary_data.len(), binary_ent);

    let summaries = vec![
        PaneEntropySummary {
            pane_id: 1,
            raw_bytes: text_data.len() as u64,
            entropy: text_ent,
            information_cost: text_cost,
            compression_ratio_bound: 8.0 / text_ent,
            eviction_score: eviction_score(text_cost, 120_000, &config),
        },
        PaneEntropySummary {
            pane_id: 2,
            raw_bytes: error_data.len() as u64,
            entropy: error_ent,
            information_cost: error_cost,
            compression_ratio_bound: 8.0 / error_ent,
            eviction_score: eviction_score(error_cost, 120_000, &config),
        },
        PaneEntropySummary {
            pane_id: 3,
            raw_bytes: binary_data.len() as u64,
            entropy: binary_ent,
            information_cost: binary_cost,
            compression_ratio_bound: if binary_ent > 0.0 {
                8.0 / binary_ent
            } else {
                f64::INFINITY
            },
            eviction_score: eviction_score(binary_cost, 120_000, &config),
        },
    ];

    // Ranking: lowest eviction_score = evict first
    let scores: Vec<(u64, f64)> = summaries
        .iter()
        .map(|s| (s.pane_id, s.eviction_score))
        .collect();
    let order = eviction_order(&scores);

    // Error pane (low entropy) should evict first
    assert_eq!(
        order[0], 2,
        "repeated error messages (pane 2) should evict first"
    );
    // Binary pane (high entropy) should evict last
    assert_eq!(
        *order.last().unwrap(),
        3,
        "high-entropy binary data (pane 3) should evict last"
    );

    // Verify compression ratio bounds make sense
    assert!(
        summaries[1].compression_ratio_bound > summaries[2].compression_ratio_bound,
        "low-entropy data should have higher compression ratio bound"
    );
}

// ═══════════════════════════════════════════════════════════════════
// E. Entropy scheduler + VOI composition
// ═══════════════════════════════════════════════════════════════════

/// Verifies that the entropy scheduler's normalized density feeds correctly
/// into VOI scheduling decisions. High-entropy panes should get both shorter
/// capture intervals (from entropy scheduler) AND higher VOI scores.
#[test]
fn entropy_scheduler_composes_with_voi_scheduler() {
    use frankenterm_core::entropy_scheduler::{EntropyScheduler, EntropySchedulerConfig};
    use frankenterm_core::voi::{VoiConfig, VoiScheduler};

    // Setup: two panes with different entropy profiles
    let mut entropy_sched = EntropyScheduler::new(EntropySchedulerConfig {
        min_samples: 10,
        ..Default::default()
    });
    entropy_sched.register_pane(1); // will get low entropy
    entropy_sched.register_pane(2); // will get high entropy

    // Feed low-entropy data to pane 1 (constant bytes)
    entropy_sched.feed_bytes(1, &[0u8; 2000]);

    // Feed high-entropy data to pane 2 (all byte values)
    let mut high_entropy_data = Vec::with_capacity(256 * 10);
    for _ in 0..10 {
        for b in 0..=255u8 {
            high_entropy_data.push(b);
        }
    }
    entropy_sched.feed_bytes(2, &high_entropy_data);

    // Verify entropy densities
    let d1 = entropy_sched.entropy_density(1).unwrap();
    let d2 = entropy_sched.entropy_density(2).unwrap();
    assert!(d2 > d1, "high-entropy pane should have higher density: d1={d1}, d2={d2}");

    // Verify entropy-based intervals
    let i1 = entropy_sched.interval_ms(1).unwrap();
    let i2 = entropy_sched.interval_ms(2).unwrap();
    assert!(i2 < i1, "high-entropy pane should have shorter interval: i1={i1}, i2={i2}");

    // Compose with VOI: use entropy density as importance weight
    let now_ms = 10_000;
    let mut voi_sched = VoiScheduler::new(VoiConfig::default());
    voi_sched.register_pane(1, now_ms);
    voi_sched.register_pane(2, now_ms);

    // Set importance proportional to entropy density
    voi_sched.set_importance(1, d1.max(0.01));
    voi_sched.set_importance(2, d2.max(0.01));

    // After some staleness, high-entropy pane should have higher VOI
    let result = voi_sched.schedule(now_ms + 5000);
    assert_eq!(result.schedule.len(), 2);
    // First entry (highest VOI) should be pane 2 (high entropy)
    assert_eq!(
        result.schedule[0].pane_id, 2,
        "high-entropy pane should have highest VOI"
    );
    assert!(
        result.schedule[0].voi > result.schedule[1].voi,
        "VOI ordering should reflect entropy-based importance"
    );
}

/// Verifies that entropy scheduling decisions are consistent with the
/// entropy accounting module's batch entropy computation.
#[test]
fn entropy_scheduler_consistent_with_batch_entropy() {
    use frankenterm_core::entropy_accounting::compute_entropy;
    use frankenterm_core::entropy_scheduler::{EntropyScheduler, EntropySchedulerConfig};

    let cfg = EntropySchedulerConfig {
        min_samples: 10,
        window_size: 100_000,
        ..Default::default()
    };

    // Test data: English text
    let text = b"The quick brown fox jumps over the lazy dog. This sentence \
        contains enough characters to provide reasonable entropy estimation.";
    let text_repeated: Vec<u8> = text.iter().copied().cycle().take(5000).collect();

    // Batch computation
    let batch_entropy = compute_entropy(&text_repeated);

    // Streaming computation via scheduler
    let mut sched = EntropyScheduler::new(cfg);
    sched.register_pane(1);
    sched.feed_bytes(1, &text_repeated);

    let streaming_entropy = sched.entropy(1).unwrap();

    // Should be within 0.5 bits (batch vs streaming with same window)
    assert!(
        (batch_entropy - streaming_entropy).abs() < 0.5,
        "batch ({batch_entropy:.3}) and streaming ({streaming_entropy:.3}) entropy should agree"
    );

    // Density should be consistent: density = entropy / 8.0
    let density = sched.entropy_density(1).unwrap();
    let expected_density = batch_entropy / 8.0;
    assert!(
        (density - expected_density).abs() < 0.1,
        "density ({density:.3}) should match batch entropy / 8 ({expected_density:.3})"
    );
}

/// Verifies that the full entropy→VOI→schedule pipeline produces
/// a meaningful capture budget allocation across a mixed workload.
#[test]
fn entropy_voi_pipeline_allocates_capture_budget() {
    use frankenterm_core::entropy_scheduler::{EntropyScheduler, EntropySchedulerConfig};
    use frankenterm_core::voi::{VoiConfig, VoiScheduler};

    let entropy_cfg = EntropySchedulerConfig {
        min_samples: 10,
        ..Default::default()
    };
    let mut entropy_sched = EntropyScheduler::new(entropy_cfg);

    // Simulate a realistic swarm: 5 panes with different activity patterns
    let pane_data: Vec<(u64, Vec<u8>)> = vec![
        (1, vec![0u8; 2000]),                                            // idle (constant)
        (2, b"ERROR: connection refused\n".repeat(100).to_vec()),        // error loop (low entropy)
        (3, (0..2000).map(|i| (i % 256) as u8).collect()),              // binary data (high entropy)
        (4, b"Processing item 12345... done.\n".repeat(80).to_vec()),    // active work (medium)
        (5, b"$ ls\nfile1.txt\nfile2.rs\n$ cargo test\n".repeat(60).to_vec()), // shell (medium-high)
    ];

    for (pane_id, data) in &pane_data {
        entropy_sched.register_pane(*pane_id);
        entropy_sched.feed_bytes(*pane_id, data);
    }

    // Compose with VOI
    let now_ms = 10_000;
    let mut voi_sched = VoiScheduler::new(VoiConfig::default());

    for (pane_id, _) in &pane_data {
        voi_sched.register_pane(*pane_id, now_ms);
        let density = entropy_sched.entropy_density(*pane_id).unwrap();
        voi_sched.set_importance(*pane_id, density.max(0.01));
    }

    // Schedule after 3 seconds of staleness
    let result = voi_sched.schedule(now_ms + 3000);
    assert_eq!(result.schedule.len(), 5);

    // The binary-data pane (id=3, highest entropy) should be first or near first
    let binary_pane_rank = result
        .schedule
        .iter()
        .position(|d| d.pane_id == 3)
        .unwrap();
    assert!(
        binary_pane_rank <= 1,
        "binary pane should be top-2 priority, got rank {binary_pane_rank}"
    );

    // The idle pane (id=1, lowest entropy) should be last or near last
    let idle_pane_rank = result
        .schedule
        .iter()
        .position(|d| d.pane_id == 1)
        .unwrap();
    assert!(
        idle_pane_rank >= 3,
        "idle pane should be bottom-2 priority, got rank {idle_pane_rank}"
    );

    // Entropy schedule should also reflect the ordering
    let entropy_result = entropy_sched.schedule();
    assert_eq!(entropy_result.decisions.len(), 5);
    // Shortest interval (first decision) should be high-entropy pane
    assert_eq!(
        entropy_result.decisions[0].pane_id, 3,
        "entropy scheduler should also prioritize high-entropy pane"
    );
}
