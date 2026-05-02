//! Property tests for the GUI adaptive-FPS loop integration seam.

use frankenterm_core::adaptive_fps::{
    AdaptiveDecision, AdaptiveDecisionReason, AdaptiveMode, BatteryLevel, BatteryThresholds,
    PowerSnapshot, PowerSource, ThermalState, WakeOverride, select_decision,
};
use frankenterm_gui::adaptive_fps_loop::{
    AdaptiveFpsConfig, AdaptiveFpsConfigError, AdaptiveFpsLoop, AdaptiveWakeState, FrameRateSink,
    PowerProbeSnapshot,
};
use proptest::prelude::*;

#[derive(Debug, Default)]
struct RecordingSink {
    applied: Vec<u32>,
}

impl FrameRateSink for RecordingSink {
    fn apply_target_fps(&mut self, target_fps: u32) {
        self.applied.push(target_fps);
    }
}

fn arb_mode() -> impl Strategy<Value = AdaptiveMode> {
    prop_oneof![
        Just(AdaptiveMode::Auto),
        Just(AdaptiveMode::Performance),
        Just(AdaptiveMode::Balanced),
        Just(AdaptiveMode::BatterySaver),
    ]
}

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

fn arb_battery_level() -> impl Strategy<Value = BatteryLevel> {
    prop_oneof![
        Just(BatteryLevel::NONE),
        any::<u8>().prop_map(BatteryLevel::from_percent),
    ]
}

fn arb_probe() -> impl Strategy<Value = PowerProbeSnapshot> {
    (arb_power_source(), arb_thermal_state(), arb_battery_level()).prop_map(
        |(power_source, thermal, battery)| PowerProbeSnapshot {
            power_source,
            thermal,
            battery,
        },
    )
}

fn arb_wake_state() -> impl Strategy<Value = AdaptiveWakeState> {
    (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
        |(active_typing, bell_received, live_resize, a11y_query_in_flight)| AdaptiveWakeState {
            active_typing,
            bell_received,
            live_resize,
            a11y_query_in_flight,
        },
    )
}

fn wake_override(wake: AdaptiveWakeState) -> WakeOverride {
    WakeOverride {
        active_typing: wake.active_typing,
        bell_received: wake.bell_received,
        live_resize: wake.live_resize,
        a11y_query_in_flight: wake.a11y_query_in_flight,
    }
}

fn expected_decision(
    config: AdaptiveFpsConfig,
    probe: PowerProbeSnapshot,
    display_max_fps: u32,
    wake: AdaptiveWakeState,
) -> AdaptiveDecision {
    if config.enabled {
        select_decision(
            PowerSnapshot {
                power_source: probe.power_source,
                thermal: probe.thermal,
                battery: probe.battery,
                display_max_fps,
                overrides: wake_override(wake),
                thresholds: config.thresholds,
            },
            config.mode,
        )
    } else {
        select_decision(
            PowerSnapshot::ac_baseline(display_max_fps),
            AdaptiveMode::Performance,
        )
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn config_new_validates_threshold_gradient(
        enabled in any::<bool>(),
        mode in arb_mode(),
        mid_low_pct in any::<u8>(),
        low_pct in any::<u8>(),
    ) {
        let result = AdaptiveFpsConfig::new(enabled, mode, mid_low_pct, low_pct);

        match (mid_low_pct, low_pct) {
            (_, 0 | 100..=u8::MAX) => {
                prop_assert_eq!(
                    result,
                    Err(AdaptiveFpsConfigError::LowThresholdOutOfRange { value: low_pct })
                );
            }
            (0 | 100..=u8::MAX, _) => {
                prop_assert_eq!(
                    result,
                    Err(AdaptiveFpsConfigError::MidLowThresholdOutOfRange {
                        value: mid_low_pct
                    })
                );
            }
            (mid, low) if low >= mid => {
                prop_assert_eq!(
                    result,
                    Err(AdaptiveFpsConfigError::LowThresholdNotBelowMid {
                        low_pct: low,
                        mid_low_pct: mid,
                    })
                );
            }
            (mid, low) => {
                let config = result.expect("valid threshold gradient");
                prop_assert_eq!(config.enabled, enabled);
                prop_assert_eq!(config.mode, mode);
                prop_assert_eq!(
                    config.thresholds,
                    BatteryThresholds {
                        mid_low_pct: mid,
                        low_pct: low,
                    }
                );
            }
        }
    }

    #[test]
    fn tick_matches_core_policy_and_records_doctor_snapshot(
        enabled in any::<bool>(),
        mode in arb_mode(),
        probe in arb_probe(),
        wake in arb_wake_state(),
        display_max_fps in 0u32..=360,
        mid_low_pct in 2u8..=99,
        low_pct in 1u8..=98,
    ) {
        prop_assume!(low_pct < mid_low_pct);
        let config = AdaptiveFpsConfig::new(enabled, mode, mid_low_pct, low_pct).unwrap();
        let mut loop_state = AdaptiveFpsLoop::new(config);
        let mut sink = RecordingSink::default();
        loop_state.update_probe(probe);

        let expected = expected_decision(config, probe, display_max_fps, wake);
        let tick = loop_state.tick(display_max_fps, wake, &mut sink);
        let snapshot = loop_state.doctor_snapshot();

        prop_assert_eq!(tick.decision, expected);
        prop_assert!(tick.applied);
        prop_assert_eq!(sink.applied, vec![expected.target_fps]);
        prop_assert_eq!(snapshot.enabled, enabled);
        prop_assert_eq!(snapshot.power_source, probe.power_source);
        prop_assert_eq!(snapshot.thermal, probe.thermal);
        prop_assert_eq!(snapshot.battery_percent, probe.battery.percent());
        prop_assert_eq!(snapshot.last_decision, Some(expected));
        prop_assert_eq!(snapshot.decision_counts.len(), 1);
        prop_assert_eq!(snapshot.decision_counts[0].reason, expected.reason);
        prop_assert_eq!(snapshot.decision_counts[0].count, 1);
    }

    #[test]
    fn repeated_identical_ticks_apply_once_but_count_every_tick(
        mode in arb_mode(),
        probe in arb_probe(),
        wake in arb_wake_state(),
        display_max_fps in 0u32..=360,
    ) {
        let config = AdaptiveFpsConfig {
            enabled: true,
            mode,
            thresholds: BatteryThresholds::default(),
        };
        let mut loop_state = AdaptiveFpsLoop::new(config);
        let mut sink = RecordingSink::default();
        loop_state.update_probe(probe);

        let expected = expected_decision(config, probe, display_max_fps, wake);
        let first = loop_state.tick(display_max_fps, wake, &mut sink);
        let second = loop_state.tick(display_max_fps, wake, &mut sink);
        let snapshot = loop_state.doctor_snapshot();

        prop_assert_eq!(first.decision, expected);
        prop_assert_eq!(second.decision, expected);
        prop_assert!(first.applied);
        prop_assert!(!second.applied);
        prop_assert_eq!(sink.applied, vec![expected.target_fps]);
        prop_assert_eq!(snapshot.last_decision, Some(expected));
        prop_assert_eq!(
            snapshot
                .decision_counts
                .iter()
                .find(|entry| entry.reason == expected.reason)
                .map(|entry| entry.count),
            Some(2)
        );
    }

    #[test]
    fn probe_transition_applies_only_when_target_fps_changes(
        first_probe in arb_probe(),
        second_probe in arb_probe(),
        wake in arb_wake_state(),
        display_max_fps in 0u32..=360,
    ) {
        let config = AdaptiveFpsConfig::default();
        let mut loop_state = AdaptiveFpsLoop::new(config);
        let mut sink = RecordingSink::default();

        loop_state.update_probe(first_probe);
        let first_expected = expected_decision(config, first_probe, display_max_fps, wake);
        let first = loop_state.tick(display_max_fps, wake, &mut sink);

        loop_state.update_probe(second_probe);
        let second_expected = expected_decision(config, second_probe, display_max_fps, wake);
        let second = loop_state.tick(display_max_fps, wake, &mut sink);

        prop_assert_eq!(first.decision, first_expected);
        prop_assert_eq!(second.decision, second_expected);
        prop_assert!(first.applied);
        prop_assert_eq!(second.applied, first_expected.target_fps != second_expected.target_fps);

        let expected_applied = if first_expected.target_fps == second_expected.target_fps {
            vec![first_expected.target_fps]
        } else {
            vec![first_expected.target_fps, second_expected.target_fps]
        };
        prop_assert_eq!(sink.applied, expected_applied);
    }

    #[test]
    fn disabled_loop_uses_performance_reason_independent_of_probe_and_wake(
        mode in arb_mode(),
        probe in arb_probe(),
        wake in arb_wake_state(),
        display_max_fps in 0u32..=360,
    ) {
        let config = AdaptiveFpsConfig {
            enabled: false,
            mode,
            thresholds: BatteryThresholds::default(),
        };
        let mut loop_state = AdaptiveFpsLoop::new(config);
        let mut sink = RecordingSink::default();
        loop_state.update_probe(probe);

        let tick = loop_state.tick(display_max_fps, wake, &mut sink);

        prop_assert_eq!(tick.decision.reason, AdaptiveDecisionReason::ModePerformance);
        prop_assert_eq!(tick.decision.target_fps, display_max_fps);
        prop_assert_eq!(sink.applied, vec![display_max_fps]);
    }
}
