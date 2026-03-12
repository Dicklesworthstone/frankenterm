//! Property-based tests for fleet_memory_controller types.
//!
//! Covers serde roundtrip for all serializable types, tier ordering,
//! action monotonicity, mapping function consistency, snapshot stability,
//! and recommend_actions contract verification.

use frankenterm_core::backpressure::BackpressureTier;
use frankenterm_core::fleet_memory_controller::{
    DecisionRecord, FleetMemoryAction, FleetMemoryConfig, FleetMemoryController,
    FleetMemorySnapshot, FleetPressureTier, PressureSignals, map_backpressure, map_budget_level,
    map_memory_pressure, recommend_actions,
};
use frankenterm_core::memory_budget::BudgetLevel;
use frankenterm_core::memory_pressure::MemoryPressureTier;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

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

fn arb_backpressure_tier() -> impl Strategy<Value = BackpressureTier> {
    prop_oneof![
        Just(BackpressureTier::Green),
        Just(BackpressureTier::Yellow),
        Just(BackpressureTier::Red),
        Just(BackpressureTier::Black),
    ]
}

fn arb_memory_pressure_tier() -> impl Strategy<Value = MemoryPressureTier> {
    prop_oneof![
        Just(MemoryPressureTier::Green),
        Just(MemoryPressureTier::Yellow),
        Just(MemoryPressureTier::Orange),
        Just(MemoryPressureTier::Red),
    ]
}

fn arb_budget_level() -> impl Strategy<Value = BudgetLevel> {
    prop_oneof![
        Just(BudgetLevel::Normal),
        Just(BudgetLevel::Throttled),
        Just(BudgetLevel::OverBudget),
    ]
}

fn arb_pressure_signals() -> impl Strategy<Value = PressureSignals> {
    (
        arb_backpressure_tier(),
        arb_memory_pressure_tier(),
        arb_budget_level(),
        0usize..500,
        0usize..200,
    )
        .prop_map(
            |(backpressure, memory_pressure, worst_budget, pane_count, paused_pane_count)| {
                PressureSignals {
                    backpressure,
                    memory_pressure,
                    worst_budget,
                    pane_count,
                    paused_pane_count: paused_pane_count.min(pane_count),
                }
            },
        )
}

fn arb_fleet_memory_config() -> impl Strategy<Value = FleetMemoryConfig> {
    (10usize..500, 1u64..20, 1u64..20).prop_map(
        |(max_audit_trail, escalation_threshold, deescalation_threshold)| FleetMemoryConfig {
            max_audit_trail,
            escalation_threshold,
            deescalation_threshold,
        },
    )
}

fn arb_fleet_memory_snapshot() -> impl Strategy<Value = FleetMemorySnapshot> {
    (
        arb_fleet_pressure_tier(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        prop::collection::vec(arb_fleet_memory_action(), 0..5),
    )
        .prop_map(
            |(compound_tier, total_evaluations, total_transitions, consecutive_at_tier, last_actions)| {
                FleetMemorySnapshot {
                    compound_tier,
                    total_evaluations,
                    total_transitions,
                    consecutive_at_tier,
                    last_actions,
                }
            },
        )
}

fn arb_decision_record() -> impl Strategy<Value = DecisionRecord> {
    (
        any::<u64>(),
        arb_pressure_signals(),
        arb_fleet_pressure_tier(),
        prop::collection::vec(arb_fleet_memory_action(), 1..5),
    )
        .prop_map(|(sequence, signals, compound_tier, actions)| DecisionRecord {
            sequence,
            signals,
            compound_tier,
            actions,
        })
}

// ── Serde Roundtrip Tests ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn proptest_fleet_pressure_tier_serde_roundtrip(tier in arb_fleet_pressure_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let parsed: FleetPressureTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, parsed);
    }

    #[test]
    fn proptest_fleet_memory_action_serde_roundtrip(action in arb_fleet_memory_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let parsed: FleetMemoryAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action, parsed);
    }

    #[test]
    fn proptest_fleet_memory_snapshot_serde_roundtrip(snap in arb_fleet_memory_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: FleetMemorySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, parsed);
    }

    #[test]
    fn proptest_fleet_memory_config_serde_roundtrip(cfg in arb_fleet_memory_config()) {
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: FleetMemoryConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cfg.max_audit_trail, parsed.max_audit_trail);
        prop_assert_eq!(cfg.escalation_threshold, parsed.escalation_threshold);
        prop_assert_eq!(cfg.deescalation_threshold, parsed.deescalation_threshold);
    }

    #[test]
    fn proptest_pressure_signals_serde_roundtrip(sig in arb_pressure_signals()) {
        let json = serde_json::to_string(&sig).unwrap();
        let parsed: PressureSignals = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sig.pane_count, parsed.pane_count);
        prop_assert_eq!(sig.paused_pane_count, parsed.paused_pane_count);
    }

    #[test]
    fn proptest_decision_record_serde_roundtrip(rec in arb_decision_record()) {
        let json = serde_json::to_string(&rec).unwrap();
        let parsed: DecisionRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rec.sequence, parsed.sequence);
        prop_assert_eq!(rec.compound_tier, parsed.compound_tier);
        prop_assert_eq!(rec.actions, parsed.actions);
    }
}

// ── Tier Ordering Tests ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn proptest_fleet_tier_as_u8_is_monotonic(a in arb_fleet_pressure_tier(), b in arb_fleet_pressure_tier()) {
        if a < b {
            prop_assert!(a.as_u8() < b.as_u8());
        } else if a > b {
            prop_assert!(a.as_u8() > b.as_u8());
        } else {
            prop_assert_eq!(a.as_u8(), b.as_u8());
        }
    }

    #[test]
    fn proptest_fleet_tier_ord_matches_u8(a in arb_fleet_pressure_tier(), b in arb_fleet_pressure_tier()) {
        prop_assert_eq!(a.cmp(&b), a.as_u8().cmp(&b.as_u8()));
    }

    #[test]
    fn proptest_fleet_action_ord_monotonic(a in arb_fleet_memory_action(), b in arb_fleet_memory_action()) {
        // Verify Ord is consistent with variant declaration order
        let rank = |a: FleetMemoryAction| match a {
            FleetMemoryAction::None => 0,
            FleetMemoryAction::ThrottlePolling => 1,
            FleetMemoryAction::EvictWarmScrollback => 2,
            FleetMemoryAction::PauseIdlePanes => 3,
            FleetMemoryAction::EmergencyCleanup => 4,
        };
        prop_assert_eq!(a.cmp(&b), rank(a).cmp(&rank(b)));
    }
}

// ── Mapping Function Tests ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn proptest_map_backpressure_preserves_order(a in arb_backpressure_tier(), b in arb_backpressure_tier()) {
        let mapped_a = map_backpressure(a);
        let mapped_b = map_backpressure(b);
        // Green < Yellow < Red < Black maps to Normal < Elevated < Critical < Emergency
        if a < b {
            prop_assert!(mapped_a <= mapped_b);
        }
    }

    #[test]
    fn proptest_map_memory_pressure_preserves_order(a in arb_memory_pressure_tier(), b in arb_memory_pressure_tier()) {
        let mapped_a = map_memory_pressure(a);
        let mapped_b = map_memory_pressure(b);
        if a < b {
            prop_assert!(mapped_a <= mapped_b);
        }
    }

    #[test]
    fn proptest_map_budget_level_preserves_order(a in arb_budget_level(), b in arb_budget_level()) {
        let mapped_a = map_budget_level(a);
        let mapped_b = map_budget_level(b);
        if a < b {
            prop_assert!(mapped_a <= mapped_b);
        }
    }

    #[test]
    fn proptest_map_backpressure_green_is_normal(_seed in any::<u32>()) {
        prop_assert_eq!(map_backpressure(BackpressureTier::Green), FleetPressureTier::Normal);
        prop_assert_eq!(map_backpressure(BackpressureTier::Black), FleetPressureTier::Emergency);
    }

    #[test]
    fn proptest_map_memory_green_is_normal(_seed in any::<u32>()) {
        prop_assert_eq!(map_memory_pressure(MemoryPressureTier::Green), FleetPressureTier::Normal);
        prop_assert_eq!(map_memory_pressure(MemoryPressureTier::Red), FleetPressureTier::Emergency);
    }

    #[test]
    fn proptest_map_budget_normal_is_normal(_seed in any::<u32>()) {
        prop_assert_eq!(map_budget_level(BudgetLevel::Normal), FleetPressureTier::Normal);
        prop_assert_eq!(map_budget_level(BudgetLevel::OverBudget), FleetPressureTier::Critical);
    }
}

// ── Action Property Tests ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn proptest_action_pausing_implies_specific_variants(action in arb_fleet_memory_action()) {
        if action.involves_pausing() {
            let check = matches!(action, FleetMemoryAction::PauseIdlePanes | FleetMemoryAction::EmergencyCleanup);
            prop_assert!(check);
        }
    }

    #[test]
    fn proptest_action_eviction_implies_specific_variants(action in arb_fleet_memory_action()) {
        if action.involves_eviction() {
            let check = matches!(action, FleetMemoryAction::EvictWarmScrollback | FleetMemoryAction::EmergencyCleanup);
            prop_assert!(check);
        }
    }

    #[test]
    fn proptest_emergency_action_involves_both(_seed in any::<u32>()) {
        prop_assert!(FleetMemoryAction::EmergencyCleanup.involves_pausing());
        prop_assert!(FleetMemoryAction::EmergencyCleanup.involves_eviction());
    }

    #[test]
    fn proptest_none_action_involves_neither(_seed in any::<u32>()) {
        prop_assert!(!FleetMemoryAction::None.involves_pausing());
        prop_assert!(!FleetMemoryAction::None.involves_eviction());
    }
}

// ── recommend_actions Contract Tests ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn proptest_recommend_actions_normal_returns_none(signals in arb_pressure_signals()) {
        let actions = recommend_actions(FleetPressureTier::Normal, &signals);
        prop_assert_eq!(actions, vec![FleetMemoryAction::None]);
    }

    #[test]
    fn proptest_recommend_actions_emergency_returns_cleanup(signals in arb_pressure_signals()) {
        let actions = recommend_actions(FleetPressureTier::Emergency, &signals);
        prop_assert_eq!(actions, vec![FleetMemoryAction::EmergencyCleanup]);
    }

    #[test]
    fn proptest_recommend_actions_critical_includes_pause(signals in arb_pressure_signals()) {
        let actions = recommend_actions(FleetPressureTier::Critical, &signals);
        prop_assert!(actions.contains(&FleetMemoryAction::PauseIdlePanes));
        prop_assert!(actions.contains(&FleetMemoryAction::ThrottlePolling));
        prop_assert!(actions.contains(&FleetMemoryAction::EvictWarmScrollback));
    }

    #[test]
    fn proptest_recommend_actions_elevated_throttles(signals in arb_pressure_signals()) {
        let actions = recommend_actions(FleetPressureTier::Elevated, &signals);
        prop_assert!(actions.contains(&FleetMemoryAction::ThrottlePolling));
    }

    #[test]
    fn proptest_recommend_actions_nonempty(tier in arb_fleet_pressure_tier(), signals in arb_pressure_signals()) {
        let actions = recommend_actions(tier, &signals);
        prop_assert!(!actions.is_empty());
    }
}

// ── Controller Integration Tests ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn proptest_controller_snapshot_roundtrip(config in arb_fleet_memory_config()) {
        let ctrl = FleetMemoryController::new(config);
        let snap = ctrl.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: FleetMemorySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, parsed);
    }

    #[test]
    fn proptest_controller_initial_state(config in arb_fleet_memory_config()) {
        let ctrl = FleetMemoryController::new(config);
        let snap = ctrl.snapshot();
        prop_assert_eq!(snap.compound_tier, FleetPressureTier::Normal);
        prop_assert_eq!(snap.total_evaluations, 0);
        prop_assert_eq!(snap.total_transitions, 0);
        prop_assert_eq!(snap.consecutive_at_tier, 0);
    }

    #[test]
    fn proptest_controller_evaluate_increments_count(signals in arb_pressure_signals()) {
        let mut ctrl = FleetMemoryController::default();
        let _ = ctrl.evaluate(&signals);
        let snap = ctrl.snapshot();
        prop_assert_eq!(snap.total_evaluations, 1);
    }

    #[test]
    fn proptest_controller_config_default_serde_roundtrip(_seed in any::<u32>()) {
        let cfg = FleetMemoryConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: FleetMemoryConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cfg.max_audit_trail, parsed.max_audit_trail);
        prop_assert_eq!(cfg.escalation_threshold, parsed.escalation_threshold);
        prop_assert_eq!(cfg.deescalation_threshold, parsed.deescalation_threshold);
    }
}
