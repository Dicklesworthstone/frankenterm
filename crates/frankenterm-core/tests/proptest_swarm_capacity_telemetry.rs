//! Property tests for the `SwarmCapacityTelemetry` per-stage accumulator in
//! `frankenterm_core::runtime_telemetry`. It records per-stage queue/outcome
//! counters and exposes them via `snapshot()`, but the accumulation +
//! stage-isolation contract was untested. (Records only; does not touch the
//! operator-owned admission decision path.)

use proptest::prelude::*;

use frankenterm_core::chaos_scale_harness::FailureClass;
use frankenterm_core::runtime_telemetry::{
    SwarmCapacityCertificateConfig, SwarmCapacityOutcome, SwarmCapacityRegressionBudget,
    SwarmCapacityRegressionGateStatus, SwarmCapacityStage, SwarmCapacityTelemetry,
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

    /// in_flight_estimate is the saturating difference between arrivals and
    /// terminal outcomes (completions + cancellations + timeouts + errors).
    /// record_outcome does not bump arrivals, so the conservation relation
    /// is exact: in_flight == max(0, arrivals - terminals).
    #[test]
    fn in_flight_estimate_is_arrivals_minus_terminals(
        arrivals in 0u32..30,
        completions in 0u32..15,
        cancellations in 0u32..15,
    ) {
        let mut telemetry = SwarmCapacityTelemetry::with_defaults();
        let stage = SwarmCapacityStage::IngestCapture;
        for _ in 0..arrivals {
            telemetry.record_arrival(stage, 1);
        }
        for _ in 0..completions {
            telemetry.record_outcome(stage, SwarmCapacityOutcome::Completed, 1.0, 1);
        }
        for _ in 0..cancellations {
            telemetry.record_outcome(stage, SwarmCapacityOutcome::Cancelled, 1.0, 1);
        }
        let snapshot = telemetry.snapshot();
        let row = snapshot
            .stages
            .iter()
            .find(|row| row.stage == stage)
            .expect("targeted stage present");

        prop_assert_eq!(row.arrivals, u64::from(arrivals));
        prop_assert_eq!(row.completions, u64::from(completions));
        prop_assert_eq!(row.cancellations, u64::from(cancellations));
        let terminals = u64::from(completions) + u64::from(cancellations);
        prop_assert_eq!(
            row.in_flight_estimate,
            u64::from(arrivals).saturating_sub(terminals),
            "in_flight must be arrivals minus terminal outcomes (saturating)"
        );
    }

    /// record_outcome feeds the per-stage service-time and queue-depth
    /// histograms once per call (before the outcome dispatch). With N
    /// identical samples, total count == N and min/max/mean all equal the
    /// recorded value (true regardless of the retained-sample cap, since
    /// every sample is identical).
    #[test]
    fn record_outcome_feeds_service_time_and_queue_histograms(
        n in 1u32..20,
        service_ms in 0.5f64..1000.0,
        queue in 1u64..5000,
    ) {
        let mut telemetry = SwarmCapacityTelemetry::with_defaults();
        let stage = SwarmCapacityStage::IngestCapture;
        for _ in 0..n {
            telemetry.record_outcome(stage, SwarmCapacityOutcome::Completed, service_ms, queue);
        }
        let snapshot = telemetry.snapshot();
        let row = snapshot
            .stages
            .iter()
            .find(|row| row.stage == stage)
            .expect("targeted stage present");

        prop_assert_eq!(row.service_time_ms.count, u64::from(n),
            "service-time histogram must count every record_outcome sample");
        prop_assert!(
            row.service_time_ms.min.is_some_and(|m| (m - service_ms).abs() < 1e-9),
            "service-time min must equal the recorded value"
        );
        prop_assert!(
            row.service_time_ms.max.is_some_and(|m| (m - service_ms).abs() < 1e-9),
            "service-time max must equal the recorded value"
        );

        prop_assert_eq!(row.queue_depth.count, u64::from(n),
            "queue-depth histogram must count every record_outcome sample");
        prop_assert!(
            row.queue_depth.min.is_some_and(|m| (m - queue as f64).abs() < 1e-9),
            "queue-depth min must equal the recorded value"
        );
    }

    /// record_wait_time_ms and record_retry_latency_ms feed their own
    /// dedicated histograms (not touched by record_outcome). With N
    /// identical samples each, both summaries count N and bracket the value.
    #[test]
    fn record_wait_and_retry_latency_feed_dedicated_histograms(
        n in 1u32..20,
        wait_ms in 0.5f64..1000.0,
        retry_ms in 0.5f64..1000.0,
    ) {
        let mut telemetry = SwarmCapacityTelemetry::with_defaults();
        let stage = SwarmCapacityStage::IngestCapture;
        for _ in 0..n {
            telemetry.record_wait_time_ms(stage, wait_ms);
            telemetry.record_retry_latency_ms(stage, retry_ms);
        }
        let snapshot = telemetry.snapshot();
        let row = snapshot
            .stages
            .iter()
            .find(|row| row.stage == stage)
            .expect("targeted stage present");

        prop_assert_eq!(row.wait_time_ms.count, u64::from(n));
        prop_assert!(
            row.wait_time_ms.min.is_some_and(|m| (m - wait_ms).abs() < 1e-9),
            "wait-time min must equal the recorded value"
        );
        prop_assert_eq!(row.retry_latency_ms.count, u64::from(n));
        prop_assert!(
            row.retry_latency_ms.max.is_some_and(|m| (m - retry_ms).abs() < 1e-9),
            "retry-latency max must equal the recorded value"
        );
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

/// capacity_certificate is a pure derivation of the snapshot + config, so
/// it must be deterministic. Compared via serialized JSON (the certificate
/// is an audit attestation without PartialEq), this complements the
/// evidence-ledger hash determinism for the deferred-proof system.
#[test]
fn capacity_certificate_is_deterministic() {
    let mut telemetry = SwarmCapacityTelemetry::with_defaults();
    // Populate with a couple of finite outcomes so derived fields are well
    // defined (and finite, avoiding non-finite serialization).
    telemetry.record_outcome(SwarmCapacityStage::IngestCapture, SwarmCapacityOutcome::Completed, 5.0, 10);
    telemetry.record_outcome(SwarmCapacityStage::StorageWrite, SwarmCapacityOutcome::Completed, 7.5, 20);
    let snapshot = telemetry.snapshot();

    let c1 = snapshot.capacity_certificate(SwarmCapacityCertificateConfig::default());
    let c2 = snapshot.capacity_certificate(SwarmCapacityCertificateConfig::default());
    let j1 = serde_json::to_string(&c1).expect("certificate serializes");
    let j2 = serde_json::to_string(&c2).expect("certificate serializes");
    assert_eq!(j1, j2, "capacity_certificate must be deterministic for a fixed snapshot + config");
}

/// The regression gate must not false-positive: a capacity certificate
/// compared against its own identical baseline produces matching baseline
/// and live hashes and never reports a regression (Fail). With the default
/// budget (baseline-update Disabled) this exercises the normal gate path.
#[test]
fn regression_gate_does_not_flag_certificate_against_itself() {
    let mut telemetry = SwarmCapacityTelemetry::with_defaults();
    telemetry.record_outcome(SwarmCapacityStage::IngestCapture, SwarmCapacityOutcome::Completed, 5.0, 10);
    telemetry.record_outcome(SwarmCapacityStage::StorageWrite, SwarmCapacityOutcome::Completed, 7.5, 20);
    let snapshot = telemetry.snapshot();
    let cert = snapshot.capacity_certificate(SwarmCapacityCertificateConfig::default());

    let report = cert.regression_budget_report(&cert, SwarmCapacityRegressionBudget::default());

    assert_eq!(
        report.baseline_hash, report.live_hash,
        "identical baseline and live certificates must hash identically"
    );
    assert_ne!(
        report.status,
        SwarmCapacityRegressionGateStatus::Fail,
        "a certificate must never regress against its own identical baseline"
    );
}
