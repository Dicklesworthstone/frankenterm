//! Property tests for the `SwarmCapacityTelemetry` per-stage accumulator in
//! `frankenterm_core::runtime_telemetry`. It records per-stage queue/outcome
//! counters and exposes them via `snapshot()`, but the accumulation +
//! stage-isolation contract was untested. (Records only; does not touch the
//! operator-owned admission decision path.)

use proptest::prelude::*;

use frankenterm_core::runtime_telemetry::{SwarmCapacityStage, SwarmCapacityTelemetry};

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
