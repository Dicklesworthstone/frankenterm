//! Property-based tests for content dedup engine telemetry counters (ft-3kxe.32).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. segments_processed tracks process_segment() calls
//! 3. inserted + deduplicated + inline = segments_processed
//! 4. releases tracks release() calls
//! 5. gc_runs tracks gc() calls
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::content_dedup::{DedupConfig, DedupEngine, DedupTelemetrySnapshot};

// =============================================================================
// Helpers — use the in-module MemoryStore via DedupEngine::new
// =============================================================================

// MemoryStore is private, but we can create one through the test infrastructure.
// DedupEngine is generic over ContentStore. We need a concrete store.
// The module has a MemoryStore in tests. Let's replicate a minimal one here.

use std::collections::HashMap;

use frankenterm_core::content_dedup::{ContentStore, StoreResult};

#[derive(Default)]
struct TestStore {
    blocks: HashMap<String, (Vec<u8>, u64)>,
}

impl ContentStore for TestStore {
    fn store(&mut self, hash: &str, content: &[u8], _ts: u64) -> Result<StoreResult, String> {
        if let Some(entry) = self.blocks.get_mut(hash) {
            entry.1 += 1;
            Ok(StoreResult::Deduplicated)
        } else {
            self.blocks.insert(hash.to_string(), (content.to_vec(), 1));
            Ok(StoreResult::Inserted)
        }
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.blocks.get(hash).map(|(data, _)| data.clone()))
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
        self.blocks.retain(|_, (_, rc)| *rc > 0);
        Ok(before - self.blocks.len())
    }

    fn stats(&self) -> Result<frankenterm_core::content_dedup::DedupStats, String> {
        let unique_bytes: u64 = self.blocks.values().map(|(d, _)| d.len() as u64).sum();
        Ok(frankenterm_core::content_dedup::DedupStats {
            total_references: self.blocks.values().map(|(_, rc)| *rc).sum(),
            unique_blocks: self.blocks.len() as u64,
            unique_bytes,
            logical_bytes: unique_bytes,
            bytes_saved: 0,
            dedup_ratio: 1.0,
        })
    }

    fn contains(&self, hash: &str) -> Result<bool, String> {
        Ok(self.blocks.contains_key(hash))
    }
}

fn test_engine() -> DedupEngine<TestStore> {
    DedupEngine::new(DedupConfig::default(), TestStore::default())
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let eng = test_engine();
    let snap = eng.telemetry().snapshot();

    assert_eq!(snap.segments_processed, 0);
    assert_eq!(snap.segments_deduplicated, 0);
    assert_eq!(snap.segments_inserted, 0);
    assert_eq!(snap.segments_inline, 0);
    assert_eq!(snap.releases, 0);
    assert_eq!(snap.gc_runs, 0);
}

#[test]
fn segments_processed_tracked() {
    let mut eng = test_engine();
    eng.process_segment(b"hello world this is a longer segment", 1)
        .unwrap();
    eng.process_segment(b"another longer unique segment here", 2)
        .unwrap();

    let snap = eng.telemetry().snapshot();
    assert_eq!(snap.segments_processed, 2);
}

#[test]
fn dedup_detected() {
    let mut eng = test_engine();
    let content = b"this content is long enough to be deduplicated properly";
    eng.process_segment(content, 1).unwrap();
    eng.process_segment(content, 2).unwrap();

    let snap = eng.telemetry().snapshot();
    assert_eq!(snap.segments_processed, 2);
    assert_eq!(snap.segments_inserted, 1);
    assert_eq!(snap.segments_deduplicated, 1);
}

#[test]
fn inline_tracked() {
    let config = DedupConfig {
        min_dedup_size: 100,
        ..DedupConfig::default()
    };
    let mut eng = DedupEngine::new(config, TestStore::default());
    eng.process_segment(b"small", 1).unwrap();

    let snap = eng.telemetry().snapshot();
    assert_eq!(snap.segments_processed, 1);
    assert_eq!(snap.segments_inline, 1);
    assert_eq!(snap.segments_inserted, 0);
}

#[test]
fn releases_tracked() {
    let mut eng = test_engine();
    let content = b"content for release testing with enough length";
    eng.process_segment(content, 1).unwrap();

    let hash = frankenterm_core::content_dedup::content_hash(content);
    eng.release(&hash).unwrap();
    eng.release(&hash).unwrap();

    let snap = eng.telemetry().snapshot();
    assert_eq!(snap.releases, 2);
}

#[test]
fn gc_runs_tracked() {
    let mut eng = test_engine();
    eng.gc().unwrap();
    eng.gc().unwrap();
    eng.gc().unwrap();

    let snap = eng.telemetry().snapshot();
    assert_eq!(snap.gc_runs, 3);
}

#[test]
fn outcome_invariant() {
    let mut eng = test_engine();
    let c1 = b"unique content one is long enough for dedup";
    let c2 = b"unique content two is also long enough here";
    eng.process_segment(c1, 1).unwrap();
    eng.process_segment(c2, 2).unwrap();
    eng.process_segment(c1, 3).unwrap(); // dedup

    let snap = eng.telemetry().snapshot();
    assert_eq!(
        snap.segments_inserted + snap.segments_deduplicated + snap.segments_inline,
        snap.segments_processed,
    );
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = DedupTelemetrySnapshot {
        segments_processed: 10000,
        segments_deduplicated: 3000,
        segments_inserted: 6000,
        segments_inline: 1000,
        releases: 500,
        gc_runs: 50,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: DedupTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn segments_processed_equals_call_count(
        count in 1usize..20,
    ) {
        let mut eng = test_engine();
        for i in 0..count {
            let content = format!("content segment number {} padded to sufficient length", i);
            eng.process_segment(content.as_bytes(), i as u64).unwrap();
        }
        let snap = eng.telemetry().snapshot();
        prop_assert_eq!(snap.segments_processed, count as u64);
    }

    #[test]
    fn outcome_sum_invariant(
        n_unique in 1usize..10,
        n_repeat in 0usize..10,
    ) {
        let mut eng = test_engine();
        for i in 0..n_unique {
            let content = format!("unique segment {} with enough length to avoid inline", i);
            eng.process_segment(content.as_bytes(), i as u64).unwrap();
        }
        for i in 0..n_repeat {
            let content = format!("unique segment {} with enough length to avoid inline", i % n_unique);
            eng.process_segment(content.as_bytes(), (n_unique + i) as u64).unwrap();
        }
        let snap = eng.telemetry().snapshot();
        prop_assert_eq!(
            snap.segments_inserted + snap.segments_deduplicated + snap.segments_inline,
            snap.segments_processed,
            "inserted ({}) + deduplicated ({}) + inline ({}) != processed ({})",
            snap.segments_inserted, snap.segments_deduplicated,
            snap.segments_inline, snap.segments_processed,
        );
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..20),
    ) {
        let mut eng = test_engine();
        let mut prev = eng.telemetry().snapshot();

        for (op_count, op) in ops.iter().enumerate() {
            let op_count = op_count as u64;
            match op {
                0 => {
                    let content = format!("segment {} padded to avoid inline storage", op_count);
                    let _ = eng.process_segment(content.as_bytes(), op_count);
                }
                1 => {
                    let content = format!("repeated segment padded to sufficient length here");
                    let _ = eng.process_segment(content.as_bytes(), op_count);
                }
                2 => { let _ = eng.release("nonexistent_hash"); }
                3 => { let _ = eng.gc(); }
                _ => unreachable!(),
            }
            let snap = eng.telemetry().snapshot();
            prop_assert!(snap.segments_processed >= prev.segments_processed,
                "segments_processed decreased");
            prop_assert!(snap.segments_deduplicated >= prev.segments_deduplicated,
                "segments_deduplicated decreased");
            prop_assert!(snap.segments_inserted >= prev.segments_inserted,
                "segments_inserted decreased");
            prop_assert!(snap.segments_inline >= prev.segments_inline,
                "segments_inline decreased");
            prop_assert!(snap.releases >= prev.releases,
                "releases decreased");
            prop_assert!(snap.gc_runs >= prev.gc_runs,
                "gc_runs decreased");

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        segments_processed in 0u64..100000,
        segments_deduplicated in 0u64..50000,
        segments_inserted in 0u64..50000,
        segments_inline in 0u64..50000,
        releases in 0u64..10000,
        gc_runs in 0u64..1000,
    ) {
        let snap = DedupTelemetrySnapshot {
            segments_processed,
            segments_deduplicated,
            segments_inserted,
            segments_inline,
            releases,
            gc_runs,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: DedupTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
