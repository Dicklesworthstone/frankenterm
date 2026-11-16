use proptest::prelude::*;

use frankenterm_core::adaptive_fps::{
    AdaptiveDecision, AdaptiveDecisionReason, AdaptiveMode, BatteryLevel, BatteryThresholds,
    PowerSnapshot, PowerSource, Quality, ThermalState, WAKE_OVERRIDE_FPS, WakeOverride,
    select_decision,
};

fn arb_power_source() -> impl Strategy<Value = PowerSource> {
    prop_oneof![
        Just(PowerSource::Ac),
        Just(PowerSource::Battery),
        Just(PowerSource::Unknown),
    ]
}

fn arb_thermal_state() -> impl Strategy<Value = ThermalState> {
    prop_oneof![
        Just(ThermalState::Nominal),
        Just(ThermalState::Fair),
        Just(ThermalState::Serious),
        Just(ThermalState::Critical),
    ]
}

fn arb_mode() -> impl Strategy<Value = AdaptiveMode> {
    prop_oneof![
        Just(AdaptiveMode::Auto),
        Just(AdaptiveMode::Performance),
        Just(AdaptiveMode::Balanced),
        Just(AdaptiveMode::BatterySaver),
    ]
}

fn arb_battery() -> impl Strategy<Value = BatteryLevel> {
    prop_oneof![
        Just(BatteryLevel::NONE),
        any::<u8>().prop_map(BatteryLevel::from_percent)
    ]
}

fn arb_overrides() -> impl Strategy<Value = WakeOverride> {
    (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
        |(active_typing, bell_received, live_resize, a11y_query_in_flight)| WakeOverride {
            active_typing,
            bell_received,
            live_resize,
            a11y_query_in_flight,
        },
    )
}

fn arb_thresholds() -> impl Strategy<Value = BatteryThresholds> {
    (0u8..=100, 0u8..=100).prop_map(|(mid_low_pct, low_pct)| BatteryThresholds {
        mid_low_pct,
        low_pct,
    })
}

fn arb_snapshot() -> impl Strategy<Value = PowerSnapshot> {
    (
        arb_power_source(),
        arb_thermal_state(),
        arb_battery(),
        0u32..=360,
        arb_overrides(),
        arb_thresholds(),
    )
        .prop_map(
            |(power_source, thermal, battery, display_max_fps, overrides, thresholds)| {
                PowerSnapshot {
                    power_source,
                    thermal,
                    battery,
                    display_max_fps,
                    overrides,
                    thresholds,
                }
            },
        )
}

fn expected_decision(snapshot: PowerSnapshot, mode: AdaptiveMode) -> AdaptiveDecision {
    if snapshot.overrides.live_resize {
        return AdaptiveDecision {
            target_fps: snapshot.display_max_fps.max(WAKE_OVERRIDE_FPS),
            quality: Quality::Standard,
            reason: AdaptiveDecisionReason::OverrideLiveResize,
        };
    }
    if snapshot.overrides.a11y_query_in_flight {
        return AdaptiveDecision {
            target_fps: WAKE_OVERRIDE_FPS,
            quality: Quality::Standard,
            reason: AdaptiveDecisionReason::OverrideA11y,
        };
    }
    if snapshot.overrides.active_typing {
        return AdaptiveDecision {
            target_fps: WAKE_OVERRIDE_FPS,
            quality: Quality::Standard,
            reason: AdaptiveDecisionReason::OverrideTyping,
        };
    }
    if snapshot.overrides.bell_received {
        return AdaptiveDecision {
            target_fps: WAKE_OVERRIDE_FPS,
            quality: Quality::Standard,
            reason: AdaptiveDecisionReason::OverrideBell,
        };
    }

    match mode {
        AdaptiveMode::Performance => {
            return AdaptiveDecision {
                target_fps: snapshot.display_max_fps,
                quality: Quality::Fancy,
                reason: AdaptiveDecisionReason::ModePerformance,
            };
        }
        AdaptiveMode::BatterySaver => {
            return AdaptiveDecision {
                target_fps: 15,
                quality: Quality::Draft,
                reason: AdaptiveDecisionReason::ModeBatterySaver,
            };
        }
        AdaptiveMode::Auto | AdaptiveMode::Balanced => {}
    }

    if snapshot.thermal == ThermalState::Critical {
        return AdaptiveDecision {
            target_fps: 5,
            quality: Quality::Draft,
            reason: AdaptiveDecisionReason::ThermalCritical,
        };
    }

    match snapshot.power_source {
        PowerSource::Ac => match snapshot.thermal {
            ThermalState::Nominal => AdaptiveDecision {
                target_fps: snapshot.display_max_fps,
                quality: Quality::Fancy,
                reason: AdaptiveDecisionReason::AcCool,
            },
            ThermalState::Fair | ThermalState::Serious => AdaptiveDecision {
                target_fps: 60,
                quality: Quality::Standard,
                reason: AdaptiveDecisionReason::AcWarm,
            },
            ThermalState::Critical => unreachable!("critical handled above"),
        },
        PowerSource::Battery | PowerSource::Unknown => match snapshot.battery.percent() {
            Some(p) if p < snapshot.thresholds.low_pct => AdaptiveDecision {
                target_fps: 15,
                quality: Quality::Draft,
                reason: AdaptiveDecisionReason::BatteryLow,
            },
            Some(p) if p < snapshot.thresholds.mid_low_pct => AdaptiveDecision {
                target_fps: 30,
                quality: Quality::Standard,
                reason: AdaptiveDecisionReason::BatteryMid,
            },
            Some(_) => AdaptiveDecision {
                target_fps: 60,
                quality: Quality::Standard,
                reason: AdaptiveDecisionReason::BatteryHigh,
            },
            None => AdaptiveDecision {
                target_fps: 30,
                quality: Quality::Standard,
                reason: AdaptiveDecisionReason::BatteryMid,
            },
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_adaptive_fps_battery_level_clamps_and_compares(
        raw_pct in any::<u8>(),
        threshold in any::<u8>(),
    ) {
        let level = BatteryLevel::from_percent(raw_pct);
        let clamped = raw_pct.min(100);

        prop_assert_eq!(level.percent(), Some(clamped));
        prop_assert!(level.is_present());
        prop_assert_eq!(level.is_below(threshold), clamped < threshold);
        prop_assert!(!BatteryLevel::NONE.is_present());
        prop_assert!(!BatteryLevel::NONE.is_below(threshold));
    }

    #[test]
    fn proptest_adaptive_fps_wake_override_any_active_matches_fields(
        overrides in arb_overrides(),
    ) {
        prop_assert_eq!(
            overrides.any_active(),
            overrides.active_typing
                || overrides.bell_received
                || overrides.live_resize
                || overrides.a11y_query_in_flight,
        );
    }

    #[test]
    fn proptest_adaptive_fps_select_decision_matches_policy_table(
        snapshot in arb_snapshot(),
        mode in arb_mode(),
    ) {
        prop_assert_eq!(select_decision(snapshot, mode), expected_decision(snapshot, mode));
    }

    #[test]
    fn proptest_adaptive_fps_balanced_matches_auto_for_all_snapshots(
        snapshot in arb_snapshot(),
    ) {
        prop_assert_eq!(
            select_decision(snapshot, AdaptiveMode::Balanced),
            select_decision(snapshot, AdaptiveMode::Auto),
        );
    }

    #[test]
    fn proptest_adaptive_fps_ac_baseline_uses_display_max(display_max_fps in 0u32..=360) {
        let snapshot = PowerSnapshot::ac_baseline(display_max_fps);
        let decision = select_decision(snapshot, AdaptiveMode::Auto);

        prop_assert_eq!(snapshot.power_source, PowerSource::Ac);
        prop_assert_eq!(snapshot.thermal, ThermalState::Nominal);
        prop_assert_eq!(snapshot.battery, BatteryLevel::NONE);
        prop_assert_eq!(decision.target_fps, display_max_fps);
        prop_assert_eq!(decision.quality, Quality::Fancy);
        prop_assert_eq!(decision.reason, AdaptiveDecisionReason::AcCool);
    }
}
