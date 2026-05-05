use frankenterm_core::swarm_failure_conformance::{
    SWARM_FAILURE_CONFORMANCE_RCH_COMMAND, SWARM_FAILURE_CONFORMANCE_SCHEMA_VERSION,
    SwarmFailureLogPhase, SwarmFailureMode, SwarmFailureProofLevel, SwarmFailureScenarioStatus,
    swarm_failure_conformance_report,
};

#[test]
fn conformance_report_maps_all_required_failure_modes() {
    let report = swarm_failure_conformance_report();

    assert_eq!(
        report.schema_version,
        SWARM_FAILURE_CONFORMANCE_SCHEMA_VERSION
    );
    assert!(report.all_failure_modes_mapped);
    assert_eq!(report.coverage_matrix.len(), 8);
    assert_eq!(
        report.reduced_proof_command,
        SWARM_FAILURE_CONFORMANCE_RCH_COMMAND
    );
    assert!(
        report.validation_errors.is_empty(),
        "{:?}",
        report.validation_errors
    );
    assert!(report.reduced_lab_ready());
    assert!(!report.high_scale_proven);
}

#[test]
fn event_storm_row_is_the_bounded_reduced_pass_anchor() {
    let report = swarm_failure_conformance_report();
    let row = report
        .coverage_matrix
        .iter()
        .find(|row| row.failure_mode == SwarmFailureMode::EventStormSaturation)
        .expect("event storm row");

    assert_eq!(row.status, SwarmFailureScenarioStatus::Pass);
    assert_eq!(row.proof_level, SwarmFailureProofLevel::ReducedInProcess);
    assert_eq!(
        row.evidence_anchor.as_deref(),
        Some("resource_pressure_chaos.queue_saturation")
    );
    assert_eq!(
        row.expected_error_code.as_deref(),
        Some("SWARM-EVENT-STORM-DEGRADED")
    );
}

#[test]
fn live_only_rows_are_explicitly_skipped_not_proven() {
    let report = swarm_failure_conformance_report();
    let live_rows = report
        .coverage_matrix
        .iter()
        .filter(|row| row.failure_mode != SwarmFailureMode::EventStormSaturation)
        .collect::<Vec<_>>();

    assert_eq!(live_rows.len(), 7);
    assert!(live_rows.iter().all(|row| {
        row.status == SwarmFailureScenarioStatus::SkippedNotProven
            && row.proof_level == SwarmFailureProofLevel::LiveExternalDependency
            && row
                .skip_reason
                .as_deref()
                .is_some_and(|reason| !reason.is_empty())
    }));
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic
                .contains("agent_mail_degraded remains SKIPPED_NOT_PROVEN"))
    );
}

#[test]
fn every_scenario_declares_structured_log_fields_and_typed_receipts() {
    let report = swarm_failure_conformance_report();

    for scenario in &report.scenarios {
        assert!(
            scenario.has_structured_log_fields(),
            "{} missing structured log fields",
            scenario.failure_mode
        );
        assert!(
            scenario.expected_receipt_code.starts_with("swarm.failure."),
            "{} missing stable receipt code",
            scenario.failure_mode
        );
        assert!(
            scenario
                .expected_error_code
                .as_deref()
                .is_some_and(|code| !code.is_empty()),
            "{} missing typed degraded/blocking error code",
            scenario.failure_mode
        );
        assert!(
            scenario.proof_command.contains("rch exec")
                && scenario
                    .proof_command
                    .contains("cargo test -p frankenterm-core"),
            "{} proof command is not RCH-routed",
            scenario.failure_mode
        );
    }
}

#[test]
fn every_scenario_emits_required_structured_log_phases() {
    let report = swarm_failure_conformance_report();

    for scenario in &report.scenarios {
        let records = scenario.structured_log_records();
        assert!(
            records
                .iter()
                .any(|record| record.phase == SwarmFailureLogPhase::Setup),
            "{} missing setup log",
            scenario.failure_mode
        );
        assert!(
            records
                .iter()
                .any(|record| record.phase == SwarmFailureLogPhase::InjectedFault),
            "{} missing injected-fault log",
            scenario.failure_mode
        );
        assert!(
            records
                .iter()
                .any(|record| { record.phase == SwarmFailureLogPhase::ExpectedDegradedBehavior }),
            "{} missing degraded-behavior log",
            scenario.failure_mode
        );
        assert!(
            records
                .iter()
                .any(|record| record.phase == SwarmFailureLogPhase::RecoverySignal),
            "{} missing recovery-signal log",
            scenario.failure_mode
        );
        assert!(
            records
                .iter()
                .any(|record| record.phase == SwarmFailureLogPhase::FinalInvariantCheck),
            "{} missing invariant-check log",
            scenario.failure_mode
        );
    }

    assert_eq!(
        report.structured_logs.len(),
        report
            .scenarios
            .iter()
            .map(|scenario| scenario.structured_log_records().len())
            .sum::<usize>()
    );
}
