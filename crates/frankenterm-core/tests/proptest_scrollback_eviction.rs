//! Property-based tests for scrollback eviction.
//!
//! Bead: wa-3r5e
//!
//! Verifies the following properties:
//! 1. Tier ordering invariant: dormant gets fewer segments than idle, etc.
//! 2. No over-eviction: segments_to_remove <= current_segments - min_segments
//! 3. Pressure monotonicity: higher pressure → equal or more aggressive trimming
//! 4. Plan idempotency: planning twice yields identical results
//! 5. Min segments floor: every limit is >= min_segments
//! 6. Config round-trip: serialization preserves all fields
//! 7. Plan total consistency: sum of per-pane removals equals total_segments_to_remove
//! 8. Unknown panes default to dormant tier

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::pane_tiers::PaneTier;
use frankenterm_core::scrollback_eviction::{
    EvictionConfig, PaneTierSource, ScrollbackEvictor, SegmentStore,
};

// ── Test helpers ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct PropStore {
    segments: HashMap<u64, usize>,
}

impl SegmentStore for PropStore {
    fn count_segments(&self, pane_id: u64) -> Result<usize, String> {
        Ok(*self.segments.get(&pane_id).unwrap_or(&0))
    }

    fn delete_oldest_segments(&self, _pane_id: u64, count: usize) -> Result<usize, String> {
        Ok(count)
    }

    fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
        let mut ids: Vec<_> = self.segments.keys().copied().collect();
        ids.sort();
        Ok(ids)
    }
}

struct PropTierSource {
    tiers: HashMap<u64, PaneTier>,
}

impl PaneTierSource for PropTierSource {
    fn tier_for(&self, pane_id: u64) -> Option<PaneTier> {
        self.tiers.get(&pane_id).copied()
    }
}

/// Strategy that generates a valid EvictionConfig with
/// descending limits: active >= thinking >= idle >= background >= dormant >= min_segments.
fn arb_eviction_config() -> impl Strategy<Value = EvictionConfig> {
    // min_segments: 1..50, then build ascending limits
    (1usize..50, 1usize..200, 1usize..200, 1usize..200, 1usize..200, 1usize..200, 1usize..500)
        .prop_map(|(min_seg, d0, d1, d2, d3, d4, pressure_max)| {
            // Build limits by accumulating deltas onto min_segments
            let dormant = min_seg + d0;
            let background = dormant + d1;
            let idle = background + d2;
            let thinking = idle + d3;
            let active = thinking + d4;
            let pressure = min_seg.max(pressure_max.min(active));

            EvictionConfig {
                active_max_segments: active,
                thinking_max_segments: thinking,
                idle_max_segments: idle,
                background_max_segments: background,
                dormant_max_segments: dormant,
                pressure_max_segments: pressure,
                min_segments: min_seg,
            }
        })
}

fn arb_pressure() -> impl Strategy<Value = MemoryPressureTier> {
    prop_oneof![
        Just(MemoryPressureTier::Green),
        Just(MemoryPressureTier::Yellow),
        Just(MemoryPressureTier::Orange),
        Just(MemoryPressureTier::Red),
    ]
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

const ALL_PRESSURES: [MemoryPressureTier; 4] = [
    MemoryPressureTier::Green,
    MemoryPressureTier::Yellow,
    MemoryPressureTier::Orange,
    MemoryPressureTier::Red,
];

const ALL_TIERS: [PaneTier; 5] = [
    PaneTier::Active,
    PaneTier::Thinking,
    PaneTier::Idle,
    PaneTier::Background,
    PaneTier::Dormant,
];

// =============================================================================
// 1. Tier ordering invariant
// =============================================================================

proptest! {
    /// For any valid config and pressure level, segment limits must be
    /// non-increasing as tiers go from Active → Dormant.
    #[test]
    fn proptest_tier_ordering_invariant(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
    ) {
        let active = config.max_segments_for(PaneTier::Active, pressure);
        let thinking = config.max_segments_for(PaneTier::Thinking, pressure);
        let idle = config.max_segments_for(PaneTier::Idle, pressure);
        let background = config.max_segments_for(PaneTier::Background, pressure);
        let dormant = config.max_segments_for(PaneTier::Dormant, pressure);

        prop_assert!(active >= thinking,
            "active({}) >= thinking({}) at {:?}", active, thinking, pressure);
        prop_assert!(thinking >= idle,
            "thinking({}) >= idle({}) at {:?}", thinking, idle, pressure);
        prop_assert!(idle >= background,
            "idle({}) >= background({}) at {:?}", idle, background, pressure);
        prop_assert!(background >= dormant,
            "background({}) >= dormant({}) at {:?}", background, dormant, pressure);
    }
}

// =============================================================================
// 2. No over-eviction
// =============================================================================

proptest! {
    /// segments_to_remove for any target never exceeds (current_segments - max_segments),
    /// and max_segments is always >= min_segments.
    #[test]
    fn proptest_no_over_eviction(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
        pane_segments in prop::collection::vec(0usize..20_000, 1..30),
        pane_tiers in prop::collection::vec(arb_tier(), 1..30),
    ) {
        let n = pane_segments.len().min(pane_tiers.len());
        let store = PropStore {
            segments: (0..n).map(|i| (i as u64, pane_segments[i])).collect(),
        };
        let tier_source = PropTierSource {
            tiers: (0..n).map(|i| (i as u64, pane_tiers[i])).collect(),
        };
        let evictor = ScrollbackEvictor::new(config.clone(), store, tier_source);

        let plan = evictor.plan(pressure).unwrap();
        for target in &plan.targets {
            // Can never remove more than what exists
            prop_assert!(
                target.segments_to_remove <= target.current_segments,
                "pane {}: removing {} > current {}",
                target.pane_id, target.segments_to_remove, target.current_segments
            );
            // After eviction, remaining segments == max_segments
            prop_assert!(
                target.current_segments - target.segments_to_remove == target.max_segments,
                "pane {}: remaining {} != max {}",
                target.pane_id,
                target.current_segments - target.segments_to_remove,
                target.max_segments
            );
            // max_segments always respects the floor
            prop_assert!(
                target.max_segments >= config.min_segments,
                "pane {}: max {} < min_segments {}",
                target.pane_id, target.max_segments, config.min_segments
            );
        }
    }
}

// =============================================================================
// 3. Pressure monotonicity
// =============================================================================

proptest! {
    /// Higher memory pressure levels must produce equal or more aggressive
    /// eviction (higher total_segments_to_remove) for the same pane layout.
    #[test]
    fn proptest_pressure_monotonicity(
        config in arb_eviction_config(),
        pane_segments in prop::collection::vec(0usize..20_000, 1..30),
        pane_tiers in prop::collection::vec(arb_tier(), 1..30),
    ) {
        let n = pane_segments.len().min(pane_tiers.len());
        let segments: HashMap<u64, usize> = (0..n).map(|i| (i as u64, pane_segments[i])).collect();
        let tiers: HashMap<u64, PaneTier> = (0..n).map(|i| (i as u64, pane_tiers[i])).collect();

        let mut prev_total = 0usize;
        for pressure in ALL_PRESSURES {
            let store = PropStore { segments: segments.clone() };
            let tier_source = PropTierSource { tiers: tiers.clone() };
            let evictor = ScrollbackEvictor::new(config.clone(), store, tier_source);
            let plan = evictor.plan(pressure).unwrap();

            prop_assert!(
                plan.total_segments_to_remove >= prev_total,
                "{:?}: total {} < previous {}",
                pressure, plan.total_segments_to_remove, prev_total
            );
            prev_total = plan.total_segments_to_remove;
        }
    }
}

// =============================================================================
// 4. Plan idempotency
// =============================================================================

proptest! {
    /// Computing the plan twice on the same state yields identical results.
    #[test]
    fn proptest_plan_idempotency(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
        pane_segments in prop::collection::vec(0usize..20_000, 1..20),
    ) {
        let n = pane_segments.len();
        let store = PropStore {
            segments: (0..n).map(|i| (i as u64, pane_segments[i])).collect(),
        };
        // All dormant for simplicity
        let tier_source = PropTierSource {
            tiers: (0..n).map(|i| (i as u64, PaneTier::Dormant)).collect(),
        };
        let evictor = ScrollbackEvictor::new(config, store, tier_source);

        let plan1 = evictor.plan(pressure).unwrap();
        let plan2 = evictor.plan(pressure).unwrap();

        prop_assert_eq!(plan1.total_segments_to_remove, plan2.total_segments_to_remove);
        prop_assert_eq!(plan1.panes_affected, plan2.panes_affected);
        prop_assert_eq!(plan1.targets.len(), plan2.targets.len());
    }
}

// =============================================================================
// 5. Min segments floor
// =============================================================================

proptest! {
    /// For any tier × pressure combination, the computed limit is >= min_segments.
    #[test]
    fn proptest_min_segments_floor(
        config in arb_eviction_config(),
    ) {
        for tier in ALL_TIERS {
            for pressure in ALL_PRESSURES {
                let max = config.max_segments_for(tier, pressure);
                prop_assert!(
                    max >= config.min_segments,
                    "{:?} at {:?}: max {} < min_segments {}",
                    tier, pressure, max, config.min_segments
                );
            }
        }
    }
}

// =============================================================================
// 6. Config serde round-trip
// =============================================================================

proptest! {
    /// Serializing and deserializing an EvictionConfig preserves all fields.
    #[test]
    fn proptest_config_roundtrip(
        config in arb_eviction_config(),
    ) {
        let json = serde_json::to_string(&config).unwrap();
        let parsed: EvictionConfig = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(config.active_max_segments, parsed.active_max_segments);
        prop_assert_eq!(config.thinking_max_segments, parsed.thinking_max_segments);
        prop_assert_eq!(config.idle_max_segments, parsed.idle_max_segments);
        prop_assert_eq!(config.background_max_segments, parsed.background_max_segments);
        prop_assert_eq!(config.dormant_max_segments, parsed.dormant_max_segments);
        prop_assert_eq!(config.pressure_max_segments, parsed.pressure_max_segments);
        prop_assert_eq!(config.min_segments, parsed.min_segments);
    }
}

// =============================================================================
// 7. Plan total consistency
// =============================================================================

proptest! {
    /// The sum of per-target segments_to_remove must equal total_segments_to_remove.
    #[test]
    fn proptest_plan_total_consistency(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
        pane_segments in prop::collection::vec(0usize..20_000, 1..30),
        pane_tiers in prop::collection::vec(arb_tier(), 1..30),
    ) {
        let n = pane_segments.len().min(pane_tiers.len());
        let store = PropStore {
            segments: (0..n).map(|i| (i as u64, pane_segments[i])).collect(),
        };
        let tier_source = PropTierSource {
            tiers: (0..n).map(|i| (i as u64, pane_tiers[i])).collect(),
        };
        let evictor = ScrollbackEvictor::new(config, store, tier_source);

        let plan = evictor.plan(pressure).unwrap();
        let computed_total: usize = plan.targets.iter().map(|t| t.segments_to_remove).sum();

        prop_assert_eq!(
            plan.total_segments_to_remove, computed_total,
            "plan total {} != sum of targets {}",
            plan.total_segments_to_remove, computed_total
        );
    }
}

// =============================================================================
// 8. Unknown panes default to dormant
// =============================================================================

proptest! {
    /// Panes not in the tier source are treated as Dormant, which has the
    /// most aggressive eviction limits.
    #[test]
    fn proptest_unknown_panes_dormant(
        config in arb_eviction_config(),
        pressure in arb_pressure(),
        segments in 1usize..20_000,
    ) {
        let store = PropStore {
            segments: [(42u64, segments)].into_iter().collect(),
        };
        // Empty tier source — pane 42 is unknown
        let tier_source = PropTierSource {
            tiers: HashMap::new(),
        };
        let evictor = ScrollbackEvictor::new(config.clone(), store, tier_source);

        let plan = evictor.plan(pressure).unwrap();

        // Compute expected limit for Dormant tier
        let dormant_limit = config.max_segments_for(PaneTier::Dormant, pressure);

        if segments > dormant_limit {
            prop_assert_eq!(plan.panes_affected, 1);
            let target = &plan.targets[0];
            prop_assert_eq!(target.max_segments, dormant_limit);
            prop_assert_eq!(target.segments_to_remove, segments - dormant_limit);
        } else {
            prop_assert!(plan.is_empty(),
                "should not evict {} segments when limit is {}",
                segments, dormant_limit);
        }
    }
}
