//! Property-based tests for reservoir_sampler module.
//!
//! Verifies the reservoir sampling invariants:
//! - Capacity bound: len() <= capacity() at all times
//! - Fill phase: first k items are always kept
//! - Seen counter: seen() == total observe() calls
//! - Determinism: same seed produces same sample
//! - Subset property: every sample item was observed
//! - Clear resets all state
//! - Stats consistency: sampling_rate correct
//! - Stats serde roundtrip
//! - Weighted variant: same structural invariants

use proptest::prelude::*;
use std::collections::HashSet;

use frankenterm_core::reservoir_sampler::{ReservoirSampler, SamplerStats, WeightedReservoir};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_capacity() -> impl Strategy<Value = usize> {
    1usize..=20
}

fn arb_items(max_len: usize) -> impl Strategy<Value = Vec<i32>> {
    prop::collection::vec(any::<i32>(), 1..max_len)
}

fn arb_seed() -> impl Strategy<Value = u64> {
    any::<u64>()
}

fn arb_weight() -> impl Strategy<Value = f64> {
    (1u32..100).prop_map(|w| w as f64 / 10.0) // 0.1 to 10.0
}

// ────────────────────────────────────────────────────────────────────
// ReservoirSampler: capacity bound
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// len() never exceeds capacity() regardless of how many items are observed.
    #[test]
    fn prop_len_bounded_by_capacity(
        capacity in arb_capacity(),
        items in arb_items(100),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
            prop_assert!(
                rs.len() <= capacity,
                "len {} > capacity {}", rs.len(), capacity
            );
        }
    }

    /// After observing at least capacity items, len == capacity.
    #[test]
    fn prop_full_after_capacity_items(
        capacity in arb_capacity(),
        extra in 0usize..50,
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for i in 0..(capacity + extra) as i32 {
            rs.observe(i);
        }
        prop_assert_eq!(rs.len(), capacity);
    }
}

// ────────────────────────────────────────────────────────────────────
// Fill phase: first k items always kept
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// During the fill phase (seen <= capacity), all observed items are in the sample.
    #[test]
    fn prop_fill_phase_keeps_all(
        capacity in 5usize..20,
        n_items in 1usize..5,
    ) {
        let n = n_items.min(capacity);
        let mut rs = ReservoirSampler::new(capacity);
        let items: Vec<i32> = (0..n as i32).collect();

        for &item in &items {
            rs.observe(item);
        }

        prop_assert_eq!(rs.len(), n);
        let sample = rs.sample();
        for &item in &items {
            prop_assert!(
                sample.contains(&item),
                "Fill phase: item {} missing from sample", item
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Seen counter
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// seen() equals the total number of observe() calls.
    #[test]
    fn prop_seen_counter_accurate(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for (i, &item) in items.iter().enumerate() {
            rs.observe(item);
            prop_assert_eq!(rs.seen(), (i + 1) as u64);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Determinism: same seed → same sample
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Two samplers with the same seed produce identical samples.
    #[test]
    fn prop_deterministic_with_same_seed(
        capacity in arb_capacity(),
        seed in arb_seed(),
        items in arb_items(50),
    ) {
        let mut rs1 = ReservoirSampler::with_seed(capacity, seed);
        let mut rs2 = ReservoirSampler::with_seed(capacity, seed);

        for &item in &items {
            rs1.observe(item);
            rs2.observe(item);
        }

        prop_assert_eq!(rs1.sample(), rs2.sample(), "Same seed should give same sample");
    }
}

// ────────────────────────────────────────────────────────────────────
// Subset property: sample items come from the observed stream
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Every item in the sample was actually observed.
    #[test]
    fn prop_sample_subset_of_observed(
        capacity in arb_capacity(),
        seed in arb_seed(),
        items in arb_items(50),
    ) {
        let mut rs = ReservoirSampler::with_seed(capacity, seed);
        let observed: HashSet<i32> = items.iter().copied().collect();

        for &item in &items {
            rs.observe(item);
        }

        for &item in rs.sample() {
            prop_assert!(
                observed.contains(&item),
                "Sample contains {} which was never observed", item
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Clear
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// clear() resets all state.
    #[test]
    fn prop_clear_resets_all(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }

        rs.clear();

        prop_assert!(rs.is_empty());
        prop_assert_eq!(rs.len(), 0);
        prop_assert_eq!(rs.seen(), 0);
        prop_assert!(rs.sample().is_empty());
    }

    /// After clear, new observations work normally.
    #[test]
    fn prop_clear_then_reuse(
        capacity in arb_capacity(),
        items1 in arb_items(20),
        items2 in arb_items(20),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items1 {
            rs.observe(item);
        }

        rs.clear();

        for &item in &items2 {
            rs.observe(item);
        }

        prop_assert_eq!(rs.seen(), items2.len() as u64);
        prop_assert!(rs.len() <= capacity);
    }
}

// ────────────────────────────────────────────────────────────────────
// into_sample
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// into_sample returns a Vec with the same length as len().
    #[test]
    fn prop_into_sample_length(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }

        let expected_len = rs.len();
        let sample = rs.into_sample();
        prop_assert_eq!(sample.len(), expected_len);
    }
}

// ────────────────────────────────────────────────────────────────────
// Stats
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// sampling_rate == 1.0 during fill, capacity/seen after.
    #[test]
    fn prop_sampling_rate_correct(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }

        let stats = rs.stats();
        if items.len() <= capacity {
            prop_assert!(
                (stats.sampling_rate - 1.0).abs() < 1e-9,
                "During fill, rate should be 1.0 but got {}", stats.sampling_rate
            );
        } else {
            let expected = capacity as f64 / items.len() as f64;
            prop_assert!(
                (stats.sampling_rate - expected).abs() < 1e-9,
                "Rate {} != expected {}", stats.sampling_rate, expected
            );
        }
    }

    /// Stats fields are consistent.
    #[test]
    fn prop_stats_consistency(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }

        let stats = rs.stats();
        prop_assert_eq!(stats.capacity, capacity);
        prop_assert_eq!(stats.current_size, rs.len());
        prop_assert_eq!(stats.total_seen, rs.seen());
        prop_assert!(stats.sampling_rate > 0.0 && stats.sampling_rate <= 1.0);
    }

    /// SamplerStats JSON roundtrip preserves all fields.
    #[test]
    fn prop_stats_serde_roundtrip(
        capacity in arb_capacity(),
        items in arb_items(20),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }

        let stats = rs.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: SamplerStats = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(stats.capacity, back.capacity);
        prop_assert_eq!(stats.current_size, back.current_size);
        prop_assert_eq!(stats.total_seen, back.total_seen);
        prop_assert!((stats.sampling_rate - back.sampling_rate).abs() < 1e-9);
    }
}

// ────────────────────────────────────────────────────────────────────
// WeightedReservoir: capacity bound
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Weighted reservoir len() never exceeds capacity().
    #[test]
    fn prop_weighted_len_bounded(
        capacity in arb_capacity(),
        items in prop::collection::vec((any::<i32>(), arb_weight()), 1..50),
    ) {
        let mut wr = WeightedReservoir::new(capacity);
        for &(item, weight) in &items {
            wr.observe(item, weight);
            prop_assert!(
                wr.len() <= capacity,
                "weighted len {} > capacity {}", wr.len(), capacity
            );
        }
    }

    /// Weighted reservoir seen() matches observe count.
    #[test]
    fn prop_weighted_seen_accurate(
        capacity in arb_capacity(),
        items in prop::collection::vec((any::<i32>(), arb_weight()), 1..50),
    ) {
        let mut wr = WeightedReservoir::new(capacity);
        for (i, &(item, weight)) in items.iter().enumerate() {
            wr.observe(item, weight);
            prop_assert_eq!(wr.seen(), (i + 1) as u64);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// WeightedReservoir: determinism
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Same seed produces same weighted sample.
    #[test]
    fn prop_weighted_deterministic(
        capacity in arb_capacity(),
        seed in arb_seed(),
        items in prop::collection::vec((any::<i32>(), arb_weight()), 1..30),
    ) {
        let mut wr1 = WeightedReservoir::with_seed(capacity, seed);
        let mut wr2 = WeightedReservoir::with_seed(capacity, seed);

        for &(item, weight) in &items {
            wr1.observe(item, weight);
            wr2.observe(item, weight);
        }

        let s1 = wr1.into_sample();
        let s2 = wr2.into_sample();
        prop_assert_eq!(s1, s2);
    }
}

// ────────────────────────────────────────────────────────────────────
// WeightedReservoir: subset property
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Every item in weighted sample was observed.
    #[test]
    fn prop_weighted_subset(
        capacity in arb_capacity(),
        items in prop::collection::vec((any::<i32>(), arb_weight()), 1..30),
    ) {
        let mut wr = WeightedReservoir::new(capacity);
        let observed: HashSet<i32> = items.iter().map(|&(item, _)| item).collect();

        for &(item, weight) in &items {
            wr.observe(item, weight);
        }

        for &item in &wr.sample() {
            prop_assert!(
                observed.contains(item),
                "Weighted sample contains {} not in observed set", item
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// WeightedReservoir: clear
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// clear() resets weighted reservoir completely.
    #[test]
    fn prop_weighted_clear(
        capacity in arb_capacity(),
        items in prop::collection::vec((any::<i32>(), arb_weight()), 1..20),
    ) {
        let mut wr = WeightedReservoir::new(capacity);
        for &(item, weight) in &items {
            wr.observe(item, weight);
        }

        wr.clear();

        prop_assert!(wr.is_empty());
        prop_assert_eq!(wr.len(), 0);
        prop_assert_eq!(wr.seen(), 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// WeightedReservoir: stats
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Weighted stats are consistent.
    #[test]
    fn prop_weighted_stats_consistency(
        capacity in arb_capacity(),
        items in prop::collection::vec((any::<i32>(), arb_weight()), 1..30),
    ) {
        let mut wr = WeightedReservoir::new(capacity);
        for &(item, weight) in &items {
            wr.observe(item, weight);
        }

        let stats = wr.stats();
        prop_assert_eq!(stats.capacity, capacity);
        prop_assert_eq!(stats.current_size, wr.len());
        prop_assert_eq!(stats.total_seen, wr.seen());
        prop_assert!(stats.sampling_rate > 0.0 && stats.sampling_rate <= 1.0);
    }
}

// ────────────────────────────────────────────────────────────────────
// Capacity-one edge cases
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Capacity-1 sampler always has at most 1 item.
    #[test]
    fn prop_capacity_one(
        items in arb_items(20),
    ) {
        let mut rs = ReservoirSampler::new(1);
        for &item in &items {
            rs.observe(item);
            prop_assert!(rs.len() <= 1);
        }
        prop_assert_eq!(rs.len(), 1);
        prop_assert_eq!(rs.seen(), items.len() as u64);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: Capacity accessor returns constructor arg
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn sampler_capacity_accessor(capacity in arb_capacity()) {
        let rs = ReservoirSampler::<i32>::new(capacity);
        prop_assert_eq!(rs.capacity(), capacity);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: is_empty initially true
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn sampler_is_empty_initially(capacity in arb_capacity()) {
        let rs = ReservoirSampler::<i32>::new(capacity);
        prop_assert!(rs.is_empty());
        prop_assert_eq!(rs.len(), 0);
        prop_assert_eq!(rs.seen(), 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: Not empty after observe
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn sampler_not_empty_after_observe(capacity in arb_capacity()) {
        let mut rs = ReservoirSampler::new(capacity);
        rs.observe(42);
        prop_assert!(!rs.is_empty());
        prop_assert_eq!(rs.len(), 1);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: Debug output non-empty
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn sampler_debug_nonempty(capacity in arb_capacity()) {
        let rs = ReservoirSampler::<i32>::new(capacity);
        let dbg = format!("{:?}", rs);
        prop_assert!(!dbg.is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: WeightedReservoir capacity accessor
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn weighted_capacity_accessor(capacity in arb_capacity()) {
        let wr = WeightedReservoir::<i32>::new(capacity);
        prop_assert_eq!(wr.capacity(), capacity);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: WeightedReservoir is_empty initially
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn weighted_is_empty_initially(capacity in arb_capacity()) {
        let wr = WeightedReservoir::<i32>::new(capacity);
        prop_assert!(wr.is_empty());
        prop_assert_eq!(wr.len(), 0);
        prop_assert_eq!(wr.seen(), 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: WeightedReservoir not empty after observe
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn weighted_not_empty_after_observe(capacity in arb_capacity()) {
        let mut wr = WeightedReservoir::new(capacity);
        wr.observe(42, 1.0);
        prop_assert!(!wr.is_empty());
        prop_assert_eq!(wr.len(), 1);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: WeightedReservoir Debug non-empty
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn weighted_debug_nonempty(capacity in arb_capacity()) {
        let wr = WeightedReservoir::<i32>::new(capacity);
        let dbg = format!("{:?}", wr);
        prop_assert!(!dbg.is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: SamplerStats Debug non-empty
// ────────────────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn stats_debug_nonempty(capacity in arb_capacity()) {
        let rs = ReservoirSampler::<i32>::new(capacity);
        let stats = rs.stats();
        let dbg = format!("{:?}", stats);
        prop_assert!(!dbg.is_empty());
        prop_assert!(dbg.contains("SamplerStats"));
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: SamplerStats Clone preserves
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn stats_clone_preserves(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }
        let stats = rs.stats();
        let cloned = stats.clone();
        prop_assert_eq!(cloned.capacity, stats.capacity);
        prop_assert_eq!(cloned.current_size, stats.current_size);
        prop_assert_eq!(cloned.total_seen, stats.total_seen);
        prop_assert!((cloned.sampling_rate - stats.sampling_rate).abs() < 1e-15);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: SamplerStats serde deterministic
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn stats_serde_deterministic(
        capacity in arb_capacity(),
        items in arb_items(20),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }
        let stats = rs.stats();
        let j1 = serde_json::to_string(&stats).unwrap();
        let j2 = serde_json::to_string(&stats).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// ────────────────────────────────────────────────────────────────────
// SamplerStats: strategy + serde roundtrip
// ────────────────────────────────────────────────────────────────────

fn arb_sampler_stats() -> impl Strategy<Value = SamplerStats> {
    (1usize..1000, 0usize..1000, 0u64..100_000, 0.0f64..=1.0)
        .prop_map(|(capacity, current_size, total_seen, sampling_rate)| {
            let current_size = current_size.min(capacity);
            SamplerStats {
                capacity,
                current_size,
                total_seen,
                sampling_rate,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SamplerStats serde roundtrip preserves all fields.
    #[test]
    fn prop_sampler_stats_serde(stats in arb_sampler_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let back: SamplerStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.capacity, stats.capacity);
        prop_assert_eq!(back.current_size, stats.current_size);
        prop_assert_eq!(back.total_seen, stats.total_seen);
        prop_assert!((back.sampling_rate - stats.sampling_rate).abs() < 1e-12);
    }

    /// SamplerStats JSON keys are present.
    #[test]
    fn prop_sampler_stats_json_keys(stats in arb_sampler_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        prop_assert!(json.contains("\"capacity\""));
        prop_assert!(json.contains("\"current_size\""));
        prop_assert!(json.contains("\"total_seen\""));
        prop_assert!(json.contains("\"sampling_rate\""));
    }

    /// SamplerStats from sampler has consistent current_size <= capacity.
    #[test]
    fn prop_sampler_stats_size_bounded(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }
        let stats = rs.stats();
        prop_assert!(stats.current_size <= stats.capacity,
            "current_size {} > capacity {}", stats.current_size, stats.capacity);
    }

    /// SamplerStats total_seen equals items observed.
    #[test]
    fn prop_sampler_stats_total_seen(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }
        let stats = rs.stats();
        prop_assert_eq!(stats.total_seen, items.len() as u64,
            "total_seen {} != items.len() {}", stats.total_seen, items.len());
    }

    /// SamplerStats sampling_rate is in [0, 1].
    #[test]
    fn prop_sampler_stats_rate_bounded(
        capacity in arb_capacity(),
        items in arb_items(50),
    ) {
        let mut rs = ReservoirSampler::new(capacity);
        for &item in &items {
            rs.observe(item);
        }
        let stats = rs.stats();
        prop_assert!(stats.sampling_rate >= 0.0 && stats.sampling_rate <= 1.0 + 1e-10,
            "sampling_rate {} out of [0, 1]", stats.sampling_rate);
    }

    /// WeightedReservoir stats has consistent fields.
    #[test]
    fn prop_weighted_stats_consistent(
        capacity in arb_capacity(),
        items in arb_items(30),
    ) {
        let mut wr = WeightedReservoir::new(capacity);
        for &item in &items {
            wr.observe(item, 1.0);
        }
        let stats = wr.stats();
        prop_assert!(stats.current_size <= stats.capacity);
        prop_assert_eq!(stats.total_seen, items.len() as u64);
    }
}
