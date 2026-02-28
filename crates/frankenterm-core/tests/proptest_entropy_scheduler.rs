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
//! - Standalone schedule_interval functions
//! - Config serde/Debug/Clone traits

use proptest::prelude::*;
use std::collections::HashSet;

use frankenterm_core::entropy_scheduler::{
    EntropyScheduler, EntropySchedulerConfig, EntropySchedulerSnapshot,
    EntropySchedulerTelemetrySnapshot, schedule_interval, schedule_interval_default,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_config() -> impl Strategy<Value = EntropySchedulerConfig> {
    (
        100u64..=5000,    // base_interval_ms
        10u64..=100,      // min_interval_ms
        5000u64..=60_000, // max_interval_ms
        1u64..=200,       // min_samples
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
    prop::collection::vec(1u64..=100, 1..max_count).prop_map(|mut ids| {
        ids.sort();
        ids.dedup();
        ids
    })
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
        for (i, &size) in data_sizes.iter().enumerate().take(count) {
            let pid = (i + 1) as u64;
            sched.register_pane(pid);
            let data: Vec<u8> = (0..size).map(|j| (j % 256) as u8).collect();
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
            (0.0..=1.0).contains(&result.mean_density),
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
        let expected_panes: HashSet<u64> = pane_ids.iter().copied().collect();

        prop_assert_eq!(decision_panes, expected_panes);
    }
}

// ────────────────────────────────────────────────────────────────────
// Standalone schedule_interval functions
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// schedule_interval is always clamped to [min_interval_ms, max_interval_ms].
    #[test]
    fn prop_schedule_interval_clamped(
        config in arb_config(),
        data in arb_data(2000),
    ) {
        let interval = schedule_interval(&data, &config);
        prop_assert!(
            interval >= config.min_interval_ms,
            "interval {} < min {}", interval, config.min_interval_ms
        );
        prop_assert!(
            interval <= config.max_interval_ms,
            "interval {} > max {}", interval, config.max_interval_ms
        );
    }

    /// schedule_interval_default returns a positive value.
    #[test]
    fn prop_schedule_interval_default_positive(
        data in arb_data(2000),
    ) {
        let interval = schedule_interval_default(&data);
        prop_assert!(interval > 0, "default interval should be > 0, got {}", interval);
    }

    /// Constant-byte data gets longer interval than uniform data via schedule_interval.
    #[test]
    fn prop_schedule_interval_constant_vs_uniform(
        const_byte in any::<u8>(),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let constant_data = vec![const_byte; 2000];
        let uniform_data: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();

        let interval_const = schedule_interval(&constant_data, &cfg);
        let interval_uniform = schedule_interval(&uniform_data, &cfg);

        prop_assert!(
            interval_uniform <= interval_const,
            "uniform {} should be <= constant {}", interval_uniform, interval_const
        );
    }

    /// schedule_interval with empty-like small data still returns bounded value.
    #[test]
    fn prop_schedule_interval_small_data(
        byte in any::<u8>(),
    ) {
        let cfg = EntropySchedulerConfig::default();
        let data = vec![byte; 1];
        let interval = schedule_interval(&data, &cfg);
        prop_assert!(
            interval >= cfg.min_interval_ms && interval <= cfg.max_interval_ms,
            "single-byte interval {} out of [{}, {}]",
            interval, cfg.min_interval_ms, cfg.max_interval_ms
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Config trait properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// EntropySchedulerConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: EntropySchedulerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.base_interval_ms, config.base_interval_ms);
        prop_assert_eq!(back.min_interval_ms, config.min_interval_ms);
        prop_assert_eq!(back.max_interval_ms, config.max_interval_ms);
        prop_assert_eq!(back.min_samples, config.min_samples);
        prop_assert_eq!(back.window_size, config.window_size);
        prop_assert_eq!(back.warmup_interval_ms, config.warmup_interval_ms);
        prop_assert!(
            (back.density_floor - config.density_floor).abs() < 1e-10,
            "density_floor: {} vs {}", back.density_floor, config.density_floor
        );
    }

    /// Config Clone preserves all fields.
    #[test]
    fn prop_config_clone(config in arb_config()) {
        let cloned = config.clone();
        prop_assert_eq!(cloned.base_interval_ms, config.base_interval_ms);
        prop_assert_eq!(cloned.min_interval_ms, config.min_interval_ms);
        prop_assert_eq!(cloned.max_interval_ms, config.max_interval_ms);
        prop_assert_eq!(cloned.min_samples, config.min_samples);
    }

    /// Config Debug representation contains type name.
    #[test]
    fn prop_config_debug(config in arb_config()) {
        let debug = format!("{:?}", config);
        prop_assert!(
            debug.contains("EntropySchedulerConfig"),
            "Debug should contain type name, got: {}", debug
        );
    }

    /// Default config has sensible invariants.
    #[test]
    fn prop_default_config_invariants(_dummy in Just(())) {
        let cfg = EntropySchedulerConfig::default();
        prop_assert!(
            cfg.min_interval_ms <= cfg.max_interval_ms,
            "min {} should be <= max {}", cfg.min_interval_ms, cfg.max_interval_ms
        );
        prop_assert!(cfg.density_floor > 0.0, "density_floor should be positive");
        prop_assert!(cfg.window_size > 0, "window_size should be positive");
        prop_assert!(cfg.min_samples > 0, "min_samples should be positive");
    }

    /// Multiple feed_bytes calls are equivalent to one concatenated call.
    #[test]
    fn prop_feed_bytes_concatenation(
        chunk1 in arb_data(500),
        chunk2 in arb_data(500),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            window_size: 4096,
            ..Default::default()
        };

        // Single concatenated feed
        let mut combined = chunk1.clone();
        combined.extend_from_slice(&chunk2);
        let mut sched_single = EntropyScheduler::new(cfg.clone());
        sched_single.register_pane(1);
        sched_single.feed_bytes(1, &combined);

        // Two separate feeds
        let mut sched_multi = EntropyScheduler::new(cfg);
        sched_multi.register_pane(1);
        sched_multi.feed_bytes(1, &chunk1);
        sched_multi.feed_bytes(1, &chunk2);

        let d1 = sched_single.entropy_density(1).unwrap();
        let d2 = sched_multi.entropy_density(1).unwrap();

        prop_assert!(
            (d1 - d2).abs() < 0.01,
            "concatenated density {} vs multi-feed density {} differ too much", d1, d2
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Snapshot pane state consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Snapshot pane_states contain all registered pane IDs.
    #[test]
    fn prop_snapshot_contains_all_pane_ids(
        pane_ids in arb_pane_ids(8),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        for &pid in &pane_ids {
            sched.register_pane(pid);
            sched.feed_bytes(pid, &[42u8; 50]);
        }

        let snap = sched.snapshot();
        let snap_pane_ids: HashSet<u64> = snap.pane_states.iter().map(|ps| ps.pane_id).collect();
        let expected: HashSet<u64> = pane_ids.iter().copied().collect();
        prop_assert_eq!(snap_pane_ids, expected);
    }

    /// Snapshot pane density values are bounded [0.0, 1.0].
    #[test]
    fn prop_snapshot_pane_density_bounded(
        data in arb_data(2000),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);
        sched.feed_bytes(1, &data);

        let snap = sched.snapshot();
        for ps in &snap.pane_states {
            prop_assert!(
                ps.density >= 0.0 && ps.density <= 1.0,
                "snapshot pane density {} out of [0,1]", ps.density
            );
        }
    }

    /// Snapshot total_bytes is non-negative for each pane.
    #[test]
    fn prop_snapshot_total_bytes_nonneg(
        pane_ids in arb_pane_ids(5),
        data in arb_data(1000),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        for &pid in &pane_ids {
            sched.register_pane(pid);
            sched.feed_bytes(pid, &data);
        }

        let snap = sched.snapshot();
        for ps in &snap.pane_states {
            prop_assert!(
                ps.total_bytes > 0,
                "pane {} total_bytes should be > 0 after feeding data", ps.pane_id
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Telemetry counter invariants (ft-3kxe.18)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Telemetry: panes_registered counts every register_pane call.
    #[test]
    fn telemetry_panes_registered_exact(
        pane_ids in prop::collection::vec(1u64..100, 1..30),
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        for &pid in &pane_ids {
            sched.register_pane(pid);
        }
        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(
            snap.panes_registered, pane_ids.len() as u64,
            "panes_registered={} != calls={}", snap.panes_registered, pane_ids.len()
        );
    }

    /// Telemetry: panes_added counts only first-time registrations.
    #[test]
    fn telemetry_panes_added_unique(
        pane_ids in prop::collection::vec(1u64..50, 1..30),
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        for &pid in &pane_ids {
            sched.register_pane(pid);
        }
        let unique: HashSet<_> = pane_ids.iter().collect();
        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(
            snap.panes_added, unique.len() as u64,
            "panes_added={} != unique_panes={}", snap.panes_added, unique.len()
        );
    }

    /// Telemetry: panes_unregistered counts successful removals.
    #[test]
    fn telemetry_panes_unregistered_exact(
        pane_count in 1usize..20,
        remove_ids in prop::collection::vec(0u64..30, 1..15),
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        for i in 0..pane_count {
            sched.register_pane(i as u64);
        }

        let mut removed = HashSet::new();
        for &pid in &remove_ids {
            if pid < pane_count as u64 && !removed.contains(&pid) {
                removed.insert(pid);
            }
            sched.unregister_pane(pid);
        }

        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(
            snap.panes_unregistered, removed.len() as u64,
            "panes_unregistered={} != expected={}", snap.panes_unregistered, removed.len()
        );
    }

    /// Telemetry: byte_feeds counts feed_bytes/feed_byte calls.
    #[test]
    fn telemetry_byte_feeds_exact(
        feed_count in 1usize..30,
        data_len in 1usize..100,
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 1,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);

        let data = vec![0x42u8; data_len];
        for _ in 0..feed_count {
            sched.feed_bytes(1, &data);
        }

        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(
            snap.byte_feeds, feed_count as u64,
            "byte_feeds={} != calls={}", snap.byte_feeds, feed_count
        );
    }

    /// Telemetry: total_bytes_fed tracks all bytes accurately.
    #[test]
    fn telemetry_total_bytes_fed_exact(
        feeds in prop::collection::vec(prop::collection::vec(0u8..255, 1..50), 1..20),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 1,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);

        let mut expected_bytes = 0u64;
        for feed in &feeds {
            sched.feed_bytes(1, feed);
            expected_bytes += feed.len() as u64;
        }

        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(
            snap.total_bytes_fed, expected_bytes,
            "total_bytes_fed={} != expected={}", snap.total_bytes_fed, expected_bytes
        );
    }

    /// Telemetry: warmup_completions fires exactly once per pane.
    #[test]
    fn telemetry_warmup_completions_per_pane(
        pane_count in 1usize..10,
    ) {
        let min_samples = 100u64;
        let cfg = EntropySchedulerConfig {
            min_samples,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);

        for i in 0..pane_count {
            sched.register_pane(i as u64);
        }

        // Feed enough data to exit warmup
        let data = vec![0x41u8; min_samples as usize + 10];
        for i in 0..pane_count {
            sched.feed_bytes(i as u64, &data);
        }

        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(
            snap.warmup_completions, pane_count as u64,
            "warmup_completions={} != pane_count={}", snap.warmup_completions, pane_count
        );
    }

    /// Telemetry: warmup_completions does not fire if still in warmup.
    #[test]
    fn telemetry_no_warmup_completion_before_threshold(
        feed_bytes_count in 1usize..50,
    ) {
        let min_samples = 1000u64;
        let cfg = EntropySchedulerConfig {
            min_samples,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        sched.register_pane(1);

        // Feed fewer bytes than min_samples
        let to_feed = feed_bytes_count.min(min_samples as usize - 1);
        sched.feed_bytes(1, &vec![0x42u8; to_feed]);

        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(
            snap.warmup_completions, 0,
            "should be 0 warmup completions when below min_samples"
        );
    }

    /// Telemetry: schedules_computed counts schedule() calls.
    #[test]
    fn telemetry_schedules_computed_exact(
        schedule_count in 1usize..20,
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        sched.register_pane(1);
        sched.feed_bytes(1, &[0x42u8; 10]);

        for _ in 0..schedule_count {
            sched.schedule();
        }

        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(
            snap.schedules_computed, schedule_count as u64,
            "schedules_computed={} != calls={}", snap.schedules_computed, schedule_count
        );
    }

    /// Telemetry: counters start at zero.
    #[test]
    fn telemetry_starts_at_zero(_dummy in 0u8..1) {
        let sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(snap.panes_registered, 0);
        prop_assert_eq!(snap.panes_added, 0);
        prop_assert_eq!(snap.panes_unregistered, 0);
        prop_assert_eq!(snap.byte_feeds, 0);
        prop_assert_eq!(snap.total_bytes_fed, 0);
        prop_assert_eq!(snap.schedules_computed, 0);
        prop_assert_eq!(snap.warmup_completions, 0);
    }

    /// Telemetry: counters are monotonically non-decreasing.
    #[test]
    fn telemetry_counters_monotonic(
        ops in prop::collection::vec(
            prop_oneof![
                (1u64..50).prop_map(|p| (0u8, p, vec![])),
                (1u64..50).prop_map(|p| (1u8, p, vec![])),
                (1u64..50, prop::collection::vec(0u8..255, 1..50))
                    .prop_map(|(p, d)| (2u8, p, d)),
                Just((3u8, 0, vec![])),
            ],
            5..30
        ),
    ) {
        let cfg = EntropySchedulerConfig {
            min_samples: 10,
            ..Default::default()
        };
        let mut sched = EntropyScheduler::new(cfg);
        let mut prev = sched.telemetry().snapshot();

        for (op, pid, data) in &ops {
            match op {
                0 => sched.register_pane(*pid),
                1 => sched.unregister_pane(*pid),
                2 => sched.feed_bytes(*pid, data),
                _ => { sched.schedule(); }
            }

            let snap = sched.telemetry().snapshot();
            prop_assert!(snap.panes_registered >= prev.panes_registered,
                "panes_registered decreased");
            prop_assert!(snap.panes_added >= prev.panes_added,
                "panes_added decreased");
            prop_assert!(snap.panes_unregistered >= prev.panes_unregistered,
                "panes_unregistered decreased");
            prop_assert!(snap.byte_feeds >= prev.byte_feeds,
                "byte_feeds decreased");
            prop_assert!(snap.total_bytes_fed >= prev.total_bytes_fed,
                "total_bytes_fed decreased");
            prop_assert!(snap.schedules_computed >= prev.schedules_computed,
                "schedules_computed decreased");
            prop_assert!(snap.warmup_completions >= prev.warmup_completions,
                "warmup_completions decreased");
            prev = snap;
        }
    }

    /// Telemetry: cross-counter invariant — panes_added <= panes_registered.
    #[test]
    fn telemetry_added_lte_registered(
        pane_ids in prop::collection::vec(1u64..30, 1..40),
    ) {
        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
        for &pid in &pane_ids {
            sched.register_pane(pid);
        }
        let snap = sched.telemetry().snapshot();
        prop_assert!(
            snap.panes_added <= snap.panes_registered,
            "panes_added={} > panes_registered={}",
            snap.panes_added, snap.panes_registered
        );
    }

    /// Telemetry: snapshot survives JSON serde roundtrip.
    #[test]
    fn telemetry_snapshot_serde_roundtrip(
        panes_registered in 0u64..1000,
        panes_added in 0u64..500,
        panes_unregistered in 0u64..500,
        byte_feeds in 0u64..10000,
        total_bytes_fed in 0u64..100000,
        schedules_computed in 0u64..500,
        warmup_completions in 0u64..200,
    ) {
        let snap = EntropySchedulerTelemetrySnapshot {
            panes_registered,
            panes_added,
            panes_unregistered,
            byte_feeds,
            total_bytes_fed,
            schedules_computed,
            warmup_completions,
        };

        let json = serde_json::to_string(&snap).unwrap();
        let restored: EntropySchedulerTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, snap);
    }
}
