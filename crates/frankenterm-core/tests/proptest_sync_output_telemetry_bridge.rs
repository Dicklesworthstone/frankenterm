use proptest::prelude::*;

use frankenterm_core::sync_output_buffer_orchestrator::{
    BufferAdmissionDecision, BufferDrainOutcome, DrainCause, SyncOutputOrchestratorTelemetry,
};
use frankenterm_core::sync_output_telemetry_bridge::{
    forward_admission, forward_drain, forward_mode_query, AuditorConfig, TelemetryDrift,
    TelemetryDriftAuditor,
};
use frankenterm_core::sync_output_watchdog::{BsuDepthOutcome, SyncOutputTelemetry};

fn arb_admission_decision() -> impl Strategy<Value = BufferAdmissionDecision> {
    prop_oneof![
        Just(BufferAdmissionDecision::Accepted),
        any::<u64>().prop_map(|dropped_bytes| BufferAdmissionDecision::Truncated { dropped_bytes }),
        Just(BufferAdmissionDecision::Refused),
    ]
}

fn arb_drain_cause() -> impl Strategy<Value = DrainCause> {
    prop_oneof![
        Just(DrainCause::Esu),
        Just(DrainCause::Watchdog),
        Just(DrainCause::LiveResizeForce),
        Just(DrainCause::Operator),
    ]
}

fn arb_depth_outcome() -> impl Strategy<Value = BsuDepthOutcome> {
    prop_oneof![
        any::<u32>().prop_map(|new_depth| BsuDepthOutcome::Opened { new_depth }),
        any::<u32>().prop_map(|new_depth| BsuDepthOutcome::Closed { new_depth }),
        Just(BsuDepthOutcome::Flushed),
        Just(BsuDepthOutcome::Underflow),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_sync_output_bridge_forward_admission_counts_only_admitted_bytes(
        decision in arb_admission_decision(),
        incoming_bytes in any::<u64>(),
    ) {
        let mut watchdog = SyncOutputTelemetry::default();

        forward_admission(decision, incoming_bytes, &mut watchdog);

        let expected = if decision.is_admitted() { incoming_bytes } else { 0 };
        prop_assert_eq!(watchdog.mid_bsu_byte_count(), expected);
    }

    #[test]
    fn proptest_sync_output_bridge_forward_drain_records_only_real_drains(
        depth_outcome in arb_depth_outcome(),
        cause in arb_drain_cause(),
        bytes in any::<u64>(),
        current_max in any::<u32>(),
    ) {
        let mut drained_watchdog = SyncOutputTelemetry::default();
        forward_drain(
            BufferDrainOutcome::Drained { bytes, cause },
            depth_outcome,
            current_max,
            &mut drained_watchdog,
        );

        match depth_outcome {
            BsuDepthOutcome::Opened { new_depth } => {
                prop_assert_eq!(drained_watchdog.bsu_count(), 1);
                prop_assert_eq!(
                    drained_watchdog.max_bsu_depth_observed(),
                    new_depth.max(current_max),
                );
            }
            BsuDepthOutcome::Closed { .. } => {
                prop_assert_eq!(drained_watchdog.esu_count(), 1);
                prop_assert_eq!(drained_watchdog.max_bsu_depth_observed(), current_max);
            }
            BsuDepthOutcome::Flushed => {
                prop_assert_eq!(drained_watchdog.esu_count(), 1);
                prop_assert_eq!(drained_watchdog.esu_flush_count(), 1);
                prop_assert_eq!(drained_watchdog.max_bsu_depth_observed(), current_max);
            }
            BsuDepthOutcome::Underflow => {
                prop_assert_eq!(drained_watchdog.adversarial_esu_underflow_count(), 1);
                prop_assert_eq!(drained_watchdog.max_bsu_depth_observed(), current_max);
            }
        }

        let mut noop_watchdog = SyncOutputTelemetry::default();
        forward_drain(
            BufferDrainOutcome::NoOp,
            depth_outcome,
            current_max,
            &mut noop_watchdog,
        );
        prop_assert_eq!(noop_watchdog, SyncOutputTelemetry::default());
    }

    #[test]
    fn proptest_sync_output_bridge_forward_mode_query_accumulates(
        calls in 0usize..=256,
    ) {
        let mut watchdog = SyncOutputTelemetry::default();

        for _ in 0..calls {
            forward_mode_query(&mut watchdog);
        }

        prop_assert_eq!(watchdog.mode_query_count(), calls as u64);
    }

    #[test]
    fn proptest_sync_output_bridge_auditor_byte_parity_depends_on_forwarding_and_config(
        incoming_bytes in 1u64..=1_000_000,
        forwarded in any::<bool>(),
        expect_byte_count_parity in any::<bool>(),
    ) {
        let mut orchestrator = SyncOutputOrchestratorTelemetry::default();
        let mut watchdog = SyncOutputTelemetry::default();
        watchdog.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);
        orchestrator.record_admission(BufferAdmissionDecision::Accepted, incoming_bytes);
        if forwarded {
            forward_admission(BufferAdmissionDecision::Accepted, incoming_bytes, &mut watchdog);
        }

        let auditor = TelemetryDriftAuditor::new(AuditorConfig {
            expect_bytes_per_bsu: true,
            expect_byte_count_parity,
        });
        let findings = auditor.audit(&orchestrator, &watchdog);
        let expected_mismatch = expect_byte_count_parity && !forwarded;

        prop_assert_eq!(
            findings.iter().any(|finding| matches!(
                finding,
                TelemetryDrift::MidBsuByteCountMismatch { .. }
            )),
            expected_mismatch,
        );
        prop_assert_eq!(auditor.invariants_hold(&orchestrator, &watchdog), !expected_mismatch);
    }

    #[test]
    fn proptest_sync_output_bridge_auditor_bytes_per_bsu_check_is_configurable(
        bsu_count in 1u64..=128,
        expect_bytes_per_bsu in any::<bool>(),
    ) {
        let orchestrator = SyncOutputOrchestratorTelemetry::default();
        let mut watchdog = SyncOutputTelemetry::default();
        for _ in 0..bsu_count {
            watchdog.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 1);
        }

        let auditor = TelemetryDriftAuditor::new(AuditorConfig {
            expect_bytes_per_bsu,
            expect_byte_count_parity: true,
        });
        let findings = auditor.audit(&orchestrator, &watchdog);

        prop_assert_eq!(
            findings.iter().any(|finding| matches!(
                finding,
                TelemetryDrift::BsuWithoutAdmissions { bsu_count: found }
                    if *found == bsu_count
            )),
            expect_bytes_per_bsu,
        );
        prop_assert_eq!(auditor.invariants_hold(&orchestrator, &watchdog), !expect_bytes_per_bsu);
    }
}
