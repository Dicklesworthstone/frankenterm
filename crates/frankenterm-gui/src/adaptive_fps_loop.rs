//! GUI-side adaptive-FPS loop integration.
//!
//! The core `adaptive_fps` module owns the deterministic policy table.
//! This module owns the mutable GUI seam: probe snapshot caching,
//! decision counters for `ft doctor`, and applying changed target
//! rates to the renderer's frame-budget/scheduler surface.

use std::collections::HashMap;

use frankenterm_core::adaptive_fps::{
    AdaptiveDecision, AdaptiveDecisionReason, AdaptiveMode, BatteryLevel, BatteryThresholds,
    PowerSnapshot, PowerSource, ThermalState, WakeOverride, select_decision,
};

/// Static operator config parsed from `[adaptive_fps]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveFpsConfig {
    pub enabled: bool,
    pub mode: AdaptiveMode,
    pub thresholds: BatteryThresholds,
}

impl Default for AdaptiveFpsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: AdaptiveMode::Auto,
            thresholds: BatteryThresholds::default(),
        }
    }
}

impl AdaptiveFpsConfig {
    /// Build config with the low threshold validation required by
    /// the bead. `0` and `100` are rejected because they collapse
    /// the battery gradient.
    pub fn new(
        enabled: bool,
        mode: AdaptiveMode,
        mid_low_pct: u8,
        low_pct: u8,
    ) -> Result<Self, AdaptiveFpsConfigError> {
        if !(1..=99).contains(&low_pct) {
            return Err(AdaptiveFpsConfigError::LowThresholdOutOfRange { value: low_pct });
        }
        if !(1..=99).contains(&mid_low_pct) {
            return Err(AdaptiveFpsConfigError::MidLowThresholdOutOfRange { value: mid_low_pct });
        }
        if low_pct >= mid_low_pct {
            return Err(AdaptiveFpsConfigError::LowThresholdNotBelowMid {
                low_pct,
                mid_low_pct,
            });
        }
        Ok(Self {
            enabled,
            mode,
            thresholds: BatteryThresholds {
                mid_low_pct,
                low_pct,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveFpsConfigError {
    LowThresholdOutOfRange { value: u8 },
    MidLowThresholdOutOfRange { value: u8 },
    LowThresholdNotBelowMid { low_pct: u8, mid_low_pct: u8 },
}

/// Platform probe output consumed by the GUI tick loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerProbeSnapshot {
    pub power_source: PowerSource,
    pub thermal: ThermalState,
    pub battery: BatteryLevel,
}

impl Default for PowerProbeSnapshot {
    fn default() -> Self {
        Self {
            power_source: PowerSource::Unknown,
            thermal: ThermalState::Nominal,
            battery: BatteryLevel::NONE,
        }
    }
}

/// Per-tick wake flags supplied by input, BEL, resize, and a11y observers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdaptiveWakeState {
    pub active_typing: bool,
    pub bell_received: bool,
    pub live_resize: bool,
    pub a11y_query_in_flight: bool,
}

impl From<AdaptiveWakeState> for WakeOverride {
    fn from(value: AdaptiveWakeState) -> Self {
        Self {
            active_typing: value.active_typing,
            bell_received: value.bell_received,
            live_resize: value.live_resize,
            a11y_query_in_flight: value.a11y_query_in_flight,
        }
    }
}

/// Mutable sink implemented by the renderer/frame scheduler boundary.
pub trait FrameRateSink {
    fn apply_target_fps(&mut self, target_fps: u32);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveFpsTick {
    pub decision: AdaptiveDecision,
    pub applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveFpsDoctorSnapshot {
    pub enabled: bool,
    pub power_source: PowerSource,
    pub thermal: ThermalState,
    pub battery_percent: Option<u8>,
    pub last_decision: Option<AdaptiveDecision>,
    pub decision_counts: Vec<AdaptiveDecisionCount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveDecisionCount {
    pub reason: AdaptiveDecisionReason,
    pub count: u64,
}

/// Cached adaptive-FPS loop state. Platform notifications update
/// the probe cache; the GUI tick calls `tick` with display and wake
/// state and receives the current decision.
#[derive(Debug, Clone)]
pub struct AdaptiveFpsLoop {
    config: AdaptiveFpsConfig,
    probe: PowerProbeSnapshot,
    last_decision: Option<AdaptiveDecision>,
    decision_counts: HashMap<AdaptiveDecisionReason, u64>,
}

impl AdaptiveFpsLoop {
    #[must_use]
    pub fn new(config: AdaptiveFpsConfig) -> Self {
        Self {
            config,
            probe: PowerProbeSnapshot::default(),
            last_decision: None,
            decision_counts: HashMap::new(),
        }
    }

    pub fn update_probe(&mut self, probe: PowerProbeSnapshot) {
        self.probe = probe;
    }

    pub fn tick<S: FrameRateSink>(
        &mut self,
        display_max_fps: u32,
        wake: AdaptiveWakeState,
        sink: &mut S,
    ) -> AdaptiveFpsTick {
        let decision = if self.config.enabled {
            let snapshot = PowerSnapshot {
                power_source: self.probe.power_source,
                thermal: self.probe.thermal,
                battery: self.probe.battery,
                display_max_fps,
                overrides: wake.into(),
                thresholds: self.config.thresholds,
            };
            select_decision(snapshot, self.config.mode)
        } else {
            select_decision(
                PowerSnapshot::ac_baseline(display_max_fps),
                AdaptiveMode::Performance,
            )
        };

        let applied = self
            .last_decision
            .is_none_or(|last| last.target_fps != decision.target_fps);
        if applied {
            sink.apply_target_fps(decision.target_fps);
        }

        self.last_decision = Some(decision);
        *self.decision_counts.entry(decision.reason).or_default() += 1;

        AdaptiveFpsTick { decision, applied }
    }

    #[must_use]
    pub fn doctor_snapshot(&self) -> AdaptiveFpsDoctorSnapshot {
        let mut decision_counts: Vec<_> = self
            .decision_counts
            .iter()
            .map(|(&reason, &count)| AdaptiveDecisionCount { reason, count })
            .collect();
        decision_counts.sort_by_key(|entry| reason_order(entry.reason));

        AdaptiveFpsDoctorSnapshot {
            enabled: self.config.enabled,
            power_source: self.probe.power_source,
            thermal: self.probe.thermal,
            battery_percent: self.probe.battery.percent(),
            last_decision: self.last_decision,
            decision_counts,
        }
    }
}

fn reason_order(reason: AdaptiveDecisionReason) -> u8 {
    match reason {
        AdaptiveDecisionReason::OverrideTyping => 0,
        AdaptiveDecisionReason::OverrideBell => 1,
        AdaptiveDecisionReason::OverrideLiveResize => 2,
        AdaptiveDecisionReason::OverrideA11y => 3,
        AdaptiveDecisionReason::ModePerformance => 4,
        AdaptiveDecisionReason::ModeBatterySaver => 5,
        AdaptiveDecisionReason::AcCool => 6,
        AdaptiveDecisionReason::AcWarm => 7,
        AdaptiveDecisionReason::BatteryHigh => 8,
        AdaptiveDecisionReason::BatteryMid => 9,
        AdaptiveDecisionReason::BatteryLow => 10,
        AdaptiveDecisionReason::ThermalCritical => 11,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_core::adaptive_fps::Quality;

    #[derive(Default)]
    struct RecordingSink {
        applied: Vec<u32>,
    }

    impl FrameRateSink for RecordingSink {
        fn apply_target_fps(&mut self, target_fps: u32) {
            self.applied.push(target_fps);
        }
    }

    #[test]
    fn config_rejects_gradient_collapsing_thresholds() {
        assert_eq!(
            AdaptiveFpsConfig::new(true, AdaptiveMode::Auto, 50, 0),
            Err(AdaptiveFpsConfigError::LowThresholdOutOfRange { value: 0 })
        );
        assert_eq!(
            AdaptiveFpsConfig::new(true, AdaptiveMode::Auto, 50, 100),
            Err(AdaptiveFpsConfigError::LowThresholdOutOfRange { value: 100 })
        );
        assert_eq!(
            AdaptiveFpsConfig::new(true, AdaptiveMode::Auto, 20, 20),
            Err(AdaptiveFpsConfigError::LowThresholdNotBelowMid {
                low_pct: 20,
                mid_low_pct: 20
            })
        );
    }

    #[test]
    fn tick_applies_when_target_fps_changes_only() {
        let mut loop_state = AdaptiveFpsLoop::new(AdaptiveFpsConfig::default());
        let mut sink = RecordingSink::default();

        loop_state.update_probe(PowerProbeSnapshot {
            power_source: PowerSource::Ac,
            thermal: ThermalState::Nominal,
            battery: BatteryLevel::NONE,
        });

        let first = loop_state.tick(120, AdaptiveWakeState::default(), &mut sink);
        let second = loop_state.tick(120, AdaptiveWakeState::default(), &mut sink);

        assert_eq!(first.decision.target_fps, 120);
        assert_eq!(first.decision.quality, Quality::Fancy);
        assert!(first.applied);
        assert!(!second.applied);
        assert_eq!(sink.applied, vec![120]);
    }

    #[test]
    fn probe_transition_updates_decision_within_one_tick() {
        let mut loop_state = AdaptiveFpsLoop::new(AdaptiveFpsConfig::default());
        let mut sink = RecordingSink::default();

        loop_state.update_probe(PowerProbeSnapshot {
            power_source: PowerSource::Ac,
            thermal: ThermalState::Nominal,
            battery: BatteryLevel::NONE,
        });
        loop_state.tick(120, AdaptiveWakeState::default(), &mut sink);

        loop_state.update_probe(PowerProbeSnapshot {
            power_source: PowerSource::Battery,
            thermal: ThermalState::Nominal,
            battery: BatteryLevel::from_percent(10),
        });
        let tick = loop_state.tick(120, AdaptiveWakeState::default(), &mut sink);

        assert_eq!(tick.decision.target_fps, 15);
        assert_eq!(tick.decision.reason, AdaptiveDecisionReason::BatteryLow);
        assert_eq!(sink.applied, vec![120, 15]);
    }

    #[test]
    fn wake_override_preempts_low_battery_on_next_tick() {
        let mut loop_state = AdaptiveFpsLoop::new(AdaptiveFpsConfig::default());
        let mut sink = RecordingSink::default();

        loop_state.update_probe(PowerProbeSnapshot {
            power_source: PowerSource::Battery,
            thermal: ThermalState::Nominal,
            battery: BatteryLevel::from_percent(5),
        });
        loop_state.tick(120, AdaptiveWakeState::default(), &mut sink);

        let tick = loop_state.tick(
            120,
            AdaptiveWakeState {
                active_typing: true,
                ..AdaptiveWakeState::default()
            },
            &mut sink,
        );

        assert_eq!(tick.decision.target_fps, 60);
        assert_eq!(tick.decision.reason, AdaptiveDecisionReason::OverrideTyping);
        assert_eq!(sink.applied, vec![15, 60]);
    }

    #[test]
    fn doctor_snapshot_reports_power_state_last_decision_and_counts() {
        let mut loop_state = AdaptiveFpsLoop::new(AdaptiveFpsConfig::default());
        let mut sink = RecordingSink::default();

        loop_state.update_probe(PowerProbeSnapshot {
            power_source: PowerSource::Battery,
            thermal: ThermalState::Critical,
            battery: BatteryLevel::from_percent(42),
        });
        let tick = loop_state.tick(60, AdaptiveWakeState::default(), &mut sink);
        let snapshot = loop_state.doctor_snapshot();

        assert!(snapshot.enabled);
        assert_eq!(snapshot.power_source, PowerSource::Battery);
        assert_eq!(snapshot.thermal, ThermalState::Critical);
        assert_eq!(snapshot.battery_percent, Some(42));
        assert_eq!(snapshot.last_decision, Some(tick.decision));
        assert_eq!(
            snapshot.decision_counts,
            vec![AdaptiveDecisionCount {
                reason: AdaptiveDecisionReason::ThermalCritical,
                count: 1,
            }]
        );
    }
}
