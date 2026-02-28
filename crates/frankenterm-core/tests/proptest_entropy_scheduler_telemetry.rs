//! Property-based tests for entropy scheduler telemetry counters (ft-3kxe.18).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. panes_registered / panes_added count registrations
//! 3. panes_unregistered counts removals
//! 4. byte_feeds / total_bytes_fed track feed calls
//! 5. warmup_completions fires on warmup→ready transition
//! 6. schedules_computed tracks schedule() calls
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::entropy_scheduler::{
    EntropySchedulerConfig, EntropyScheduler, EntropySchedulerTelemetrySnapshot,
};

// =============================================================================
// Helpers
// =============================================================================

fn make_scheduler() -> EntropyScheduler {
    EntropyScheduler::new(EntropySchedulerConfig::default())
}

fn make_small_warmup_scheduler() -> EntropyScheduler {
    EntropyScheduler::new(EntropySchedulerConfig {
        min_samples: 4,
        ..EntropySchedulerConfig::default()
    })
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let sched = make_scheduler();
    let snap = sched.telemetry().snapshot();

    assert_eq!(snap.panes_registered, 0);
    assert_eq!(snap.panes_added, 0);
    assert_eq!(snap.panes_unregistered, 0);
    assert_eq!(snap.byte_feeds, 0);
    assert_eq!(snap.total_bytes_fed, 0);
    assert_eq!(snap.schedules_computed, 0);
    assert_eq!(snap.warmup_completions, 0);
}

#[test]
fn register_pane_counts_all_calls() {
    let mut sched = make_scheduler();

    sched.register_pane(1);
    sched.register_pane(2);
    sched.register_pane(1); // re-registration

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 3); // counts all calls
    assert_eq!(snap.panes_added, 2);      // only unique
}

#[test]
fn unregister_pane_counts_only_existing() {
    let mut sched = make_scheduler();

    sched.register_pane(1);
    sched.register_pane(2);
    sched.unregister_pane(1);   // exists → counted
    sched.unregister_pane(99);  // doesn't exist → not counted

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.panes_unregistered, 1);
}

#[test]
fn feed_bytes_tracked() {
    let mut sched = make_scheduler();
    sched.register_pane(1);

    sched.feed_bytes(1, b"hello");
    sched.feed_bytes(1, b"world!");

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.byte_feeds, 2);
    assert_eq!(snap.total_bytes_fed, 11); // 5 + 6
}

#[test]
fn feed_byte_tracked() {
    let mut sched = make_scheduler();
    sched.register_pane(1);

    sched.feed_byte(1, b'a');
    sched.feed_byte(1, b'b');
    sched.feed_byte(1, b'c');

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.byte_feeds, 3);
    assert_eq!(snap.total_bytes_fed, 3);
}

#[test]
fn feed_to_unknown_pane_not_counted() {
    let mut sched = make_scheduler();

    sched.feed_bytes(99, b"nope");
    sched.feed_byte(99, b'x');

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.byte_feeds, 0);
    assert_eq!(snap.total_bytes_fed, 0);
}

#[test]
fn warmup_completion_tracked() {
    let mut sched = make_small_warmup_scheduler(); // min_samples=4
    sched.register_pane(1);

    // Feed 3 bytes — still in warmup
    sched.feed_bytes(1, &[1, 2, 3]);
    assert_eq!(sched.telemetry().snapshot().warmup_completions, 0);

    // Feed 1 more byte — crosses threshold (4 total)
    sched.feed_byte(1, 4);
    assert_eq!(sched.telemetry().snapshot().warmup_completions, 1);

    // Further feeds don't re-trigger
    sched.feed_bytes(1, &[5, 6, 7]);
    assert_eq!(sched.telemetry().snapshot().warmup_completions, 1);
}

#[test]
fn warmup_completion_per_pane() {
    let mut sched = make_small_warmup_scheduler();
    sched.register_pane(1);
    sched.register_pane(2);

    // Both panes cross warmup threshold
    sched.feed_bytes(1, &[1, 2, 3, 4, 5]);
    sched.feed_bytes(2, &[10, 20, 30, 40, 50]);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.warmup_completions, 2);
}

#[test]
fn schedule_counted() {
    let mut sched = make_scheduler();
    sched.register_pane(1);

    sched.schedule();
    sched.schedule();
    sched.schedule();

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.schedules_computed, 3);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = EntropySchedulerTelemetrySnapshot {
        panes_registered: 100,
        panes_added: 50,
        panes_unregistered: 10,
        byte_feeds: 500,
        total_bytes_fed: 1_000_000,
        schedules_computed: 200,
        warmup_completions: 45,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: EntropySchedulerTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn mixed_operations() {
    let mut sched = make_small_warmup_scheduler();

    sched.register_pane(1);
    sched.register_pane(2);
    sched.register_pane(3);

    sched.feed_bytes(1, &[1, 2, 3, 4, 5]); // crosses warmup
    sched.feed_bytes(2, &[10, 20]);          // still in warmup
    sched.feed_byte(3, 42);                  // still in warmup

    sched.schedule();
    sched.unregister_pane(3);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 3);
    assert_eq!(snap.panes_added, 3);
    assert_eq!(snap.panes_unregistered, 1);
    assert_eq!(snap.byte_feeds, 3); // 2 feed_bytes + 1 feed_byte
    assert_eq!(snap.total_bytes_fed, 8); // 5 + 2 + 1
    assert_eq!(snap.schedules_computed, 1);
    assert_eq!(snap.warmup_completions, 1); // only pane 1
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn panes_registered_equals_call_count(
        pane_ids in prop::collection::vec(1u64..10, 1..30),
    ) {
        let mut sched = make_scheduler();
        for &pid in &pane_ids {
            sched.register_pane(pid);
        }
        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(snap.panes_registered, pane_ids.len() as u64);
    }

    #[test]
    fn panes_added_counts_unique_ids(
        pane_ids in prop::collection::vec(1u64..5, 1..20),
    ) {
        let mut sched = make_scheduler();
        for &pid in &pane_ids {
            sched.register_pane(pid);
        }
        let snap = sched.telemetry().snapshot();
        let unique = pane_ids.iter().collect::<std::collections::HashSet<_>>().len();
        prop_assert_eq!(snap.panes_added, unique as u64);
    }

    #[test]
    fn total_bytes_fed_equals_sum(
        chunks in prop::collection::vec(
            prop::collection::vec(0u8..255, 1..50),
            1..20,
        ),
    ) {
        let mut sched = make_scheduler();
        sched.register_pane(1);

        let mut expected_bytes: u64 = 0;
        for chunk in &chunks {
            sched.feed_bytes(1, chunk);
            expected_bytes += chunk.len() as u64;
        }

        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(snap.total_bytes_fed, expected_bytes);
        prop_assert_eq!(snap.byte_feeds, chunks.len() as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..30),
    ) {
        let mut sched = make_small_warmup_scheduler();
        sched.register_pane(1);
        let mut prev = sched.telemetry().snapshot();

        for op in &ops {
            match op {
                0 => { sched.register_pane((*op as u64) + 1); }
                1 => { sched.feed_bytes(1, &[42, 43]); }
                2 => { sched.schedule(); }
                3 => { sched.unregister_pane(100); } // no-op unregister
                _ => unreachable!(),
            }

            let snap = sched.telemetry().snapshot();
            prop_assert!(snap.panes_registered >= prev.panes_registered,
                "panes_registered decreased: {} -> {}",
                prev.panes_registered, snap.panes_registered);
            prop_assert!(snap.panes_added >= prev.panes_added,
                "panes_added decreased: {} -> {}",
                prev.panes_added, snap.panes_added);
            prop_assert!(snap.byte_feeds >= prev.byte_feeds,
                "byte_feeds decreased: {} -> {}",
                prev.byte_feeds, snap.byte_feeds);
            prop_assert!(snap.total_bytes_fed >= prev.total_bytes_fed,
                "total_bytes_fed decreased: {} -> {}",
                prev.total_bytes_fed, snap.total_bytes_fed);
            prop_assert!(snap.schedules_computed >= prev.schedules_computed,
                "schedules_computed decreased: {} -> {}",
                prev.schedules_computed, snap.schedules_computed);
            prop_assert!(snap.warmup_completions >= prev.warmup_completions,
                "warmup_completions decreased: {} -> {}",
                prev.warmup_completions, snap.warmup_completions);
            prop_assert!(snap.panes_unregistered >= prev.panes_unregistered,
                "panes_unregistered decreased: {} -> {}",
                prev.panes_unregistered, snap.panes_unregistered);

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        reg in 0u64..50000,
        added in 0u64..50000,
        unreg in 0u64..10000,
        feeds in 0u64..100000,
        bytes in 0u64..1000000,
        scheds in 0u64..50000,
        warmups in 0u64..10000,
    ) {
        let snap = EntropySchedulerTelemetrySnapshot {
            panes_registered: reg,
            panes_added: added,
            panes_unregistered: unreg,
            byte_feeds: feeds,
            total_bytes_fed: bytes,
            schedules_computed: scheds,
            warmup_completions: warmups,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: EntropySchedulerTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
