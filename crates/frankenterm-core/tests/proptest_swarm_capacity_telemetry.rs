//! Property tests for the `SwarmCapacityTelemetry` per-stage accumulator in
//! `frankenterm_core::runtime_telemetry`. It records per-stage queue/outcome
//! counters and exposes them via `snapshot()`, but the accumulation +
//! stage-isolation contract was untested. (Records only; does not touch the
//! operator-owned admission decision path.)

use proptest::prelude::*;

use frankenterm_core::chaos_scale_harness::FailureClass;
use frankenterm_core::runtime_telemetry::{
    SwarmCapacityOutcome, SwarmCapacityStage, SwarmCapacityTelemetry,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// record_arrival increments only the targeted stage's arrival counter;
    /// the snapshot reports one row per stage and leaves every other stage
    /// at zero arrivals.
    #[test]
    fn telemetry_arrivals_accumulate_on_targeted_stage_only(
        n in 0u32..50,
        depth in 0u64..10_000,
    ) {
        let mut telemetry = SwarmCapacityTelemetry::with_defaults();
        let target = SwarmCapacityStage::IngestCapture;
        for _ in 0..n {
            telemetry.record_arrival(target, depth);
        }
        let snapshot = telemetry.snapshot();

        prop_assert_eq!(
            snapshot.stages.len(),
            SwarmCapacityStage::COUNT,
            "snapshot must report one row per stage"
        );
        let target_row = snapshot
            .stages
            .iter()
            .find(|row| row.stage == target)
            .expect("targeted stage must appear in the snapshot");
        prop_assert_eq!(target_row.arrivals, u64::from(n),
            "targeted stage must accumulate exactly the recorded arrivals");

        for row in snapshot.stages.iter().filter(|row| row.stage != target) {
            prop_assert_eq!(row.arrivals, 0,
                "non-targeted stage {:?} must not accumulate arrivals", row.stage);
        }
    }
}

/// record_outcome dispatches each outcome to the correct stage counter.
/// Notably, an Error whose class is Timeout increments BOTH errors and
/// timeouts (documented double-count), while a non-timeout Error touches
/// only errors. Returns (completions, cancellations, timeouts, errors).
#[test]
fn record_outcome_dispatches_to_correct_counter() {
    let stage = SwarmCapacityStage::IngestCapture;

    let probe = |outcome: SwarmCapacityOutcome| -> (u64, u64, u64, u64) {
        let mut telemetry = SwarmCapacityTelemetry::with_defaults();
        telemetry.record_outcome(stage, outcome, 5.0, 10);
        let snapshot = telemetry.snapshot();
        let row = snapshot
            .stages
            .iter()
            .find(|row| row.stage == stage)
            .expect("targeted stage present");
        (row.completions, row.cancellations, row.timeouts, row.errors)
    };

    assert_eq!(probe(SwarmCapacityOutcome::Completed), (1, 0, 0, 0));
    assert_eq!(probe(SwarmCapacityOutcome::Cancelled), (0, 1, 0, 0));
    assert_eq!(probe(SwarmCapacityOutcome::Timeout), (0, 0, 1, 0));
    // Non-timeout error: errors only.
    assert_eq!(
        probe(SwarmCapacityOutcome::Error(FailureClass::CpuOverload)),
        (0, 0, 0, 1)
    );
    // Timeout-classed error: both errors and timeouts increment.
    assert_eq!(
        probe(SwarmCapacityOutcome::Error(FailureClass::Timeout)),
        (0, 0, 1, 1)
    );
}
