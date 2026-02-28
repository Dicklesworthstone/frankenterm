//! Property-based tests for survival model telemetry counters (ft-3kxe.37).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. observations_recorded tracks observe() calls
//! 3. hazard_evaluations tracks evaluate_action() calls
//! 4. reports_generated tracks report() calls
//! 5. parameter_updates tracks updates after warmup
//! 6. shutdowns tracks shutdown() calls
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::survival::{
    Covariates, Observation, SurvivalConfig, SurvivalModel, SurvivalTelemetrySnapshot,
};

// =============================================================================
// Helpers
// =============================================================================

fn test_model() -> SurvivalModel {
    SurvivalModel::new(SurvivalConfig {
        warmup_observations: 0,
        ..SurvivalConfig::default()
    })
}

fn test_model_with_warmup(warmup: usize) -> SurvivalModel {
    SurvivalModel::new(SurvivalConfig {
        warmup_observations: warmup,
        ..SurvivalConfig::default()
    })
}

fn test_observation(t: f64) -> Observation {
    Observation {
        time: t,
        event_observed: true,
        covariates: Covariates::default(),
        timestamp_secs: 0,
    }
}

fn test_covariates() -> Covariates {
    Covariates::default()
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let model = test_model();
    let snap = model.telemetry().snapshot();

    assert_eq!(snap.observations_recorded, 0);
    assert_eq!(snap.parameter_updates, 0);
    assert_eq!(snap.reports_generated, 0);
    assert_eq!(snap.hazard_evaluations, 0);
    assert_eq!(snap.shutdowns, 0);
}

#[test]
fn observations_tracked() {
    let model = test_model();
    model.observe(test_observation(1.0));
    model.observe(test_observation(2.0));
    model.observe(test_observation(3.0));

    let snap = model.telemetry().snapshot();
    assert_eq!(snap.observations_recorded, 3);
}

#[test]
fn parameter_updates_tracked_after_warmup() {
    let model = test_model(); // warmup=0, so every observe triggers update
    model.observe(test_observation(1.0));
    model.observe(test_observation(2.0));

    let snap = model.telemetry().snapshot();
    assert_eq!(snap.parameter_updates, 2);
}

#[test]
fn parameter_updates_skip_during_warmup() {
    let model = test_model_with_warmup(5);
    model.observe(test_observation(1.0));
    model.observe(test_observation(2.0));
    model.observe(test_observation(3.0));

    let snap = model.telemetry().snapshot();
    assert_eq!(snap.observations_recorded, 3);
    // Still in warmup (3 < 5), no parameter updates
    assert_eq!(snap.parameter_updates, 0);
}

#[test]
fn reports_tracked() {
    let model = test_model();
    model.report(1.0, &test_covariates());
    model.report(2.0, &test_covariates());

    let snap = model.telemetry().snapshot();
    assert_eq!(snap.reports_generated, 2);
}

#[test]
fn hazard_evaluations_tracked() {
    let model = test_model();
    model.evaluate_action(1.0, &test_covariates());
    model.evaluate_action(2.0, &test_covariates());
    model.evaluate_action(3.0, &test_covariates());

    let snap = model.telemetry().snapshot();
    assert_eq!(snap.hazard_evaluations, 3);
}

#[test]
fn shutdowns_tracked() {
    let model = test_model();
    model.shutdown();

    let snap = model.telemetry().snapshot();
    assert_eq!(snap.shutdowns, 1);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = SurvivalTelemetrySnapshot {
        observations_recorded: 10000,
        parameter_updates: 9500,
        reports_generated: 5000,
        hazard_evaluations: 20000,
        shutdowns: 1,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: SurvivalTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn observations_equals_call_count(
        count in 1usize..20,
    ) {
        let model = test_model();
        for i in 0..count {
            model.observe(test_observation((i + 1) as f64));
        }
        let snap = model.telemetry().snapshot();
        prop_assert_eq!(snap.observations_recorded, count as u64);
    }

    #[test]
    fn parameter_updates_match_post_warmup_observations(
        warmup in 0usize..10,
        total in 0usize..20,
    ) {
        let model = test_model_with_warmup(warmup);
        for i in 0..total {
            model.observe(test_observation((i + 1) as f64));
        }
        let snap = model.telemetry().snapshot();
        // parameter_updates should equal obs past warmup
        // The check is `observation_count() as usize >= warmup`
        // After the i-th observe (0-indexed), obs_count = i+1
        // So updates happen when i+1 >= warmup, i.e., i >= warmup-1
        // That means for observations at indices warmup-1..total-1 (inclusive)
        // which is total - warmup + 1 if warmup > 0 and total >= warmup
        // If warmup == 0, all observations trigger updates
        let expected = if warmup == 0 {
            total as u64
        } else if total >= warmup {
            (total - warmup + 1) as u64
        } else {
            0
        };
        prop_assert_eq!(
            snap.parameter_updates,
            expected,
            "warmup={}, total={}, expected {}, got {}",
            warmup, total, expected, snap.parameter_updates,
        );
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..5, 1..20),
    ) {
        let model = test_model();
        let mut prev = model.telemetry().snapshot();

        for (i, op) in ops.iter().enumerate() {
            match op {
                0 => { model.observe(test_observation((i + 1) as f64)); }
                1 => { model.evaluate_action(1.0, &test_covariates()); }
                2 => { model.report(1.0, &test_covariates()); }
                3 => { model.observe(test_observation(0.5)); }
                4 => { /* no-op */ }
                _ => unreachable!(),
            }

            let snap = model.telemetry().snapshot();
            prop_assert!(snap.observations_recorded >= prev.observations_recorded,
                "observations_recorded decreased");
            prop_assert!(snap.parameter_updates >= prev.parameter_updates,
                "parameter_updates decreased");
            prop_assert!(snap.reports_generated >= prev.reports_generated,
                "reports_generated decreased");
            prop_assert!(snap.hazard_evaluations >= prev.hazard_evaluations,
                "hazard_evaluations decreased");
            prop_assert!(snap.shutdowns >= prev.shutdowns,
                "shutdowns decreased");

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        observations_recorded in 0u64..100000,
        parameter_updates in 0u64..100000,
        reports_generated in 0u64..50000,
        hazard_evaluations in 0u64..100000,
        shutdowns in 0u64..10,
    ) {
        let snap = SurvivalTelemetrySnapshot {
            observations_recorded,
            parameter_updates,
            reports_generated,
            hazard_evaluations,
            shutdowns,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: SurvivalTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
