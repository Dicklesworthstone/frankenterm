//! Battery-aware adaptive-FPS policy substrate (ft-2okh0.7).
//!
//! Pure-logic decision tree mapping (power source, thermal state,
//! battery level, operator override) onto target FPS + render
//! quality. The integration layer probes the OS state and feeds it
//! in; this module returns the deterministic per-tick policy
//! decision. Per-platform power probes (IOPSGetPowerSource on
//! macOS, /sys/class/power_supply on Linux, GetSystemPowerStatus on
//! Windows) live in the integration crate's startup + tick loop.
//!
//! ## What this module ships
//!
//! - `PowerSource` (Ac / Battery / Unknown) — runtime-probed.
//! - `ThermalState` (Nominal / Fair / Serious / Critical) — mirrors
//!   macOS NSProcessInfoThermalState semantics; cross-platform.
//! - `BatteryLevel` (`u8`-percent 0..=100, with sentinel `None` for
//!   AC-only / probe-failed).
//! - `AdaptiveMode` (Auto / Performance / Balanced / BatterySaver)
//!   from the bead's `[adaptive_fps] mode` config.
//! - `WakeOverride` — per the bead's "DO NOT BREAK" rules:
//!   active typing / BEL / live-resize / a11y query must override
//!   to high-rate regardless of power/thermal state.
//! - `AdaptiveDecision` — `{ target_fps, quality, reason }` the
//!   integration consumes.
//! - `select_decision(snapshot, mode) -> AdaptiveDecision` — pure
//!   policy. Override always wins; mode then power+thermal
//!   gradient.
//! - `BatteryThresholds { low_pct }` operator config.
//! - `AdaptiveDecisionReason` 9-variant enum for `ft doctor`
//!   surface (operator visibility into *why* the renderer picked
//!   this rate).
//!
//! ## What is deferred to the integration bead (ft-2okh0.7.cont)
//!
//! - macOS power probe via `IOPSGetPowerSource` + thermal probe
//!   via `NSProcessInfo.thermalState`.
//! - Linux probes via `/sys/class/power_supply/AC*/online` +
//!   `/sys/class/thermal/thermal_zone*/temp`.
//! - Windows probes via `GetSystemPowerStatus` + WMI thermal zone.
//! - Live notification subscriptions: macOS
//!   `IOPSNotificationCreateRunLoopSource`, Linux udev power-supply
//!   events, Windows `WM_POWERBROADCAST`.
//! - Tick-loop integration: each frame, probe → `select_decision` →
//!   apply target_fps to FrameBudget (cross-link ft-mpc9b.5.2).
//! - frankenterm.toml `[adaptive_fps]` config parsing.
//! - Cross-link a11y_detection from ft-mpc9b.10.5 for the a11y
//!   override path.

#![allow(dead_code)]

// ============================================================================
// Power source
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PowerSource {
    /// Wall power. The renderer can use the display's max refresh.
    Ac,
    /// Battery. Drives the FPS gradient.
    Battery,
    /// Probe didn't run / unsupported platform. Conservative
    /// fallback: treat as `Battery` (the renderer prefers
    /// power-conscious defaults under uncertainty).
    #[default]
    Unknown,
}

// ============================================================================
// Thermal state
// ============================================================================

/// Mirrors macOS `NSProcessInfoThermalState`. Linux maps thermal
/// zone temperatures to the closest tier; Windows ditto via WMI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ThermalState {
    /// System is cool; no throttling needed.
    #[default]
    Nominal,
    /// Slight thermal pressure; minor throttling acceptable.
    Fair,
    /// Significant thermal pressure; aggressive throttling required.
    Serious,
    /// Imminent thermal shutdown risk; minimum-rate emergency mode.
    Critical,
}

// ============================================================================
// Battery level
// ============================================================================

/// Battery percentage 0..=100, or `None` when the host is
/// AC-only / probe-failed. Constructor clamps; impossible values
/// (>100) are folded to 100 rather than refused (defensive — a
/// flaky probe shouldn't break startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BatteryLevel(Option<u8>);

impl BatteryLevel {
    pub const NONE: Self = Self(None);

    #[must_use]
    pub fn from_percent(pct: u8) -> Self {
        Self(Some(pct.min(100)))
    }

    #[must_use]
    pub const fn percent(&self) -> Option<u8> {
        self.0
    }

    #[must_use]
    pub fn is_present(&self) -> bool {
        self.0.is_some()
    }

    #[must_use]
    pub fn is_below(&self, threshold_pct: u8) -> bool {
        matches!(self.0, Some(p) if p < threshold_pct)
    }
}

// ============================================================================
// Adaptive mode (operator config)
// ============================================================================

/// `[adaptive_fps] mode` in frankenterm.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AdaptiveMode {
    /// Auto-select per the bead's table. Default.
    #[default]
    Auto,
    /// Always max performance regardless of power state.
    Performance,
    /// Mid-tier; ignores low-battery downgrade.
    Balanced,
    /// Always favour battery; clamps target_fps low even on AC.
    BatterySaver,
}

// ============================================================================
// Wake overrides (bead's DO NOT BREAK)
// ============================================================================

/// Per the bead, these conditions override any adaptive-FPS
/// downgrade and force the renderer back to high-rate. Mutually
/// non-exclusive — if any are set, the override wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WakeOverride {
    /// User is typing. Must dispatch frames at full input-latency
    /// rate.
    pub active_typing: bool,
    /// Bell character emitted; must wake.
    pub bell_received: bool,
    /// Live-resize gesture in progress (cross-link
    /// LiveResizeState from ft-mpc9b.2.1).
    pub live_resize: bool,
    /// AT-SPI / NSAccessibility / UIA query in flight (cross-link
    /// ft-mpc9b.10.5 a11y prefs).
    pub a11y_query_in_flight: bool,
}

impl WakeOverride {
    #[must_use]
    pub fn any_active(&self) -> bool {
        self.active_typing || self.bell_received || self.live_resize || self.a11y_query_in_flight
    }
}

// ============================================================================
// Battery thresholds (operator config)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryThresholds {
    /// Below this percent, drop FPS to 30. Bead default 50.
    pub mid_low_pct: u8,
    /// Below this percent, drop further to 15. Bead default 20.
    pub low_pct: u8,
}

impl Default for BatteryThresholds {
    fn default() -> Self {
        Self {
            mid_low_pct: 50,
            low_pct: 20,
        }
    }
}

// ============================================================================
// Quality levels (cross-link ft-mpc9b.10.5 RenderQualityHint)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Quality {
    /// Highest quality (extra effects, AA, post-processing).
    Fancy,
    /// Default quality.
    #[default]
    Standard,
    /// Reduced quality during heavy bursts / low-power.
    Draft,
}

// ============================================================================
// Decision
// ============================================================================

/// Per-tick policy snapshot the integration feeds in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerSnapshot {
    pub power_source: PowerSource,
    pub thermal: ThermalState,
    pub battery: BatteryLevel,
    pub display_max_fps: u32,
    pub overrides: WakeOverride,
    pub thresholds: BatteryThresholds,
}

impl PowerSnapshot {
    /// Fully on AC, no thermal pressure, no overrides. Useful as a
    /// test baseline.
    #[must_use]
    pub fn ac_baseline(display_max_fps: u32) -> Self {
        Self {
            power_source: PowerSource::Ac,
            thermal: ThermalState::Nominal,
            battery: BatteryLevel::NONE,
            display_max_fps,
            overrides: WakeOverride::default(),
            thresholds: BatteryThresholds::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveDecision {
    pub target_fps: u32,
    pub quality: Quality,
    pub reason: AdaptiveDecisionReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdaptiveDecisionReason {
    /// One of the WakeOverride rules fired.
    OverrideTyping,
    OverrideBell,
    OverrideLiveResize,
    OverrideA11y,
    /// AdaptiveMode forced this rate.
    ModePerformance,
    ModeBatterySaver,
    /// AC + cool — the bead's "AC + cool" row.
    AcCool,
    /// AC + warm — Fair/Serious thermal on AC.
    AcWarm,
    /// Battery + percent threshold + thermal compose into the
    /// remaining table rows.
    BatteryHigh,
    BatteryMid,
    BatteryLow,
    ThermalCritical,
}

/// Wake-override target FPS — high enough for typing-latency and
/// AT-update responsiveness. The bead doesn't pin a specific
/// number; 60 is the safe default that matches the renderer's
/// non-Draft path.
pub const WAKE_OVERRIDE_FPS: u32 = 60;

/// Pure-logic adaptive-FPS policy. Resolution order:
/// 1. WakeOverride wins unconditionally (matches bead's DO NOT
///    BREAK rules — typing / BEL / live-resize / a11y).
/// 2. AdaptiveMode::Performance forces max regardless of state.
/// 3. AdaptiveMode::BatterySaver forces low regardless of state.
/// 4. Auto / Balanced apply the bead's table:
///    - Thermal Critical → 5 fps Draft (always)
///    - AC + Nominal → display max, Fancy
///    - AC + Fair/Serious → 60 fps Standard (the bead's "warm" row)
///    - Battery present + < low_pct → 15 fps Draft
///    - Battery present + < mid_low_pct → 30 fps Standard
///    - Battery present + ≥ mid_low_pct → 60 fps Standard
///    - Unknown power → treat as Battery for safety; mid_low row
///      unless battery probe is also missing, in which case 30 fps
///      Standard (conservative middle).
#[must_use]
pub fn select_decision(snapshot: PowerSnapshot, mode: AdaptiveMode) -> AdaptiveDecision {
    // Step 1: wake overrides win unconditionally per the bead's
    // DO NOT BREAK section.
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

    // Step 2: adaptive-mode forced overrides.
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

    // Step 3: thermal Critical wins next.
    if matches!(snapshot.thermal, ThermalState::Critical) {
        return AdaptiveDecision {
            target_fps: 5,
            quality: Quality::Draft,
            reason: AdaptiveDecisionReason::ThermalCritical,
        };
    }

    // Step 4: power-source + thermal + battery gradient.
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
            ThermalState::Critical => unreachable!("handled above"),
        },
        PowerSource::Battery | PowerSource::Unknown => {
            let pct = snapshot.battery.percent();
            match pct {
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
                // Unknown power + missing battery probe → conservative
                // middle.
                None => AdaptiveDecision {
                    target_fps: 30,
                    quality: Quality::Standard,
                    reason: AdaptiveDecisionReason::BatteryMid,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(power: PowerSource, thermal: ThermalState, battery: BatteryLevel) -> PowerSnapshot {
        PowerSnapshot {
            power_source: power,
            thermal,
            battery,
            display_max_fps: 120,
            overrides: WakeOverride::default(),
            thresholds: BatteryThresholds::default(),
        }
    }

    // ----------------------------------------------------------------
    // BatteryLevel
    // ----------------------------------------------------------------

    #[test]
    fn battery_none_is_default() {
        assert_eq!(BatteryLevel::default(), BatteryLevel::NONE);
        assert_eq!(BatteryLevel::default().percent(), None);
        assert!(!BatteryLevel::default().is_present());
    }

    #[test]
    fn battery_from_percent_clamps_to_100() {
        assert_eq!(BatteryLevel::from_percent(150).percent(), Some(100));
        assert_eq!(BatteryLevel::from_percent(100).percent(), Some(100));
        assert_eq!(BatteryLevel::from_percent(0).percent(), Some(0));
        assert_eq!(BatteryLevel::from_percent(50).percent(), Some(50));
    }

    #[test]
    fn battery_is_below_works_with_none() {
        assert!(!BatteryLevel::NONE.is_below(50));
        assert!(BatteryLevel::from_percent(10).is_below(20));
        assert!(!BatteryLevel::from_percent(20).is_below(20));
        assert!(!BatteryLevel::from_percent(30).is_below(20));
    }

    // ----------------------------------------------------------------
    // WakeOverride
    // ----------------------------------------------------------------

    #[test]
    fn wake_override_default_inactive() {
        assert!(!WakeOverride::default().any_active());
    }

    #[test]
    fn wake_override_any_active_detects_each() {
        let mut o = WakeOverride::default();
        o.active_typing = true;
        assert!(o.any_active());
        let mut o = WakeOverride::default();
        o.bell_received = true;
        assert!(o.any_active());
        let mut o = WakeOverride::default();
        o.live_resize = true;
        assert!(o.any_active());
        let mut o = WakeOverride::default();
        o.a11y_query_in_flight = true;
        assert!(o.any_active());
    }

    // ----------------------------------------------------------------
    // BatteryThresholds default
    // ----------------------------------------------------------------

    #[test]
    fn battery_thresholds_default_match_bead() {
        let t = BatteryThresholds::default();
        assert_eq!(t.mid_low_pct, 50);
        assert_eq!(t.low_pct, 20);
    }

    // ----------------------------------------------------------------
    // Bead's table — Auto mode
    // ----------------------------------------------------------------

    #[test]
    fn auto_ac_cool_picks_max_fps_fancy() {
        let s = snap(PowerSource::Ac, ThermalState::Nominal, BatteryLevel::NONE);
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 120);
        assert_eq!(d.quality, Quality::Fancy);
        assert_eq!(d.reason, AdaptiveDecisionReason::AcCool);
    }

    #[test]
    fn auto_ac_warm_picks_60_standard() {
        for thermal in [ThermalState::Fair, ThermalState::Serious] {
            let s = snap(PowerSource::Ac, thermal, BatteryLevel::NONE);
            let d = select_decision(s, AdaptiveMode::Auto);
            assert_eq!(d.target_fps, 60);
            assert_eq!(d.quality, Quality::Standard);
            assert_eq!(d.reason, AdaptiveDecisionReason::AcWarm);
        }
    }

    #[test]
    fn auto_battery_high_picks_60_standard() {
        let s = snap(
            PowerSource::Battery,
            ThermalState::Nominal,
            BatteryLevel::from_percent(80),
        );
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 60);
        assert_eq!(d.quality, Quality::Standard);
        assert_eq!(d.reason, AdaptiveDecisionReason::BatteryHigh);
    }

    #[test]
    fn auto_battery_mid_picks_30_standard() {
        let s = snap(
            PowerSource::Battery,
            ThermalState::Nominal,
            BatteryLevel::from_percent(35),
        );
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 30);
        assert_eq!(d.quality, Quality::Standard);
        assert_eq!(d.reason, AdaptiveDecisionReason::BatteryMid);
    }

    #[test]
    fn auto_battery_low_picks_15_draft() {
        let s = snap(
            PowerSource::Battery,
            ThermalState::Nominal,
            BatteryLevel::from_percent(10),
        );
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 15);
        assert_eq!(d.quality, Quality::Draft);
        assert_eq!(d.reason, AdaptiveDecisionReason::BatteryLow);
    }

    #[test]
    fn auto_thermal_critical_overrides_battery() {
        // Thermal Critical fires before the battery table.
        let s = snap(
            PowerSource::Battery,
            ThermalState::Critical,
            BatteryLevel::from_percent(80),
        );
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 5);
        assert_eq!(d.quality, Quality::Draft);
        assert_eq!(d.reason, AdaptiveDecisionReason::ThermalCritical);
    }

    #[test]
    fn auto_thermal_critical_overrides_ac_cool() {
        let s = snap(PowerSource::Ac, ThermalState::Critical, BatteryLevel::NONE);
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 5);
        assert_eq!(d.reason, AdaptiveDecisionReason::ThermalCritical);
    }

    // ----------------------------------------------------------------
    // Mode overrides (Performance / BatterySaver)
    // ----------------------------------------------------------------

    #[test]
    fn mode_performance_forces_max_fancy_regardless_of_battery() {
        let s = snap(
            PowerSource::Battery,
            ThermalState::Nominal,
            BatteryLevel::from_percent(5),
        );
        let d = select_decision(s, AdaptiveMode::Performance);
        assert_eq!(d.target_fps, 120);
        assert_eq!(d.quality, Quality::Fancy);
        assert_eq!(d.reason, AdaptiveDecisionReason::ModePerformance);
    }

    #[test]
    fn mode_battery_saver_forces_15_draft_regardless_of_ac() {
        let s = snap(PowerSource::Ac, ThermalState::Nominal, BatteryLevel::NONE);
        let d = select_decision(s, AdaptiveMode::BatterySaver);
        assert_eq!(d.target_fps, 15);
        assert_eq!(d.quality, Quality::Draft);
        assert_eq!(d.reason, AdaptiveDecisionReason::ModeBatterySaver);
    }

    #[test]
    fn mode_balanced_behaves_like_auto() {
        let s = snap(
            PowerSource::Battery,
            ThermalState::Nominal,
            BatteryLevel::from_percent(80),
        );
        let auto = select_decision(s, AdaptiveMode::Auto);
        let balanced = select_decision(s, AdaptiveMode::Balanced);
        assert_eq!(auto, balanced);
    }

    // ----------------------------------------------------------------
    // Wake overrides (DO NOT BREAK rules)
    // ----------------------------------------------------------------

    #[test]
    fn typing_override_beats_battery_low() {
        let mut s = snap(
            PowerSource::Battery,
            ThermalState::Nominal,
            BatteryLevel::from_percent(5),
        );
        s.overrides.active_typing = true;
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, WAKE_OVERRIDE_FPS);
        assert_eq!(d.reason, AdaptiveDecisionReason::OverrideTyping);
    }

    #[test]
    fn bell_override_beats_thermal_critical() {
        // The bead's DO NOT BREAK is unconditional — bell wakes
        // even from Critical state. A user audible alert must not
        // be suppressed by power management.
        let mut s = snap(
            PowerSource::Battery,
            ThermalState::Critical,
            BatteryLevel::from_percent(10),
        );
        s.overrides.bell_received = true;
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, WAKE_OVERRIDE_FPS);
        assert_eq!(d.reason, AdaptiveDecisionReason::OverrideBell);
    }

    #[test]
    fn live_resize_override_takes_display_max() {
        let mut s = snap(
            PowerSource::Battery,
            ThermalState::Critical,
            BatteryLevel::from_percent(5),
        );
        s.overrides.live_resize = true;
        let d = select_decision(s, AdaptiveMode::Auto);
        // Live resize uses display_max (120) — must be smooth.
        assert_eq!(d.target_fps, 120);
        assert_eq!(d.reason, AdaptiveDecisionReason::OverrideLiveResize);
    }

    #[test]
    fn live_resize_override_floors_at_60_when_display_max_lower() {
        let mut s = PowerSnapshot {
            power_source: PowerSource::Battery,
            thermal: ThermalState::Critical,
            battery: BatteryLevel::from_percent(5),
            display_max_fps: 30, // lower than the 60 floor
            overrides: WakeOverride::default(),
            thresholds: BatteryThresholds::default(),
        };
        s.overrides.live_resize = true;
        let d = select_decision(s, AdaptiveMode::Auto);
        // Floor at 60 to keep resize smooth even on low-refresh
        // displays.
        assert_eq!(d.target_fps, 60);
    }

    #[test]
    fn a11y_override_beats_battery_saver_mode() {
        let mut s = snap(PowerSource::Ac, ThermalState::Nominal, BatteryLevel::NONE);
        s.overrides.a11y_query_in_flight = true;
        // Even with operator opted into BatterySaver, AT must keep
        // updating.
        let d = select_decision(s, AdaptiveMode::BatterySaver);
        assert_eq!(d.target_fps, WAKE_OVERRIDE_FPS);
        assert_eq!(d.reason, AdaptiveDecisionReason::OverrideA11y);
    }

    #[test]
    fn override_priority_live_resize_beats_typing() {
        let mut s = snap(PowerSource::Ac, ThermalState::Nominal, BatteryLevel::NONE);
        s.overrides.active_typing = true;
        s.overrides.live_resize = true;
        let d = select_decision(s, AdaptiveMode::Auto);
        // Live resize is checked first in the substrate (it's the
        // most cycle-sensitive).
        assert_eq!(d.reason, AdaptiveDecisionReason::OverrideLiveResize);
    }

    #[test]
    fn override_priority_a11y_beats_typing_and_bell() {
        let mut s = snap(PowerSource::Ac, ThermalState::Nominal, BatteryLevel::NONE);
        s.overrides.active_typing = true;
        s.overrides.bell_received = true;
        s.overrides.a11y_query_in_flight = true;
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.reason, AdaptiveDecisionReason::OverrideA11y);
    }

    // ----------------------------------------------------------------
    // Unknown power source defensive handling
    // ----------------------------------------------------------------

    #[test]
    fn unknown_power_with_missing_battery_picks_conservative_middle() {
        let s = snap(
            PowerSource::Unknown,
            ThermalState::Nominal,
            BatteryLevel::NONE,
        );
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 30);
        assert_eq!(d.quality, Quality::Standard);
    }

    #[test]
    fn unknown_power_with_battery_present_uses_battery_table() {
        let s = snap(
            PowerSource::Unknown,
            ThermalState::Nominal,
            BatteryLevel::from_percent(80),
        );
        let d = select_decision(s, AdaptiveMode::Auto);
        // Battery >= mid_low_pct → BatteryHigh → 60 fps.
        assert_eq!(d.target_fps, 60);
        assert_eq!(d.reason, AdaptiveDecisionReason::BatteryHigh);
    }

    // ----------------------------------------------------------------
    // Threshold edge cases
    // ----------------------------------------------------------------

    #[test]
    fn battery_at_exactly_low_threshold_is_mid_not_low() {
        // is_below uses strict <, so `pct == low_pct` is mid not low.
        let s = snap(
            PowerSource::Battery,
            ThermalState::Nominal,
            BatteryLevel::from_percent(20), // exactly at low_pct
        );
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 30);
        assert_eq!(d.reason, AdaptiveDecisionReason::BatteryMid);
    }

    #[test]
    fn battery_at_exactly_mid_low_threshold_is_high_not_mid() {
        let s = snap(
            PowerSource::Battery,
            ThermalState::Nominal,
            BatteryLevel::from_percent(50), // exactly at mid_low_pct
        );
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 60);
        assert_eq!(d.reason, AdaptiveDecisionReason::BatteryHigh);
    }

    #[test]
    fn operator_can_tune_thresholds() {
        // Operator wants to keep 60 fps until 30% remaining.
        let mut s = snap(
            PowerSource::Battery,
            ThermalState::Nominal,
            BatteryLevel::from_percent(40),
        );
        s.thresholds = BatteryThresholds {
            mid_low_pct: 30,
            low_pct: 10,
        };
        let d = select_decision(s, AdaptiveMode::Auto);
        assert_eq!(d.target_fps, 60);
        assert_eq!(d.reason, AdaptiveDecisionReason::BatteryHigh);
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic scenarios from the bead
    // ----------------------------------------------------------------

    #[test]
    fn scenario_laptop_unplugged_workday() {
        // Walk a battery from 100% down to 5% on Battery + Nominal
        // and confirm the FPS gradient.
        let stages = [
            (100, 60, AdaptiveDecisionReason::BatteryHigh),
            (75, 60, AdaptiveDecisionReason::BatteryHigh),
            (50, 60, AdaptiveDecisionReason::BatteryHigh),
            (49, 30, AdaptiveDecisionReason::BatteryMid),
            (25, 30, AdaptiveDecisionReason::BatteryMid),
            (20, 30, AdaptiveDecisionReason::BatteryMid),
            (19, 15, AdaptiveDecisionReason::BatteryLow),
            (5, 15, AdaptiveDecisionReason::BatteryLow),
        ];
        for (pct, expected_fps, expected_reason) in stages {
            let s = snap(
                PowerSource::Battery,
                ThermalState::Nominal,
                BatteryLevel::from_percent(pct),
            );
            let d = select_decision(s, AdaptiveMode::Auto);
            assert_eq!(d.target_fps, expected_fps, "at {pct}%");
            assert_eq!(d.reason, expected_reason, "at {pct}%");
        }
    }

    #[test]
    fn scenario_accessibility_user_battery_saver_mode() {
        // Operator on AdaptiveMode::BatterySaver but using a screen
        // reader — AT updates must keep flowing at WAKE_OVERRIDE_FPS.
        let mut s = snap(PowerSource::Ac, ThermalState::Nominal, BatteryLevel::NONE);
        s.overrides.a11y_query_in_flight = true;
        let d = select_decision(s, AdaptiveMode::BatterySaver);
        assert_eq!(d.target_fps, WAKE_OVERRIDE_FPS);
        assert_eq!(d.quality, Quality::Standard); // not Draft — a11y reads the visible state
        assert_eq!(d.reason, AdaptiveDecisionReason::OverrideA11y);
    }

    #[test]
    fn scenario_thermal_critical_typing_bell_storm() {
        // System is thermal-critical, but user is actively typing
        // and a bell just fired — wake overrides keep us responsive.
        let mut s = snap(
            PowerSource::Battery,
            ThermalState::Critical,
            BatteryLevel::from_percent(10),
        );
        s.overrides.active_typing = true;
        s.overrides.bell_received = true;
        let d = select_decision(s, AdaptiveMode::Auto);
        // A11y > typing > bell in priority; here neither a11y nor
        // resize is set, so live_resize→a11y→typing→bell ordering
        // fires typing first (after a11y/resize empty).
        assert_eq!(d.target_fps, WAKE_OVERRIDE_FPS);
        assert_eq!(d.reason, AdaptiveDecisionReason::OverrideTyping);
    }
}
