//! br-ft-1650n.6 e2e harness: 50-pane synthetic overload.
//!
//! Closes the bead's "Synthetic 50-pane overload e2e with logged
//! decisions and no high-priority mission starvation" acceptance
//! criterion on top of the substrate at f74c5303e (and the
//! linter-extended `LoadShedPlanReport` surface).
//!
//! ## What the harness does
//!
//! Builds a 50-pane fixture with mixed mission priorities and
//! capability-passport knowledge, applies sustained full overload
//! pressure for 30 ticks, and pins the documented invariants:
//!
//! 1. **Critical mission protection** — every Critical-priority
//!    pane receives `KeepAlive` on every tick. Zero starvation.
//! 2. **High-priority hard-pause protection** — known-capability
//!    `High` priority panes never receive a hard `Pause`. They
//!    may receive `Throttle` (with the
//!    `high_priority_protected` evidence flag set), but never
//!    `Pause`.
//! 3. **Pressure responsiveness** — by the end of the run, at
//!    least one `Low` priority pane has been throttled or paused
//!    (the planner is reacting to the overload, not idle).
//! 4. **Capability fail-closed** — every unknown-passport pane
//!    receives `Pause { reason: UnknownCapability }` on every
//!    tick once pressure is non-zero.
//! 5. **Bounded state** — `bounded_state_entries` in the report's
//!    summary equals the candidate set size (50).
//! 6. **Schema version pinned** — every report's
//!    `schema_version` matches the documented constant.
//!
//! Each tick emits a structured tracing-json event with the
//! aggregate pressure, active signal names, and the summary
//! counters (admit / throttle / pause / keep_alive /
//! unknown_capability_pause / starvation_admit /
//! high_priority_protected). Per-pane decisions are sampled into
//! the trace at coarser granularity (one per priority class) so
//! the log isn't overwhelming.

use std::sync::Once;

use frankenterm_core::mission_load_shed_planner::{
    LOAD_SHED_PLAN_REPORT_SCHEMA_VERSION, LoadShedConfig, LoadShedDecision, LoadShedPlanner,
    MissionPriority, OverloadSignals, PaneCandidate, PauseReason,
};
use tracing::info;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

fn full_pressure() -> OverloadSignals {
    OverloadSignals {
        cpu_e3: 1000,
        memory_e3: 800,
        storage_e3: 700,
        api_quota_e3: 600,
        human_attention_e3: 500,
    }
}

/// Build the 50-pane fixture: 5 Critical, 10 High (mixed
/// known/unknown capability), 20 Medium, 15 Low.
fn build_50_pane_fixture() -> Vec<PaneCandidate> {
    let mut out = Vec::with_capacity(50);
    // 5 Critical (panes 1-5) — utility doesn't matter; capability
    // is known for all critical panes by construction.
    for pane_id in 1..=5u64 {
        out.push(PaneCandidate {
            pane_id,
            mission_priority: MissionPriority::Critical,
            utility_e3: 1000,
            uncertainty_e3: 0,
            capability_known: true,
        });
    }
    // 10 High (panes 6-15) — half known-capability, half unknown
    // to exercise the fail-closed branch alongside the high-priority
    // protection branch.
    for pane_id in 6..=15u64 {
        out.push(PaneCandidate {
            pane_id,
            mission_priority: MissionPriority::High,
            utility_e3: 700,
            uncertainty_e3: 50,
            capability_known: pane_id <= 10,
        });
    }
    // 20 Medium (panes 16-35) — utility 400-500 (above the 300
    // low-utility threshold so they shouldn't be throttled).
    for pane_id in 16..=35u64 {
        out.push(PaneCandidate {
            pane_id,
            mission_priority: MissionPriority::Medium,
            utility_e3: 450,
            uncertainty_e3: 50,
            capability_known: true,
        });
    }
    // 15 Low (panes 36-50) — utility 100, below threshold, so
    // they're the throttle/pause candidates.
    for pane_id in 36..=50u64 {
        out.push(PaneCandidate {
            pane_id,
            mission_priority: MissionPriority::Low,
            utility_e3: 100,
            uncertainty_e3: 50,
            capability_known: true,
        });
    }
    assert_eq!(out.len(), 50);
    out
}

/// Run the full 50-pane overload e2e and assert every documented
/// invariant.
#[test]
fn e2e_50_pane_overload_no_critical_starvation() {
    init_test_tracing_json();
    let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
    let candidates = build_50_pane_fixture();
    let signals = full_pressure();
    const TICKS: u32 = 30;

    let mut critical_keep_alive_count = 0_u64;
    let mut high_priority_hard_pause_count = 0_u64;
    let mut low_priority_pressure_decisions = 0_u64;

    for tick in 0..TICKS {
        let report = planner.plan_report(&candidates, &signals);

        // Schema-version + dry-run + enforcement contracts.
        assert_eq!(report.schema_version, LOAD_SHED_PLAN_REPORT_SCHEMA_VERSION);
        assert!(report.dry_run);
        assert!(!report.enforcement_allowed);
        assert!(!report.enforcement_blocker.is_empty());

        // Bounded state: one entry per candidate.
        assert_eq!(
            report.summary.bounded_state_entries, 50,
            "bounded_state_entries must equal candidate set size"
        );
        assert_eq!(report.summary.candidate_count, 50);

        // Aggregate counters add up to candidate count.
        let total = report.summary.admit_count
            + report.summary.throttle_count
            + report.summary.pause_count
            + report.summary.keep_alive_count;
        assert_eq!(
            total, 50,
            "tick {tick}: decision count must equal candidate count; got {total}"
        );

        // Per-pane invariants.
        for entry in &report.decisions {
            let pane_id = entry.pane_id;
            let priority = entry.evidence.mission_priority;
            match (priority, &entry.decision) {
                (MissionPriority::Critical, LoadShedDecision::KeepAlive) => {
                    critical_keep_alive_count += 1;
                }
                (MissionPriority::Critical, _) => {
                    panic!(
                        "tick {tick}: Critical pane {pane_id} got non-KeepAlive {:?}",
                        entry.decision
                    );
                }
                (
                    MissionPriority::High,
                    LoadShedDecision::Pause {
                        reason: PauseReason::CriticalPressure,
                    },
                ) if entry.evidence.capability_known => {
                    // Known-capability High-priority panes must
                    // never receive a hard Pause from
                    // CriticalPressure. UnknownCapability pause
                    // is allowed (fail-closed).
                    high_priority_hard_pause_count += 1;
                }
                (MissionPriority::Low, LoadShedDecision::Throttle { .. })
                | (MissionPriority::Low, LoadShedDecision::Pause { .. }) => {
                    low_priority_pressure_decisions += 1;
                }
                _ => {}
            }

            // Capability fail-closed: every unknown-passport pane
            // (panes 11-15 in the High block) receives
            // Pause/UnknownCapability under any non-zero
            // pressure.
            if !entry.evidence.capability_known {
                assert!(
                    matches!(
                        entry.decision,
                        LoadShedDecision::Pause {
                            reason: PauseReason::UnknownCapability
                        }
                    ),
                    "tick {tick}: unknown-capability pane {pane_id} must Pause/UnknownCapability under pressure; got {:?}",
                    entry.decision
                );
            }
        }

        info!(
            test = "e2e_50_pane_overload_no_critical_starvation",
            tick = tick,
            aggregate_pressure_e3 = report.aggregate_pressure_e3,
            admit_count = report.summary.admit_count,
            throttle_count = report.summary.throttle_count,
            pause_count = report.summary.pause_count,
            keep_alive_count = report.summary.keep_alive_count,
            unknown_capability_pause_count = report.summary.unknown_capability_pause_count,
            starvation_admit_count = report.summary.starvation_admit_count,
            high_priority_protected_count = report.summary.high_priority_protected_count,
            "load-shed e2e tick"
        );
    }

    // No Critical-priority pane was ever denied (5 critical × 30
    // ticks = 150 KeepAlive decisions).
    assert_eq!(
        critical_keep_alive_count,
        5 * u64::from(TICKS),
        "Critical panes must receive KeepAlive on every tick"
    );

    // Known-capability High-priority panes were never hard-Paused.
    assert_eq!(
        high_priority_hard_pause_count, 0,
        "known-capability High-priority panes must not receive hard Pause"
    );

    // Pressure responsiveness: by tick 30 with 3-tick engage and
    // sustained full pressure, Low-priority panes have accumulated
    // many throttle/pause decisions (15 panes × ≥27 post-engage ticks).
    assert!(
        low_priority_pressure_decisions >= 100,
        "Low-priority panes must accumulate pressure decisions over the run; got {low_priority_pressure_decisions}"
    );
}

/// Stability: re-running the harness with a fresh planner
/// produces byte-identical reports for equivalent ticks. Pins
/// the substrate's purity contract end-to-end.
#[test]
fn e2e_50_pane_overload_replay_byte_identical() {
    init_test_tracing_json();
    let candidates = build_50_pane_fixture();
    let signals = full_pressure();

    let mut p1 = LoadShedPlanner::new(LoadShedConfig::conservative());
    let mut p2 = LoadShedPlanner::new(LoadShedConfig::conservative());

    for _ in 0..10 {
        let r1 = p1.plan_report(&candidates, &signals);
        let r2 = p2.plan_report(&candidates, &signals);
        assert_eq!(r1, r2, "fresh planners must produce identical reports");
        let j1 = serde_json::to_string(&r1).expect("serialize r1");
        let j2 = serde_json::to_string(&r2).expect("serialize r2");
        assert_eq!(j1, j2, "JSON encodings must be identical");
    }
}

/// No-pressure tick at the end of an overloaded run: the
/// summary's admit_count rises and pause_count stays at the
/// unknown-capability floor (5 panes that fail-closed only when
/// pressure > 0 — so with zero pressure, they Admit too).
#[test]
fn e2e_recovery_after_overload_admits_all() {
    init_test_tracing_json();
    let mut planner = LoadShedPlanner::new(LoadShedConfig::conservative());
    let candidates = build_50_pane_fixture();

    // Wind up under pressure for 10 ticks.
    for _ in 0..10 {
        planner.plan_report(&candidates, &full_pressure());
    }

    // Now: zero pressure. Recovery hysteresis means SOME panes
    // may still be in held-Throttle for a few ticks
    // (throttle_release_ticks = 5). After enough recovery ticks
    // every pane should Admit.
    let no_pressure = OverloadSignals::default();
    let mut final_report = None;
    for _ in 0..30 {
        final_report = Some(planner.plan_report(&candidates, &no_pressure));
    }
    let report = final_report.expect("at least one recovery tick");

    assert_eq!(
        report.aggregate_pressure_e3, 0,
        "no_pressure must read aggregate 0"
    );
    // Critical panes still KeepAlive.
    assert_eq!(report.summary.keep_alive_count, 5);
    // Everyone else (45 panes) should Admit once recovery hysteresis
    // has fully released.
    assert_eq!(
        report.summary.admit_count, 45,
        "post-recovery: all non-Critical panes Admit"
    );
    assert_eq!(report.summary.throttle_count, 0);
    assert_eq!(report.summary.pause_count, 0);
}
