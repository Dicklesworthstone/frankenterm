//! Property-based tests for entropy_scheduler module.
//!
//! Covers the stateful EntropyScheduler invariants beyond what the
//! inline proptests verify:
//! - Register/unregister lifecycle
//! - Warmup → steady-state transition
//! - Density bounds (0.0..=1.0) for all data patterns
//! - Interval clamping (min_interval_ms..=max_interval_ms)
//! - Higher entropy → shorter interval (monotonicity)
//! - feed_bytes vs feed_byte equivalence
//! - Schedule result ordering and statistics
//! - Snapshot consistency
//! - Pane isolation: data in one pane doesn't affect another

use proptest::prelude::*;
use std::collections::HashSet;

use frankenterm_core::entropy_scheduler::{
    EntropyScheduler, EntropySchedulerConfig, EntropySchedulerSnapshot,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_config() -> impl Strategy<Value = EntropySchedulerConfig> {
    (
        100u64..=5000,      // base_interval_ms
        10u64..=100,        // min_interval_ms
        5000u64..=60_000,   // max_interval_ms
        1u64..=200,         // min_samples
    )
        .prop_map(|(base, min_i, max_i, min_s)| EntropySchedulerConfig {
            base_interval_ms: base,
            min_interval_ms: min_i,
            max_interval_ms: max_i.max(min_i + 1), // ensure max > min
            density_floor: 0.05,
            window_size: 4096,
            min_samples: min_s,
            warmup_interval_ms: 500,
        })
}

fn arb_data(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..max_len)
}

fn arb_pane_ids(max_count: usize) -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(1u64..=100, 1..max_count)
        .prop_map(|mut ids| { ids.sort(); ids.dedup(); ids })
}

// ────────────────────────────────────────────────────────────────────
// Register/unregister lifecycle
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Registering panes increases count; unregistering decreases it.
    #[test]
    fn prop_register_unregister_count(
        pane_ids in arb_pane_ids(10),
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());

        for &pid in &pane_ids {
            sched.register_pane(pid);
        }
        prop_assert_eq!(sched.pane_count(), pane_ids.len());

        for &pid in &pane_ids {
            sched.unregister_pane(pid);
        }
        prop_assert_eq!(sched.pane_count(), 0);
    }

    /// Re-registering an existing pane is a no-op (preserves data).
    #[test]
    fn prop_register_idempotent(
        data in arb_data(2000),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.feed_bytes(1, &data);

        let density_before = sched.entropy_density(1);

        // Re-register should preserve state
        sched.register_pane(1);
        prop_assert_eq!(sched.pane_count(), 1);
        prop_assert_eq!(sched.entropy_density(1), density_before);
    }

    /// Unregistered pane returns None for all queries.
    #[test]
    fn prop_unregistered_returns_none(
        pane_id in 1u64..=100,
    ) {
        let sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        prop_assert!(sched.entropy_density(pane_id).is_none());
        prop_assert!(sched.entropy(pane_id).is_none());
        prop_assert!(sched.interval_ms(pane_id).is_none());
        prop_assert!(sched.in_warmup(pane_id).is_none());
    }
}

// ────────────────────────────────────────────────────────────────────
// Warmup transition
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Pane is in warmup when total bytes < min_samples.
    #[test]
    fn prop_warmup_before_min_samples(
        min_samples in 100u64..=500,
        feed_size in 1usize..=50,
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);

        let data = vec![42u8; feed_size];
        sched.feed_bytes(1, &data);

        if (feed_size as u64) < min_samples {
            prop_assert_eq!(sched.in_warmup(1), Some(true));
        }
    }

    /// Pane exits warmup after feeding >= min_samples bytes.
    #[test]
    fn prop_exits_warmup_after_min_samples(
        min_samples in 10u64..=200,
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);

        let data: Vec<u8> = (0..(min_samples as usize + 100))
            .map(|i| (i % 256) as u8)
            .collect();
        sched.feed_bytes(1, &data);

        prop_assert_eq!(sched.in_warmup(1), Some(false));
    }

    /// During warmup, interval equals warmup_interval_ms.
    #[test]
    fn prop_warmup_uses_warmup_interval(
        warmup_ms in 100u64..=2000,
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 1000,
            warmup_interval_ms: warmup_ms,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.feed_bytes(1, &[42u8; 10]); // still in warmup

        prop_assert_eq!(sched.interval_ms(1), Some(warmup_ms));
    }
}

// ────────────────────────────────────────────────────────────────────
// Density bounds
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Density is always in [0.0, 1.0] for any data pattern.
    #[test]
    fn prop_density_bounded(
        data in arb_data(5000),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            window_size: 4096,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.feed_bytes(1, &data);

        let d = sched.entropy_density(1).unwrap();
        prop_assert!(d >= 0.0, "density {} < 0", d);
        prop_assert!(d <= 1.0, "density {} > 1", d);
    }

    /// Entropy is always in [0.0, 8.0].
    #[test]
    fn prop_entropy_bounded(
        data in arb_data(5000),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            window_size: 4096,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.feed_bytes(1, &data);

        let h = sched.entropy(1).unwrap();
        prop_assert!(h >= 0.0, "entropy {} < 0", h);
        prop_assert!(h <= 8.001, "entropy {} > 8", h); // tiny tolerance for float
    }
}

// ────────────────────────────────────────────────────────────────────
// Interval clamping
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Computed interval is always within [min_interval_ms, max_interval_ms].
    #[test]
    fn prop_interval_clamped(
        config in arb_config(),
        data in arb_data(3000),
    ) {
        let min_i = config.min_interval_ms;
        let max_i = config.max_interval_ms;
        let mut sched = EntropyScheduler::new(config);
        sched.register_pane(1);
        sched.feed_bytes(1, &data);

        // If still in warmup, interval is warmup_interval_ms
        if sched.in_warmup(1) == Some(false) {
            let interval = sched.interval_ms(1).unwrap();
            prop_assert!(
                interval >= min_i,
                "interval {} < min {}", interval, min_i
            );
            prop_assert!(
                interval <= max_i,
                "interval {} > max {}", interval, max_i
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Monotonicity: higher entropy → shorter interval
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Constant-byte pane gets longer interval than uniform-byte pane.
    #[test]
    fn prop_constant_vs_uniform_interval(
        const_byte in any::<u8>(),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1); // constant
        sched.register_pane(2); // uniform

        sched.feed_bytes(1, &vec![const_byte; 2000]);

        let uniform: Vec<u8> = (0..2560).map(|i| (i % 256) as u8).collect();
        sched.feed_bytes(2, &uniform);

        let interval_const = sched.interval_ms(1).unwrap();
        let interval_uniform = sched.interval_ms(2).unwrap();

        prop_assert!(
            interval_uniform <= interval_const,
            "uniform interval {} should be <= constant interval {}",
            interval_uniform, interval_const
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// feed_bytes vs feed_byte equivalence
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// feed_bytes(data) produces same state as sequential feed_byte calls.
    #[test]
    fn prop_feed_bytes_vs_feed_byte(
        data in arb_data(500),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            window_size: 1024,
            ..Default::default()
        };

        let mut sched_block = EntropyScheduler::new(cfg.clone());
        sched_block.register_pane(1);
        sched_block.feed_bytes(1, &data);

        let mut sched_byte = EntropyScheduler::new(cfg);
        sched_byte.register_pane(1);
        for &b in &data {
            sched_byte.feed_byte(1, b);
        }

        let d1 = sched_block.entropy_density(1).unwrap();
        let d2 = sched_byte.entropy_density(1).unwrap();

        prop_assert!(
            (d1 - d2).abs() < 0.01,
            "block density {} vs byte density {} differ too much", d1, d2
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Schedule result invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// schedule() decisions are sorted by interval ascending.
    #[test]
    fn prop_schedule_sorted_ascending(
        n_panes in 2usize..=8,
        data_sizes in prop::collection::vec(100usize..2000, 2..8),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);

        let count = n_panes.min(data_sizes.len());
        for i in 0..count {
            let pid = (i + 1) as u64;
            sched.register_pane(pid);
            let data: Vec<u8> = (0..data_sizes[i]).map(|j| (j % 256) as u8).collect();
            sched.feed_bytes(pid, &data);
        }

        let result = sched.schedule();
        for w in result.decisions.windows(2) {
            prop_assert!(
                w[0].interval_ms <= w[1].interval_ms,
                "decisions not sorted: {} > {}", w[0].interval_ms, w[1].interval_ms
            );
        }
    }

    /// schedule() decision count equals pane_count.
    #[test]
    fn prop_schedule_decision_count(
        pane_ids in arb_pane_ids(8),
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        for &pid in &pane_ids {
            sched.register_pane(pid);
        }

        let result = sched.schedule();
        prop_assert_eq!(result.decisions.len(), pane_ids.len());
    }

    /// schedule() mean_density is [0.0, 1.0].
    #[test]
    fn prop_schedule_mean_density_bounded(
        n_panes in 1usize..=5,
        data in arb_data(2000),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);

        for i in 0..n_panes {
            let pid = (i + 1) as u64;
            sched.register_pane(pid);
            sched.feed_bytes(pid, &data);
        }

        let result = sched.schedule();
        prop_assert!(
            result.mean_density >= 0.0 && result.mean_density <= 1.0,
            "mean_density {} out of [0,1]", result.mean_density
        );
    }

    /// schedule() warmup_count <= decision count.
    #[test]
    fn prop_schedule_warmup_count_bounded(
        n_panes in 1usize..=5,
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig {
            min_samples: 1000,
            ..Default::default()
        });

        for i in 0..n_panes {
            sched.register_pane((i + 1) as u64);
            // Feed tiny amount — all should be in warmup
            sched.feed_bytes((i + 1) as u64, &[42u8; 10]);
        }

        let result = sched.schedule();
        prop_assert!(result.warmup_count <= result.decisions.len());
        // All panes fed 10 bytes, min_samples=1000, so all in warmup
        prop_assert_eq!(result.warmup_count, n_panes);
    }
}

// ────────────────────────────────────────────────────────────────────
// Snapshot consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Snapshot pane_count matches scheduler pane_count.
    #[test]
    fn prop_snapshot_pane_count(
        pane_ids in arb_pane_ids(8),
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        for &pid in &pane_ids {
            sched.register_pane(pid);
        }

        let snap = sched.snapshot();
        prop_assert_eq!(snap.pane_count, pane_ids.len());
        prop_assert_eq!(snap.pane_states.len(), pane_ids.len());
    }

    /// Snapshot serializes and deserializes correctly.
    #[test]
    fn prop_snapshot_serde_roundtrip(
        n_panes in 1usize..=5,
        data in arb_data(1000),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);

        for i in 0..n_panes {
            let pid = (i + 1) as u64;
            sched.register_pane(pid);
            sched.feed_bytes(pid, &data);
        }

        let snap = sched.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let snap2: EntropySchedulerSnapshot = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(snap.pane_count, snap2.pane_count);
        prop_assert_eq!(snap.pane_states.len(), snap2.pane_states.len());
    }
}

// ────────────────────────────────────────────────────────────────────
// Pane isolation
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Feeding data to pane 1 doesn't change pane 2's state.
    #[test]
    fn prop_pane_isolation(
        data1 in arb_data(1000),
        data2 in arb_data(1000),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.register_pane(2);

        // Feed initial data to pane 2
        sched.feed_bytes(2, &data2);
        let density2_before = sched.entropy_density(2);
        let interval2_before = sched.interval_ms(2);

        // Feed data to pane 1
        sched.feed_bytes(1, &data1);

        // Pane 2 should be unchanged
        prop_assert_eq!(sched.entropy_density(2), density2_before);
        prop_assert_eq!(sched.interval_ms(2), interval2_before);
    }
}

// ────────────────────────────────────────────────────────────────────
// Feed to unregistered pane
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feeding data to an unregistered pane is a safe no-op.
    #[test]
    fn prop_feed_unregistered_noop(
        data in arb_data(500),
        pane_id in 1u64..=100,
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        // No panic
        sched.feed_bytes(pane_id, &data);
        for &b in &data {
            sched.feed_byte(pane_id, b);
        }
        prop_assert_eq!(sched.pane_count(), 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// Decision pane_id set matches registered panes
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Every pane appears exactly once in schedule decisions.
    #[test]
    fn prop_schedule_covers_all_panes(
        pane_ids in arb_pane_ids(8),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        for &pid in &pane_ids {
            sched.register_pane(pid);
            sched.feed_bytes(pid, &[42u8; 100]);
        }

        let result = sched.schedule();
        let decision_panes: HashSet<u64> = result.decisions.iter().map(|d| d.pane_id).collect();
        let expected_panes: HashSet<u64> = pane_ids.iter().cloned().collect();

        prop_assert_eq!(decision_panes, expected_panes);
    }
}
