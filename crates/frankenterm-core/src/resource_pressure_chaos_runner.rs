//! Aggregated runner and report surface for resource-pressure chaos fixtures.
//!
//! The scenario modules keep their own deterministic fixtures. This module
//! collects them into one reduced-mode suite report and one explicit high-scale
//! report so parent `ft-lmg3g` completion cannot accidentally treat reduced
//! evidence as real high-scale proof.

use serde::{Deserialize, Serialize};

use crate::resource_pressure_chaos::{
    HighScaleHardwareEvidence, ResourcePressureChaosMode, ResourcePressureChaosStatus,
    ResourcePressureChaosVerdict, ResourcePressureClass, ResourcePressureCoverageAssessment,
    ResourcePressureCoverageMatrix, ResourcePressureCoverageRowStatus, ResourcePressureProofLevel,
    cpu_admission_high_scale_skipped_not_proven_verdict, cpu_admission_reduced_pass_verdict,
    external_service_high_scale_skipped_not_proven_verdict,
    external_service_mcp_recoverable_stall_reduced_pass_verdict,
    external_service_policy_audit_fail_closed_reduced_pass_verdict,
    external_service_unbounded_wait_fail_verdict,
    memory_tiering_high_scale_skipped_not_proven_verdict, memory_tiering_reduced_pass_verdict,
    memory_tiering_unbounded_fail_verdict, queue_saturation_reduced_pass_verdict,
    queue_saturation_unbounded_fail_verdict,
};
use crate::resource_pressure_clock_timer_chaos::{
    clock_timer_high_scale_skipped_not_proven_verdict, clock_timer_initial_verdicts,
};
use crate::resource_pressure_storage_io_search_chaos::{
    storage_io_search_high_scale_skipped_not_proven_verdict, storage_io_search_initial_verdicts,
};

/// Current schema version for the aggregated suite report.
pub const RESOURCE_PRESSURE_CHAOS_RUNNER_SCHEMA_VERSION: u32 = 1;

/// Focused reduced-mode RCH proof command for this runner.
pub const RESOURCE_PRESSURE_CHAOS_REDUCED_RCH_COMMAND: &str = "env -u CARGO_TARGET_DIR RCH_DAEMON_TIMEOUT_MS=120000 rch exec -- cargo test -p frankenterm-core --lib --no-default-features resource_pressure_chaos_reduced_report";

/// Focused high-scale truthfulness RCH proof command for this runner.
pub const RESOURCE_PRESSURE_CHAOS_HIGH_SCALE_RCH_COMMAND: &str = "env -u CARGO_TARGET_DIR RCH_DAEMON_TIMEOUT_MS=120000 rch exec -- cargo test -p frankenterm-core --lib --no-default-features resource_pressure_chaos_high_scale_report";

/// Suite-level verdict for an aggregated resource-pressure chaos report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureChaosSuiteStatus {
    /// Reduced fixtures cover all required rows, but do not prove high-scale behavior.
    ReducedPass,
    /// Real high-scale pass verdicts cover every required row with hardware predicates met.
    HighScaleProven,
    /// The report is valid, but proof predicates are absent or intentionally skipped.
    SkippedNotProven,
    /// Known proof infrastructure prevented execution.
    ExpectedBlockedByInfra,
    /// The report is invalid or reduced-mode required rows are incomplete.
    Fail,
}

impl ResourcePressureChaosSuiteStatus {
    /// Stable machine label for the suite status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReducedPass => "REDUCED_PASS",
            Self::HighScaleProven => "HIGH_SCALE_PROVEN",
            Self::SkippedNotProven => "SKIPPED_NOT_PROVEN",
            Self::ExpectedBlockedByInfra => "EXPECTED_BLOCKED_BY_INFRA",
            Self::Fail => "FAIL",
        }
    }
}

/// Human-readable row included in the suite coverage matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureChaosCoverageReportRow {
    /// Resource-pressure class represented by this row.
    pub pressure_class: ResourcePressureClass,
    /// Human-facing row label from the canonical matrix.
    pub label: String,
    /// Whether this row gates parent completion.
    pub required_for_parent_completion: bool,
    /// Whether the row is satisfied by a valid covering verdict.
    pub satisfied: bool,
    /// Scenario that satisfies this row, when any valid scenario does.
    pub satisfying_scenario_id: Option<String>,
    /// Latest observed status for this row, including skipped high-scale rows.
    pub observed_status: Option<ResourcePressureChaosStatus>,
    /// Accounting rationale from the canonical assessor.
    pub reason: String,
}

/// Summary row for one scenario verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureChaosScenarioReportRow {
    /// Stable scenario identifier.
    pub scenario_id: String,
    /// Resource-pressure class under test.
    pub pressure_class: ResourcePressureClass,
    /// Scenario execution mode.
    pub mode: ResourcePressureChaosMode,
    /// Scenario proof level.
    pub proof_level: ResourcePressureProofLevel,
    /// Top-level scenario status.
    pub status: ResourcePressureChaosStatus,
    /// Diagnostic codes emitted by the scenario.
    pub diagnostic_codes: Vec<String>,
    /// Scenario logs path when the fixture executed.
    pub logs_path: Option<String>,
    /// Skip or proof-blocking reason when present.
    pub skip_reason: Option<String>,
}

/// Command that future release evidence should run and attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureChaosProofCommand {
    /// Short command label.
    pub label: String,
    /// Exact command line to run.
    pub command: String,
    /// Artifact path or payload expected from the command.
    pub artifact_hint: String,
    /// Suite status this command can support.
    pub supports_status: ResourcePressureChaosSuiteStatus,
}

/// Aggregated resource-pressure chaos suite report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureChaosSuiteReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Reduced or high-scale suite mode.
    pub mode: ResourcePressureChaosMode,
    /// Suite-level status.
    pub suite_status: ResourcePressureChaosSuiteStatus,
    /// Whether every required row has real high-scale pass proof.
    pub high_scale_proven: bool,
    /// Full machine-readable verdict records.
    pub verdicts: Vec<ResourcePressureChaosVerdict>,
    /// Canonical parent-completion assessment.
    pub coverage_assessment: ResourcePressureCoverageAssessment,
    /// Human-readable coverage matrix rows.
    pub coverage_matrix: Vec<ResourcePressureChaosCoverageReportRow>,
    /// Per-scenario diagnostics and artifact pointers.
    pub scenarios: Vec<ResourcePressureChaosScenarioReportRow>,
    /// Exact proof commands future agents should attach to release evidence.
    pub proof_commands: Vec<ResourcePressureChaosProofCommand>,
    /// Top-level report diagnostics.
    pub diagnostics: Vec<String>,
}

impl ResourcePressureChaosSuiteReport {
    /// Whether this report is valid reduced-mode completion evidence.
    pub fn reduced_parent_completion_ready(&self) -> bool {
        self.mode == ResourcePressureChaosMode::Reduced
            && self.suite_status == ResourcePressureChaosSuiteStatus::ReducedPass
            && self.coverage_assessment.parent_completion_ready
            && !self.high_scale_proven
    }
}

/// Build the default reduced-mode resource-pressure chaos suite report.
#[must_use]
pub fn resource_pressure_chaos_reduced_report() -> ResourcePressureChaosSuiteReport {
    resource_pressure_chaos_report_for_verdicts(
        ResourcePressureChaosMode::Reduced,
        resource_pressure_chaos_reduced_verdicts(),
    )
}

/// Build the explicit high-scale resource-pressure chaos suite report.
#[must_use]
pub fn resource_pressure_chaos_high_scale_report() -> ResourcePressureChaosSuiteReport {
    resource_pressure_chaos_report_for_verdicts(
        ResourcePressureChaosMode::HighScale,
        resource_pressure_chaos_high_scale_verdicts(),
    )
}

/// Build a report from a caller-supplied verdict set.
#[must_use]
pub fn resource_pressure_chaos_report_for_verdicts(
    mode: ResourcePressureChaosMode,
    verdicts: Vec<ResourcePressureChaosVerdict>,
) -> ResourcePressureChaosSuiteReport {
    let matrix = ResourcePressureCoverageMatrix::default();
    let coverage_assessment = matrix.assess_parent_completion(&verdicts);
    let high_scale_proven = required_rows_have_real_high_scale_passes(&matrix, &verdicts);
    let invalid_verdicts = invalid_verdict_messages(&verdicts);
    let coverage_matrix = coverage_report_rows(&matrix, &coverage_assessment);
    let scenarios = scenario_report_rows(&verdicts);
    let suite_status = suite_status(mode, &coverage_assessment, high_scale_proven, &verdicts);
    let proof_commands = resource_pressure_chaos_proof_commands();
    let diagnostics = report_diagnostics(
        mode,
        suite_status,
        high_scale_proven,
        &coverage_assessment,
        &invalid_verdicts,
    );

    ResourcePressureChaosSuiteReport {
        schema_version: RESOURCE_PRESSURE_CHAOS_RUNNER_SCHEMA_VERSION,
        mode,
        suite_status: if invalid_verdicts.is_empty() {
            suite_status
        } else {
            ResourcePressureChaosSuiteStatus::Fail
        },
        high_scale_proven: high_scale_proven && invalid_verdicts.is_empty(),
        verdicts,
        coverage_assessment,
        coverage_matrix,
        scenarios,
        proof_commands,
        diagnostics,
    }
}

/// Verdicts used by the reduced-mode suite entry point.
#[must_use]
pub fn resource_pressure_chaos_reduced_verdicts() -> Vec<ResourcePressureChaosVerdict> {
    let mut verdicts = vec![
        cpu_admission_reduced_pass_verdict(),
        queue_saturation_reduced_pass_verdict(),
        queue_saturation_unbounded_fail_verdict(),
        cpu_admission_high_scale_skipped_not_proven_verdict(),
        memory_tiering_reduced_pass_verdict(),
        memory_tiering_unbounded_fail_verdict(),
        memory_tiering_high_scale_skipped_not_proven_verdict(),
        external_service_mcp_recoverable_stall_reduced_pass_verdict(),
        external_service_policy_audit_fail_closed_reduced_pass_verdict(),
        external_service_unbounded_wait_fail_verdict(),
        external_service_high_scale_skipped_not_proven_verdict(),
    ];
    verdicts.extend(storage_io_search_initial_verdicts());
    verdicts.extend(clock_timer_initial_verdicts());
    verdicts
}

/// Verdicts used by the high-scale suite entry point.
#[must_use]
pub fn resource_pressure_chaos_high_scale_verdicts() -> Vec<ResourcePressureChaosVerdict> {
    vec![
        cpu_admission_high_scale_skipped_not_proven_verdict(),
        queue_saturation_high_scale_skipped_not_proven_verdict(),
        memory_tiering_high_scale_skipped_not_proven_verdict(),
        storage_io_search_high_scale_skipped_not_proven_verdict(),
        external_service_high_scale_skipped_not_proven_verdict(),
        clock_timer_high_scale_skipped_not_proven_verdict(),
    ]
}

/// Exact proof commands future release evidence should attach.
#[must_use]
pub fn resource_pressure_chaos_proof_commands() -> Vec<ResourcePressureChaosProofCommand> {
    vec![
        ResourcePressureChaosProofCommand {
            label: "reduced resource-pressure chaos suite".into(),
            command: RESOURCE_PRESSURE_CHAOS_REDUCED_RCH_COMMAND.into(),
            artifact_hint: "artifacts/resource-pressure/reduced-suite-report.json".into(),
            supports_status: ResourcePressureChaosSuiteStatus::ReducedPass,
        },
        ResourcePressureChaosProofCommand {
            label: "high-scale resource-pressure chaos truthfulness".into(),
            command: RESOURCE_PRESSURE_CHAOS_HIGH_SCALE_RCH_COMMAND.into(),
            artifact_hint: "artifacts/resource-pressure/high-scale-suite-report.json".into(),
            supports_status: ResourcePressureChaosSuiteStatus::HighScaleProven,
        },
    ]
}

fn suite_status(
    mode: ResourcePressureChaosMode,
    coverage_assessment: &ResourcePressureCoverageAssessment,
    high_scale_proven: bool,
    verdicts: &[ResourcePressureChaosVerdict],
) -> ResourcePressureChaosSuiteStatus {
    if verdicts
        .iter()
        .any(|verdict| verdict.status == ResourcePressureChaosStatus::ExpectedBlockedByInfra)
    {
        return ResourcePressureChaosSuiteStatus::ExpectedBlockedByInfra;
    }

    match mode {
        ResourcePressureChaosMode::Reduced if coverage_assessment.parent_completion_ready => {
            ResourcePressureChaosSuiteStatus::ReducedPass
        }
        ResourcePressureChaosMode::Reduced => ResourcePressureChaosSuiteStatus::Fail,
        ResourcePressureChaosMode::HighScale if high_scale_proven => {
            ResourcePressureChaosSuiteStatus::HighScaleProven
        }
        ResourcePressureChaosMode::HighScale => ResourcePressureChaosSuiteStatus::SkippedNotProven,
    }
}

fn required_rows_have_real_high_scale_passes(
    matrix: &ResourcePressureCoverageMatrix,
    verdicts: &[ResourcePressureChaosVerdict],
) -> bool {
    matrix
        .rows
        .iter()
        .filter(|row| row.required_for_parent_completion)
        .all(|row| {
            verdicts.iter().any(|verdict| {
                verdict.pressure_class == row.pressure_class
                    && verdict.mode == ResourcePressureChaosMode::HighScale
                    && verdict.proof_level == ResourcePressureProofLevel::RealHighScale
                    && verdict.status == ResourcePressureChaosStatus::Pass
                    && verdict.validate().is_ok()
                    && verdict
                        .hardware_evidence
                        .as_ref()
                        .is_some_and(HighScaleHardwareEvidence::predicates_met)
            })
        })
}

fn invalid_verdict_messages(verdicts: &[ResourcePressureChaosVerdict]) -> Vec<String> {
    verdicts
        .iter()
        .filter_map(|verdict| {
            verdict.validate().err().map(|error| {
                format!(
                    "{} ({}) schema validation failed: {error}",
                    verdict.scenario_id, verdict.pressure_class
                )
            })
        })
        .collect()
}

fn coverage_report_rows(
    matrix: &ResourcePressureCoverageMatrix,
    assessment: &ResourcePressureCoverageAssessment,
) -> Vec<ResourcePressureChaosCoverageReportRow> {
    matrix
        .rows
        .iter()
        .map(|row| {
            let status = assessment
                .row_statuses
                .iter()
                .find(|status| status.pressure_class == row.pressure_class)
                .cloned()
                .unwrap_or_else(|| missing_row_status(row.pressure_class));

            ResourcePressureChaosCoverageReportRow {
                pressure_class: row.pressure_class,
                label: row.label.clone(),
                required_for_parent_completion: row.required_for_parent_completion,
                satisfied: status.satisfied,
                satisfying_scenario_id: status.satisfying_scenario_id,
                observed_status: status.observed_status,
                reason: status.reason,
            }
        })
        .collect()
}

fn scenario_report_rows(
    verdicts: &[ResourcePressureChaosVerdict],
) -> Vec<ResourcePressureChaosScenarioReportRow> {
    verdicts
        .iter()
        .map(|verdict| ResourcePressureChaosScenarioReportRow {
            scenario_id: verdict.scenario_id.clone(),
            pressure_class: verdict.pressure_class,
            mode: verdict.mode,
            proof_level: verdict.proof_level,
            status: verdict.status,
            diagnostic_codes: verdict
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.code.clone())
                .collect(),
            logs_path: verdict.logs_path.clone(),
            skip_reason: verdict.skip_reason.clone(),
        })
        .collect()
}

fn report_diagnostics(
    mode: ResourcePressureChaosMode,
    suite_status: ResourcePressureChaosSuiteStatus,
    high_scale_proven: bool,
    coverage_assessment: &ResourcePressureCoverageAssessment,
    invalid_verdicts: &[String],
) -> Vec<String> {
    let mut diagnostics = Vec::new();

    if !invalid_verdicts.is_empty() {
        diagnostics.extend(invalid_verdicts.iter().cloned());
        return diagnostics;
    }

    if mode == ResourcePressureChaosMode::Reduced && !high_scale_proven {
        diagnostics.push(
            "reduced-mode report is completion evidence only; it is not high-scale proof".into(),
        );
    }

    if suite_status == ResourcePressureChaosSuiteStatus::SkippedNotProven {
        diagnostics.push(
            "high-scale report stayed SKIPPED_NOT_PROVEN because real hardware predicates are absent"
                .into(),
        );
    }

    diagnostics.extend(
        coverage_assessment
            .blocking_pressure_classes
            .iter()
            .map(|pressure_class| format!("blocking required row: {pressure_class}")),
    );

    diagnostics
}

fn missing_row_status(pressure_class: ResourcePressureClass) -> ResourcePressureCoverageRowStatus {
    ResourcePressureCoverageRowStatus {
        pressure_class,
        required_for_parent_completion: true,
        satisfied: false,
        satisfying_scenario_id: None,
        observed_status: None,
        reason: "coverage row missing from assessment".into(),
    }
}

fn queue_saturation_high_scale_skipped_not_proven_verdict() -> ResourcePressureChaosVerdict {
    let mut verdict = queue_saturation_reduced_pass_verdict();
    verdict.scenario_id = "ft-lmg3g.2.queue_saturation.high_scale.skipped".into();
    verdict.mode = ResourcePressureChaosMode::HighScale;
    verdict.proof_level = ResourcePressureProofLevel::SimulatedHighScale;
    verdict.logs_path = None;
    verdict.status = ResourcePressureChaosStatus::SkippedNotProven;
    verdict.skip_reason =
        Some("64-core/256 GiB plus high-fanout queue pressure evidence is absent".into());
    verdict.hardware_evidence = Some(HighScaleHardwareEvidence::skipped(
        "64-core/256 GiB plus high-fanout queue pressure evidence is absent",
    ));
    verdict.admission_observation = None;
    verdict
}

#[cfg(test)]
mod tests {
    use crate::resource_pressure_chaos::{
        HighScaleHardwareEvidence, ResourcePressureChaosMode, ResourcePressureChaosStatus,
        ResourcePressureChaosVerdict, ResourcePressureClass, ResourcePressureDiagnosticSeverity,
        ResourcePressureProofLevel, cpu_admission_reduced_pass_verdict,
        external_service_mcp_recoverable_stall_reduced_pass_verdict,
        memory_tiering_reduced_pass_verdict, queue_saturation_reduced_pass_verdict,
    };
    use crate::resource_pressure_clock_timer_chaos::clock_timer_reduced_pass_verdict;
    use crate::resource_pressure_storage_io_search_chaos::storage_io_search_reduced_pass_verdict;

    use super::{
        RESOURCE_PRESSURE_CHAOS_HIGH_SCALE_RCH_COMMAND,
        RESOURCE_PRESSURE_CHAOS_REDUCED_RCH_COMMAND, ResourcePressureChaosSuiteStatus,
        resource_pressure_chaos_high_scale_report, resource_pressure_chaos_reduced_report,
        resource_pressure_chaos_reduced_verdicts, resource_pressure_chaos_report_for_verdicts,
    };

    #[test]
    fn resource_pressure_chaos_reduced_report_covers_all_rows_but_not_high_scale_proof() {
        let report = resource_pressure_chaos_reduced_report();

        assert_eq!(report.mode, ResourcePressureChaosMode::Reduced);
        assert_eq!(
            report.suite_status,
            ResourcePressureChaosSuiteStatus::ReducedPass
        );
        assert!(report.reduced_parent_completion_ready());
        assert!(!report.high_scale_proven);
        assert_eq!(report.coverage_matrix.len(), 6);
        assert!(
            report
                .coverage_matrix
                .iter()
                .all(|row| row.required_for_parent_completion && row.satisfied),
            "{:?}",
            report.coverage_matrix
        );
        assert!(
            report
                .verdicts
                .iter()
                .any(|verdict| verdict.status == ResourcePressureChaosStatus::Fail)
        );
        assert!(report.verdicts.iter().any(|verdict| {
            verdict.status == ResourcePressureChaosStatus::SkippedNotProven
                && verdict.mode == ResourcePressureChaosMode::HighScale
        }));
        assert!(report.proof_commands.iter().any(|proof| {
            proof.command == RESOURCE_PRESSURE_CHAOS_REDUCED_RCH_COMMAND
                && proof.supports_status == ResourcePressureChaosSuiteStatus::ReducedPass
        }));
    }

    #[test]
    fn resource_pressure_chaos_high_scale_report_is_skipped_until_real_predicates_exist() {
        let report = resource_pressure_chaos_high_scale_report();

        assert_eq!(report.mode, ResourcePressureChaosMode::HighScale);
        assert_eq!(
            report.suite_status,
            ResourcePressureChaosSuiteStatus::SkippedNotProven
        );
        assert!(!report.high_scale_proven);
        assert!(!report.coverage_assessment.parent_completion_ready);
        assert_eq!(
            report.coverage_assessment.blocking_pressure_classes.len(),
            6
        );
        assert!(
            report
                .verdicts
                .iter()
                .all(|verdict| verdict.status == ResourcePressureChaosStatus::SkippedNotProven)
        );
        assert!(report.proof_commands.iter().any(|proof| {
            proof.command == RESOURCE_PRESSURE_CHAOS_HIGH_SCALE_RCH_COMMAND
                && proof.supports_status == ResourcePressureChaosSuiteStatus::HighScaleProven
        }));
    }

    #[test]
    fn missing_required_row_marks_reduced_report_incomplete() {
        let verdicts = resource_pressure_chaos_reduced_verdicts()
            .into_iter()
            .filter(|verdict| verdict.pressure_class != ResourcePressureClass::StorageIoSearch)
            .collect();

        let report = resource_pressure_chaos_report_for_verdicts(
            ResourcePressureChaosMode::Reduced,
            verdicts,
        );

        assert_eq!(report.suite_status, ResourcePressureChaosSuiteStatus::Fail);
        assert!(!report.coverage_assessment.parent_completion_ready);
        assert!(
            report
                .coverage_assessment
                .blocking_pressure_classes
                .contains(&ResourcePressureClass::StorageIoSearch)
        );
    }

    #[test]
    fn high_scale_proven_requires_real_high_scale_passes_for_every_required_row() {
        let report = resource_pressure_chaos_report_for_verdicts(
            ResourcePressureChaosMode::HighScale,
            real_high_scale_passes_for_all_rows(),
        );

        assert_eq!(
            report.suite_status,
            ResourcePressureChaosSuiteStatus::HighScaleProven
        );
        assert!(report.high_scale_proven);
        assert!(report.coverage_assessment.parent_completion_ready);
        assert!(
            report
                .coverage_assessment
                .blocking_pressure_classes
                .is_empty()
        );
    }

    #[test]
    fn high_scale_report_rejects_reduced_passes_even_when_coverage_rows_exist() {
        let report = resource_pressure_chaos_report_for_verdicts(
            ResourcePressureChaosMode::HighScale,
            vec![
                cpu_admission_reduced_pass_verdict(),
                queue_saturation_reduced_pass_verdict(),
                memory_tiering_reduced_pass_verdict(),
                storage_io_search_reduced_pass_verdict(),
                external_service_mcp_recoverable_stall_reduced_pass_verdict(),
                clock_timer_reduced_pass_verdict(),
            ],
        );

        assert_eq!(
            report.suite_status,
            ResourcePressureChaosSuiteStatus::SkippedNotProven
        );
        assert!(!report.high_scale_proven);
        assert!(report.coverage_assessment.parent_completion_ready);
    }

    fn real_high_scale_passes_for_all_rows() -> Vec<ResourcePressureChaosVerdict> {
        vec![
            promote_to_real_high_scale(cpu_admission_reduced_pass_verdict()),
            promote_to_real_high_scale(queue_saturation_reduced_pass_verdict()),
            promote_to_real_high_scale(memory_tiering_reduced_pass_verdict()),
            promote_to_real_high_scale(storage_io_search_reduced_pass_verdict()),
            promote_to_real_high_scale(
                external_service_mcp_recoverable_stall_reduced_pass_verdict(),
            ),
            promote_to_real_high_scale(clock_timer_reduced_pass_verdict()),
        ]
    }

    fn promote_to_real_high_scale(
        mut verdict: ResourcePressureChaosVerdict,
    ) -> ResourcePressureChaosVerdict {
        verdict.scenario_id = format!("{}.real_high_scale", verdict.scenario_id);
        verdict.mode = ResourcePressureChaosMode::HighScale;
        verdict.proof_level = ResourcePressureProofLevel::RealHighScale;
        verdict.status = ResourcePressureChaosStatus::Pass;
        verdict.hardware_evidence = Some(HighScaleHardwareEvidence::satisfied(
            "test fixture met 64-core and 256 GiB predicates",
        ));
        verdict.skip_reason = None;
        verdict.logs_path = Some(format!(
            "artifacts/resource-pressure/{}/real-high-scale.jsonl",
            verdict.pressure_class.as_str()
        ));
        verdict
            .diagnostics
            .push(crate::resource_pressure_chaos::ResourcePressureDiagnostic {
                code: format!(
                    "resource.{}.real_high_scale_proven",
                    verdict.pressure_class.as_str()
                ),
                message: "test fixture recorded real high-scale proof predicates".into(),
                severity: ResourcePressureDiagnosticSeverity::Info,
            });
        verdict
    }
}
