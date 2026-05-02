use proptest::prelude::*;

use frankenterm_core::atlas_tiered_swap::{
    AtlasTier, BudgetPressure, EvictionAction, TierSwapStats, TieredAtlasRegion, compute_pressure,
    decide_cascade_action, select_eviction_target, should_evict_from,
};

fn arb_tier() -> impl Strategy<Value = AtlasTier> {
    prop_oneof![
        Just(AtlasTier::Vram),
        Just(AtlasTier::HostRam),
        Just(AtlasTier::Disk),
    ]
}

fn arb_pressure() -> impl Strategy<Value = BudgetPressure> {
    prop_oneof![
        Just(BudgetPressure::Nominal),
        Just(BudgetPressure::Warning),
        Just(BudgetPressure::Critical),
    ]
}

fn arb_region() -> impl Strategy<Value = TieredAtlasRegion> {
    (any::<u64>(), arb_tier(), 0u64..=10_000, 0u64..=1_000_000).prop_map(
        |(id, tier, last_access_frame, bytes)| {
            TieredAtlasRegion::new(id, tier, last_access_frame, bytes)
        },
    )
}

fn expected_pressure(
    current_bytes: u64,
    budget_bytes: u64,
    warning_pct: u8,
    critical_pct: u8,
) -> BudgetPressure {
    if budget_bytes == 0 {
        return BudgetPressure::Critical;
    }
    let pct = current_bytes.saturating_mul(100) / budget_bytes;
    if pct >= u64::from(critical_pct) {
        BudgetPressure::Critical
    } else if pct >= u64::from(warning_pct) {
        BudgetPressure::Warning
    } else {
        BudgetPressure::Nominal
    }
}

fn expected_target(
    regions: &[TieredAtlasRegion],
    tier: AtlasTier,
    current_frame: u64,
) -> Option<TieredAtlasRegion> {
    regions
        .iter()
        .filter(|region| region.tier == tier)
        .copied()
        .max_by(|a, b| {
            a.idle_frames(current_frame)
                .cmp(&b.idle_frames(current_frame))
                .then_with(|| b.id.cmp(&a.id))
        })
}

fn expected_cascade_action(
    regions: &[TieredAtlasRegion],
    current_frame: u64,
    vram_pressure: BudgetPressure,
    host_ram_pressure: BudgetPressure,
    disk_pressure: BudgetPressure,
) -> EvictionAction {
    if vram_pressure == BudgetPressure::Critical {
        if let Some(target) = expected_target(regions, AtlasTier::Vram, current_frame) {
            return EvictionAction::Demote {
                region_id: target.id,
                from: AtlasTier::Vram,
                to: AtlasTier::HostRam,
            };
        }
    }
    if host_ram_pressure == BudgetPressure::Critical {
        if let Some(target) = expected_target(regions, AtlasTier::HostRam, current_frame) {
            return EvictionAction::Demote {
                region_id: target.id,
                from: AtlasTier::HostRam,
                to: AtlasTier::Disk,
            };
        }
    }
    if disk_pressure == BudgetPressure::Critical {
        if let Some(target) = expected_target(regions, AtlasTier::Disk, current_frame) {
            return EvictionAction::Evict {
                region_id: target.id,
                from: AtlasTier::Disk,
            };
        }
    }
    if vram_pressure == BudgetPressure::Warning {
        if let Some(target) = expected_target(regions, AtlasTier::Vram, current_frame) {
            if target.idle_frames(current_frame) >= 60 {
                return EvictionAction::Demote {
                    region_id: target.id,
                    from: AtlasTier::Vram,
                    to: AtlasTier::HostRam,
                };
            }
        }
    }
    EvictionAction::NoOp
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn proptest_atlas_tiered_swap_tier_order_and_targets_are_consistent(tier in arb_tier()) {
        prop_assert_eq!(tier.is_hotter_than(tier), false);
        prop_assert_eq!(tier.promotion_target().is_none(), tier == AtlasTier::Vram);
        prop_assert_eq!(tier.demotion_target().is_none(), tier == AtlasTier::Disk);

        if let Some(promoted) = tier.promotion_target() {
            prop_assert!(promoted.is_hotter_than(tier));
            prop_assert_eq!(promoted.ordinal() + 1, tier.ordinal());
        }
        if let Some(demoted) = tier.demotion_target() {
            prop_assert!(tier.is_hotter_than(demoted));
            prop_assert_eq!(tier.ordinal() + 1, demoted.ordinal());
        }
    }

    #[test]
    fn proptest_atlas_tiered_swap_pressure_and_evict_predicate_match_policy(
        current_bytes in any::<u64>(),
        budget_bytes in any::<u64>(),
        warning_pct in any::<u8>(),
        critical_pct in any::<u8>(),
        has_cold_regions in any::<bool>(),
    ) {
        let pressure = compute_pressure(current_bytes, budget_bytes, warning_pct, critical_pct);
        prop_assert_eq!(
            pressure,
            expected_pressure(current_bytes, budget_bytes, warning_pct, critical_pct),
        );
        prop_assert_eq!(
            should_evict_from(pressure, has_cold_regions),
            pressure == BudgetPressure::Critical
                || (pressure == BudgetPressure::Warning && has_cold_regions),
        );
    }

    #[test]
    fn proptest_atlas_tiered_swap_region_touch_and_idle_frames_saturate(
        mut region in arb_region(),
        touch_frame in any::<u64>(),
        current_frame in any::<u64>(),
    ) {
        region.touch(touch_frame);

        prop_assert_eq!(region.last_access_frame, touch_frame);
        prop_assert_eq!(region.idle_frames(current_frame), current_frame.saturating_sub(touch_frame));
    }

    #[test]
    fn proptest_atlas_tiered_swap_select_eviction_target_matches_lru_tiebreak(
        regions in prop::collection::vec(arb_region(), 0..64),
        tier in arb_tier(),
        current_frame in 0u64..=20_000,
    ) {
        prop_assert_eq!(
            select_eviction_target(&regions, tier, current_frame),
            expected_target(&regions, tier, current_frame),
        );
    }

    #[test]
    fn proptest_atlas_tiered_swap_cascade_action_matches_priority_tree(
        regions in prop::collection::vec(arb_region(), 0..64),
        current_frame in 0u64..=20_000,
        vram_pressure in arb_pressure(),
        host_ram_pressure in arb_pressure(),
        disk_pressure in arb_pressure(),
    ) {
        prop_assert_eq!(
            decide_cascade_action(
                &regions,
                current_frame,
                vram_pressure,
                host_ram_pressure,
                disk_pressure,
            ),
            expected_cascade_action(
                &regions,
                current_frame,
                vram_pressure,
                host_ram_pressure,
                disk_pressure,
            ),
        );
    }

    #[test]
    fn proptest_atlas_tiered_swap_stats_record_actions_and_peaks(
        action in prop_oneof![
            Just(EvictionAction::Promote {
                region_id: 1,
                from: AtlasTier::HostRam,
                to: AtlasTier::Vram,
            }),
            Just(EvictionAction::Promote {
                region_id: 2,
                from: AtlasTier::Disk,
                to: AtlasTier::HostRam,
            }),
            Just(EvictionAction::Demote {
                region_id: 3,
                from: AtlasTier::Vram,
                to: AtlasTier::HostRam,
            }),
            Just(EvictionAction::Demote {
                region_id: 4,
                from: AtlasTier::HostRam,
                to: AtlasTier::Disk,
            }),
            Just(EvictionAction::Evict {
                region_id: 5,
                from: AtlasTier::Disk,
            }),
            Just(EvictionAction::NoOp),
        ],
        region_bytes in any::<u64>(),
        first_vram_peak in any::<u64>(),
        first_host_peak in any::<u64>(),
        second_vram_peak in any::<u64>(),
        second_host_peak in any::<u64>(),
    ) {
        let mut stats = TierSwapStats::default();
        stats.record_action(action, region_bytes);

        prop_assert_eq!(stats.vram_swap_in_count, if matches!(
            action,
            EvictionAction::Promote { to: AtlasTier::Vram, .. }
        ) { 1 } else { 0 });
        prop_assert_eq!(stats.host_ram_swap_in_count, if matches!(
            action,
            EvictionAction::Promote { to: AtlasTier::HostRam, .. }
        ) { 1 } else { 0 });
        prop_assert_eq!(stats.vram_swap_out_count, if matches!(
            action,
            EvictionAction::Demote { from: AtlasTier::Vram, .. }
        ) { 1 } else { 0 });
        prop_assert_eq!(stats.host_ram_swap_out_count, if matches!(
            action,
            EvictionAction::Demote { from: AtlasTier::HostRam, .. }
        ) { 1 } else { 0 });
        prop_assert_eq!(stats.disk_eviction_count, if matches!(action, EvictionAction::Evict { .. }) {
            1
        } else {
            0
        });
        prop_assert_eq!(stats.swap_total_bytes, if matches!(action, EvictionAction::Promote { .. } | EvictionAction::Demote { .. }) {
            region_bytes
        } else {
            0
        });

        stats.record_peak(first_vram_peak, first_host_peak);
        stats.record_peak(second_vram_peak, second_host_peak);
        prop_assert_eq!(stats.vram_peak_bytes, first_vram_peak.max(second_vram_peak));
        prop_assert_eq!(stats.host_ram_peak_bytes, first_host_peak.max(second_host_peak));
    }
}
