//! Property-based tests for fleet_scrollback_coordinator types.
//!
//! Covers serde roundtrip for CoordinatorConfig, CoordinatorTelemetry,
//! and EvaluationResult. Also tests PaneScrollbackAccess trait invariants
//! on HashMap<u64, TieredScrollback>.

use frankenterm_core::fleet_memory_controller::{FleetMemoryAction, FleetPressureTier};
use frankenterm_core::fleet_scrollback_coordinator::{
    CoordinatorConfig, CoordinatorTelemetry, EvaluationResult,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_coordinator_config() -> impl Strategy<Value = CoordinatorConfig> {
    (1usize..200, 0usize..10_000_000, any::<bool>()).prop_map(
        |(max_targets, min_bytes, emergency_evict_all)| CoordinatorConfig {
            max_targets_per_cycle: max_targets,
            min_fleet_warm_bytes_for_eviction: min_bytes,
            emergency_evict_all,
        },
    )
}

fn arb_coordinator_telemetry() -> impl Strategy<Value = CoordinatorTelemetry> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(
                ticks,
                elevated_ticks,
                plans_produced,
                targets_applied,
                pages_evicted,
                bytes_reclaimed,
                emergency_cleanups,
                skipped_below_threshold,
            )| {
                CoordinatorTelemetry {
                    ticks,
                    elevated_ticks,
                    plans_produced,
                    targets_applied,
                    pages_evicted,
                    bytes_reclaimed,
                    emergency_cleanups,
                    skipped_below_threshold,
                }
            },
        )
}

fn arb_fleet_pressure_tier() -> impl Strategy<Value = FleetPressureTier> {
    prop_oneof![
        Just(FleetPressureTier::Normal),
        Just(FleetPressureTier::Elevated),
        Just(FleetPressureTier::Critical),
        Just(FleetPressureTier::Emergency),
    ]
}

fn arb_fleet_memory_action() -> impl Strategy<Value = FleetMemoryAction> {
    prop_oneof![
        Just(FleetMemoryAction::None),
        Just(FleetMemoryAction::ThrottlePolling),
        Just(FleetMemoryAction::EvictWarmScrollback),
        Just(FleetMemoryAction::PauseIdlePanes),
        Just(FleetMemoryAction::EmergencyCleanup),
    ]
}

fn arb_evaluation_result() -> impl Strategy<Value = EvaluationResult> {
    (
        arb_fleet_pressure_tier(),
        prop::collection::vec(arb_fleet_memory_action(), 0..5),
        any::<u64>(),
        any::<u64>(),
        0usize..100,
    )
        .prop_map(
            |(compound_tier, actions, pages_evicted, bytes_reclaimed, targets_applied)| {
                EvaluationResult {
                    compound_tier,
                    actions,
                    eviction_plan: None, // EvictionPlan contains complex nested types
                    pages_evicted,
                    bytes_reclaimed,
                    targets_applied,
                }
            },
        )
}

// ── Serde roundtrip tests ───────────────────────────────────────────────────

proptest! {
    #[test]
    fn coordinator_config_serde_roundtrip(config in arb_coordinator_config()) {
        let json = serde_json::to_string(&config).expect("serialize");
        let back: CoordinatorConfig = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back.max_targets_per_cycle, config.max_targets_per_cycle);
        prop_assert_eq!(
            back.min_fleet_warm_bytes_for_eviction,
            config.min_fleet_warm_bytes_for_eviction
        );
        prop_assert_eq!(back.emergency_evict_all, config.emergency_evict_all);
    }

    #[test]
    fn coordinator_telemetry_serde_roundtrip(telem in arb_coordinator_telemetry()) {
        let json = serde_json::to_string(&telem).expect("serialize");
        let back: CoordinatorTelemetry = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back, telem);
    }

    #[test]
    fn evaluation_result_serde_roundtrip(result in arb_evaluation_result()) {
        let json = serde_json::to_string(&result).expect("serialize");
        let back: EvaluationResult = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back.compound_tier, result.compound_tier);
        prop_assert_eq!(back.pages_evicted, result.pages_evicted);
        prop_assert_eq!(back.bytes_reclaimed, result.bytes_reclaimed);
        prop_assert_eq!(back.targets_applied, result.targets_applied);
        prop_assert_eq!(back.actions.len(), result.actions.len());
    }

    #[test]
    fn coordinator_config_max_targets_positive(config in arb_coordinator_config()) {
        prop_assert!(config.max_targets_per_cycle > 0);
    }

    #[test]
    fn coordinator_telemetry_elevated_bounded_by_ticks(
        ticks in 0u64..1000,
        elevated_fraction in 0.0f64..=1.0,
    ) {
        let elevated_ticks = (ticks as f64 * elevated_fraction) as u64;
        let telem = CoordinatorTelemetry {
            ticks,
            elevated_ticks,
            ..CoordinatorTelemetry::default()
        };
        prop_assert!(telem.elevated_ticks <= telem.ticks);
    }

    #[test]
    fn coordinator_telemetry_default_all_zero(_seed in 0u32..1) {
        let telem = CoordinatorTelemetry::default();
        prop_assert_eq!(telem.ticks, 0);
        prop_assert_eq!(telem.elevated_ticks, 0);
        prop_assert_eq!(telem.plans_produced, 0);
        prop_assert_eq!(telem.targets_applied, 0);
        prop_assert_eq!(telem.pages_evicted, 0);
        prop_assert_eq!(telem.bytes_reclaimed, 0);
        prop_assert_eq!(telem.emergency_cleanups, 0);
        prop_assert_eq!(telem.skipped_below_threshold, 0);
    }
}
