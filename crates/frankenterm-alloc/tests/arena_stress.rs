//! Stress tests for per-pane arena lifecycle at swarm scale.
//!
//! Validates:
//! - 200 panes can be reserved, tracked, and released without leaks
//! - Rapid creation/destruction cycles leave zero residual state
//! - Peak memory watermarks are correctly maintained under churn
//! - Concurrent-style interleaved reserve/release patterns stay consistent
//! - Stats snapshots remain sorted and complete at all scales

use frankenterm_alloc::PaneArenaRegistry;

/// Acceptance criterion: 200 panes reserved, tracked, released → 0 residual.
#[test]
fn stress_200_panes_full_lifecycle_no_leak() {
    let registry = PaneArenaRegistry::new();

    // Phase 1: Reserve 200 panes
    for pane_id in 0..200u64 {
        let outcome = registry.reserve(pane_id);
        assert!(
            outcome.is_created(),
            "pane {pane_id} should be freshly created"
        );
    }
    assert_eq!(registry.count(), 200);

    // Phase 2: Simulate scrollback growth (5 rounds of increasing allocation)
    for round in 1..=5u64 {
        for pane_id in 0..200u64 {
            let bytes = (pane_id as usize + 1) * 4096 * round as usize;
            let stats = registry
                .set_tracked_bytes(pane_id, bytes)
                .expect("pane should have arena");
            assert_eq!(stats.tracked_bytes, bytes);
            assert_eq!(stats.updates, round);
        }
    }

    // Phase 3: Verify peak watermarks
    for pane_id in 0..200u64 {
        let stats = registry.stats(pane_id).expect("stats should exist");
        let expected_peak = (pane_id as usize + 1) * 4096 * 5;
        assert_eq!(
            stats.peak_tracked_bytes, expected_peak,
            "pane {pane_id} peak mismatch"
        );
        assert_eq!(stats.updates, 5);
    }

    // Phase 4: Snapshot consistency
    let snapshot = registry.stats_snapshot();
    assert_eq!(snapshot.len(), 200);
    // Verify sorted by pane_id
    for i in 1..snapshot.len() {
        assert!(
            snapshot[i].arena.pane_id() > snapshot[i - 1].arena.pane_id(),
            "snapshot not sorted at index {i}"
        );
    }

    // Phase 5: Release all panes
    for pane_id in 0..200u64 {
        let released = registry.release(pane_id);
        assert!(released.is_some(), "pane {pane_id} should be releasable");
    }

    // Acceptance: zero residual state
    assert!(registry.is_empty());
    assert_eq!(registry.count(), 0);
    assert_eq!(registry.snapshot(), [] as [frankenterm_alloc::PaneArena; 0]);
    assert_eq!(
        registry.stats_snapshot(),
        [] as [frankenterm_alloc::PaneArenaSnapshot; 0]
    );
}

/// 10 rounds of create-all/destroy-all at 200-pane scale.
#[test]
fn stress_rapid_churn_200_panes_10_rounds() {
    let registry = PaneArenaRegistry::new();

    for round in 0..10u64 {
        // Create 200 panes
        for pane_id in 0..200u64 {
            let outcome = registry.reserve(pane_id);
            assert!(
                outcome.is_created(),
                "round {round} pane {pane_id}: should be created after prior release"
            );
            registry.set_tracked_bytes(pane_id, 4096);
        }
        assert_eq!(registry.count(), 200);

        // Verify snapshot
        let snap = registry.stats_snapshot();
        assert_eq!(snap.len(), 200);
        for s in &snap {
            assert_eq!(s.stats.tracked_bytes, 4096);
        }

        // Destroy all
        for pane_id in 0..200u64 {
            registry.release(pane_id);
        }
        assert!(
            registry.is_empty(),
            "round {round}: registry not empty after full release"
        );
    }
}

/// Interleaved reserve/release simulating agent restarts mid-swarm.
#[test]
fn stress_interleaved_reserve_release() {
    let registry = PaneArenaRegistry::new();

    // Initial swarm: 200 panes
    for pane_id in 0..200u64 {
        registry.reserve(pane_id);
        registry.set_tracked_bytes(pane_id, (pane_id as usize + 1) * 1024);
    }

    // Simulate half the agents restarting (release + re-reserve even IDs)
    for pane_id in (0..200u64).filter(|id| id % 2 == 0) {
        registry.release(pane_id);
    }
    assert_eq!(registry.count(), 100); // Only odd panes remain

    // Re-reserve the even panes (simulating agent restart)
    for pane_id in (0..200u64).filter(|id| id % 2 == 0) {
        let outcome = registry.reserve(pane_id);
        assert!(
            outcome.is_created(),
            "pane {pane_id} should be freshly created after release"
        );
        // New arenas start with fresh stats
        let stats = registry.stats(pane_id).unwrap();
        assert_eq!(stats.tracked_bytes, 0);
        assert_eq!(stats.peak_tracked_bytes, 0);
        assert_eq!(stats.updates, 0);
    }

    // Full count restored
    assert_eq!(registry.count(), 200);

    // Odd panes still have their original stats
    for pane_id in (0..200u64).filter(|id| id % 2 == 1) {
        let stats = registry.stats(pane_id).unwrap();
        assert_eq!(stats.tracked_bytes, (pane_id as usize + 1) * 1024);
    }

    // Cleanup
    for pane_id in 0..200u64 {
        registry.release(pane_id);
    }
    assert!(registry.is_empty());
}

/// Arena IDs remain monotonically increasing across churn cycles.
#[test]
fn stress_arena_id_monotonicity_across_churn() {
    let registry = PaneArenaRegistry::new();
    let mut max_arena_id = 0u32;

    for _round in 0..5 {
        for pane_id in 0..50u64 {
            let outcome = registry.reserve(pane_id);
            let arena_id = outcome.arena().arena_id().raw();
            if outcome.is_created() {
                assert!(
                    arena_id > max_arena_id,
                    "arena id {arena_id} not greater than previous max {max_arena_id}"
                );
                max_arena_id = arena_id;
            }
        }
        for pane_id in 0..50u64 {
            registry.release(pane_id);
        }
    }
}

/// Peak tracked bytes never decrease even as current bytes fluctuate.
#[test]
fn stress_peak_watermark_correctness_under_fluctuation() {
    let registry = PaneArenaRegistry::new();
    registry.reserve(0);

    // Simulate fluctuating memory: up, down, up higher, down, up highest
    let pattern = [100, 50, 200, 80, 300, 150, 400, 200, 500, 100];
    let mut expected_peak = 0usize;

    for (i, &bytes) in pattern.iter().enumerate() {
        expected_peak = expected_peak.max(bytes);
        let stats = registry.set_tracked_bytes(0, bytes).unwrap();
        assert_eq!(
            stats.peak_tracked_bytes, expected_peak,
            "step {i}: peak should be {expected_peak}"
        );
        assert_eq!(stats.tracked_bytes, bytes);
        assert_eq!(stats.updates, (i + 1) as u64);
    }

    registry.release(0);
}

/// Reserve duplicate pane IDs returns Existing (idempotent).
#[test]
fn stress_duplicate_reserve_idempotent() {
    let registry = PaneArenaRegistry::new();

    let first = registry.reserve(42);
    assert!(first.is_created());

    // 100 duplicate reserves should all return Existing with same arena
    for _ in 0..100 {
        let dup = registry.reserve(42);
        assert!(
            !dup.is_created(),
            "duplicate reserve should return Existing"
        );
        assert_eq!(dup.arena(), first.arena());
    }

    assert_eq!(registry.count(), 1);
    registry.release(42);
    assert!(registry.is_empty());
}

/// Large-scale pane IDs (u64 range) work correctly.
#[test]
fn stress_large_pane_ids() {
    let registry = PaneArenaRegistry::new();
    let large_ids: Vec<u64> = vec![
        0,
        1,
        u64::MAX,
        u64::MAX - 1,
        u64::MAX / 2,
        1_000_000,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
    ];

    for &pane_id in &large_ids {
        let outcome = registry.reserve(pane_id);
        assert!(outcome.is_created(), "pane {pane_id} should be created");
    }
    assert_eq!(registry.count(), large_ids.len());

    for &pane_id in &large_ids {
        registry.set_tracked_bytes(pane_id, 1024);
    }

    let snap = registry.stats_snapshot();
    assert_eq!(snap.len(), large_ids.len());

    for &pane_id in &large_ids {
        registry.release(pane_id);
    }
    assert!(registry.is_empty());
}

/// Total tracked bytes across all panes sum correctly.
#[test]
fn stress_aggregate_tracked_bytes() {
    let registry = PaneArenaRegistry::new();
    let pane_count = 200u64;

    for pane_id in 0..pane_count {
        registry.reserve(pane_id);
        registry.set_tracked_bytes(pane_id, (pane_id as usize + 1) * 1024);
    }

    let snapshot = registry.stats_snapshot();
    let total: usize = snapshot.iter().map(|s| s.stats.tracked_bytes).sum();
    // Sum of (1 + 2 + ... + 200) * 1024 = 200*201/2 * 1024 = 20_505_600
    let expected = (pane_count * (pane_count + 1) / 2) as usize * 1024;
    assert_eq!(total, expected);

    for pane_id in 0..pane_count {
        registry.release(pane_id);
    }
}

/// Multi-threaded stress: concurrent reserve/release from multiple threads.
#[test]
fn stress_multithreaded_concurrent_access() {
    use std::sync::Arc;

    let registry = Arc::new(PaneArenaRegistry::new());
    let thread_count = 8;
    let panes_per_thread = 25u64; // 8 * 25 = 200 total

    let handles: Vec<_> = (0..thread_count)
        .map(|thread_idx| {
            let reg = Arc::clone(&registry);
            let base = thread_idx as u64 * panes_per_thread;
            std::thread::spawn(move || {
                // Reserve panes
                for i in 0..panes_per_thread {
                    let outcome = reg.reserve(base + i);
                    assert!(outcome.is_created());
                }

                // Track bytes
                for i in 0..panes_per_thread {
                    reg.set_tracked_bytes(base + i, 4096);
                }

                // Verify stats
                for i in 0..panes_per_thread {
                    let stats = reg.stats(base + i).unwrap();
                    assert_eq!(stats.tracked_bytes, 4096);
                }

                // Release panes
                for i in 0..panes_per_thread {
                    reg.release(base + i);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    assert!(registry.is_empty());
}
