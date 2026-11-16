//! Cross-pane WatchdogedTripleBuffer fleet-health substrate
//! (ft-l0oe3 / BR-TERM-EMULATOR-UPLIFT-2.3.cont2).
//!
//! Pure-logic substrate covering the substrate-shaped pieces
//! of the bead's GUI integration. The per-pane wrapper +
//! watchdog logic ships in `watchdoged_triple_buffer.rs`
//! (commit f66cd151d). This module adds:
//!
//! - Cross-pane aggregator combining per-pane
//!   `WatchdogedHealth` snapshots into a fleet-wide view.
//! - Operator-alert policy detecting "second consecutive
//!   ForceRecycle within 60s" per the bead's
//!   "renderer suspected stuck" alert.
//! - 24h-soak acceptance predicate per the bead's
//!   "force_recycle_invocations == 0 across the full run".
//! - Reconfigure audit-log entry schema for `frankenterm.toml`
//!   `[render.triple_buffer]` reload events.
//!
//! ## What this module ships
//!
//! - `PaneId(u64)` — opaque pane identifier (substrate-local).
//! - `PaneHealthSnapshot` — per-pane acquire / release /
//!   warning / force_recycle counters + last-recycle
//!   timestamp + watchdog_active flag.
//! - `FleetHealthAggregate` — sum-across-panes counters +
//!   `panes_currently_active_watchdog` + per-pane breakdown.
//! - `aggregate_fleet_health` pure aggregation function.
//! - `ConsecutiveRecyclePolicy` — operator-tunable
//!   "alert after N consecutive force-recycles within
//!   `window_ms`" config (defaults: N=2, window=60_000 ms).
//! - `RecycleAlertDecision` — `Quiet / FireAlert` decision
//!   over a per-pane recycle-history slice.
//! - `evaluate_recycle_alert` pure decision.
//! - `SoakAcceptanceConfig` — bead default
//!   force_recycle_invocations == 0 across the soak window;
//!   max_warnings_per_hour ≤ 0 default.
//! - `SoakSummary` — 24h soak aggregate result; per-pane
//!   counters + `meets_acceptance` predicate.
//! - `TripleBufferReconfigureAuditEvent` — `{
//!   pane_id, prev_warn_ms, prev_force_ms, new_warn_ms,
//!   new_force_ms, timestamp_ms, source }` for audit log.
//! - `ReconfigureSource` — `OperatorReload / TestOverride /
//!   AutomaticDegradation`.
//! - `FleetHealthTelemetry` per-session counters.
//!
//! ## What is deferred to ft-l0oe3 follow-up
//!
//! - Renderer migration in `crates/frankenterm-gui/src/`
//!   replacing the single-buffer mutex with
//!   `WatchdogedTripleBuffer<TerminalState>` per pane.
//! - Frame-timer poll wiring (same cadence as
//!   `IdleDetector::poll`).
//! - `ft doctor --triple-buffer` CLI subcommand.
//! - `ft flush` operator action calling `force_recycle()`.
//! - Reconfigure on `frankenterm.toml` reload.
//! - 200-pane soak test harness.

#![allow(dead_code)]

// ============================================================================
// PaneId
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct PaneId(pub u64);

// ============================================================================
// Per-pane health snapshot
// ============================================================================

/// What the per-pane `WatchdogedTripleBuffer.health()` returns
/// flattened to substrate-portable fields. The integration's
/// `WatchdogedHealth` struct exposes these via accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PaneHealthSnapshot {
    pub pane_id: PaneId,
    pub acquires: u64,
    pub releases: u64,
    pub warnings: u64,
    pub force_recycles: u64,
    /// Wall-clock timestamp (ms) of the most recent
    /// force-recycle, or 0 if none have fired in this pane's
    /// lifetime.
    pub last_force_recycle_ts_ms: u64,
    /// Whether the watchdog is currently in the Pending /
    /// Triggered state (i.e., a guard is held past the
    /// warn-threshold).
    pub watchdog_active: bool,
}

impl PaneHealthSnapshot {
    /// Whether this pane has ever fired the watchdog.
    #[must_use]
    pub const fn has_fired(&self) -> bool {
        self.force_recycles > 0
    }

    /// Time since last force-recycle in ms. `None` when this
    /// pane has never fired.
    #[must_use]
    pub fn ms_since_last_recycle(&self, now_ms: u64) -> Option<u64> {
        if self.last_force_recycle_ts_ms == 0 {
            None
        } else {
            Some(now_ms.saturating_sub(self.last_force_recycle_ts_ms))
        }
    }
}

// ============================================================================
// Fleet aggregate
// ============================================================================

/// Sum-across-panes view that `ft doctor --triple-buffer`
/// renders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetHealthAggregate {
    pub total_panes: u32,
    pub panes_currently_active_watchdog: u32,
    pub panes_ever_force_recycled: u32,
    pub total_acquires: u64,
    pub total_releases: u64,
    pub total_warnings: u64,
    pub total_force_recycles: u64,
    /// Highest per-pane force_recycle count observed.
    pub max_per_pane_force_recycles: u64,
}

/// Pure aggregation. Caller passes the slice of per-pane
/// snapshots collected at one point in time.
#[must_use]
pub fn aggregate_fleet_health(panes: &[PaneHealthSnapshot]) -> FleetHealthAggregate {
    let mut agg = FleetHealthAggregate {
        total_panes: panes.len() as u32,
        ..FleetHealthAggregate::default()
    };
    for p in panes {
        if p.watchdog_active {
            agg.panes_currently_active_watchdog =
                agg.panes_currently_active_watchdog.saturating_add(1);
        }
        if p.has_fired() {
            agg.panes_ever_force_recycled = agg.panes_ever_force_recycled.saturating_add(1);
        }
        agg.total_acquires = agg.total_acquires.saturating_add(p.acquires);
        agg.total_releases = agg.total_releases.saturating_add(p.releases);
        agg.total_warnings = agg.total_warnings.saturating_add(p.warnings);
        agg.total_force_recycles = agg.total_force_recycles.saturating_add(p.force_recycles);
        if p.force_recycles > agg.max_per_pane_force_recycles {
            agg.max_per_pane_force_recycles = p.force_recycles;
        }
    }
    agg
}

// ============================================================================
// Operator-alert policy: consecutive recycles within window
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsecutiveRecyclePolicy {
    /// Trigger an alert once this many force-recycles have
    /// fired within the rolling window. Bead default 2.
    pub alert_after_consecutive: u32,
    /// Rolling-window length in milliseconds. Bead default
    /// 60_000 (60 s).
    pub window_ms: u64,
}

pub const DEFAULT_ALERT_CONSECUTIVE_RECYCLES: u32 = 2;
pub const DEFAULT_RECYCLE_WINDOW_MS: u64 = 60_000;

impl Default for ConsecutiveRecyclePolicy {
    fn default() -> Self {
        Self {
            alert_after_consecutive: DEFAULT_ALERT_CONSECUTIVE_RECYCLES,
            window_ms: DEFAULT_RECYCLE_WINDOW_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecycleAlertDecision {
    /// No alert — recycles are sparse enough not to indicate
    /// a stuck renderer.
    Quiet,
    /// Alert — the integration surfaces "renderer suspected
    /// stuck" to the operator (banner / log / pager).
    FireAlert,
}

/// Pure decision: count force-recycles in `recycle_ts_history`
/// that fall within `[now_ms - window_ms, now_ms]` and compare
/// to the threshold.
///
/// Caller provides `recycle_ts_history` sorted oldest-first;
/// substrate doesn't sort.
#[must_use]
pub fn evaluate_recycle_alert(
    recycle_ts_history: &[u64],
    policy: ConsecutiveRecyclePolicy,
    now_ms: u64,
) -> RecycleAlertDecision {
    if policy.alert_after_consecutive == 0 {
        return RecycleAlertDecision::Quiet;
    }
    let window_start = now_ms.saturating_sub(policy.window_ms);
    let count_in_window = recycle_ts_history
        .iter()
        .filter(|ts| **ts >= window_start && **ts <= now_ms)
        .count();
    if count_in_window >= policy.alert_after_consecutive as usize {
        RecycleAlertDecision::FireAlert
    } else {
        RecycleAlertDecision::Quiet
    }
}

// ============================================================================
// 24h soak acceptance
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakAcceptanceConfig {
    /// Bead's "force_recycle_invocations == 0 across the full
    /// run" — substrate exposes the threshold for operator
    /// override (test runs may permit a small budget).
    pub max_force_recycles: u64,
    /// Maximum warnings per pane per hour. Default 0 (no
    /// warnings during clean soak).
    pub max_warnings_per_pane_per_hour: u64,
    /// Minimum panes the soak must drive. Bead default 200.
    pub min_panes: u32,
    /// Minimum elapsed wall-clock duration in ms. Bead
    /// default 24h = 86_400_000 ms.
    pub min_elapsed_ms: u64,
}

pub const SOAK_BEAD_FORCE_RECYCLE_BUDGET: u64 = 0;
pub const SOAK_BEAD_MAX_WARNINGS_PER_HOUR: u64 = 0;
pub const SOAK_BEAD_MIN_PANES: u32 = 200;
pub const SOAK_BEAD_MIN_ELAPSED_MS: u64 = 86_400_000;

impl Default for SoakAcceptanceConfig {
    fn default() -> Self {
        Self {
            max_force_recycles: SOAK_BEAD_FORCE_RECYCLE_BUDGET,
            max_warnings_per_pane_per_hour: SOAK_BEAD_MAX_WARNINGS_PER_HOUR,
            min_panes: SOAK_BEAD_MIN_PANES,
            min_elapsed_ms: SOAK_BEAD_MIN_ELAPSED_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakSummary {
    pub panes: u32,
    pub elapsed_ms: u64,
    pub total_force_recycles: u64,
    pub total_warnings: u64,
    pub max_per_pane_force_recycles: u64,
}

impl SoakSummary {
    /// Bead's acceptance predicate.
    ///
    /// Self-review fix (br-ft-ieoxt): warnings-per-pane-per-hour
    /// previously used `total / panes / hours` integer division
    /// which truncated 199 warnings / 200 panes / 24 hours to 0,
    /// passing the gate. Cross-multiplication catches the
    /// threshold directly:
    /// `total_warnings > max_per_pane_per_hour * panes * hours`.
    #[must_use]
    pub fn meets_acceptance(&self, config: SoakAcceptanceConfig) -> bool {
        if self.panes < config.min_panes {
            return false;
        }
        if self.elapsed_ms < config.min_elapsed_ms {
            return false;
        }
        if self.total_force_recycles > config.max_force_recycles {
            return false;
        }
        if self.panes > 0 && self.elapsed_ms > 0 {
            let hours = self.elapsed_ms.div_ceil(60 * 60 * 1000);
            let max_total = config
                .max_warnings_per_pane_per_hour
                .saturating_mul(self.panes as u64)
                .saturating_mul(hours);
            if self.total_warnings > max_total {
                return false;
            }
        }
        true
    }
}

// ============================================================================
// Reconfigure audit
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReconfigureSource {
    /// Operator reloaded `frankenterm.toml`.
    OperatorReload,
    /// Test code overrode the watchdog config (e.g., to a
    /// shorter timeout for fast tests).
    TestOverride,
    /// Substrate or integration auto-degraded the config in
    /// response to backpressure / resource pressure.
    AutomaticDegradation,
}

impl ReconfigureSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OperatorReload => "operator_reload",
            Self::TestOverride => "test_override",
            Self::AutomaticDegradation => "automatic_degradation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TripleBufferReconfigureAuditEvent {
    pub pane_id: PaneId,
    pub prev_warn_ms: u64,
    pub prev_force_ms: u64,
    pub new_warn_ms: u64,
    pub new_force_ms: u64,
    pub timestamp_ms: u64,
    pub source: ReconfigureSource,
}

impl TripleBufferReconfigureAuditEvent {
    /// Whether this reconfigure changed any value.
    #[must_use]
    pub const fn is_no_op(&self) -> bool {
        self.prev_warn_ms == self.new_warn_ms && self.prev_force_ms == self.new_force_ms
    }

    /// Whether the new config is more permissive (longer
    /// timeouts) than the previous.
    #[must_use]
    pub const fn is_relaxed(&self) -> bool {
        self.new_warn_ms > self.prev_warn_ms || self.new_force_ms > self.prev_force_ms
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FleetHealthTelemetry {
    pub aggregations_total: u64,
    pub alerts_fired: u64,
    pub alerts_suppressed_quiet: u64,
    pub reconfigures_total: u64,
    pub reconfigures_no_op: u64,
    pub reconfigures_relaxed: u64,
    pub soak_runs_total: u64,
    pub soak_runs_passed: u64,
}

impl FleetHealthTelemetry {
    pub fn record_aggregation(&mut self) {
        self.aggregations_total = self.aggregations_total.saturating_add(1);
    }

    pub fn record_alert_decision(&mut self, decision: RecycleAlertDecision) {
        match decision {
            RecycleAlertDecision::FireAlert => {
                self.alerts_fired = self.alerts_fired.saturating_add(1);
            }
            RecycleAlertDecision::Quiet => {
                self.alerts_suppressed_quiet = self.alerts_suppressed_quiet.saturating_add(1);
            }
        }
    }

    pub fn record_reconfigure(&mut self, event: &TripleBufferReconfigureAuditEvent) {
        self.reconfigures_total = self.reconfigures_total.saturating_add(1);
        if event.is_no_op() {
            self.reconfigures_no_op = self.reconfigures_no_op.saturating_add(1);
        }
        if event.is_relaxed() {
            self.reconfigures_relaxed = self.reconfigures_relaxed.saturating_add(1);
        }
    }

    pub fn record_soak(&mut self, passed: bool) {
        self.soak_runs_total = self.soak_runs_total.saturating_add(1);
        if passed {
            self.soak_runs_passed = self.soak_runs_passed.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(
        id: u64,
        acquires: u64,
        force_recycles: u64,
        last_ts: u64,
        watchdog_active: bool,
    ) -> PaneHealthSnapshot {
        PaneHealthSnapshot {
            pane_id: PaneId(id),
            acquires,
            releases: acquires,
            warnings: 0,
            force_recycles,
            last_force_recycle_ts_ms: last_ts,
            watchdog_active,
        }
    }

    // ----------------------------------------------------------------
    // PaneHealthSnapshot
    // ----------------------------------------------------------------

    #[test]
    fn pane_has_fired_predicate() {
        let s_clean = snap(1, 100, 0, 0, false);
        let s_fired = snap(1, 100, 1, 1_000, false);
        assert!(!s_clean.has_fired());
        assert!(s_fired.has_fired());
    }

    #[test]
    fn pane_ms_since_last_recycle_none_when_never_fired() {
        let s = snap(1, 100, 0, 0, false);
        assert!(s.ms_since_last_recycle(10_000).is_none());
    }

    #[test]
    fn pane_ms_since_last_recycle_some_when_fired() {
        let s = snap(1, 100, 1, 5_000, false);
        assert_eq!(s.ms_since_last_recycle(10_000), Some(5_000));
    }

    #[test]
    fn pane_ms_since_saturates_when_clock_skewed() {
        let s = snap(1, 100, 1, 10_000, false);
        // now < last_ts → saturates at 0.
        assert_eq!(s.ms_since_last_recycle(5_000), Some(0));
    }

    // ----------------------------------------------------------------
    // FleetHealthAggregate
    // ----------------------------------------------------------------

    #[test]
    fn aggregate_empty_fleet() {
        let agg = aggregate_fleet_health(&[]);
        assert_eq!(agg.total_panes, 0);
        assert_eq!(agg.total_acquires, 0);
    }

    #[test]
    fn aggregate_three_clean_panes() {
        let panes = [
            snap(1, 100, 0, 0, false),
            snap(2, 200, 0, 0, false),
            snap(3, 50, 0, 0, false),
        ];
        let agg = aggregate_fleet_health(&panes);
        assert_eq!(agg.total_panes, 3);
        assert_eq!(agg.total_acquires, 350);
        assert_eq!(agg.total_force_recycles, 0);
        assert_eq!(agg.panes_ever_force_recycled, 0);
        assert_eq!(agg.panes_currently_active_watchdog, 0);
    }

    #[test]
    fn aggregate_mixed_health_correctly() {
        let panes = [
            snap(1, 100, 0, 0, false),
            snap(2, 100, 2, 5_000, true),
            snap(3, 100, 1, 8_000, false),
        ];
        let agg = aggregate_fleet_health(&panes);
        assert_eq!(agg.total_panes, 3);
        assert_eq!(agg.total_force_recycles, 3);
        assert_eq!(agg.panes_ever_force_recycled, 2);
        assert_eq!(agg.panes_currently_active_watchdog, 1);
        assert_eq!(agg.max_per_pane_force_recycles, 2);
    }

    #[test]
    fn aggregate_respects_warnings() {
        let mut panes = [snap(1, 100, 0, 0, false), snap(2, 100, 0, 0, false)];
        panes[0].warnings = 5;
        panes[1].warnings = 3;
        let agg = aggregate_fleet_health(&panes);
        assert_eq!(agg.total_warnings, 8);
    }

    // ----------------------------------------------------------------
    // ConsecutiveRecyclePolicy + evaluate_recycle_alert
    // ----------------------------------------------------------------

    #[test]
    fn alert_default_is_2_in_60s() {
        let p = ConsecutiveRecyclePolicy::default();
        assert_eq!(p.alert_after_consecutive, 2);
        assert_eq!(p.window_ms, 60_000);
    }

    #[test]
    fn alert_quiet_with_one_recycle_in_window() {
        let policy = ConsecutiveRecyclePolicy::default();
        let history = [1_000_000];
        let d = evaluate_recycle_alert(&history, policy, 1_005_000);
        assert_eq!(d, RecycleAlertDecision::Quiet);
    }

    #[test]
    fn alert_fires_with_two_recycles_in_window() {
        let policy = ConsecutiveRecyclePolicy::default();
        let history = [1_000_000, 1_010_000];
        let d = evaluate_recycle_alert(&history, policy, 1_020_000);
        assert_eq!(d, RecycleAlertDecision::FireAlert);
    }

    #[test]
    fn alert_quiet_when_oldest_is_outside_window() {
        let policy = ConsecutiveRecyclePolicy::default();
        // Two recycles at 1_000_000 and 1_010_000; now at
        // 2_000_000. Window is 60_000 ms back, so window
        // start = 1_940_000. Both recycles are pre-window.
        let history = [1_000_000, 1_010_000];
        let d = evaluate_recycle_alert(&history, policy, 2_000_000);
        assert_eq!(d, RecycleAlertDecision::Quiet);
    }

    #[test]
    fn alert_zero_threshold_always_quiet() {
        let policy = ConsecutiveRecyclePolicy {
            alert_after_consecutive: 0,
            window_ms: 60_000,
        };
        let history = [1_000_000, 1_010_000, 1_020_000];
        let d = evaluate_recycle_alert(&history, policy, 1_030_000);
        assert_eq!(d, RecycleAlertDecision::Quiet);
    }

    #[test]
    fn alert_higher_threshold_doesnt_fire_with_two() {
        let policy = ConsecutiveRecyclePolicy {
            alert_after_consecutive: 5,
            window_ms: 60_000,
        };
        let history = [1_000_000, 1_010_000];
        let d = evaluate_recycle_alert(&history, policy, 1_020_000);
        assert_eq!(d, RecycleAlertDecision::Quiet);
    }

    #[test]
    fn alert_at_exact_window_boundary_inclusive() {
        let policy = ConsecutiveRecyclePolicy {
            alert_after_consecutive: 2,
            window_ms: 1_000,
        };
        let history = [1_000_000, 1_001_000];
        // window_start = 1_000_000; both samples at boundary.
        let d = evaluate_recycle_alert(&history, policy, 1_001_000);
        assert_eq!(d, RecycleAlertDecision::FireAlert);
    }

    // ----------------------------------------------------------------
    // SoakAcceptanceConfig + SoakSummary
    // ----------------------------------------------------------------

    #[test]
    fn soak_config_defaults_match_bead() {
        let c = SoakAcceptanceConfig::default();
        assert_eq!(c.max_force_recycles, 0);
        assert_eq!(c.min_panes, 200);
        assert_eq!(c.min_elapsed_ms, 86_400_000);
    }

    #[test]
    fn soak_clean_run_meets_acceptance() {
        let s = SoakSummary {
            panes: 200,
            elapsed_ms: 86_400_000,
            total_force_recycles: 0,
            total_warnings: 0,
            max_per_pane_force_recycles: 0,
        };
        let config = SoakAcceptanceConfig::default();
        assert!(s.meets_acceptance(config));
    }

    #[test]
    fn soak_one_force_recycle_fails() {
        let s = SoakSummary {
            panes: 200,
            elapsed_ms: 86_400_000,
            total_force_recycles: 1,
            total_warnings: 0,
            max_per_pane_force_recycles: 1,
        };
        let config = SoakAcceptanceConfig::default();
        assert!(!s.meets_acceptance(config));
    }

    #[test]
    fn soak_under_pane_count_fails() {
        let s = SoakSummary {
            panes: 199,
            elapsed_ms: 86_400_000,
            total_force_recycles: 0,
            total_warnings: 0,
            max_per_pane_force_recycles: 0,
        };
        let config = SoakAcceptanceConfig::default();
        assert!(!s.meets_acceptance(config));
    }

    #[test]
    fn soak_under_elapsed_fails() {
        let s = SoakSummary {
            panes: 200,
            elapsed_ms: 80_000_000, // < 24h
            total_force_recycles: 0,
            total_warnings: 0,
            max_per_pane_force_recycles: 0,
        };
        let config = SoakAcceptanceConfig::default();
        assert!(!s.meets_acceptance(config));
    }

    #[test]
    fn soak_warnings_above_threshold_fail() {
        // 200 panes * 24h * 1 warning/pane/hour = 4800 warnings
        let s = SoakSummary {
            panes: 200,
            elapsed_ms: 86_400_000,
            total_force_recycles: 0,
            total_warnings: 4_800,
            max_per_pane_force_recycles: 0,
        };
        let config = SoakAcceptanceConfig::default();
        assert!(!s.meets_acceptance(config));
    }

    #[test]
    fn soak_warnings_truncation_regression_199_warnings_caught() {
        // Self-review fix (br-ft-ieoxt): with default
        // max=0, even 1 warning fails. Previously
        // total/panes/hours integer-truncated to 0 and let
        // 199 warnings sail past on a 200-pane 24h soak.
        let s = SoakSummary {
            panes: 200,
            elapsed_ms: 86_400_000,
            total_force_recycles: 0,
            total_warnings: 199,
            max_per_pane_force_recycles: 0,
        };
        let config = SoakAcceptanceConfig::default();
        assert!(
            !s.meets_acceptance(config),
            "199 warnings on a clean-soak gate should fail",
        );
    }

    #[test]
    fn soak_warnings_at_exact_max_total_passes() {
        // max=2 per pane per hour; 200 panes; 24h. Max total
        // = 2 * 200 * 24 = 9_600. Substrate fails at 9_601 but
        // accepts 9_600.
        let config = SoakAcceptanceConfig {
            max_warnings_per_pane_per_hour: 2,
            ..SoakAcceptanceConfig::default()
        };
        let exact = SoakSummary {
            panes: 200,
            elapsed_ms: 86_400_000,
            total_force_recycles: 0,
            total_warnings: 9_600,
            max_per_pane_force_recycles: 0,
        };
        let over = SoakSummary {
            panes: 200,
            elapsed_ms: 86_400_000,
            total_force_recycles: 0,
            total_warnings: 9_601,
            max_per_pane_force_recycles: 0,
        };
        assert!(exact.meets_acceptance(config));
        assert!(!over.meets_acceptance(config));
    }

    // ----------------------------------------------------------------
    // ReconfigureAuditEvent
    // ----------------------------------------------------------------

    #[test]
    fn reconfigure_no_op_detected() {
        let e = TripleBufferReconfigureAuditEvent {
            pane_id: PaneId(1),
            prev_warn_ms: 1_000,
            prev_force_ms: 5_000,
            new_warn_ms: 1_000,
            new_force_ms: 5_000,
            timestamp_ms: 0,
            source: ReconfigureSource::OperatorReload,
        };
        assert!(e.is_no_op());
        assert!(!e.is_relaxed());
    }

    #[test]
    fn reconfigure_relaxed_detected() {
        let e = TripleBufferReconfigureAuditEvent {
            pane_id: PaneId(1),
            prev_warn_ms: 1_000,
            prev_force_ms: 5_000,
            new_warn_ms: 1_000,
            new_force_ms: 10_000, // longer
            timestamp_ms: 0,
            source: ReconfigureSource::OperatorReload,
        };
        assert!(!e.is_no_op());
        assert!(e.is_relaxed());
    }

    #[test]
    fn reconfigure_tightened_not_relaxed() {
        let e = TripleBufferReconfigureAuditEvent {
            pane_id: PaneId(1),
            prev_warn_ms: 1_000,
            prev_force_ms: 5_000,
            new_warn_ms: 500,    // tighter
            new_force_ms: 2_000, // tighter
            timestamp_ms: 0,
            source: ReconfigureSource::TestOverride,
        };
        assert!(!e.is_no_op());
        assert!(!e.is_relaxed());
    }

    #[test]
    fn reconfigure_source_label_stable() {
        assert_eq!(ReconfigureSource::OperatorReload.label(), "operator_reload");
        assert_eq!(ReconfigureSource::TestOverride.label(), "test_override");
        assert_eq!(
            ReconfigureSource::AutomaticDegradation.label(),
            "automatic_degradation",
        );
    }

    // ----------------------------------------------------------------
    // FleetHealthTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_record_alert_routes() {
        let mut t = FleetHealthTelemetry::default();
        t.record_alert_decision(RecycleAlertDecision::FireAlert);
        t.record_alert_decision(RecycleAlertDecision::Quiet);
        t.record_alert_decision(RecycleAlertDecision::Quiet);
        assert_eq!(t.alerts_fired, 1);
        assert_eq!(t.alerts_suppressed_quiet, 2);
    }

    #[test]
    fn telemetry_record_reconfigure_routes() {
        let mut t = FleetHealthTelemetry::default();
        let no_op = TripleBufferReconfigureAuditEvent {
            pane_id: PaneId(1),
            prev_warn_ms: 1_000,
            prev_force_ms: 5_000,
            new_warn_ms: 1_000,
            new_force_ms: 5_000,
            timestamp_ms: 0,
            source: ReconfigureSource::OperatorReload,
        };
        let relaxed = TripleBufferReconfigureAuditEvent {
            pane_id: PaneId(1),
            prev_warn_ms: 1_000,
            prev_force_ms: 5_000,
            new_warn_ms: 2_000,
            new_force_ms: 10_000,
            timestamp_ms: 0,
            source: ReconfigureSource::OperatorReload,
        };
        t.record_reconfigure(&no_op);
        t.record_reconfigure(&relaxed);
        assert_eq!(t.reconfigures_total, 2);
        assert_eq!(t.reconfigures_no_op, 1);
        assert_eq!(t.reconfigures_relaxed, 1);
    }

    #[test]
    fn telemetry_record_soak_routes() {
        let mut t = FleetHealthTelemetry::default();
        t.record_soak(true);
        t.record_soak(false);
        t.record_soak(true);
        assert_eq!(t.soak_runs_total, 3);
        assert_eq!(t.soak_runs_passed, 2);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_clean_24h_200_pane_soak_passes() {
        // Bead's 200-pane 24h soak with zero force-recycles.
        let s = SoakSummary {
            panes: 200,
            elapsed_ms: 86_400_000,
            total_force_recycles: 0,
            total_warnings: 0,
            max_per_pane_force_recycles: 0,
        };
        assert!(s.meets_acceptance(SoakAcceptanceConfig::default()));
    }

    #[test]
    fn scenario_crashed_renderer_alert_fires() {
        // Bead's "crashed renderer" pattern: pane forces
        // recycle twice within 60s. Substrate fires alert.
        let policy = ConsecutiveRecyclePolicy::default();
        let now = 1_000_000;
        let history = [now - 30_000, now - 5_000];
        let d = evaluate_recycle_alert(&history, policy, now);
        assert_eq!(d, RecycleAlertDecision::FireAlert);
    }

    #[test]
    fn scenario_one_pane_stuck_others_unaffected() {
        // Bead's "200-pane fleet with one pane's renderer
        // slowed" — the aggregate flags the one pane as
        // active; the rest are clean.
        let mut panes: Vec<PaneHealthSnapshot> =
            (0..200).map(|i| snap(i, 1_000, 0, 0, false)).collect();
        panes[42].watchdog_active = true;
        panes[42].force_recycles = 1;
        panes[42].last_force_recycle_ts_ms = 5_000;
        let agg = aggregate_fleet_health(&panes);
        assert_eq!(agg.total_panes, 200);
        assert_eq!(agg.panes_currently_active_watchdog, 1);
        assert_eq!(agg.panes_ever_force_recycled, 1);
        assert_eq!(agg.total_force_recycles, 1);
    }

    #[test]
    fn scenario_audit_log_records_relaxed_reconfigure() {
        // Operator reloads frankenterm.toml with a relaxed
        // watchdog. Audit event flags is_relaxed; integration
        // logs at warn level so a regression is auditable.
        let event = TripleBufferReconfigureAuditEvent {
            pane_id: PaneId(7),
            prev_warn_ms: 1_000,
            prev_force_ms: 5_000,
            new_warn_ms: 5_000,
            new_force_ms: 30_000,
            timestamp_ms: 1_700_000_000_000,
            source: ReconfigureSource::OperatorReload,
        };
        assert!(event.is_relaxed());
        assert!(!event.is_no_op());
    }
}
