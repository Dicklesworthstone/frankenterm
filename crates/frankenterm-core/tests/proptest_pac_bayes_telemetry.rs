//! Property-based tests for PAC-Bayesian backpressure telemetry counters (ft-3kxe.29).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. observations tracks observe() calls
//! 3. frame_drops_observed tracks frame_dropped=true observations
//! 4. throttle_activations tracks severity > 0.01 observations
//! 5. starvation_guards tracks starvation guard activations
//! 6. posterior_updates tracks post-warmup posterior updates
//! 7. pane_resets tracks reset_pane() calls
//! 8. full_resets tracks reset() calls
//! 9. Serde roundtrip for snapshot
//! 10. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::aegis_backpressure::{
    ExternalCauseEvidence, PacBayesBackpressure, PacBayesConfig, PacBayesTelemetrySnapshot,
    QueueObservation,
};

// =============================================================================
// Helpers
// =============================================================================

fn default_controller() -> PacBayesBackpressure {
    PacBayesBackpressure::with_defaults()
}

fn obs(pane_id: u64, fill_ratio: f64, frame_dropped: bool) -> QueueObservation {
    QueueObservation {
        pane_id,
        fill_ratio,
        frame_dropped,
        external_cause: None,
    }
}

fn obs_with_external(
    pane_id: u64,
    fill_ratio: f64,
    frame_dropped: bool,
    evidence: ExternalCauseEvidence,
) -> QueueObservation {
    QueueObservation {
        pane_id,
        fill_ratio,
        frame_dropped,
        external_cause: Some(evidence),
    }
}

fn high_external_cause() -> ExternalCauseEvidence {
    ExternalCauseEvidence {
        system_load: 10.0,
        other_panes_slow_fraction: 1.0,
        pty_producing: false,
        io_wait_fraction: 1.0,
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let ctrl = default_controller();
    let snap = ctrl.telemetry().snapshot();

    assert_eq!(snap.observations, 0);
    assert_eq!(snap.frame_drops_observed, 0);
    assert_eq!(snap.throttle_activations, 0);
    assert_eq!(snap.starvation_guards, 0);
    assert_eq!(snap.posterior_updates, 0);
    assert_eq!(snap.pane_resets, 0);
    assert_eq!(snap.full_resets, 0);
}

#[test]
fn observations_tracked() {
    let mut ctrl = default_controller();
    ctrl.observe(&obs(1, 0.1, false));
    ctrl.observe(&obs(1, 0.2, false));
    ctrl.observe(&obs(2, 0.3, false));

    let snap = ctrl.telemetry().snapshot();
    assert_eq!(snap.observations, 3);
}

#[test]
fn frame_drops_tracked() {
    let mut ctrl = default_controller();
    ctrl.observe(&obs(1, 0.5, true));
    ctrl.observe(&obs(1, 0.5, false));
    ctrl.observe(&obs(1, 0.5, true));

    let snap = ctrl.telemetry().snapshot();
    assert_eq!(snap.frame_drops_observed, 2);
}

#[test]
fn throttle_activations_bounded() {
    let mut ctrl = default_controller();
    ctrl.observe(&obs(1, 0.1, false));
    ctrl.observe(&obs(1, 0.99, true));
    ctrl.observe(&obs(1, 0.5, false));

    let snap = ctrl.telemetry().snapshot();
    // Throttle activations are bounded by total observations
    assert!(snap.throttle_activations <= snap.observations);
    assert_eq!(snap.observations, 3);
}

#[test]
fn starvation_guard_tracked() {
    let mut ctrl = default_controller();

    // Normal observation — no starvation guard
    ctrl.observe(&obs(1, 0.8, true));
    let snap = ctrl.telemetry().snapshot();
    assert_eq!(snap.starvation_guards, 0);

    // Observation with high external cause evidence
    ctrl.observe(&obs_with_external(1, 0.8, true, high_external_cause()));
    let snap = ctrl.telemetry().snapshot();
    assert_eq!(snap.starvation_guards, 1);
}

#[test]
fn posterior_updates_after_warmup() {
    let config = PacBayesConfig {
        warmup_observations: 2,
        ..PacBayesConfig::default()
    };
    let mut ctrl = PacBayesBackpressure::new(config);

    // During warmup (obs 1, 2) — no posterior updates
    ctrl.observe(&obs(1, 0.5, true));
    ctrl.observe(&obs(1, 0.5, true));
    let snap = ctrl.telemetry().snapshot();
    assert_eq!(snap.posterior_updates, 0);

    // After warmup (obs 3) with frame_dropped=true — posterior updates
    ctrl.observe(&obs(1, 0.5, true));
    let snap = ctrl.telemetry().snapshot();
    assert_eq!(snap.posterior_updates, 1);
}

#[test]
fn pane_resets_tracked() {
    let mut ctrl = default_controller();
    ctrl.observe(&obs(1, 0.1, false));
    ctrl.observe(&obs(2, 0.2, false));

    ctrl.reset_pane(1);
    ctrl.reset_pane(2);
    ctrl.reset_pane(999); // nonexistent pane still counts

    let snap = ctrl.telemetry().snapshot();
    assert_eq!(snap.pane_resets, 3);
}

#[test]
fn full_resets_tracked() {
    let mut ctrl = default_controller();
    ctrl.observe(&obs(1, 0.1, false));
    ctrl.reset();
    ctrl.reset();

    let snap = ctrl.telemetry().snapshot();
    assert_eq!(snap.full_resets, 2);
}

#[test]
fn telemetry_survives_reset() {
    let mut ctrl = default_controller();
    ctrl.observe(&obs(1, 0.1, false));
    ctrl.observe(&obs(1, 0.2, true));
    ctrl.reset();

    let snap = ctrl.telemetry().snapshot();
    // Counters persist across reset (reset clears pane state, not telemetry)
    assert_eq!(snap.observations, 2);
    assert_eq!(snap.frame_drops_observed, 1);
    assert_eq!(snap.full_resets, 1);
}

#[test]
fn mixed_operations() {
    let config = PacBayesConfig {
        warmup_observations: 0,
        ..PacBayesConfig::default()
    };
    let mut ctrl = PacBayesBackpressure::new(config);

    ctrl.observe(&obs(1, 0.1, false));
    ctrl.observe(&obs(1, 0.8, true));
    ctrl.observe(&obs_with_external(2, 0.9, true, high_external_cause()));
    ctrl.reset_pane(1);
    ctrl.reset();

    let snap = ctrl.telemetry().snapshot();
    assert_eq!(snap.observations, 3);
    assert_eq!(snap.frame_drops_observed, 2);
    assert_eq!(snap.pane_resets, 1);
    assert_eq!(snap.full_resets, 1);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = PacBayesTelemetrySnapshot {
        observations: 1000,
        frame_drops_observed: 50,
        throttle_activations: 200,
        starvation_guards: 10,
        posterior_updates: 800,
        pane_resets: 5,
        full_resets: 2,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: PacBayesTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn observations_equals_call_count(
        count in 1usize..30,
    ) {
        let mut ctrl = default_controller();
        for _ in 0..count {
            ctrl.observe(&obs(1, 0.5, false));
        }
        let snap = ctrl.telemetry().snapshot();
        prop_assert_eq!(snap.observations, count as u64);
    }

    #[test]
    fn frame_drops_bounded_by_observations(
        n_dropped in 0usize..15,
        n_ok in 0usize..15,
    ) {
        let mut ctrl = default_controller();
        for _ in 0..n_dropped {
            ctrl.observe(&obs(1, 0.5, true));
        }
        for _ in 0..n_ok {
            ctrl.observe(&obs(1, 0.5, false));
        }
        let snap = ctrl.telemetry().snapshot();
        prop_assert_eq!(snap.frame_drops_observed, n_dropped as u64);
        prop_assert!(
            snap.frame_drops_observed <= snap.observations,
            "frame_drops ({}) > observations ({})",
            snap.frame_drops_observed, snap.observations,
        );
    }

    #[test]
    fn throttle_activations_bounded_by_observations(
        count in 1usize..30,
    ) {
        let mut ctrl = default_controller();
        for _ in 0..count {
            ctrl.observe(&obs(1, 0.99, true));
        }
        let snap = ctrl.telemetry().snapshot();
        prop_assert!(
            snap.throttle_activations <= snap.observations,
            "throttle_activations ({}) > observations ({})",
            snap.throttle_activations, snap.observations,
        );
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..5, 1..30),
    ) {
        let mut ctrl = default_controller();
        let mut prev = ctrl.telemetry().snapshot();

        for op in &ops {
            match op {
                0 => { ctrl.observe(&obs(1, 0.1, false)); }
                1 => { ctrl.observe(&obs(1, 0.9, true)); }
                2 => { ctrl.observe(&obs_with_external(1, 0.9, true, high_external_cause())); }
                3 => { ctrl.reset_pane(1); }
                4 => { ctrl.reset(); }
                _ => unreachable!(),
            }

            let snap = ctrl.telemetry().snapshot();
            prop_assert!(snap.observations >= prev.observations,
                "observations decreased: {} -> {}",
                prev.observations, snap.observations);
            prop_assert!(snap.frame_drops_observed >= prev.frame_drops_observed,
                "frame_drops_observed decreased: {} -> {}",
                prev.frame_drops_observed, snap.frame_drops_observed);
            prop_assert!(snap.throttle_activations >= prev.throttle_activations,
                "throttle_activations decreased: {} -> {}",
                prev.throttle_activations, snap.throttle_activations);
            prop_assert!(snap.starvation_guards >= prev.starvation_guards,
                "starvation_guards decreased: {} -> {}",
                prev.starvation_guards, snap.starvation_guards);
            prop_assert!(snap.posterior_updates >= prev.posterior_updates,
                "posterior_updates decreased: {} -> {}",
                prev.posterior_updates, snap.posterior_updates);
            prop_assert!(snap.pane_resets >= prev.pane_resets,
                "pane_resets decreased: {} -> {}",
                prev.pane_resets, snap.pane_resets);
            prop_assert!(snap.full_resets >= prev.full_resets,
                "full_resets decreased: {} -> {}",
                prev.full_resets, snap.full_resets);

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        observations in 0u64..100000,
        frame_drops_observed in 0u64..50000,
        throttle_activations in 0u64..50000,
        starvation_guards in 0u64..10000,
        posterior_updates in 0u64..50000,
        pane_resets in 0u64..10000,
        full_resets in 0u64..1000,
    ) {
        let snap = PacBayesTelemetrySnapshot {
            observations,
            frame_drops_observed,
            throttle_activations,
            starvation_guards,
            posterior_updates,
            pane_resets,
            full_resets,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: PacBayesTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
