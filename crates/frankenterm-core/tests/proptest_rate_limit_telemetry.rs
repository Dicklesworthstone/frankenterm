//! Property-based tests for rate-limit tracker telemetry counters (ft-3kxe.11).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. Snapshot serde roundtrip
//! 3. events_recorded increments on every record_at call
//! 4. panes_evicted_lru counts LRU evictions at MAX_TRACKED_PANES
//! 5. events_pruned counts per-pane overflow at MAX_EVENTS_PER_PANE
//! 6. gc_runs and gc_panes_collected track GC operations
//! 7. panes_removed tracks explicit remove_pane calls
//! 8. Counter monotonicity across mixed operations

use proptest::prelude::*;

use frankenterm_core::patterns::AgentType;
use frankenterm_core::rate_limit_tracker::{RateLimitTelemetrySnapshot, RateLimitTracker};
use std::time::{Duration, Instant};

// =============================================================================
// Helpers
// =============================================================================

fn arb_agent_type() -> impl Strategy<Value = AgentType> {
    prop_oneof![
        Just(AgentType::Codex),
        Just(AgentType::ClaudeCode),
        Just(AgentType::Gemini),
    ]
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let tracker = RateLimitTracker::new();
    let snap = tracker.telemetry().snapshot();

    assert_eq!(snap.events_recorded, 0);
    assert_eq!(snap.panes_evicted_lru, 0);
    assert_eq!(snap.panes_removed, 0);
    assert_eq!(snap.gc_runs, 0);
    assert_eq!(snap.gc_panes_collected, 0);
    assert_eq!(snap.events_pruned, 0);
}

#[test]
fn events_recorded_increments() {
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    tracker.record_at(1, AgentType::Codex, "r1".into(), None, now);
    tracker.record_at(2, AgentType::Codex, "r2".into(), None, now);
    tracker.record_at(1, AgentType::Codex, "r3".into(), None, now);

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.events_recorded, 3);
}

#[test]
fn panes_evicted_lru_at_capacity() {
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    // Fill to MAX_TRACKED_PANES (256)
    for i in 0..256 {
        tracker.record_at(i, AgentType::Codex, "r".into(), None, now);
    }

    let snap_before = tracker.telemetry().snapshot();
    assert_eq!(snap_before.panes_evicted_lru, 0);

    // One more triggers LRU eviction
    tracker.record_at(256, AgentType::Codex, "r".into(), None, now);

    let snap_after = tracker.telemetry().snapshot();
    assert_eq!(snap_after.panes_evicted_lru, 1);
}

#[test]
fn events_pruned_at_per_pane_cap() {
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    // MAX_EVENTS_PER_PANE is 64 — fill without pruning
    for i in 0..64 {
        tracker.record_at(
            1,
            AgentType::Codex,
            format!("r{i}"),
            Some("10 seconds".into()),
            now + Duration::from_secs(i),
        );
    }

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.events_pruned, 0);
    assert_eq!(snap.events_recorded, 64);

    // One more triggers pruning
    tracker.record_at(
        1,
        AgentType::Codex,
        "overflow".into(),
        Some("10 seconds".into()),
        now + Duration::from_secs(64),
    );

    let snap2 = tracker.telemetry().snapshot();
    assert_eq!(snap2.events_pruned, 1);
    assert_eq!(snap2.events_recorded, 65);
}

#[test]
fn gc_telemetry_tracks_runs_and_collections() {
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    tracker.record_at(
        1,
        AgentType::Codex,
        "r1".into(),
        Some("10 seconds".into()),
        now,
    );
    tracker.record_at(
        2,
        AgentType::Codex,
        "r2".into(),
        Some("60 seconds".into()),
        now,
    );

    // GC before any expire
    tracker.gc_at(now + Duration::from_secs(5));
    let snap1 = tracker.telemetry().snapshot();
    assert_eq!(snap1.gc_runs, 1);
    assert_eq!(snap1.gc_panes_collected, 0);

    // GC after pane 1 expires
    tracker.gc_at(now + Duration::from_secs(15));
    let snap2 = tracker.telemetry().snapshot();
    assert_eq!(snap2.gc_runs, 2);
    assert_eq!(snap2.gc_panes_collected, 1);

    // GC after all expire
    tracker.gc_at(now + Duration::from_secs(65));
    let snap3 = tracker.telemetry().snapshot();
    assert_eq!(snap3.gc_runs, 3);
    assert_eq!(snap3.gc_panes_collected, 2);
}

#[test]
fn panes_removed_tracks_explicit_removal() {
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    tracker.record_at(1, AgentType::Codex, "r1".into(), None, now);
    tracker.record_at(2, AgentType::Codex, "r2".into(), None, now);

    tracker.remove_pane(1);
    let snap1 = tracker.telemetry().snapshot();
    assert_eq!(snap1.panes_removed, 1);

    // Removing non-existent pane doesn't increment
    tracker.remove_pane(999);
    let snap2 = tracker.telemetry().snapshot();
    assert_eq!(snap2.panes_removed, 1);

    tracker.remove_pane(2);
    let snap3 = tracker.telemetry().snapshot();
    assert_eq!(snap3.panes_removed, 2);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = RateLimitTelemetrySnapshot {
        events_recorded: 100,
        panes_evicted_lru: 5,
        panes_removed: 3,
        gc_runs: 10,
        gc_panes_collected: 8,
        events_pruned: 12,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: RateLimitTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn events_recorded_equals_call_count(
        pane_ids in prop::collection::vec(0u64..100, 1..30),
        agent_types in prop::collection::vec(arb_agent_type(), 1..30),
    ) {
        let count = pane_ids.len().min(agent_types.len());
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for i in 0..count {
            tracker.record_at(
                pane_ids[i],
                agent_types[i],
                format!("r{i}"),
                None,
                now + Duration::from_secs(i as u64),
            );
        }

        let snap = tracker.telemetry().snapshot();
        prop_assert_eq!(snap.events_recorded, count as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        pane_ids in prop::collection::vec(0u64..50, 1..20),
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        let mut prev_events = 0u64;
        let mut prev_evicted = 0u64;
        let mut prev_pruned = 0u64;

        for (i, &pane_id) in pane_ids.iter().enumerate() {
            tracker.record_at(
                pane_id,
                AgentType::Codex,
                format!("r{i}"),
                Some("30 seconds".into()),
                now + Duration::from_secs(i as u64),
            );

            let snap = tracker.telemetry().snapshot();
            prop_assert!(
                snap.events_recorded >= prev_events,
                "events_recorded must not decrease: prev={}, cur={}",
                prev_events, snap.events_recorded
            );
            prop_assert!(
                snap.panes_evicted_lru >= prev_evicted,
                "panes_evicted_lru must not decrease: prev={}, cur={}",
                prev_evicted, snap.panes_evicted_lru
            );
            prop_assert!(
                snap.events_pruned >= prev_pruned,
                "events_pruned must not decrease: prev={}, cur={}",
                prev_pruned, snap.events_pruned
            );

            prev_events = snap.events_recorded;
            prev_evicted = snap.panes_evicted_lru;
            prev_pruned = snap.events_pruned;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        events in 0u64..10000,
        evicted in 0u64..5000,
        removed in 0u64..3000,
        gc_r in 0u64..1000,
        gc_c in 0u64..5000,
        pruned in 0u64..8000,
    ) {
        let snap = RateLimitTelemetrySnapshot {
            events_recorded: events,
            panes_evicted_lru: evicted,
            panes_removed: removed,
            gc_runs: gc_r,
            gc_panes_collected: gc_c,
            events_pruned: pruned,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: RateLimitTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }

    #[test]
    fn gc_runs_equals_call_count(
        gc_count in 1usize..10,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        // Add some panes first
        for i in 0..5u64 {
            tracker.record_at(
                i,
                AgentType::Codex,
                "r".into(),
                Some("10 seconds".into()),
                now,
            );
        }

        for i in 0..gc_count {
            tracker.gc_at(now + Duration::from_secs((i * 5) as u64));
        }

        let snap = tracker.telemetry().snapshot();
        prop_assert_eq!(snap.gc_runs, gc_count as u64);
    }

    #[test]
    fn lru_evictions_bounded_by_overflow(
        extra_panes in 1usize..20,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        // Fill to capacity
        for i in 0..256u64 {
            tracker.record_at(i, AgentType::Codex, "r".into(), None, now);
        }

        // Add extra_panes more — each should trigger one LRU eviction
        for i in 0..extra_panes {
            tracker.record_at(
                256 + i as u64,
                AgentType::Codex,
                "r".into(),
                None,
                now,
            );
        }

        let snap = tracker.telemetry().snapshot();
        prop_assert_eq!(
            snap.panes_evicted_lru, extra_panes as u64,
            "each overflow insertion should evict exactly one pane"
        );
    }

    #[test]
    fn mixed_ops_counters_consistent(
        ops in prop::collection::vec(
            prop_oneof![
                (0u64..100).prop_map(|id| ("record", id)),
                (0u64..100).prop_map(|id| ("remove", id)),
                Just(("gc", 0u64)),
            ],
            1..30,
        ),
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        let mut expected_records = 0u64;
        let mut expected_gc = 0u64;

        for (i, (op, pane_id)) in ops.iter().enumerate() {
            match *op {
                "record" => {
                    tracker.record_at(
                        *pane_id,
                        AgentType::Codex,
                        format!("r{i}"),
                        Some("30 seconds".into()),
                        now + Duration::from_secs(i as u64),
                    );
                    expected_records += 1;
                }
                "remove" => {
                    tracker.remove_pane(*pane_id);
                }
                "gc" => {
                    tracker.gc_at(now + Duration::from_secs(i as u64));
                    expected_gc += 1;
                }
                _ => unreachable!(),
            }
        }

        let snap = tracker.telemetry().snapshot();
        prop_assert_eq!(
            snap.events_recorded, expected_records,
            "events_recorded should match record call count"
        );
        prop_assert_eq!(
            snap.gc_runs, expected_gc,
            "gc_runs should match gc call count"
        );
    }
}
