//! Property tests for context_budget module (ft-3681t.9.4).
//!
//! Covers serde roundtrips, pressure tier classification boundaries,
//! tracker utilization arithmetic, compaction history eviction,
//! recovery guidance tier mapping, registry aggregation, and snapshot
//! consistency.

use frankenterm_core::context_budget::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_pressure_tier() -> impl Strategy<Value = ContextPressureTier> {
    prop_oneof![
        Just(ContextPressureTier::Green),
        Just(ContextPressureTier::Yellow),
        Just(ContextPressureTier::Red),
        Just(ContextPressureTier::Black),
    ]
}

fn arb_compaction_trigger() -> impl Strategy<Value = CompactionTrigger> {
    prop_oneof![
        Just(CompactionTrigger::Automatic),
        Just(CompactionTrigger::OperatorInitiated),
        Just(CompactionTrigger::SessionRotation),
        Just(CompactionTrigger::PatternDetected),
    ]
}

fn arb_recovery_action() -> impl Strategy<Value = RecoveryAction> {
    prop_oneof![
        Just(RecoveryAction::SendCompact),
        Just(RecoveryAction::RotateSession),
        Just(RecoveryAction::ReduceVerbosity),
        Just(RecoveryAction::Monitor),
    ]
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_pressure_tier(tier in arb_pressure_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let back: ContextPressureTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, back);
    }

    #[test]
    fn serde_roundtrip_compaction_trigger(trigger in arb_compaction_trigger()) {
        let json = serde_json::to_string(&trigger).unwrap();
        let back: CompactionTrigger = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(trigger, back);
    }

    #[test]
    fn serde_roundtrip_recovery_action(action in arb_recovery_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: RecoveryAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action, back);
    }
}

// =============================================================================
// Pressure tier classification
// =============================================================================

proptest! {
    #[test]
    fn pressure_tier_boundaries(ratio in 0.0..1.0f64) {
        let tier = ContextPressureTier::from_utilization(ratio);
        if ratio >= 0.90 {
            prop_assert_eq!(tier, ContextPressureTier::Black);
        } else if ratio >= 0.75 {
            prop_assert_eq!(tier, ContextPressureTier::Red);
        } else if ratio >= 0.50 {
            prop_assert_eq!(tier, ContextPressureTier::Yellow);
        } else {
            prop_assert_eq!(tier, ContextPressureTier::Green);
        }
    }

    #[test]
    fn pressure_tier_ordering(ratio in 0.0..1.0f64) {
        let tier = ContextPressureTier::from_utilization(ratio);
        // Green < Yellow < Red < Black (derives PartialOrd, Ord)
        prop_assert!(tier >= ContextPressureTier::Green);
        prop_assert!(tier <= ContextPressureTier::Black);
    }

    #[test]
    fn needs_attention_only_red_and_black(tier in arb_pressure_tier()) {
        let expected = matches!(tier, ContextPressureTier::Red | ContextPressureTier::Black);
        prop_assert_eq!(tier.needs_attention(), expected);
    }
}

// =============================================================================
// Tracker utilization arithmetic
// =============================================================================

proptest! {
    #[test]
    fn utilization_in_bounds(
        max_tokens in 1..500_000u64,
        current_tokens in 0..500_000u64,
    ) {
        let config = ContextBudgetConfig {
            max_tokens,
            max_compaction_history: 10,
        };
        let mut tracker = ContextBudgetTracker::new(1, config);
        tracker.update_tokens(current_tokens);

        let util = tracker.utilization();
        let expected = current_tokens as f64 / max_tokens as f64;
        prop_assert!((util - expected).abs() < 1e-10,
            "utilization {} != expected {}", util, expected);
    }

    #[test]
    fn peak_tokens_monotonic(
        tokens_a in 0..100_000u64,
        tokens_b in 0..100_000u64,
    ) {
        let config = ContextBudgetConfig {
            max_tokens: 200_000,
            max_compaction_history: 10,
        };
        let mut tracker = ContextBudgetTracker::new(1, config);

        tracker.update_tokens(tokens_a);
        let snap1 = tracker.snapshot();
        prop_assert_eq!(snap1.peak_tokens, tokens_a);

        tracker.update_tokens(tokens_b);
        let snap2 = tracker.snapshot();
        prop_assert_eq!(snap2.peak_tokens, tokens_a.max(tokens_b));
    }

    #[test]
    fn zero_max_tokens_safe(_dummy in 0..1u32) {
        let config = ContextBudgetConfig {
            max_tokens: 0,
            max_compaction_history: 10,
        };
        let tracker = ContextBudgetTracker::new(1, config);
        prop_assert_eq!(tracker.utilization(), 0.0);
    }
}

// =============================================================================
// Compaction history eviction
// =============================================================================

proptest! {
    #[test]
    fn compaction_history_bounded(
        n_compactions in 0..20usize,
        max_history in 1..10usize,
    ) {
        let config = ContextBudgetConfig {
            max_tokens: 200_000,
            max_compaction_history: max_history,
        };
        let mut tracker = ContextBudgetTracker::new(1, config);

        for i in 0..n_compactions {
            tracker.record_compaction(
                100_000 + i as u64 * 1000,
                50_000,
                CompactionTrigger::Automatic,
            );
        }

        let snap = tracker.snapshot();
        prop_assert_eq!(snap.total_compactions, n_compactions as u64);
        // Recent compactions in snapshot are capped at 5
        prop_assert!(snap.recent_compactions.len() <= 5);
    }

    #[test]
    fn compaction_updates_tokens(
        before in 50_000..200_000u64,
        after in 0..50_000u64,
    ) {
        let config = ContextBudgetConfig {
            max_tokens: 200_000,
            max_compaction_history: 10,
        };
        let mut tracker = ContextBudgetTracker::new(1, config);
        tracker.update_tokens(before);
        tracker.record_compaction(before, after, CompactionTrigger::OperatorInitiated);

        let snap = tracker.snapshot();
        prop_assert_eq!(snap.estimated_tokens, after);
    }
}

// =============================================================================
// Recovery guidance tier mapping
// =============================================================================

proptest! {
    #[test]
    fn guidance_action_matches_tier(
        max_tokens in 100_000..300_000u64,
        util_pct in 0..100u32,
    ) {
        let config = ContextBudgetConfig {
            max_tokens,
            max_compaction_history: 10,
        };
        let mut tracker = ContextBudgetTracker::new(1, config);
        let tokens = (max_tokens as f64 * util_pct as f64 / 100.0) as u64;
        tracker.update_tokens(tokens);

        let guidance = tracker.recovery_guidance();
        let tier = tracker.pressure_tier();

        match tier {
            ContextPressureTier::Black => {
                prop_assert_eq!(guidance.action, RecoveryAction::RotateSession);
                prop_assert!(guidance.estimated_freed_tokens.is_some());
            }
            ContextPressureTier::Red => {
                prop_assert_eq!(guidance.action, RecoveryAction::SendCompact);
                prop_assert!(guidance.estimated_freed_tokens.is_some());
            }
            ContextPressureTier::Yellow | ContextPressureTier::Green => {
                prop_assert_eq!(guidance.action, RecoveryAction::Monitor);
            }
        }
    }
}

// =============================================================================
// Registry aggregation
// =============================================================================

proptest! {
    #[test]
    fn registry_tracked_count(
        n_panes in 0..10usize,
    ) {
        let config = ContextBudgetConfig::default();
        let mut registry = ContextBudgetRegistry::new(config);

        for i in 0..n_panes {
            registry.tracker_mut(i as u64);
        }

        prop_assert_eq!(registry.tracked_count(), n_panes);
    }

    #[test]
    fn registry_remove_decrements_count(
        n_panes in 1..5usize,
        remove_idx in 0..5usize,
    ) {
        let config = ContextBudgetConfig::default();
        let mut registry = ContextBudgetRegistry::new(config);

        for i in 0..n_panes {
            registry.tracker_mut(i as u64);
        }

        let target = remove_idx.min(n_panes - 1) as u64;
        registry.remove(target);
        prop_assert_eq!(registry.tracked_count(), n_panes - 1);
    }

    #[test]
    fn fleet_snapshot_pane_count_matches(
        n_panes in 0..5usize,
    ) {
        let config = ContextBudgetConfig::default();
        let mut registry = ContextBudgetRegistry::new(config);

        for i in 0..n_panes {
            registry.tracker_mut(i as u64);
        }

        let snap = registry.fleet_snapshot();
        prop_assert_eq!(snap.tracked_panes, n_panes);
        prop_assert_eq!(snap.panes.len(), n_panes);
    }

    #[test]
    fn fleet_snapshot_worst_tier_correct(
        n_green in 0..3usize,
        has_red in any::<bool>(),
    ) {
        let config = ContextBudgetConfig {
            max_tokens: 200_000,
            max_compaction_history: 10,
        };
        let mut registry = ContextBudgetRegistry::new(config);

        // Add green panes
        for i in 0..n_green {
            let tracker = registry.tracker_mut(i as u64);
            tracker.update_tokens(50_000); // 25% utilization
        }

        if has_red {
            let tracker = registry.tracker_mut(100);
            tracker.update_tokens(170_000); // 85% utilization → Red
        }

        let snap = registry.fleet_snapshot();
        if n_green == 0 && !has_red {
            // No panes at all
            prop_assert_eq!(snap.worst_pressure_tier, ContextPressureTier::Green);
        } else if has_red {
            prop_assert!(snap.worst_pressure_tier >= ContextPressureTier::Red);
        }
    }
}

// =============================================================================
// Snapshot serialization
// =============================================================================

proptest! {
    #[test]
    fn snapshot_serde_roundtrip(
        n_panes in 0..3usize,
    ) {
        let config = ContextBudgetConfig::default();
        let mut registry = ContextBudgetRegistry::new(config);

        for i in 0..n_panes {
            let tracker = registry.tracker_mut(i as u64);
            tracker.update_tokens(50_000);
        }

        let snap = registry.fleet_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: ContextBudgetSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.tracked_panes, back.tracked_panes);
        prop_assert_eq!(snap.worst_pressure_tier, back.worst_pressure_tier);
        prop_assert_eq!(snap.panes_needing_attention, back.panes_needing_attention);
    }
}

// =============================================================================
// Standard factory invariants
// =============================================================================

#[test]
fn default_config_reasonable() {
    let config = ContextBudgetConfig::default();
    assert!(config.max_tokens > 0);
    assert!(config.max_compaction_history > 0);
}

#[test]
fn default_pressure_is_green() {
    assert_eq!(ContextPressureTier::default(), ContextPressureTier::Green);
}

#[test]
fn new_tracker_is_green() {
    let tracker = ContextBudgetTracker::new(1, ContextBudgetConfig::default());
    assert_eq!(tracker.pressure_tier(), ContextPressureTier::Green);
    assert_eq!(tracker.utilization(), 0.0);
}
