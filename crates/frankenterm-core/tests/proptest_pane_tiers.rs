//! Property-based tests for pane tier classification invariants.
//!
//! Bead: wa-d3dw
//!
//! Validates:
//! 1. Tier ordering: Active < Thinking < Idle < Background < Dormant
//! 2. Interval monotonicity: higher tiers have longer default intervals
//! 3. Backpressure multiplier monotonic: Green <= Yellow <= Red <= Black
//! 4. Effective interval >= base interval: backpressure only increases
//! 5. Register/unregister count: pane_count tracks correctly
//! 6. New panes start Active: registered panes begin at Active tier
//! 7. Rate-limited → Dormant: rate-limited always classifies as Dormant
//! 8. Background → Background tier: background flag always wins over thinking
//! 9. Output promotes to Active: on_pane_output resets to Active
//! 10. classify_all covers all panes: result has entry for every registered pane
//! 11. Config interval_for consistent: custom config intervals applied correctly
//! 12. Metrics total_panes consistent: metrics.total_panes == pane_count
//!
//! Serde roundtrip coverage:
//! 13. PaneTier serde roundtrip: all variants survive JSON encode/decode
//! 14. PaneTier serializes to snake_case: "active", "thinking", etc.
//! 15. PaneTier deterministic: double-serialize produces identical JSON
//! 16. PaneTier rejects invalid: non-existent variant fails deserialization
//! 17. TierConfig serde roundtrip: all fields survive JSON encode/decode
//! 18. TierConfig default from empty JSON: {} produces Default::default()
//! 19. TierConfig deterministic: double-serialize produces identical JSON
//! 20. TierMetrics serde roundtrip: all fields survive JSON encode/decode
//! 21. TierMetrics deterministic: double-serialize produces identical JSON
//! 22. TierMetrics tier_counts roundtrip: HashMap<String,u64> preserves keys
//! 23. TierMetrics estimated_rps precision: f64 survives JSON roundtrip within tolerance

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::backpressure::BackpressureTier;
use frankenterm_core::pane_tiers::{PaneTier, PaneTierClassifier, TierConfig, TierMetrics};

// =============================================================================
// Strategies
// =============================================================================

fn arb_pane_id() -> impl Strategy<Value = u64> {
    1_u64..10_000
}

fn arb_pane_ids(
    count: impl Into<proptest::collection::SizeRange>,
) -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::hash_set(arb_pane_id(), count).prop_map(|s| s.into_iter().collect())
}

fn arb_tier() -> impl Strategy<Value = PaneTier> {
    prop_oneof![
        Just(PaneTier::Active),
        Just(PaneTier::Thinking),
        Just(PaneTier::Idle),
        Just(PaneTier::Background),
        Just(PaneTier::Dormant),
    ]
}

fn arb_bp_tier() -> impl Strategy<Value = BackpressureTier> {
    prop_oneof![
        Just(BackpressureTier::Green),
        Just(BackpressureTier::Yellow),
        Just(BackpressureTier::Red),
        Just(BackpressureTier::Black),
    ]
}

fn arb_config() -> impl Strategy<Value = TierConfig> {
    (
        50_u64..500,      // active_ms
        500_u64..5000,    // thinking_ms
        2000_u64..10000,  // idle_ms
        5000_u64..30000,  // background_ms
        10000_u64..60000, // dormant_ms
        10_u64..120,      // idle_threshold_secs
        120_u64..600,     // dormant_threshold_secs
    )
        .prop_map(|(a, t, i, b, d, it, dt)| TierConfig {
            active_ms: a,
            thinking_ms: t,
            idle_ms: i,
            background_ms: b,
            dormant_ms: d,
            idle_threshold_secs: it,
            dormant_threshold_secs: dt,
        })
}

fn arb_tier_metrics() -> impl Strategy<Value = TierMetrics> {
    (
        0_u64..1000,   // active count
        0_u64..1000,   // thinking count
        0_u64..1000,   // idle count
        0_u64..1000,   // background count
        0_u64..1000,   // dormant count
        0_u64..100_000, // total_transitions
        0.001_f64..1000.0, // estimated_rps
    )
        .prop_map(|(ac, tc, ic, bc, dc, transitions, rps)| {
            let mut tier_counts = HashMap::new();
            tier_counts.insert("active".to_string(), ac);
            tier_counts.insert("thinking".to_string(), tc);
            tier_counts.insert("idle".to_string(), ic);
            tier_counts.insert("background".to_string(), bc);
            tier_counts.insert("dormant".to_string(), dc);
            let total = ac + tc + ic + bc + dc;
            TierMetrics {
                tier_counts,
                total_transitions: transitions,
                total_panes: total,
                estimated_rps: rps,
            }
        })
}

// =============================================================================
// Property: Tier ordering
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn tier_ordering_consistent(
        _dummy in 0..1_u32,
    ) {
        let tiers = PaneTier::all();
        for i in 1..tiers.len() {
            prop_assert!(tiers[i] > tiers[i - 1],
                "{:?} should be > {:?}", tiers[i], tiers[i - 1]);
        }
    }
}

// =============================================================================
// Property: Interval monotonicity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn default_intervals_increase(
        _dummy in 0..1_u32,
    ) {
        let tiers = PaneTier::all();
        for i in 1..tiers.len() {
            prop_assert!(tiers[i].default_interval() > tiers[i - 1].default_interval(),
                "{:?} interval should be > {:?} interval",
                tiers[i], tiers[i - 1]);
        }
    }

    #[test]
    fn config_intervals_applied(
        config in arb_config(),
        tier in arb_tier(),
    ) {
        let expected = match tier {
            PaneTier::Active => std::time::Duration::from_millis(config.active_ms),
            PaneTier::Thinking => std::time::Duration::from_millis(config.thinking_ms),
            PaneTier::Idle => std::time::Duration::from_millis(config.idle_ms),
            PaneTier::Background => std::time::Duration::from_millis(config.background_ms),
            PaneTier::Dormant => std::time::Duration::from_millis(config.dormant_ms),
        };
        prop_assert_eq!(config.interval_for(tier), expected);
    }
}

// =============================================================================
// Property: Backpressure multiplier monotonic
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn backpressure_multiplier_monotonic(
        tier in arb_tier(),
    ) {
        let m_g = tier.backpressure_multiplier(BackpressureTier::Green);
        let m_y = tier.backpressure_multiplier(BackpressureTier::Yellow);
        let m_r = tier.backpressure_multiplier(BackpressureTier::Red);
        let m_b = tier.backpressure_multiplier(BackpressureTier::Black);

        prop_assert!(m_g <= m_y, "Green {} <= Yellow {}", m_g, m_y);
        prop_assert!(m_y <= m_r, "Yellow {} <= Red {}", m_y, m_r);
        prop_assert!(m_r <= m_b, "Red {} <= Black {}", m_r, m_b);
    }
}

// =============================================================================
// Property: Effective interval >= base interval
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn effective_interval_gte_base(
        tier in arb_tier(),
        bp in arb_bp_tier(),
    ) {
        let base = tier.default_interval();
        let effective = tier.effective_interval(bp);
        prop_assert!(effective >= base,
            "effective {:?} should be >= base {:?} for tier={:?}, bp={:?}",
            effective, base, tier, bp);
    }
}

// =============================================================================
// Property: Register/unregister tracks count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn register_unregister_count(
        pane_ids in arb_pane_ids(1..=30),
    ) {
        let clf = PaneTierClassifier::new(TierConfig::default());

        for &id in &pane_ids {
            clf.register_pane(id);
        }
        prop_assert_eq!(clf.pane_count(), pane_ids.len());

        // Remove half
        let half = pane_ids.len() / 2;
        for &id in &pane_ids[..half] {
            clf.unregister_pane(id);
        }
        prop_assert_eq!(clf.pane_count(), pane_ids.len() - half);
    }
}

// =============================================================================
// Property: New panes start Active
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn new_panes_start_active(
        pane_id in arb_pane_id(),
    ) {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(pane_id);
        prop_assert_eq!(clf.current_tier(pane_id), PaneTier::Active);
    }
}

// =============================================================================
// Property: Rate-limited → Dormant
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn rate_limited_goes_dormant(
        pane_id in arb_pane_id(),
    ) {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(pane_id);
        clf.set_rate_limited(pane_id, true);
        prop_assert_eq!(clf.classify(pane_id), PaneTier::Dormant);
    }
}

// =============================================================================
// Property: Background wins over thinking
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn background_wins_over_thinking(
        pane_id in arb_pane_id(),
    ) {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(pane_id);
        clf.set_background(pane_id, true);
        clf.set_thinking(pane_id, true);
        prop_assert_eq!(clf.classify(pane_id), PaneTier::Background);
    }
}

// =============================================================================
// Property: Rate-limited wins over background and thinking
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn rate_limited_wins_all(
        pane_id in arb_pane_id(),
    ) {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(pane_id);
        clf.set_rate_limited(pane_id, true);
        clf.set_background(pane_id, true);
        clf.set_thinking(pane_id, true);
        prop_assert_eq!(clf.classify(pane_id), PaneTier::Dormant);
    }
}

// =============================================================================
// Property: Output promotes to Active
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn output_promotes_to_active(
        pane_id in arb_pane_id(),
    ) {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(pane_id);

        // Force to non-active (via rate_limited → classify → clear rate_limited)
        clf.set_rate_limited(pane_id, true);
        let _ = clf.classify(pane_id);
        clf.set_rate_limited(pane_id, false);

        // Now send output
        clf.on_pane_output(pane_id);
        prop_assert_eq!(clf.current_tier(pane_id), PaneTier::Active);
    }
}

// =============================================================================
// Property: classify_all covers all panes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn classify_all_covers_all(
        pane_ids in arb_pane_ids(1..=30),
    ) {
        let clf = PaneTierClassifier::new(TierConfig::default());
        for &id in &pane_ids {
            clf.register_pane(id);
        }

        let result = clf.classify_all();
        prop_assert_eq!(result.len(), pane_ids.len());
        for &id in &pane_ids {
            prop_assert!(result.contains_key(&id),
                "classify_all should contain pane {}", id);
        }
    }
}

// =============================================================================
// Property: Metrics consistent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn metrics_consistent(
        n in 1_usize..30,
    ) {
        let clf = PaneTierClassifier::new(TierConfig::default());
        for i in 0..n as u64 {
            clf.register_pane(i);
        }

        let m = clf.metrics();
        prop_assert_eq!(m.total_panes, n as u64);
        prop_assert!(m.estimated_rps > 0.0,
            "estimated_rps should be > 0 with {} panes", n);

        // Sum of tier counts should equal total panes
        let tier_sum: u64 = m.tier_counts.values().sum();
        prop_assert_eq!(tier_sum, n as u64,
            "sum of tier counts {} should equal total panes {}", tier_sum, n);
    }
}

// =============================================================================
// Property: as_u8 unique for each tier
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn as_u8_unique(
        _dummy in 0..1_u32,
    ) {
        let mut seen = std::collections::HashSet::new();
        for tier in PaneTier::all() {
            prop_assert!(seen.insert(tier.as_u8()),
                "duplicate as_u8 for {:?}", tier);
        }
    }
}

// =============================================================================
// Property: Unknown pane returns Active
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn unknown_pane_returns_active(
        pane_id in arb_pane_id(),
    ) {
        let clf = PaneTierClassifier::new(TierConfig::default());
        // Don't register the pane
        prop_assert_eq!(clf.current_tier(pane_id), PaneTier::Active);
        prop_assert_eq!(clf.classify(pane_id), PaneTier::Active);
    }
}

// =============================================================================
// Serde: PaneTier roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_tier_serde_roundtrip(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let back: PaneTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, back);
    }

    #[test]
    fn pane_tier_snake_case(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let expected = match tier {
            PaneTier::Active => "\"active\"",
            PaneTier::Thinking => "\"thinking\"",
            PaneTier::Idle => "\"idle\"",
            PaneTier::Background => "\"background\"",
            PaneTier::Dormant => "\"dormant\"",
        };
        prop_assert_eq!(json.as_str(), expected,
            "PaneTier::{:?} should serialize to {}", tier, expected);
    }

    #[test]
    fn pane_tier_deterministic(tier in arb_tier()) {
        let json1 = serde_json::to_string(&tier).unwrap();
        let json2 = serde_json::to_string(&tier).unwrap();
        prop_assert_eq!(json1, json2);
    }

    #[test]
    fn pane_tier_rejects_invalid(_dummy in 0..1_u32) {
        let result = serde_json::from_str::<PaneTier>("\"nonexistent_tier\"");
        prop_assert!(result.is_err(), "should reject invalid tier variant");
    }
}

// =============================================================================
// Serde: TierConfig roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn tier_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: TierConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.active_ms, config.active_ms);
        prop_assert_eq!(back.thinking_ms, config.thinking_ms);
        prop_assert_eq!(back.idle_ms, config.idle_ms);
        prop_assert_eq!(back.background_ms, config.background_ms);
        prop_assert_eq!(back.dormant_ms, config.dormant_ms);
        prop_assert_eq!(back.idle_threshold_secs, config.idle_threshold_secs);
        prop_assert_eq!(back.dormant_threshold_secs, config.dormant_threshold_secs);
    }

    #[test]
    fn tier_config_default_from_empty_json(_dummy in 0..1_u32) {
        let back: TierConfig = serde_json::from_str("{}").unwrap();
        let def = TierConfig::default();
        prop_assert_eq!(back.active_ms, def.active_ms);
        prop_assert_eq!(back.thinking_ms, def.thinking_ms);
        prop_assert_eq!(back.idle_ms, def.idle_ms);
        prop_assert_eq!(back.background_ms, def.background_ms);
        prop_assert_eq!(back.dormant_ms, def.dormant_ms);
        prop_assert_eq!(back.idle_threshold_secs, def.idle_threshold_secs);
        prop_assert_eq!(back.dormant_threshold_secs, def.dormant_threshold_secs);
    }

    #[test]
    fn tier_config_deterministic(config in arb_config()) {
        let json1 = serde_json::to_string(&config).unwrap();
        let json2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(json1, json2);
    }
}

// =============================================================================
// Serde: TierMetrics roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn tier_metrics_serde_roundtrip(metrics in arb_tier_metrics()) {
        let json = serde_json::to_string(&metrics).unwrap();
        let back: TierMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_transitions, metrics.total_transitions);
        prop_assert_eq!(back.total_panes, metrics.total_panes);
        prop_assert_eq!(back.tier_counts.len(), metrics.tier_counts.len());
        for (k, v) in &metrics.tier_counts {
            let back_v = back.tier_counts.get(k).copied().unwrap_or(0);
            prop_assert_eq!(back_v, *v, "tier_counts[{}] mismatch", k);
        }
        // f64 comparison with tolerance for JSON roundtrip
        prop_assert!((back.estimated_rps - metrics.estimated_rps).abs() < 1e-10,
            "estimated_rps: {} vs {}", back.estimated_rps, metrics.estimated_rps);
    }

    #[test]
    fn tier_metrics_deterministic(metrics in arb_tier_metrics()) {
        let json1 = serde_json::to_string(&metrics).unwrap();
        let json2 = serde_json::to_string(&metrics).unwrap();
        prop_assert_eq!(json1, json2);
    }

    #[test]
    fn tier_metrics_tier_counts_keys(metrics in arb_tier_metrics()) {
        let json = serde_json::to_string(&metrics).unwrap();
        let back: TierMetrics = serde_json::from_str(&json).unwrap();
        // All original keys should be preserved
        for key in metrics.tier_counts.keys() {
            prop_assert!(back.tier_counts.contains_key(key),
                "tier_counts should preserve key '{}'", key);
        }
    }

    #[test]
    fn tier_metrics_rps_precision(
        rps in 0.001_f64..100_000.0,
    ) {
        let metrics = TierMetrics {
            tier_counts: HashMap::new(),
            total_transitions: 0,
            total_panes: 0,
            estimated_rps: rps,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let back: TierMetrics = serde_json::from_str(&json).unwrap();
        prop_assert!((back.estimated_rps - rps).abs() < 1e-10,
            "rps should survive roundtrip: {} vs {}", back.estimated_rps, rps);
    }
}
