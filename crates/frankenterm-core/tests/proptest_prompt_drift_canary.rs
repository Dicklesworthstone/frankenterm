//! Property tests for [`prompt_drift_canary`] (ft-1650n.7).
//!
//! Pins the bounded-CUSUM detector's documented invariants over
//! arbitrary observation sequences. Complements the unit tests in
//! `prompt_drift_canary.rs::tests` (13 cases) and the synthetic-
//! transcript e2e at `tests/prompt_drift_canary_e2e.rs` (7
//! fixtures).
//!
//! Properties pinned here:
//!
//! 1. **Alarms ≤ budget** — `alarms_count` never exceeds
//!    `false_alarm_budget` for any observation sequence.
//! 2. **Counter monotonicity** — `observations_count`,
//!    `alarms_count`, `suppressed_alarms_count` only ever
//!    increase across calls to `update`.
//! 3. **observations_count == calls** — after N `update` calls,
//!    `observations_count == N`.
//! 4. **Bounded state** — the snapshot's struct shape is the
//!    same after 1 observation as after N observations (no
//!    history accumulation).
//! 5. **Baseline noise never alarms** — observations within
//!    [-k, +k] (the reference value) never accumulate enough
//!    evidence to cross the alarm threshold.
//! 6. **Snapshot is pure** — calling `snapshot` does not advance
//!    state.
//! 7. **CUSUM clamping** — `cusum_high` and `cusum_low` are
//!    always ≥ 0 (the `.max(0.0)` clamp at the substrate's
//!    update path).
//! 8. **Determinism** — two fresh canaries with the same
//!    parameters and same observation sequence produce
//!    byte-identical snapshots.

use std::sync::Once;

use frankenterm_core::prompt_drift_canary::{
    DriftAlert, DriftStatistic, DriftStatisticParams,
};
use proptest::prelude::*;

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

/// Canonical CUSUM params for the property tests:
/// baseline_mean = 0, k = 0.5, h = 5.0, budget = 5.
fn canonical_params() -> DriftStatisticParams {
    DriftStatisticParams {
        baseline_mean: 0.0,
        reference_k: 0.5,
        alarm_threshold_h: 5.0,
        false_alarm_budget: 5,
    }
}

/// Bounded observation generator: -3.0 to +3.0 in 0.1 steps.
/// Encoded as integer deciles to avoid f64 generation hassles.
fn arb_observation() -> impl Strategy<Value = f64> {
    (-30i32..=30i32).prop_map(|d| f64::from(d) * 0.1)
}

/// Bounded baseline-noise observation: within ±k (the reference
/// value). With k=0.5, noise is in [-0.5, +0.5] — observations
/// in this band never accumulate evidence (each step's
/// contribution to `cusum_high` is `obs - 0.0 - 0.5 = obs - 0.5`,
/// which is ≤ 0 for obs ≤ 0.5; symmetric for `cusum_low`). The
/// CUSUM clamps at 0, so it can never grow.
fn arb_baseline_noise() -> impl Strategy<Value = f64> {
    (-5i32..=5i32).prop_map(|d| f64::from(d) * 0.1)
}

fn arb_observation_sequence() -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(arb_observation(), 1..=120)
}

fn arb_baseline_sequence() -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(arb_baseline_noise(), 1..=200)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// **Alarms ≤ budget**: across any observation sequence, the
    /// alarm counter never exceeds `false_alarm_budget`.
    #[test]
    fn alarms_count_never_exceeds_budget(
        observations in arb_observation_sequence(),
    ) {
        init_test_tracing_json();
        let params = canonical_params();
        let mut stat = DriftStatistic::new(params);
        for o in &observations {
            let _ = stat.update(*o);
        }
        let snap = stat.snapshot();
        prop_assert!(
            snap.alarms_count <= params.false_alarm_budget,
            "alarms {} exceeded budget {}",
            snap.alarms_count, params.false_alarm_budget
        );
    }

    /// **Counter monotonicity**: every counter only increases
    /// across consecutive `update` calls. The substrate uses
    /// `saturating_add` so counters never wrap.
    #[test]
    fn counters_are_monotonic(
        observations in arb_observation_sequence(),
    ) {
        init_test_tracing_json();
        let mut stat = DriftStatistic::new(canonical_params());
        let mut prev = stat.snapshot();
        for o in &observations {
            let _ = stat.update(*o);
            let curr = stat.snapshot();
            prop_assert!(curr.observations_count >= prev.observations_count);
            prop_assert!(curr.alarms_count >= prev.alarms_count);
            prop_assert!(curr.suppressed_alarms_count >= prev.suppressed_alarms_count);
            prev = curr;
        }
    }

    /// **observations_count == calls**: after N `update` calls,
    /// `observations_count == N`.
    #[test]
    fn observations_count_equals_call_count(
        observations in arb_observation_sequence(),
    ) {
        init_test_tracing_json();
        let mut stat = DriftStatistic::new(canonical_params());
        for o in &observations {
            let _ = stat.update(*o);
        }
        prop_assert_eq!(
            stat.snapshot().observations_count,
            observations.len() as u64
        );
    }

    /// **Baseline noise never alarms**: observations within
    /// [-k, +k] never accumulate enough evidence to cross the
    /// alarm threshold (CUSUM contribution is ≤ 0 per step;
    /// the substrate's `.max(0.0)` clamp keeps the accumulator
    /// at zero).
    #[test]
    fn baseline_noise_never_alarms(
        observations in arb_baseline_sequence(),
    ) {
        init_test_tracing_json();
        let mut stat = DriftStatistic::new(canonical_params());
        let mut alerted = false;
        for o in &observations {
            if stat.update(*o).is_some() {
                alerted = true;
                break;
            }
        }
        prop_assert!(
            !alerted,
            "baseline noise should never alarm; observations: {:?}",
            observations
        );
        let snap = stat.snapshot();
        prop_assert_eq!(snap.alarms_count, 0);
    }

    /// **CUSUM clamping**: cusum_high and cusum_low are always ≥
    /// 0. The `.max(0.0)` clamp at the substrate's update path
    /// is part of the documented contract — pinned here over
    /// arbitrary observation sequences.
    #[test]
    fn cusum_accumulators_are_non_negative(
        observations in arb_observation_sequence(),
    ) {
        init_test_tracing_json();
        let mut stat = DriftStatistic::new(canonical_params());
        for o in &observations {
            let _ = stat.update(*o);
            let snap = stat.snapshot();
            prop_assert!(snap.cusum_high >= 0.0, "cusum_high went negative: {}", snap.cusum_high);
            prop_assert!(snap.cusum_low >= 0.0, "cusum_low went negative: {}", snap.cusum_low);
        }
    }

    /// **Snapshot is pure**: calling snapshot does not mutate
    /// state.
    #[test]
    fn snapshot_does_not_mutate(
        observations in arb_observation_sequence(),
    ) {
        init_test_tracing_json();
        let mut stat = DriftStatistic::new(canonical_params());
        for o in &observations {
            let _ = stat.update(*o);
        }
        let s1 = stat.snapshot();
        let s2 = stat.snapshot();
        let s3 = stat.snapshot();
        prop_assert_eq!(&s1, &s2);
        prop_assert_eq!(&s2, &s3);
    }

    /// **Determinism**: two fresh canaries with the same params
    /// + same observation sequence produce byte-identical
    /// snapshots.
    #[test]
    fn fresh_canaries_with_same_input_match(
        observations in arb_observation_sequence(),
    ) {
        init_test_tracing_json();
        let params = canonical_params();
        let mut a = DriftStatistic::new(params);
        let mut b = DriftStatistic::new(params);
        for o in &observations {
            let _ = a.update(*o);
            let _ = b.update(*o);
        }
        prop_assert_eq!(a.snapshot(), b.snapshot());
    }

    /// **Bounded state**: the snapshot struct's serialized JSON
    /// length is bounded above by a small constant regardless of
    /// observation count. Pins the documented bounded-state
    /// contract — there's no history buffer that grows per
    /// observation.
    #[test]
    fn snapshot_size_is_bounded(
        observations in arb_observation_sequence(),
    ) {
        init_test_tracing_json();
        let mut stat = DriftStatistic::new(canonical_params());
        for o in &observations {
            let _ = stat.update(*o);
        }
        let json = serde_json::to_string(&stat.snapshot()).expect("serialize");
        // The snapshot has 7 numeric fields; at most ~300 bytes
        // of JSON regardless of how many observations were ingested.
        prop_assert!(
            json.len() < 1024,
            "snapshot grew beyond bounded size: {} bytes",
            json.len()
        );
    }

    /// **budget_remaining + alarms_count == initial_budget**: the
    /// substrate's bookkeeping invariant. Pinned over arbitrary
    /// sequences.
    #[test]
    fn budget_remaining_plus_alarms_equals_initial(
        observations in arb_observation_sequence(),
    ) {
        init_test_tracing_json();
        let params = canonical_params();
        let mut stat = DriftStatistic::new(params);
        for o in &observations {
            let _ = stat.update(*o);
        }
        let snap = stat.snapshot();
        prop_assert_eq!(
            snap.budget_remaining + snap.alarms_count,
            params.false_alarm_budget
        );
    }

    /// **Returned alert variants are typed correctly**: any
    /// returned alert from `update` is one of the two documented
    /// variants. The substrate guarantees this at the type level
    /// already; the proptest pins it as a runtime invariant in
    /// case future refactors break the variant set.
    #[test]
    fn returned_alerts_are_well_typed(
        observations in arb_observation_sequence(),
    ) {
        init_test_tracing_json();
        let mut stat = DriftStatistic::new(canonical_params());
        for o in &observations {
            if let Some(alert) = stat.update(*o) {
                let is_known_variant = matches!(
                    alert,
                    DriftAlert::UpwardShift { .. } | DriftAlert::DownwardShift { .. }
                );
                prop_assert!(is_known_variant);
            }
        }
    }
}
