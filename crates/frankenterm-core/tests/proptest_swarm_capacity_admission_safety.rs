//! Safety-invariant property tests for the swarm-capacity workload
//! admission planner in `frankenterm_core::runtime_telemetry`. These build
//! arbitrary signal/evidence combinations through the public input API and
//! drive the real `plan_swarm_capacity_workload_admission` decision path
//! (not just the curated dry-run examples), asserting the core admission
//! SAFETY invariants:
//!
//! - units are admitted only on the `Admit` action (all-or-nothing);
//! - a degraded/unavailable kill switch forbids admitting any units;
//! - planning never executes side effects.

use proptest::prelude::*;

use frankenterm_core::runtime_telemetry::{
    plan_swarm_capacity_workload_admission, HealthTier, SwarmCapacityAdmissionAction,
    SwarmCapacityAgentWorkloadClass, SwarmCapacityWorkloadAdmissionInput,
    SwarmCapacityWorkloadAdmissionSignal, SwarmCapacityWorkloadEvidenceState,
    SwarmCapacityWorkloadSignalKind,
};

fn arb_evidence() -> impl Strategy<Value = SwarmCapacityWorkloadEvidenceState> {
    prop::sample::select(vec![
        SwarmCapacityWorkloadEvidenceState::Measured,
        SwarmCapacityWorkloadEvidenceState::Inferred,
        SwarmCapacityWorkloadEvidenceState::Simulated,
        SwarmCapacityWorkloadEvidenceState::Stale,
        SwarmCapacityWorkloadEvidenceState::Redacted,
        SwarmCapacityWorkloadEvidenceState::Contradictory,
        SwarmCapacityWorkloadEvidenceState::Unavailable,
    ])
}

fn arb_tier() -> impl Strategy<Value = HealthTier> {
    prop::sample::select(vec![
        HealthTier::Green,
        HealthTier::Yellow,
        HealthTier::Red,
        HealthTier::Black,
    ])
}

fn arb_class() -> impl Strategy<Value = SwarmCapacityAgentWorkloadClass> {
    let all = SwarmCapacityAgentWorkloadClass::ALL.to_vec();
    prop::sample::select(all)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(384))]

    /// Over any signal/evidence/tier combination, the admission decision
    /// obeys the safety invariants: admit is all-or-nothing and only on the
    /// Admit action; a kill switch admits nothing; planning is side-effect free.
    #[test]
    fn admission_decision_respects_safety_invariants(
        class in arb_class(),
        pane_scale in 0u32..1000,
        e0 in arb_evidence(), t0 in arb_tier(),
        e1 in arb_evidence(), t1 in arb_tier(),
        e2 in arb_evidence(), t2 in arb_tier(),
        e3 in arb_evidence(), t3 in arb_tier(),
    ) {
        let mut input = SwarmCapacityWorkloadAdmissionInput::new("safety.test", pane_scale, class);
        input.signals.context_horizon = SwarmCapacityWorkloadAdmissionSignal::new(
            SwarmCapacityWorkloadSignalKind::ContextHorizon, e0, t0);
        input.signals.blocker_radar = SwarmCapacityWorkloadAdmissionSignal::new(
            SwarmCapacityWorkloadSignalKind::BlockerRadar, e1, t1);
        input.signals.herd_wave = SwarmCapacityWorkloadAdmissionSignal::new(
            SwarmCapacityWorkloadSignalKind::HerdWave, e2, t2);
        input.signals.resource_pressure = SwarmCapacityWorkloadAdmissionSignal::new(
            SwarmCapacityWorkloadSignalKind::ResourcePressure, e3, t3);

        let plan = plan_swarm_capacity_workload_admission(1_700_000_000_000, "test", &[input]);
        prop_assert_eq!(plan.decisions.len(), 1);
        let d = &plan.decisions[0];

        // Units are admitted only on the Admit action.
        if d.admitted_units > 0 {
            prop_assert_eq!(d.action, SwarmCapacityAdmissionAction::Admit,
                "units admitted under a non-Admit action: {:?}", d.action);
        }
        // Admission is all-or-nothing: either zero or exactly the request.
        prop_assert!(
            d.admitted_units == 0 || d.admitted_units == d.requested_units,
            "admitted_units {} must be 0 or requested_units {}",
            d.admitted_units, d.requested_units
        );
        // A kill switch must forbid admitting any units.
        if d.kill_switch_active {
            prop_assert_eq!(d.admitted_units, 0,
                "kill switch active but {} units admitted", d.admitted_units);
        }
        // Planning never executes side effects.
        prop_assert!(!d.side_effects_executed, "planning must be side-effect free");
        prop_assert!(!plan.side_effects_executed);
    }
}
