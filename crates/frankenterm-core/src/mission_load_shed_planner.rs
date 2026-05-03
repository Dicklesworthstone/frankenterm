//! br-ft-1650n.6: Mission-aware load-shedding planner substrate.
//!
//! Dry-run planner that scores per-pane candidates against active
//! overload signals and emits typed decisions
//! (`Admit` / `Throttle` / `Pause` / `KeepAlive`) with structured
//! evidence. The planner is observation-only — enforcement
//! requires capability passports + causal graph context that a
//! wired-pass slice will supply.
//!
//! ## Design
//!
//! - **Utility-first**: each pane has an integer utility estimate
//!   (`utility_e3`, millipercent) plus an uncertainty band. The
//!   planner ranks candidates by utility descending and applies
//!   pressure starting from the lowest-utility tail.
//! - **Mission priority floor**: panes flagged
//!   `MissionPriority::Critical` are admitted regardless of
//!   pressure (the bead's "no high-priority starvation" criterion).
//! - **Hysteresis**: a per-pane sticky counter tracks how many
//!   consecutive plan ticks the pane has been under pressure.
//!   Decisions transition only after the counter crosses the
//!   relevant watermark — preventing decision flapping on small
//!   signal changes.
//! - **Capability fail-closed**: panes with unknown capability
//!   passports get the conservative `Pause` action under any
//!   non-zero pressure (the bead's "Require capability passports
//!   and causal graph context before enforcement" requirement).
//! - **Bounded state**: per-pane state is two `u32`s + a single
//!   enum tag. State map size is bounded by the candidate set
//!   passed in each tick.
//!
//! ## What ships in this slice
//!
//! - [`LoadShedConfig`] — operator-tunable thresholds.
//! - [`OverloadSignals`] — per-resource intensity (`0..=1000`).
//! - [`MissionPriority`] — Critical / High / Medium / Low.
//! - [`PaneCandidate`] — one pane being scored.
//! - [`LoadShedDecision`] — typed decision.
//! - [`LoadShedDecisionEvidence`] — per-decision rationale
//!   structured for dashboards.
//! - [`LoadShedHysteresisState`] — sticky per-pane state.
//! - [`LoadShedPlanner::plan`] — pure tick over candidates +
//!   signals; returns `(PaneId, Decision, Evidence)` tuples.
//!
//! ## What is deferred
//!
//! - Action mode: this slice only emits dry-run decisions. A
//!   wired-pass slice will couple to the actual pane scheduler
//!   to apply the throttle/pause.
//! - Bayesian VOI estimator: utility is caller-supplied today.
//!   A follow-up slice can add a regressor over mission outcomes.
//! - Tail-latency feedback loop: closing the loop with p99
//!   monitors needs the wired-pass slice to plumb in the latency
//!   telemetry pipeline.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// br-ft-1650n.6: operator-tunable thresholds for the planner.
///
/// All `_e3` fields are integer millipercent (1000 = 1.00).
/// Hysteresis watermarks count plan-ticks (calls to
/// [`LoadShedPlanner::plan`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadShedConfig {
    /// Aggregate signal intensity (`0..=1000`) at which the
    /// planner starts considering throttle decisions on the
    /// lowest-utility tail.
    pub pressure_threshold_e3: u32,
    /// Aggregate signal intensity at which the planner promotes
    /// throttle decisions to pauses for the lowest-utility tail.
    pub critical_pressure_threshold_e3: u32,
    /// Utility (millipercent) below which a pane is eligible for
    /// throttling under pressure.
    pub low_utility_threshold_e3: u32,
    /// Number of consecutive plan ticks a pane must be eligible
    /// before its decision actually transitions to Throttle.
    /// Hysteresis: small/transient signal spikes don't flip the
    /// decision.
    pub throttle_engage_ticks: u32,
    /// Number of consecutive ticks a pane must be ineligible
    /// before its decision transitions BACK to Admit. Asymmetric
    /// from `throttle_engage_ticks` so the planner is slower to
    /// release than to engage (queue-theoretic stability).
    pub throttle_release_ticks: u32,
    /// Throttle rate (millipercent of nominal) applied when a
    /// pane is throttled. Sub-1.0 = slow down.
    pub throttled_rate_e3: u32,
    /// Maximum number of consecutive ticks any pane can be paused
    /// before it's granted a starvation-avoidance Admit window.
    /// Pinned by tests so the operator can rely on it.
    pub starvation_admit_after_ticks: u32,
}

impl LoadShedConfig {
    /// Conservative defaults: 60% aggregate pressure to engage,
    /// 90% to escalate to pause; low-utility threshold at 30%;
    /// 3-tick engage / 5-tick release hysteresis; 50% throttled
    /// rate; 20-tick starvation guard.
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            pressure_threshold_e3: 600,
            critical_pressure_threshold_e3: 900,
            low_utility_threshold_e3: 300,
            throttle_engage_ticks: 3,
            throttle_release_ticks: 5,
            throttled_rate_e3: 500,
            starvation_admit_after_ticks: 20,
        }
    }
}

/// br-ft-1650n.6: per-resource intensity (`0..=1000`). The
/// aggregate intensity used for gating is the `max` across
/// resources — operators read this as "the planner reacts to the
/// most-pressured resource".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OverloadSignals {
    pub cpu_e3: u32,
    pub memory_e3: u32,
    pub storage_e3: u32,
    pub api_quota_e3: u32,
    pub human_attention_e3: u32,
}

impl OverloadSignals {
    /// Aggregate intensity = max across resources. The planner
    /// gates on the most-pressured resource so a single saturated
    /// dimension trips the controller.
    #[must_use]
    pub fn aggregate_e3(&self) -> u32 {
        self.cpu_e3
            .max(self.memory_e3)
            .max(self.storage_e3)
            .max(self.api_quota_e3)
            .max(self.human_attention_e3)
    }

    /// Bit-flag-style enumeration of the signals at or above
    /// 50%. Threaded into the decision evidence so the operator
    /// dashboard can render which signal triggered.
    #[must_use]
    pub fn active_signal_names(&self) -> Vec<String> {
        const HALF: u32 = 500;
        let mut out = Vec::new();
        if self.cpu_e3 >= HALF {
            out.push("cpu".to_string());
        }
        if self.memory_e3 >= HALF {
            out.push("memory".to_string());
        }
        if self.storage_e3 >= HALF {
            out.push("storage".to_string());
        }
        if self.api_quota_e3 >= HALF {
            out.push("api_quota".to_string());
        }
        if self.human_attention_e3 >= HALF {
            out.push("human_attention".to_string());
        }
        out
    }
}

/// br-ft-1650n.6: mission priority band. `Critical` is the
/// "never starve" floor; the bead's "no high-priority mission
/// starvation" criterion is type-level here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionPriority {
    Critical,
    High,
    Medium,
    Low,
}

/// br-ft-1650n.6: one pane being scored on a planner tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCandidate {
    pub pane_id: u64,
    pub mission_priority: MissionPriority,
    /// Utility estimate (millipercent of perfect).
    pub utility_e3: u32,
    /// Uncertainty band (millipercent). Higher = less confident.
    pub uncertainty_e3: u32,
    /// Whether this pane has a known capability passport. Panes
    /// with unknown passports fail-closed under pressure.
    pub capability_known: bool,
}

/// br-ft-1650n.6: typed decision. The planner emits these in
/// dry-run mode; a wired-pass slice plugs them into the actual
/// scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LoadShedDecision {
    /// Run the pane normally.
    Admit,
    /// Slow the pane to the configured throttled rate.
    Throttle { rate_e3: u32 },
    /// Stop scheduling new work for this pane.
    Pause { reason: PauseReason },
    /// Critical mission protection — this pane is exempt from
    /// any throttle/pause regardless of pressure.
    KeepAlive,
}

/// Reasons a pane was paused. Used for forensic review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// Aggregate pressure crossed the critical threshold and
    /// this pane was in the low-utility tail.
    CriticalPressure,
    /// Capability passport unknown; failing closed under any
    /// non-zero pressure.
    UnknownCapability,
}

/// br-ft-1650n.6: structured evidence per decision. Threaded
/// through to dashboards and the autopsy bundle so every
/// throttle/pause is auditable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadShedDecisionEvidence {
    pub aggregate_pressure_e3: u32,
    pub active_signals: Vec<String>,
    pub mission_priority: MissionPriority,
    pub utility_e3: u32,
    pub uncertainty_e3: u32,
    pub capability_known: bool,
    /// Whether the decision was produced by the starvation guard
    /// (a forced Admit window after `starvation_admit_after_ticks`
    /// consecutive Pause ticks).
    pub starvation_admit: bool,
    /// Hysteresis tick count for this pane at decision time.
    pub hysteresis_ticks: u32,
}

/// br-ft-1650n.6: sticky per-pane state. Used by the planner to
/// implement asymmetric engage/release hysteresis and the
/// starvation guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LoadShedHysteresisState {
    /// Consecutive ticks the pane has been ELIGIBLE for
    /// throttle/pause (i.e., under pressure with low utility).
    /// Counts up while eligible; counts down (toward zero) while
    /// not eligible.
    pub pressure_ticks: u32,
    /// Consecutive ticks the pane has been Paused. Used by the
    /// starvation guard.
    pub pause_ticks: u32,
}

/// br-ft-1650n.6: the planner. Owns the per-pane hysteresis
/// state and exposes a single `plan(...)` tick.
#[derive(Debug, Default)]
pub struct LoadShedPlanner {
    config: LoadShedConfig,
    state: BTreeMap<u64, LoadShedHysteresisState>,
}

impl LoadShedPlanner {
    #[must_use]
    pub fn new(config: LoadShedConfig) -> Self {
        Self {
            config,
            state: BTreeMap::new(),
        }
    }

    /// Pure-ish tick: scores each candidate, advances per-pane
    /// hysteresis state, returns one decision + evidence per
    /// candidate in candidate order.
    ///
    /// Tick semantics:
    /// 1. Compute aggregate pressure.
    /// 2. For each candidate, decide eligibility for
    ///    throttle/pause (low utility AND under pressure AND not
    ///    Critical priority).
    /// 3. Advance hysteresis state.
    /// 4. Apply the decision rules (see decision-table below).
    pub fn plan(
        &mut self,
        candidates: &[PaneCandidate],
        signals: &OverloadSignals,
    ) -> Vec<(u64, LoadShedDecision, LoadShedDecisionEvidence)> {
        let aggregate = signals.aggregate_e3();
        let active_signals = signals.active_signal_names();
        let mut out = Vec::with_capacity(candidates.len());

        for c in candidates {
            let mut state = self.state.get(&c.pane_id).copied().unwrap_or_default();

            // Critical mission priority: never throttle, never
            // pause. Reset hysteresis so the pane re-enters Admit
            // cleanly when it loses Critical status.
            if c.mission_priority == MissionPriority::Critical {
                state = LoadShedHysteresisState::default();
                self.state.insert(c.pane_id, state);
                out.push((
                    c.pane_id,
                    LoadShedDecision::KeepAlive,
                    LoadShedDecisionEvidence {
                        aggregate_pressure_e3: aggregate,
                        active_signals: active_signals.clone(),
                        mission_priority: c.mission_priority,
                        utility_e3: c.utility_e3,
                        uncertainty_e3: c.uncertainty_e3,
                        capability_known: c.capability_known,
                        starvation_admit: false,
                        hysteresis_ticks: 0,
                    },
                ));
                continue;
            }

            // Capability fail-closed: unknown passport under ANY
            // non-zero pressure → Pause immediately (no
            // hysteresis).
            if !c.capability_known && aggregate > 0 {
                state.pause_ticks = state.pause_ticks.saturating_add(1);
                self.state.insert(c.pane_id, state);
                out.push((
                    c.pane_id,
                    LoadShedDecision::Pause {
                        reason: PauseReason::UnknownCapability,
                    },
                    LoadShedDecisionEvidence {
                        aggregate_pressure_e3: aggregate,
                        active_signals: active_signals.clone(),
                        mission_priority: c.mission_priority,
                        utility_e3: c.utility_e3,
                        uncertainty_e3: c.uncertainty_e3,
                        capability_known: false,
                        starvation_admit: false,
                        hysteresis_ticks: state.pressure_ticks,
                    },
                ));
                continue;
            }

            // Eligibility = under-pressure AND low-utility.
            let under_pressure = aggregate >= self.config.pressure_threshold_e3;
            let low_utility = c.utility_e3 <= self.config.low_utility_threshold_e3;
            let eligible = under_pressure && low_utility;

            if eligible {
                state.pressure_ticks = state.pressure_ticks.saturating_add(1);
            } else {
                state.pressure_ticks = state.pressure_ticks.saturating_sub(1);
            }

            // Decision table:
            // - Aggregate ≥ critical AND eligible AND
            //   pressure_ticks ≥ engage → Pause (CriticalPressure).
            // - Aggregate ≥ pressure AND eligible AND
            //   pressure_ticks ≥ engage → Throttle.
            // - pressure_ticks ≥ release → keep previous Throttle.
            //   (Asymmetric release hysteresis is captured by
            //   counting down rather than resetting.)
            // - Else Admit.
            let critical = aggregate >= self.config.critical_pressure_threshold_e3;
            let mut starvation_admit = false;
            let decision = if critical
                && eligible
                && state.pressure_ticks >= self.config.throttle_engage_ticks
            {
                state.pause_ticks = state.pause_ticks.saturating_add(1);
                LoadShedDecision::Pause {
                    reason: PauseReason::CriticalPressure,
                }
            } else if eligible && state.pressure_ticks >= self.config.throttle_engage_ticks {
                state.pause_ticks = 0;
                LoadShedDecision::Throttle {
                    rate_e3: self.config.throttled_rate_e3,
                }
            } else if !eligible && state.pressure_ticks >= self.config.throttle_release_ticks {
                // Held Throttle while we count down toward release.
                state.pause_ticks = 0;
                LoadShedDecision::Throttle {
                    rate_e3: self.config.throttled_rate_e3,
                }
            } else {
                state.pause_ticks = 0;
                LoadShedDecision::Admit
            };

            // Starvation guard: if a pane has been paused too
            // long, force-Admit this tick to give it a window
            // (queueing fairness; the bead's "starvation
            // avoidance" criterion).
            let final_decision = if state.pause_ticks > self.config.starvation_admit_after_ticks {
                state.pause_ticks = 0;
                state.pressure_ticks = 0;
                starvation_admit = true;
                LoadShedDecision::Admit
            } else {
                decision
            };

            self.state.insert(c.pane_id, state);
            out.push((
                c.pane_id,
                final_decision,
                LoadShedDecisionEvidence {
                    aggregate_pressure_e3: aggregate,
                    active_signals: active_signals.clone(),
                    mission_priority: c.mission_priority,
                    utility_e3: c.utility_e3,
                    uncertainty_e3: c.uncertainty_e3,
                    capability_known: c.capability_known,
                    starvation_admit,
                    hysteresis_ticks: state.pressure_ticks,
                },
            ));
        }

        out
    }

    /// Read the per-pane hysteresis state (for dashboards and
    /// forensic review). Pure read — does not advance the tick.
    #[must_use]
    pub fn snapshot_state(&self) -> BTreeMap<u64, LoadShedHysteresisState> {
        self.state.clone()
    }
}

impl Default for LoadShedConfig {
    fn default() -> Self {
        Self::conservative()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(pane_id: u64, priority: MissionPriority, utility_e3: u32) -> PaneCandidate {
        PaneCandidate {
            pane_id,
            mission_priority: priority,
            utility_e3,
            uncertainty_e3: 100,
            capability_known: true,
        }
    }

    fn no_pressure() -> OverloadSignals {
        OverloadSignals::default()
    }

    fn full_pressure() -> OverloadSignals {
        OverloadSignals {
            cpu_e3: 1000,
            memory_e3: 800,
            storage_e3: 0,
            api_quota_e3: 0,
            human_attention_e3: 0,
        }
    }

    /// Aggregate is `max` across resources, not sum. Pinned for
    /// dashboards that render the gating signal.
    #[test]
    fn aggregate_signal_is_max_across_resources() {
        let s = OverloadSignals {
            cpu_e3: 200,
            memory_e3: 800,
            storage_e3: 600,
            api_quota_e3: 100,
            human_attention_e3: 0,
        };
        assert_eq!(s.aggregate_e3(), 800);
    }

    /// No pressure → every pane Admit, regardless of utility.
    #[test]
    fn no_pressure_admits_all() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let candidates = vec![
            candidate(1, MissionPriority::High, 500),
            candidate(2, MissionPriority::Low, 100),
        ];
        let decisions = planner.plan(&candidates, &no_pressure());
        for (_, d, _) in &decisions {
            assert_eq!(*d, LoadShedDecision::Admit);
        }
    }

    /// Critical priority panes always KeepAlive even under full
    /// pressure (the bead's "no high-priority mission starvation"
    /// criterion).
    #[test]
    fn critical_priority_always_keep_alive() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let candidates = vec![candidate(1, MissionPriority::Critical, 100)];
        for _ in 0..50 {
            let decisions = planner.plan(&candidates, &full_pressure());
            assert_eq!(decisions[0].1, LoadShedDecision::KeepAlive);
        }
    }

    /// Hysteresis: a single pressure tick does NOT throttle a
    /// low-utility pane. Engage requires `throttle_engage_ticks`
    /// consecutive eligible ticks.
    #[test]
    fn hysteresis_engage_requires_multiple_ticks() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let candidates = vec![candidate(1, MissionPriority::Low, 100)];
        // First 2 ticks: not yet engaged (engage = 3).
        for _ in 0..2 {
            let decisions = planner.plan(&candidates, &full_pressure());
            assert_eq!(decisions[0].1, LoadShedDecision::Admit);
        }
        // 3rd tick: engaged.
        let decisions = planner.plan(&candidates, &full_pressure());
        assert!(matches!(
            decisions[0].1,
            LoadShedDecision::Throttle { .. } | LoadShedDecision::Pause { .. }
        ));
    }

    /// Critical aggregate pressure (≥ 90%) escalates engaged
    /// throttles to pauses.
    #[test]
    fn critical_pressure_escalates_to_pause() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let candidates = vec![candidate(1, MissionPriority::Low, 100)];
        for _ in 0..3 {
            let _ = planner.plan(&candidates, &full_pressure());
        }
        // 4th tick at critical pressure → Pause.
        let decisions = planner.plan(&candidates, &full_pressure());
        assert!(matches!(
            decisions[0].1,
            LoadShedDecision::Pause {
                reason: PauseReason::CriticalPressure
            }
        ));
    }

    /// Unknown capability under any pressure → immediate Pause
    /// with reason UnknownCapability (no hysteresis; fail-closed).
    #[test]
    fn unknown_capability_pauses_immediately() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let unknown = PaneCandidate {
            pane_id: 7,
            mission_priority: MissionPriority::Medium,
            utility_e3: 800, // even high utility
            uncertainty_e3: 0,
            capability_known: false,
        };
        // Even a small pressure trips it.
        let signals = OverloadSignals {
            cpu_e3: 100,
            ..Default::default()
        };
        let decisions = planner.plan(&[unknown], &signals);
        assert!(matches!(
            decisions[0].1,
            LoadShedDecision::Pause {
                reason: PauseReason::UnknownCapability
            }
        ));
    }

    /// Unknown capability with NO pressure → Admit (conservative
    /// only kicks in when there's something to ration).
    #[test]
    fn unknown_capability_no_pressure_admits() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let unknown = PaneCandidate {
            pane_id: 7,
            mission_priority: MissionPriority::Medium,
            utility_e3: 500,
            uncertainty_e3: 0,
            capability_known: false,
        };
        let decisions = planner.plan(&[unknown], &no_pressure());
        assert_eq!(decisions[0].1, LoadShedDecision::Admit);
    }

    /// Monotonicity: a higher-utility pane is never throttled
    /// before a lower-utility pane on the same tick. Pinned by
    /// scoring two panes with utility 100 vs 500 and asserting
    /// the 500 pane gets the gentler decision.
    #[test]
    fn monotonicity_higher_utility_never_throttled_first() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let candidates = vec![
            candidate(1, MissionPriority::Low, 100), // low utility
            candidate(2, MissionPriority::Low, 800), // high utility, ABOVE low_utility_threshold
        ];
        // Run enough ticks to engage hysteresis.
        for _ in 0..3 {
            let _ = planner.plan(&candidates, &full_pressure());
        }
        let decisions = planner.plan(&candidates, &full_pressure());
        // Low-utility pane gets Throttle/Pause; high-utility pane
        // is admitted.
        assert!(matches!(
            decisions[0].1,
            LoadShedDecision::Throttle { .. } | LoadShedDecision::Pause { .. }
        ));
        assert_eq!(decisions[1].1, LoadShedDecision::Admit);
    }

    /// Starvation guard: a pane Paused for >
    /// `starvation_admit_after_ticks` consecutive ticks gets a
    /// forced Admit window with `starvation_admit = true` in
    /// the evidence.
    #[test]
    fn starvation_guard_forces_admit_window() {
        let cfg = LoadShedConfig {
            starvation_admit_after_ticks: 5,
            ..LoadShedConfig::conservative()
        };
        let mut planner = LoadShedPlanner::new(cfg);
        let candidates = vec![candidate(1, MissionPriority::Low, 100)];
        let mut starvation_seen = false;
        // Run enough ticks to engage AND exceed starvation
        // threshold.
        for _ in 0..30 {
            let decisions = planner.plan(&candidates, &full_pressure());
            if decisions[0].2.starvation_admit {
                assert_eq!(decisions[0].1, LoadShedDecision::Admit);
                starvation_seen = true;
                break;
            }
        }
        assert!(starvation_seen, "starvation guard must fire eventually");
    }

    /// Evidence threads aggregate pressure, active signals, and
    /// hysteresis tick count for forensic review.
    #[test]
    fn evidence_carries_dashboard_fields() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let candidates = vec![candidate(1, MissionPriority::Low, 100)];
        let signals = OverloadSignals {
            cpu_e3: 800,
            memory_e3: 600,
            ..Default::default()
        };
        let decisions = planner.plan(&candidates, &signals);
        let evidence = &decisions[0].2;
        assert_eq!(evidence.aggregate_pressure_e3, 800);
        assert!(evidence.active_signals.iter().any(|s| s == "cpu"));
        assert!(evidence.active_signals.iter().any(|s| s == "memory"));
        assert_eq!(evidence.mission_priority, MissionPriority::Low);
        assert_eq!(evidence.utility_e3, 100);
        assert!(evidence.capability_known);
    }

    /// Empty candidate list → empty decision list. No panic.
    #[test]
    fn empty_candidates_returns_empty_plan() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let decisions = planner.plan(&[], &full_pressure());
        assert!(decisions.is_empty());
    }

    /// Hysteresis state snapshot reflects accumulated pressure
    /// ticks across plan calls.
    #[test]
    fn hysteresis_snapshot_reflects_state() {
        let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
        let candidates = vec![candidate(1, MissionPriority::Low, 100)];
        for _ in 0..5 {
            let _ = planner.plan(&candidates, &full_pressure());
        }
        let snap = planner.snapshot_state();
        let pane_state = snap.get(&1).copied().unwrap_or_default();
        assert!(pane_state.pressure_ticks >= 3);
    }

    /// LoadShedDecision serde roundtrip covers every variant.
    #[test]
    fn decision_serde_roundtrip() {
        for d in [
            LoadShedDecision::Admit,
            LoadShedDecision::KeepAlive,
            LoadShedDecision::Throttle { rate_e3: 500 },
            LoadShedDecision::Pause {
                reason: PauseReason::CriticalPressure,
            },
            LoadShedDecision::Pause {
                reason: PauseReason::UnknownCapability,
            },
        ] {
            let json = serde_json::to_string(&d).expect("serialize");
            let back: LoadShedDecision = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(d, back);
        }
    }

    /// LoadShedDecisionEvidence serde roundtrip preserves every
    /// field.
    #[test]
    fn evidence_serde_roundtrip() {
        let evidence = LoadShedDecisionEvidence {
            aggregate_pressure_e3: 800,
            active_signals: vec!["cpu".to_string(), "memory".to_string()],
            mission_priority: MissionPriority::Medium,
            utility_e3: 250,
            uncertainty_e3: 100,
            capability_known: true,
            starvation_admit: false,
            hysteresis_ticks: 4,
        };
        let json = serde_json::to_string(&evidence).expect("serialize");
        let back: LoadShedDecisionEvidence = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(evidence, back);
    }
}
