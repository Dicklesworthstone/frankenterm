//! Property-based tests for the `tantivy_policy` module.
//!
//! Covers `LoadRegime` classify + serde, `MergeStrategy` serde,
//! `CommitPolicy` serde/defaults/for_regime, `MergePolicyConfig`
//! serde/defaults/for_regime, and `IndexTuningConfig` serde/defaults.

use std::time::Duration;

use frankenterm_core::tantivy_policy::{
    CommitPolicy, IndexTuningConfig, LoadRegime, MergePolicyConfig, MergeStrategy,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_load_regime() -> impl Strategy<Value = LoadRegime> {
    prop_oneof![
        Just(LoadRegime::Idle),
        Just(LoadRegime::Steady),
        Just(LoadRegime::Burst),
        Just(LoadRegime::Overload),
    ]
}

fn arb_merge_strategy() -> impl Strategy<Value = MergeStrategy> {
    prop_oneof![
        Just(MergeStrategy::LogMerge),
        Just(MergeStrategy::Aggressive),
        Just(MergeStrategy::Conservative),
        Just(MergeStrategy::NoMerge),
    ]
}

fn arb_commit_policy() -> impl Strategy<Value = CommitPolicy> {
    (
        1_u64..200_000,
        1_u64..500_000_000,
        1_u64..60_000,
        1_u64..10_000,
    )
        .prop_map(
            |(max_docs, max_bytes, max_interval_ms, min_interval_ms)| CommitPolicy {
                max_docs_before_commit: max_docs,
                max_bytes_before_commit: max_bytes,
                max_interval: Duration::from_millis(max_interval_ms),
                min_interval: Duration::from_millis(min_interval_ms),
            },
        )
}

fn arb_merge_policy_config() -> impl Strategy<Value = MergePolicyConfig> {
    (
        arb_merge_strategy(),
        1_u32..200,
        1_u32..50,
        1_u64..10_000_000,
        1_u64..10_000_000_000,
        1_u32..50,
        1_u32..10,
    )
        .prop_map(
            |(
                strategy,
                max_segment_count,
                target_segment_count,
                min_segment_size_bytes,
                max_segment_size_bytes,
                max_merge_factor,
                max_concurrent_merges,
            )| MergePolicyConfig {
                strategy,
                max_segment_count,
                target_segment_count,
                min_segment_size_bytes,
                max_segment_size_bytes,
                max_merge_factor,
                max_concurrent_merges,
            },
        )
}

// =========================================================================
// LoadRegime — classify + serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_regime_serde(regime in arb_load_regime()) {
        let json = serde_json::to_string(&regime).unwrap();
        let back: LoadRegime = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, regime);
    }

    #[test]
    fn prop_regime_snake_case(regime in arb_load_regime()) {
        let json = serde_json::to_string(&regime).unwrap();
        let expected = match regime {
            LoadRegime::Idle => "\"idle\"",
            LoadRegime::Steady => "\"steady\"",
            LoadRegime::Burst => "\"burst\"",
            LoadRegime::Overload => "\"overload\"",
        };
        prop_assert_eq!(&json, expected);
    }

    /// classify correctly maps event rates to load regimes.
    #[test]
    fn prop_classify_idle(rate in 0.0_f64..10.0) {
        prop_assert_eq!(LoadRegime::classify(rate), LoadRegime::Idle);
    }

    #[test]
    fn prop_classify_steady(rate in 10.0_f64..500.0) {
        prop_assert_eq!(LoadRegime::classify(rate), LoadRegime::Steady);
    }

    #[test]
    fn prop_classify_burst(rate in 500.0_f64..5000.0) {
        prop_assert_eq!(LoadRegime::classify(rate), LoadRegime::Burst);
    }

    #[test]
    fn prop_classify_overload(rate in 5000.0_f64..100_000.0) {
        prop_assert_eq!(LoadRegime::classify(rate), LoadRegime::Overload);
    }

    /// classify is deterministic.
    #[test]
    fn prop_classify_deterministic(rate in 0.0_f64..100_000.0) {
        let r1 = LoadRegime::classify(rate);
        let r2 = LoadRegime::classify(rate);
        prop_assert_eq!(r1, r2);
    }
}

// =========================================================================
// MergeStrategy — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_strategy_serde(strategy in arb_merge_strategy()) {
        let json = serde_json::to_string(&strategy).unwrap();
        let back: MergeStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, strategy);
    }

    #[test]
    fn prop_strategy_snake_case(strategy in arb_merge_strategy()) {
        let json = serde_json::to_string(&strategy).unwrap();
        let expected = match strategy {
            MergeStrategy::LogMerge => "\"log_merge\"",
            MergeStrategy::Aggressive => "\"aggressive\"",
            MergeStrategy::Conservative => "\"conservative\"",
            MergeStrategy::NoMerge => "\"no_merge\"",
        };
        prop_assert_eq!(&json, expected);
    }
}

// =========================================================================
// CommitPolicy — serde + defaults + for_regime
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_commit_policy_serde(policy in arb_commit_policy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let back: CommitPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, policy);
    }

    #[test]
    fn prop_commit_policy_defaults(_dummy in 0..1_u8) {
        let policy = CommitPolicy::default();
        prop_assert_eq!(policy.max_docs_before_commit, 10_000);
        prop_assert_eq!(policy.max_bytes_before_commit, 64 * 1024 * 1024);
        prop_assert_eq!(policy.max_interval, Duration::from_secs(5));
        prop_assert_eq!(policy.min_interval, Duration::from_millis(500));
    }

    /// for_regime returns a valid policy for every regime.
    #[test]
    fn prop_commit_for_regime_valid(regime in arb_load_regime()) {
        let policy = CommitPolicy::for_regime(regime);
        prop_assert!(policy.max_docs_before_commit > 0);
        prop_assert!(policy.max_bytes_before_commit > 0);
        prop_assert!(policy.max_interval > Duration::ZERO);
        prop_assert!(policy.min_interval > Duration::ZERO);
        prop_assert!(policy.min_interval <= policy.max_interval);
    }

    /// for_regime(Steady) returns default.
    #[test]
    fn prop_commit_steady_is_default(_dummy in 0..1_u8) {
        let steady = CommitPolicy::for_regime(LoadRegime::Steady);
        let default = CommitPolicy::default();
        prop_assert_eq!(steady, default);
    }

    /// for_regime is deterministic.
    #[test]
    fn prop_commit_for_regime_deterministic(regime in arb_load_regime()) {
        let p1 = CommitPolicy::for_regime(regime);
        let p2 = CommitPolicy::for_regime(regime);
        prop_assert_eq!(p1, p2);
    }
}

// =========================================================================
// MergePolicyConfig — serde + defaults + for_regime
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_merge_config_serde(config in arb_merge_policy_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: MergePolicyConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, config);
    }

    #[test]
    fn prop_merge_config_defaults(_dummy in 0..1_u8) {
        let config = MergePolicyConfig::default();
        prop_assert_eq!(config.strategy, MergeStrategy::LogMerge);
        prop_assert_eq!(config.max_segment_count, 30);
        prop_assert_eq!(config.target_segment_count, 8);
    }

    /// for_regime returns valid config for every regime.
    #[test]
    fn prop_merge_for_regime_valid(regime in arb_load_regime()) {
        let config = MergePolicyConfig::for_regime(regime);
        prop_assert!(config.max_segment_count > 0);
        prop_assert!(config.target_segment_count > 0);
        prop_assert!(config.target_segment_count <= config.max_segment_count);
        prop_assert!(config.max_merge_factor > 0);
        prop_assert!(config.max_concurrent_merges > 0);
    }

    /// for_regime(Steady) returns default.
    #[test]
    fn prop_merge_steady_is_default(_dummy in 0..1_u8) {
        let steady = MergePolicyConfig::for_regime(LoadRegime::Steady);
        let default = MergePolicyConfig::default();
        prop_assert_eq!(steady, default);
    }

    /// Overload regime disables merge (NoMerge strategy).
    #[test]
    fn prop_merge_overload_no_merge(_dummy in 0..1_u8) {
        let config = MergePolicyConfig::for_regime(LoadRegime::Overload);
        prop_assert_eq!(config.strategy, MergeStrategy::NoMerge);
    }
}

// =========================================================================
// IndexTuningConfig — serde + defaults
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_tuning_config_defaults(_dummy in 0..1_u8) {
        let config = IndexTuningConfig::default();
        prop_assert_eq!(config.commit, CommitPolicy::default());
        prop_assert_eq!(config.merge, MergePolicyConfig::default());
        prop_assert_eq!(config.writer_heap_bytes, 128 * 1024 * 1024);
        prop_assert_eq!(config.indexing_threads, 0);
        prop_assert!(config.adaptive);
        prop_assert_eq!(config.rate_window_secs, 30);
    }

    #[test]
    fn prop_tuning_config_serde(
        writer_heap in 1_u64..1_000_000_000,
        threads in 0_u32..16,
        adaptive in any::<bool>(),
        window in 1_u32..120,
    ) {
        let config = IndexTuningConfig {
            commit: CommitPolicy::default(),
            merge: MergePolicyConfig::default(),
            writer_heap_bytes: writer_heap,
            indexing_threads: threads,
            adaptive,
            rate_window_secs: window,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: IndexTuningConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, config);
    }

    #[test]
    fn prop_tuning_config_deterministic(
        adaptive in any::<bool>(),
    ) {
        let config = IndexTuningConfig {
            adaptive,
            ..IndexTuningConfig::default()
        };
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn all_regimes_distinct_json() {
    let regimes = [
        LoadRegime::Idle,
        LoadRegime::Steady,
        LoadRegime::Burst,
        LoadRegime::Overload,
    ];
    let jsons: Vec<_> = regimes.iter().map(|r| serde_json::to_string(r).unwrap()).collect();
    for i in 0..jsons.len() {
        for j in (i + 1)..jsons.len() {
            assert_ne!(jsons[i], jsons[j]);
        }
    }
}

#[test]
fn all_strategies_distinct_json() {
    let strategies = [
        MergeStrategy::LogMerge,
        MergeStrategy::Aggressive,
        MergeStrategy::Conservative,
        MergeStrategy::NoMerge,
    ];
    let jsons: Vec<_> = strategies.iter().map(|s| serde_json::to_string(s).unwrap()).collect();
    for i in 0..jsons.len() {
        for j in (i + 1)..jsons.len() {
            assert_ne!(jsons[i], jsons[j]);
        }
    }
}

#[test]
fn classify_boundary_values() {
    assert_eq!(LoadRegime::classify(0.0), LoadRegime::Idle);
    assert_eq!(LoadRegime::classify(9.99), LoadRegime::Idle);
    assert_eq!(LoadRegime::classify(10.0), LoadRegime::Steady);
    assert_eq!(LoadRegime::classify(499.99), LoadRegime::Steady);
    assert_eq!(LoadRegime::classify(500.0), LoadRegime::Burst);
    assert_eq!(LoadRegime::classify(4999.99), LoadRegime::Burst);
    assert_eq!(LoadRegime::classify(5000.0), LoadRegime::Overload);
}
