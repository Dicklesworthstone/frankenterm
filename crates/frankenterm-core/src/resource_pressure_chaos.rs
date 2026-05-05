//! Machine-readable resource-pressure chaos verdicts and coverage accounting.
//!
//! This module is the schema contract for the `ft-lmg3g` resource-pressure
//! chaos family. Fault injection remains in [`crate::chaos`]; this module
//! records what a scenario proved, what hardware evidence backed it, and
//! whether the parent coverage matrix is complete.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::fleet_memory_controller::{
    FleetMemoryTier, FleetMemoryTierBudgetRecord, FleetMemoryTierBudgetSnapshot, FleetPressureTier,
};
use crate::hardware_profile::HardwareProofStatus;
use crate::latency_stages::{LatencyStage, StagePressure};
use crate::swarm_scheduler::{
    AdmissionAction, AdmissionReasonCode, AdmissionRequest, QueuePressure,
    ResourceAdmissionDecisionSummary, SwarmAdmissionController, SwarmAdmissionTelemetry,
};

/// Current resource-pressure chaos verdict schema version.
pub const RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION: u32 = 1;

/// Minimum logical CPU predicate for a real high-scale resource-pressure proof.
pub const HIGH_SCALE_REQUIRED_LOGICAL_CORES: usize = 64;

/// Minimum memory predicate for a real high-scale resource-pressure proof.
pub const HIGH_SCALE_REQUIRED_MEMORY_BYTES: u64 = 256 * 1024 * 1024 * 1024;

/// Resource-pressure class covered by a chaos scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureClass {
    /// CPU pressure, admission control, and scheduler degradation decisions.
    CpuAdmission,
    /// Memory pressure, tiering, shedding, and recovery decisions.
    MemoryTiering,
    /// Storage I/O pressure and search/index catch-up behavior.
    StorageIoSearch,
    /// External services, MCP proxy calls, and search-daemon stalls.
    ExternalServiceMcpSearchDaemonStall,
    /// Queue saturation and bounded backlog behavior.
    QueueSaturation,
    /// Clock skew, timer stalls, and deadline anomalies.
    ClockTimerAnomaly,
}

impl ResourcePressureClass {
    /// Stable machine string for this pressure class.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CpuAdmission => "cpu_admission",
            Self::MemoryTiering => "memory_tiering",
            Self::StorageIoSearch => "storage_io_search",
            Self::ExternalServiceMcpSearchDaemonStall => "external_service_mcp_search_daemon_stall",
            Self::QueueSaturation => "queue_saturation",
            Self::ClockTimerAnomaly => "clock_timer_anomaly",
        }
    }
}

impl fmt::Display for ResourcePressureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Execution mode for a resource-pressure chaos run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureChaosMode {
    /// Local or CI-reduced scale. Useful regression coverage, not high-scale proof.
    Reduced,
    /// Intended high-scale run; real proof still requires hardware predicates.
    HighScale,
}

impl ResourcePressureChaosMode {
    /// Stable machine string for this execution mode.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reduced => "reduced",
            Self::HighScale => "high_scale",
        }
    }
}

impl fmt::Display for ResourcePressureChaosMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Evidence strength backing a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureProofLevel {
    /// Parser/schema/unit-only coverage. Never proves runtime behavior.
    SchemaOnly,
    /// Reduced local/CI execution against the actual scenario logic.
    ReducedLocal,
    /// High-scale-shaped replay or simulation without hardware predicates.
    SimulatedHighScale,
    /// Real high-scale hardware run with predicates met.
    RealHighScale,
}

impl ResourcePressureProofLevel {
    /// Stable machine string for this proof level.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SchemaOnly => "schema_only",
            Self::ReducedLocal => "reduced_local",
            Self::SimulatedHighScale => "simulated_high_scale",
            Self::RealHighScale => "real_high_scale",
        }
    }
}

impl fmt::Display for ResourcePressureProofLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Top-level verdict status for one scenario execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureChaosStatus {
    /// Scenario executed and all required assertions passed.
    Pass,
    /// Scenario executed and one or more required assertions failed.
    Fail,
    /// Scenario record is valid but must not be counted as high-scale proof.
    SkippedNotProven,
    /// Scenario could not execute because a known proof dependency was unavailable.
    ExpectedBlockedByInfra,
}

impl ResourcePressureChaosStatus {
    /// Stable operator label for this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::SkippedNotProven => "SKIPPED_NOT_PROVEN",
            Self::ExpectedBlockedByInfra => "EXPECTED_BLOCKED_BY_INFRA",
        }
    }

    /// Whether this status can satisfy a coverage row.
    pub const fn counts_as_covered(self) -> bool {
        matches!(self, Self::Pass)
    }
}

impl fmt::Display for ResourcePressureChaosStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Common assertion vocabulary for resource-pressure chaos scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureAssertion {
    /// The system denied or degraded work instead of proceeding unsafely.
    FailClosed,
    /// Queue or backlog growth stayed within the scenario's declared bound.
    BoundedQueueGrowth,
    /// No panic, abort, or process crash was observed.
    NoPanic,
    /// Operator-facing diagnostic was emitted.
    DiagnosticEmitted,
    /// Mitigation action was logged.
    MitigationLogged,
    /// Recovery was observed after the injected fault cleared.
    RecoveryObserved,
    /// Rollback or compensating action was observed where required.
    RollbackObserved,
}

impl ResourcePressureAssertion {
    /// Stable machine string for this assertion.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FailClosed => "fail_closed",
            Self::BoundedQueueGrowth => "bounded_queue_growth",
            Self::NoPanic => "no_panic",
            Self::DiagnosticEmitted => "diagnostic_emitted",
            Self::MitigationLogged => "mitigation_logged",
            Self::RecoveryObserved => "recovery_observed",
            Self::RollbackObserved => "rollback_observed",
        }
    }
}

impl fmt::Display for ResourcePressureAssertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Diagnostic severity attached to a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureDiagnosticSeverity {
    /// Informational diagnostic.
    Info,
    /// Warning diagnostic.
    Warn,
    /// Error diagnostic.
    Error,
}

/// Operator-facing diagnostic emitted by a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureDiagnostic {
    /// Stable diagnostic code.
    pub code: String,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Diagnostic severity.
    pub severity: ResourcePressureDiagnosticSeverity,
}

/// Explicit fail-closed decision recorded by a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureFailClosedDecision {
    /// Whether the scenario observed a fail-closed decision.
    pub fail_closed: bool,
    /// Decision rationale or denial/degrade reason.
    pub reason: String,
}

/// Queue-depth evidence captured before and after a CPU/queue pressure scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureQueueObservation {
    /// Queue depth before injecting the pressure event.
    pub queue_depth_before: u32,
    /// Queue depth after the admission mitigation ran.
    pub queue_depth_after: u32,
    /// Scenario-declared bound for acceptable queue growth.
    pub queue_depth_bound: u32,
    /// Admission-controller queue utilization in basis points (10_000 = 100%).
    pub queue_utilization_basis_points: u32,
    /// Pending items observed by the resource cockpit/admission controller.
    pub pending_items: u32,
    /// Total schedulable capacity observed by the admission controller.
    pub total_capacity: u32,
}

impl ResourcePressureQueueObservation {
    /// Whether post-injection queue depth stayed within the declared bound.
    #[must_use]
    pub const fn bounded_after_injection(&self) -> bool {
        self.queue_depth_after <= self.queue_depth_bound
    }
}

/// Resource-cockpit fields that explain a CPU/queue admission decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureCockpitTelemetry {
    /// Queue utilization in basis points (10_000 = 100%).
    pub queue_utilization_basis_points: u32,
    /// Pending items reported to the cockpit.
    pub pending_items: u32,
    /// Total schedulable capacity reported to the cockpit.
    pub total_capacity: u32,
    /// Raw pressure severity before priority protection.
    pub raw_pressure_severity: u8,
    /// Effective pressure severity after priority protection/fail-closed gates.
    pub effective_pressure_severity: u8,
    /// Final admission action reported to operators.
    pub admission_action: AdmissionAction,
    /// Primary mitigation reason reported to operators.
    pub mitigation_reason_code: AdmissionReasonCode,
}

/// Admission-controller observation required for CPU and queue chaos verdicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureAdmissionObservation {
    /// Bounded queue evidence for the scenario.
    pub queue: ResourcePressureQueueObservation,
    /// Final admission action.
    pub admission_action: AdmissionAction,
    /// Stable reason codes emitted by the admission controller.
    pub admission_reason_codes: Vec<AdmissionReasonCode>,
    /// Reason code that identifies the observed mitigation path.
    pub mitigation_reason_code: AdmissionReasonCode,
    /// Resource-cockpit telemetry surfaced with the decision.
    pub resource_cockpit: ResourcePressureCockpitTelemetry,
}

impl ResourcePressureAdmissionObservation {
    /// Build a CPU/queue observation from the real swarm admission decision summary.
    #[must_use]
    pub fn from_admission_decision(
        queue_depth_before: u32,
        queue_depth_after: u32,
        queue_depth_bound: u32,
        total_capacity: u32,
        decision: &ResourceAdmissionDecisionSummary,
        mitigation_reason_code: AdmissionReasonCode,
    ) -> Self {
        let queue_utilization_basis_points =
            utilization_to_basis_points(decision.queue_utilization);
        let pending_items = decision.pending_items.unwrap_or(queue_depth_after);
        Self {
            queue: ResourcePressureQueueObservation {
                queue_depth_before,
                queue_depth_after,
                queue_depth_bound,
                queue_utilization_basis_points,
                pending_items,
                total_capacity,
            },
            admission_action: decision.action,
            admission_reason_codes: decision.reason_codes.clone(),
            mitigation_reason_code,
            resource_cockpit: ResourcePressureCockpitTelemetry {
                queue_utilization_basis_points,
                pending_items,
                total_capacity,
                raw_pressure_severity: decision.raw_pressure_severity,
                effective_pressure_severity: decision.effective_pressure_severity,
                admission_action: decision.action,
                mitigation_reason_code,
            },
        }
    }

    fn validate(
        &self,
        status: ResourcePressureChaosStatus,
        violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
    ) {
        if self.queue.queue_depth_bound == 0 {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "admission_observation.queue.queue_depth_bound",
                "must be greater than zero",
            ));
        }

        if self.admission_reason_codes.is_empty() {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "admission_observation.admission_reason_codes",
                "must not be empty",
            ));
        }

        if !self
            .admission_reason_codes
            .contains(&self.mitigation_reason_code)
        {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "admission_observation.mitigation_reason_code",
                "must be present in admission_reason_codes",
            ));
        }

        if self.resource_cockpit.queue_utilization_basis_points
            != self.queue.queue_utilization_basis_points
            || self.resource_cockpit.pending_items != self.queue.pending_items
            || self.resource_cockpit.total_capacity != self.queue.total_capacity
            || self.resource_cockpit.admission_action != self.admission_action
            || self.resource_cockpit.mitigation_reason_code != self.mitigation_reason_code
        {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "admission_observation.resource_cockpit",
                "must mirror the queue and admission decision fields",
            ));
        }

        if status == ResourcePressureChaosStatus::Pass {
            if self.admission_action == AdmissionAction::Admit {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "admission_observation.admission_action",
                    "pass verdicts under CPU/queue pressure require defer, degrade, or shed",
                ));
            }
            if !self.queue.bounded_after_injection() {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "admission_observation.queue.queue_depth_after",
                    "pass verdicts must keep queue depth within the declared bound",
                ));
            }
        }
    }
}

/// Hardware predicate evidence for real high-scale proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighScaleHardwareEvidence {
    /// Logical CPU count required by the proof contract.
    pub required_logical_cores: usize,
    /// Memory bytes required by the proof contract.
    pub required_memory_bytes: u64,
    /// Logical CPU count observed on the proof host.
    pub observed_logical_cores: Option<usize>,
    /// Memory bytes observed on the proof host.
    pub observed_memory_bytes: Option<u64>,
    /// Predicate status from the hardware profile/proof gate.
    pub predicate_status: HardwareProofStatus,
    /// Predicate rationale from the hardware profile/proof gate.
    pub reason: String,
}

impl HighScaleHardwareEvidence {
    /// Construct satisfied 64-core / 256 GiB evidence for tests and fixtures.
    pub fn satisfied(reason: impl Into<String>) -> Self {
        Self {
            required_logical_cores: HIGH_SCALE_REQUIRED_LOGICAL_CORES,
            required_memory_bytes: HIGH_SCALE_REQUIRED_MEMORY_BYTES,
            observed_logical_cores: Some(HIGH_SCALE_REQUIRED_LOGICAL_CORES),
            observed_memory_bytes: Some(HIGH_SCALE_REQUIRED_MEMORY_BYTES),
            predicate_status: HardwareProofStatus::ProvenPredicateMet,
            reason: reason.into(),
        }
    }

    /// Construct unsatisfied evidence while preserving required predicates.
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self {
            required_logical_cores: HIGH_SCALE_REQUIRED_LOGICAL_CORES,
            required_memory_bytes: HIGH_SCALE_REQUIRED_MEMORY_BYTES,
            observed_logical_cores: None,
            observed_memory_bytes: None,
            predicate_status: HardwareProofStatus::SkippedNotProven,
            reason: reason.into(),
        }
    }

    /// Whether this evidence proves the configured high-scale predicates.
    pub fn predicates_met(&self) -> bool {
        self.predicate_status == HardwareProofStatus::ProvenPredicateMet
            && matches!(
                self.observed_logical_cores,
                Some(cores) if cores >= self.required_logical_cores
            )
            && matches!(
                self.observed_memory_bytes,
                Some(bytes) if bytes >= self.required_memory_bytes
            )
    }
}

/// One resource-pressure chaos scenario verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureChaosVerdict {
    /// Schema version for forwards-compatible artifact readers.
    pub schema_version: u32,
    /// Stable scenario ID, usually the bead-backed scenario identifier.
    pub scenario_id: String,
    /// Resource pressure class covered by the scenario.
    pub pressure_class: ResourcePressureClass,
    /// Reduced vs high-scale execution mode.
    pub mode: ResourcePressureChaosMode,
    /// Preconditions recorded before injecting the fault.
    pub preconditions: Vec<String>,
    /// Fault injected by the scenario.
    pub injected_fault: String,
    /// Mitigation observed after injection.
    pub observed_mitigation: String,
    /// Explicit fail-closed decision record.
    pub fail_closed_decision: ResourcePressureFailClosedDecision,
    /// Diagnostics emitted by the scenario.
    pub diagnostics: Vec<ResourcePressureDiagnostic>,
    /// Path to machine-readable logs or evidence artifacts, if available.
    pub logs_path: Option<String>,
    /// Evidence strength backing this verdict.
    pub proof_level: ResourcePressureProofLevel,
    /// Required when the verdict is a skip or expected infra block.
    pub skip_reason: Option<String>,
    /// Top-level scenario status.
    pub status: ResourcePressureChaosStatus,
    /// Assertions exercised by the scenario.
    pub assertions: Vec<ResourcePressureAssertion>,
    /// Hardware predicate evidence for real high-scale proof.
    pub hardware_evidence: Option<HighScaleHardwareEvidence>,
    /// CPU/queue admission observation, required for CPU and queue scenarios.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_observation: Option<ResourcePressureAdmissionObservation>,
}

impl ResourcePressureChaosVerdict {
    /// Validate schema-level invariants and proof/skip combinations.
    pub fn validate(&self) -> Result<(), ResourcePressureChaosSchemaViolations> {
        let mut violations = Vec::new();

        if self.schema_version != RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "schema_version",
                format!(
                    "expected schema version {RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION}, got {}",
                    self.schema_version
                ),
            ));
        }

        push_blank_violation(&mut violations, "scenario_id", &self.scenario_id);
        push_empty_vec_violation(&mut violations, "preconditions", &self.preconditions);
        push_blank_entry_violation(&mut violations, "preconditions", &self.preconditions);
        push_blank_violation(&mut violations, "injected_fault", &self.injected_fault);
        push_blank_violation(
            &mut violations,
            "observed_mitigation",
            &self.observed_mitigation,
        );
        push_blank_violation(
            &mut violations,
            "fail_closed_decision.reason",
            &self.fail_closed_decision.reason,
        );
        push_empty_vec_violation(&mut violations, "assertions", &self.assertions);

        if matches!(
            self.status,
            ResourcePressureChaosStatus::Pass | ResourcePressureChaosStatus::Fail
        ) {
            match self.logs_path.as_deref() {
                Some(path) if !path.trim().is_empty() => {}
                _ => violations.push(ResourcePressureChaosSchemaViolation::new(
                    "logs_path",
                    "pass/fail verdicts require a logs_path",
                )),
            }
        }

        match self.status {
            ResourcePressureChaosStatus::Pass => {
                if self.skip_reason_has_value() {
                    violations.push(ResourcePressureChaosSchemaViolation::new(
                        "skip_reason",
                        "pass verdicts must not carry skip_reason",
                    ));
                }
            }
            ResourcePressureChaosStatus::Fail => {
                if self.diagnostics.is_empty() {
                    violations.push(ResourcePressureChaosSchemaViolation::new(
                        "diagnostics",
                        "fail verdicts require at least one diagnostic",
                    ));
                }
                if self.skip_reason_has_value() {
                    violations.push(ResourcePressureChaosSchemaViolation::new(
                        "skip_reason",
                        "fail verdicts must use diagnostics instead of skip_reason",
                    ));
                }
            }
            ResourcePressureChaosStatus::SkippedNotProven
            | ResourcePressureChaosStatus::ExpectedBlockedByInfra => {
                if !self.skip_reason_has_value() {
                    violations.push(ResourcePressureChaosSchemaViolation::new(
                        "skip_reason",
                        format!("{} verdicts require skip_reason", self.status),
                    ));
                }
            }
        }

        if self.status == ResourcePressureChaosStatus::ExpectedBlockedByInfra
            && self.diagnostics.is_empty()
        {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "diagnostics",
                "expected infra blocks require a diagnostic for the blocked dependency",
            ));
        }

        if self.mode == ResourcePressureChaosMode::Reduced
            && self.proof_level == ResourcePressureProofLevel::RealHighScale
        {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "proof_level",
                "reduced runs cannot claim real_high_scale proof",
            ));
        }

        if self.status == ResourcePressureChaosStatus::Pass
            && self.mode == ResourcePressureChaosMode::HighScale
            && self.proof_level != ResourcePressureProofLevel::RealHighScale
        {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "proof_level",
                "high-scale pass verdicts require real_high_scale proof; use skipped_not_proven for simulated or reduced evidence",
            ));
        }

        if self.proof_level == ResourcePressureProofLevel::RealHighScale
            && !self
                .hardware_evidence
                .as_ref()
                .is_some_and(HighScaleHardwareEvidence::predicates_met)
        {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "hardware_evidence",
                "real_high_scale proof requires satisfied hardware predicates",
            ));
        }

        for diagnostic in &self.diagnostics {
            push_blank_violation(&mut violations, "diagnostics.code", &diagnostic.code);
            push_blank_violation(&mut violations, "diagnostics.message", &diagnostic.message);
        }

        if matches!(
            self.pressure_class,
            ResourcePressureClass::CpuAdmission | ResourcePressureClass::QueueSaturation
        ) {
            self.validate_cpu_queue_observation(&mut violations);
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(ResourcePressureChaosSchemaViolations { violations })
        }
    }

    fn skip_reason_has_value(&self) -> bool {
        self.skip_reason
            .as_deref()
            .is_some_and(|reason| !reason.trim().is_empty())
    }

    fn has_required_assertions(&self, row: &ResourcePressureCoverageRow) -> bool {
        let present: BTreeSet<_> = self.assertions.iter().copied().collect();
        row.required_assertions
            .iter()
            .all(|assertion| present.contains(assertion))
    }

    fn validate_cpu_queue_observation(
        &self,
        violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
    ) {
        let Some(observation) = self.admission_observation.as_ref() else {
            if matches!(
                self.status,
                ResourcePressureChaosStatus::Pass | ResourcePressureChaosStatus::Fail
            ) {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "admission_observation",
                    "executed CPU admission and queue saturation verdicts require admission observation evidence",
                ));
            }
            return;
        };

        observation.validate(self.status, violations);

        if self.status == ResourcePressureChaosStatus::Pass
            && !self.diagnostics.iter().any(|diagnostic| {
                diagnostic_matches_pressure_class(diagnostic, self.pressure_class)
            })
        {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "diagnostics",
                "CPU/queue pass diagnostics must identify the pressure class",
            ));
        }
    }
}

/// One schema validation violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureChaosSchemaViolation {
    /// Field or logical field that failed validation.
    pub field: String,
    /// Validation message.
    pub message: String,
}

impl ResourcePressureChaosSchemaViolation {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Collection of schema validation violations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureChaosSchemaViolations {
    /// Individual violations.
    pub violations: Vec<ResourcePressureChaosSchemaViolation>,
}

impl fmt::Display for ResourcePressureChaosSchemaViolations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (idx, violation) in self.violations.iter().enumerate() {
            if idx > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{}: {}", violation.field, violation.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ResourcePressureChaosSchemaViolations {}

/// One row in the resource-pressure coverage matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureCoverageRow {
    /// Resource pressure class represented by this row.
    pub pressure_class: ResourcePressureClass,
    /// Human-facing row label.
    pub label: String,
    /// Whether this row must have a valid pass verdict before parent completion.
    pub required_for_parent_completion: bool,
    /// Assertion vocabulary required for this row to count as covered.
    pub required_assertions: Vec<ResourcePressureAssertion>,
}

/// Resource-pressure coverage matrix for the scenario family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureCoverageMatrix {
    /// Schema version for the matrix itself.
    pub schema_version: u32,
    /// Matrix rows.
    pub rows: Vec<ResourcePressureCoverageRow>,
}

impl ResourcePressureCoverageMatrix {
    /// Build the default ft-lmg3g coverage matrix.
    pub fn default_rows() -> Vec<ResourcePressureCoverageRow> {
        vec![
            row(
                ResourcePressureClass::CpuAdmission,
                "CPU/admission pressure",
                &[
                    ResourcePressureAssertion::FailClosed,
                    ResourcePressureAssertion::BoundedQueueGrowth,
                    ResourcePressureAssertion::DiagnosticEmitted,
                    ResourcePressureAssertion::MitigationLogged,
                ],
            ),
            row(
                ResourcePressureClass::MemoryTiering,
                "memory/tiering pressure",
                &[
                    ResourcePressureAssertion::FailClosed,
                    ResourcePressureAssertion::BoundedQueueGrowth,
                    ResourcePressureAssertion::DiagnosticEmitted,
                    ResourcePressureAssertion::MitigationLogged,
                    ResourcePressureAssertion::RecoveryObserved,
                ],
            ),
            row(
                ResourcePressureClass::StorageIoSearch,
                "storage I/O and search pressure",
                &[
                    ResourcePressureAssertion::NoPanic,
                    ResourcePressureAssertion::DiagnosticEmitted,
                    ResourcePressureAssertion::MitigationLogged,
                    ResourcePressureAssertion::RecoveryObserved,
                ],
            ),
            row(
                ResourcePressureClass::ExternalServiceMcpSearchDaemonStall,
                "external-service, MCP, and search-daemon stalls",
                &[
                    ResourcePressureAssertion::FailClosed,
                    ResourcePressureAssertion::BoundedQueueGrowth,
                    ResourcePressureAssertion::DiagnosticEmitted,
                    ResourcePressureAssertion::MitigationLogged,
                    ResourcePressureAssertion::RecoveryObserved,
                ],
            ),
            row(
                ResourcePressureClass::QueueSaturation,
                "queue saturation",
                &[
                    ResourcePressureAssertion::BoundedQueueGrowth,
                    ResourcePressureAssertion::NoPanic,
                    ResourcePressureAssertion::DiagnosticEmitted,
                ],
            ),
            row(
                ResourcePressureClass::ClockTimerAnomaly,
                "clock and timer anomalies",
                &[
                    ResourcePressureAssertion::FailClosed,
                    ResourcePressureAssertion::NoPanic,
                    ResourcePressureAssertion::DiagnosticEmitted,
                    ResourcePressureAssertion::RecoveryObserved,
                ],
            ),
        ]
    }

    /// Assess whether a verdict set satisfies the matrix.
    pub fn assess_parent_completion(
        &self,
        verdicts: &[ResourcePressureChaosVerdict],
    ) -> ResourcePressureCoverageAssessment {
        let row_statuses: Vec<_> = self
            .rows
            .iter()
            .map(|row| assess_row(row, verdicts))
            .collect();

        let blocking_pressure_classes = row_statuses
            .iter()
            .filter(|status| status.required_for_parent_completion && !status.satisfied)
            .map(|status| status.pressure_class)
            .collect::<Vec<_>>();

        ResourcePressureCoverageAssessment {
            schema_version: self.schema_version,
            parent_completion_ready: blocking_pressure_classes.is_empty(),
            row_statuses,
            blocking_pressure_classes,
        }
    }
}

impl Default for ResourcePressureCoverageMatrix {
    fn default() -> Self {
        Self {
            schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
            rows: Self::default_rows(),
        }
    }
}

/// Coverage accounting for one row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureCoverageRowStatus {
    /// Resource pressure class represented by this row.
    pub pressure_class: ResourcePressureClass,
    /// Whether this row is required for parent completion.
    pub required_for_parent_completion: bool,
    /// Whether a valid pass verdict satisfied the row.
    pub satisfied: bool,
    /// Scenario that satisfied the row, when available.
    pub satisfying_scenario_id: Option<String>,
    /// Latest observed status for this row, when available.
    pub observed_status: Option<ResourcePressureChaosStatus>,
    /// Accounting rationale.
    pub reason: String,
}

/// Full coverage assessment for a verdict set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureCoverageAssessment {
    /// Schema version for the assessment.
    pub schema_version: u32,
    /// Whether every required row has a valid pass verdict.
    pub parent_completion_ready: bool,
    /// Per-row accounting.
    pub row_statuses: Vec<ResourcePressureCoverageRowStatus>,
    /// Required rows still blocking parent completion.
    pub blocking_pressure_classes: Vec<ResourcePressureClass>,
}

/// Sample PASS verdict fixture.
pub fn sample_pass_verdict() -> ResourcePressureChaosVerdict {
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.sample.pass.cpu_admission".into(),
        pressure_class: ResourcePressureClass::CpuAdmission,
        mode: ResourcePressureChaosMode::HighScale,
        preconditions: vec![
            "64 logical CPUs visible to proof host".into(),
            "256 GiB memory visible to proof host".into(),
        ],
        injected_fault: "admission controller receives sustained CPU saturation".into(),
        observed_mitigation: "scheduler degraded non-critical work and admitted critical work only"
            .into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: true,
            reason: "non-critical work denied while saturation persisted".into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.cpu_admission.degraded".into(),
            message: "CPU pressure mitigation logged".into(),
            severity: ResourcePressureDiagnosticSeverity::Info,
        }],
        logs_path: Some("artifacts/resource-pressure/cpu-admission/pass.jsonl".into()),
        proof_level: ResourcePressureProofLevel::RealHighScale,
        skip_reason: None,
        status: ResourcePressureChaosStatus::Pass,
        assertions: vec![
            ResourcePressureAssertion::FailClosed,
            ResourcePressureAssertion::BoundedQueueGrowth,
            ResourcePressureAssertion::DiagnosticEmitted,
            ResourcePressureAssertion::MitigationLogged,
        ],
        hardware_evidence: Some(HighScaleHardwareEvidence::satisfied(
            "hardware predicates met for sample high-scale pass",
        )),
        admission_observation: Some(cpu_admission_observation()),
    }
}

/// Sample FAIL verdict fixture.
pub fn sample_fail_verdict() -> ResourcePressureChaosVerdict {
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.sample.fail.memory_tiering".into(),
        pressure_class: ResourcePressureClass::MemoryTiering,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec!["memory-tier governor enabled".into()],
        injected_fault: "hot tier budget exhausted while low-priority panes grow".into(),
        observed_mitigation: "memory tier eviction diagnostic was missing".into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: false,
            reason: "low-priority work continued after budget exhaustion".into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.memory_tiering.no_fail_closed".into(),
            message: "memory pressure did not trigger required fail-closed action".into(),
            severity: ResourcePressureDiagnosticSeverity::Error,
        }],
        logs_path: Some("artifacts/resource-pressure/memory-tiering/fail.jsonl".into()),
        proof_level: ResourcePressureProofLevel::ReducedLocal,
        skip_reason: None,
        status: ResourcePressureChaosStatus::Fail,
        assertions: vec![
            ResourcePressureAssertion::FailClosed,
            ResourcePressureAssertion::BoundedQueueGrowth,
            ResourcePressureAssertion::DiagnosticEmitted,
            ResourcePressureAssertion::MitigationLogged,
            ResourcePressureAssertion::RecoveryObserved,
        ],
        hardware_evidence: None,
        admission_observation: None,
    }
}

/// Sample SKIPPED_NOT_PROVEN verdict fixture.
pub fn sample_skipped_not_proven_verdict() -> ResourcePressureChaosVerdict {
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.sample.skipped.storage_io_search".into(),
        pressure_class: ResourcePressureClass::StorageIoSearch,
        mode: ResourcePressureChaosMode::HighScale,
        preconditions: vec!["storage/search replay harness available".into()],
        injected_fault: "search indexer delayed behind storage flush pressure".into(),
        observed_mitigation: "synthetic replay completed and emitted catch-up diagnostics".into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: true,
            reason: "query path degraded while index catch-up lag exceeded the bound".into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.storage_io_search.synthetic_only".into(),
            message: "synthetic replay is valid schema coverage but not hardware proof".into(),
            severity: ResourcePressureDiagnosticSeverity::Warn,
        }],
        logs_path: None,
        proof_level: ResourcePressureProofLevel::SimulatedHighScale,
        skip_reason: Some("hardware predicates not met for real high-scale proof".into()),
        status: ResourcePressureChaosStatus::SkippedNotProven,
        assertions: vec![
            ResourcePressureAssertion::NoPanic,
            ResourcePressureAssertion::DiagnosticEmitted,
            ResourcePressureAssertion::MitigationLogged,
            ResourcePressureAssertion::RecoveryObserved,
        ],
        hardware_evidence: Some(HighScaleHardwareEvidence::skipped(
            "hardware predicates not met",
        )),
        admission_observation: None,
    }
}

/// Sample EXPECTED_BLOCKED_BY_INFRA verdict fixture.
pub fn sample_expected_blocked_by_infra_verdict() -> ResourcePressureChaosVerdict {
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.sample.blocked.external_service".into(),
        pressure_class: ResourcePressureClass::ExternalServiceMcpSearchDaemonStall,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec!["MCP/search-daemon stall harness requested".into()],
        injected_fault: "MCP proxy stall while search-daemon health probe is delayed".into(),
        observed_mitigation: "scenario did not execute because remote proof infra was unavailable"
            .into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: true,
            reason: "no unsafe release proof was claimed while infra was unavailable".into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.external_service.infra_blocked".into(),
            message: "remote proof dependency unavailable".into(),
            severity: ResourcePressureDiagnosticSeverity::Warn,
        }],
        logs_path: None,
        proof_level: ResourcePressureProofLevel::SchemaOnly,
        skip_reason: Some("RCH/proof dependency unavailable before scenario execution".into()),
        status: ResourcePressureChaosStatus::ExpectedBlockedByInfra,
        assertions: vec![
            ResourcePressureAssertion::FailClosed,
            ResourcePressureAssertion::BoundedQueueGrowth,
            ResourcePressureAssertion::DiagnosticEmitted,
            ResourcePressureAssertion::MitigationLogged,
            ResourcePressureAssertion::RecoveryObserved,
        ],
        hardware_evidence: None,
        admission_observation: None,
    }
}

/// Reduced CPU-admission fixture derived from the real swarm admission path.
pub fn cpu_admission_reduced_pass_verdict() -> ResourcePressureChaosVerdict {
    let decision = admission_decision_for_queue(cpu_admission_queue_pressure());
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.2.cpu_admission.reduced".into(),
        pressure_class: ResourcePressureClass::CpuAdmission,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec![
            "bounded CPU-admission fixture uses scheduler telemetry, not host-wide load".into(),
            "resource cockpit reports queue utilization and pending work".into(),
        ],
        injected_fault:
            "scheduler CPU scarcity represented as sustained high admission utilization".into(),
        observed_mitigation:
            "admission controller degraded non-critical work before queue growth escaped the bound"
                .into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: true,
            reason: "non-critical CPU-bound work was degraded instead of admitted at full quality"
                .into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.cpu_admission.degrade_queue_saturated".into(),
            message: "CPU-driven admission pressure degraded non-critical work".into(),
            severity: ResourcePressureDiagnosticSeverity::Warn,
        }],
        logs_path: Some("artifacts/resource-pressure/cpu-admission/reduced.jsonl".into()),
        proof_level: ResourcePressureProofLevel::ReducedLocal,
        skip_reason: None,
        status: ResourcePressureChaosStatus::Pass,
        assertions: vec![
            ResourcePressureAssertion::FailClosed,
            ResourcePressureAssertion::BoundedQueueGrowth,
            ResourcePressureAssertion::DiagnosticEmitted,
            ResourcePressureAssertion::MitigationLogged,
        ],
        hardware_evidence: None,
        admission_observation: Some(
            ResourcePressureAdmissionObservation::from_admission_decision(
                10,
                10,
                12,
                12,
                &decision,
                AdmissionReasonCode::QueueSaturated,
            ),
        ),
    }
}

/// Reduced queue-saturation fixture derived from the real swarm admission path.
pub fn queue_saturation_reduced_pass_verdict() -> ResourcePressureChaosVerdict {
    let decision = admission_decision_for_queue(queue_saturation_pressure());
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.2.queue_saturation.reduced".into(),
        pressure_class: ResourcePressureClass::QueueSaturation,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec![
            "bounded internal queue capacity declared by the scenario".into(),
            "admission controller receives complete non-memory telemetry".into(),
        ],
        injected_fault: "ready queue is held at saturation while admission requests continue"
            .into(),
        observed_mitigation:
            "admission controller shed low-priority work and held queue depth at the bound".into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: true,
            reason: "queue saturation produced a shed decision instead of unbounded backlog".into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.queue_saturation.shed_over_capacity".into(),
            message: "queue saturation produced explicit shed mitigation".into(),
            severity: ResourcePressureDiagnosticSeverity::Warn,
        }],
        logs_path: Some("artifacts/resource-pressure/queue-saturation/reduced.jsonl".into()),
        proof_level: ResourcePressureProofLevel::ReducedLocal,
        skip_reason: None,
        status: ResourcePressureChaosStatus::Pass,
        assertions: vec![
            ResourcePressureAssertion::BoundedQueueGrowth,
            ResourcePressureAssertion::NoPanic,
            ResourcePressureAssertion::DiagnosticEmitted,
        ],
        hardware_evidence: None,
        admission_observation: Some(
            ResourcePressureAdmissionObservation::from_admission_decision(
                16,
                16,
                16,
                4,
                &decision,
                AdmissionReasonCode::QueueOverCapacity,
            ),
        ),
    }
}

/// Negative reduced fixture: queue growth escaped the bound and must stay FAIL.
pub fn queue_saturation_unbounded_fail_verdict() -> ResourcePressureChaosVerdict {
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.2.queue_saturation.unbounded_fail".into(),
        pressure_class: ResourcePressureClass::QueueSaturation,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec!["misconfigured queue bound intentionally disabled".into()],
        injected_fault: "queue saturation continued after admission telemetry was ignored".into(),
        observed_mitigation: "no mitigation held queue depth inside the configured bound".into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: false,
            reason: "admission remained open while queue depth exceeded the bound".into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.queue_saturation.unbounded_backlog".into(),
            message: "queue depth exceeded the declared bound without a shed/degrade decision"
                .into(),
            severity: ResourcePressureDiagnosticSeverity::Error,
        }],
        logs_path: Some("artifacts/resource-pressure/queue-saturation/unbounded-fail.jsonl".into()),
        proof_level: ResourcePressureProofLevel::ReducedLocal,
        skip_reason: None,
        status: ResourcePressureChaosStatus::Fail,
        assertions: vec![
            ResourcePressureAssertion::BoundedQueueGrowth,
            ResourcePressureAssertion::DiagnosticEmitted,
        ],
        hardware_evidence: None,
        admission_observation: Some(ResourcePressureAdmissionObservation {
            queue: ResourcePressureQueueObservation {
                queue_depth_before: 16,
                queue_depth_after: 24,
                queue_depth_bound: 16,
                queue_utilization_basis_points: 2_500,
                pending_items: 24,
                total_capacity: 16,
            },
            admission_action: AdmissionAction::Admit,
            admission_reason_codes: vec![AdmissionReasonCode::Healthy],
            mitigation_reason_code: AdmissionReasonCode::Healthy,
            resource_cockpit: ResourcePressureCockpitTelemetry {
                queue_utilization_basis_points: 2_500,
                pending_items: 24,
                total_capacity: 16,
                raw_pressure_severity: 0,
                effective_pressure_severity: 0,
                admission_action: AdmissionAction::Admit,
                mitigation_reason_code: AdmissionReasonCode::Healthy,
            },
        }),
    }
}

/// High-scale CPU fixture that cannot claim proof without CPU topology evidence.
pub fn cpu_admission_high_scale_skipped_not_proven_verdict() -> ResourcePressureChaosVerdict {
    let mut verdict = cpu_admission_reduced_pass_verdict();
    verdict.scenario_id = "ft-lmg3g.2.cpu_admission.high_scale.skipped".into();
    verdict.mode = ResourcePressureChaosMode::HighScale;
    verdict.proof_level = ResourcePressureProofLevel::SimulatedHighScale;
    verdict.logs_path = None;
    verdict.status = ResourcePressureChaosStatus::SkippedNotProven;
    verdict.skip_reason =
        Some("64+ logical CPU topology predicate absent; high-scale CPU proof not claimed".into());
    verdict.hardware_evidence = Some(HighScaleHardwareEvidence::skipped(
        "64+ logical CPU topology predicate absent",
    ));
    verdict
}

fn cpu_admission_observation() -> ResourcePressureAdmissionObservation {
    let decision = admission_decision_for_queue(cpu_admission_queue_pressure());
    ResourcePressureAdmissionObservation::from_admission_decision(
        10,
        10,
        12,
        12,
        &decision,
        AdmissionReasonCode::QueueSaturated,
    )
}

fn admission_decision_for_queue(queue_pressure: QueuePressure) -> ResourceAdmissionDecisionSummary {
    SwarmAdmissionController::default().evaluate(
        &AdmissionRequest::standard(9, 1),
        &SwarmAdmissionTelemetry::new(
            queue_pressure,
            FleetPressureTier::Normal,
            healthy_memory_tier_budget(),
            healthy_latency_stage_pressure(),
        ),
    )
}

fn cpu_admission_queue_pressure() -> QueuePressure {
    QueuePressure {
        ready_ratio: 0.20,
        utilization: 0.93,
        starvation_count: 0,
        failure_rate: 0.0,
        pending_items: 10,
        active_agents: 4,
        total_capacity: 12,
    }
}

fn queue_saturation_pressure() -> QueuePressure {
    QueuePressure {
        ready_ratio: 0.30,
        utilization: 1.0,
        starvation_count: 0,
        failure_rate: 0.0,
        pending_items: 16,
        active_agents: 4,
        total_capacity: 4,
    }
}

fn healthy_memory_tier_budget() -> FleetMemoryTierBudgetSnapshot {
    FleetMemoryTierBudgetSnapshot::from_tiers([FleetMemoryTierBudgetRecord::new(
        FleetMemoryTier::HotResident,
        1_000,
        900,
    )])
}

fn healthy_latency_stage_pressure() -> Vec<StagePressure> {
    vec![StagePressure::compute(
        LatencyStage::PtyCapture,
        500.0,
        1_000.0,
    )]
}

fn row(
    pressure_class: ResourcePressureClass,
    label: &str,
    assertions: &[ResourcePressureAssertion],
) -> ResourcePressureCoverageRow {
    ResourcePressureCoverageRow {
        pressure_class,
        label: label.into(),
        required_for_parent_completion: true,
        required_assertions: assertions.to_vec(),
    }
}

fn assess_row(
    row: &ResourcePressureCoverageRow,
    verdicts: &[ResourcePressureChaosVerdict],
) -> ResourcePressureCoverageRowStatus {
    let matching = verdicts
        .iter()
        .filter(|verdict| verdict.pressure_class == row.pressure_class)
        .collect::<Vec<_>>();
    let observed_status = matching.last().map(|verdict| verdict.status);

    let mut validation_error = None;
    let mut missing_assertions = None;
    let mut non_covering_status = None;

    for verdict in matching.iter().rev() {
        if let Err(error) = verdict.validate() {
            validation_error = Some(error.to_string());
            continue;
        }
        if !verdict.status.counts_as_covered() {
            non_covering_status = Some(verdict.status);
            continue;
        }
        if !verdict.has_required_assertions(row) {
            let present: BTreeSet<_> = verdict.assertions.iter().copied().collect();
            let missing = row
                .required_assertions
                .iter()
                .copied()
                .filter(|assertion| !present.contains(assertion))
                .map(ResourcePressureAssertion::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            missing_assertions = Some(missing);
            continue;
        }
        return ResourcePressureCoverageRowStatus {
            pressure_class: row.pressure_class,
            required_for_parent_completion: row.required_for_parent_completion,
            satisfied: true,
            satisfying_scenario_id: Some(verdict.scenario_id.clone()),
            observed_status,
            reason: format!("covered by {} ({})", verdict.scenario_id, verdict.status),
        };
    }

    let reason = if matching.is_empty() {
        "no verdict recorded".into()
    } else if let Some(status) = non_covering_status {
        format!("latest valid verdict is {status}")
    } else if let Some(missing) = missing_assertions {
        format!("missing required assertions: {missing}")
    } else if let Some(error) = validation_error {
        format!("schema validation failed: {error}")
    } else {
        "no valid covering verdict recorded".into()
    };

    ResourcePressureCoverageRowStatus {
        pressure_class: row.pressure_class,
        required_for_parent_completion: row.required_for_parent_completion,
        satisfied: false,
        satisfying_scenario_id: None,
        observed_status,
        reason,
    }
}

fn push_blank_violation(
    violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
    field: &str,
    value: &str,
) {
    if value.trim().is_empty() {
        violations.push(ResourcePressureChaosSchemaViolation::new(
            field,
            "must not be blank",
        ));
    }
}

fn push_empty_vec_violation<T>(
    violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
    field: &str,
    values: &[T],
) {
    if values.is_empty() {
        violations.push(ResourcePressureChaosSchemaViolation::new(
            field,
            "must not be empty",
        ));
    }
}

fn push_blank_entry_violation(
    violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
    field: &str,
    values: &[String],
) {
    if values.iter().any(|value| value.trim().is_empty()) {
        violations.push(ResourcePressureChaosSchemaViolation::new(
            field,
            "must not contain blank entries",
        ));
    }
}

fn diagnostic_matches_pressure_class(
    diagnostic: &ResourcePressureDiagnostic,
    pressure_class: ResourcePressureClass,
) -> bool {
    match pressure_class {
        ResourcePressureClass::CpuAdmission => diagnostic.code.contains("cpu"),
        ResourcePressureClass::QueueSaturation => diagnostic.code.contains("queue"),
        _ => diagnostic.code.contains(pressure_class.as_str()),
    }
}

fn utilization_to_basis_points(utilization: Option<f64>) -> u32 {
    let Some(utilization) = utilization else {
        return 0;
    };
    if !utilization.is_finite() || utilization <= 0.0 {
        return 0;
    }
    (utilization * 10_000.0).round().min(u32::MAX as f64) as u32
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::swarm_scheduler::{AdmissionAction, AdmissionReasonCode};

    use super::{
        HIGH_SCALE_REQUIRED_LOGICAL_CORES, HIGH_SCALE_REQUIRED_MEMORY_BYTES,
        HighScaleHardwareEvidence, RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        ResourcePressureAssertion, ResourcePressureChaosMode, ResourcePressureChaosStatus,
        ResourcePressureChaosVerdict, ResourcePressureClass, ResourcePressureCoverageMatrix,
        ResourcePressureCoverageRow, ResourcePressureProofLevel,
        cpu_admission_high_scale_skipped_not_proven_verdict, cpu_admission_reduced_pass_verdict,
        queue_saturation_reduced_pass_verdict, queue_saturation_unbounded_fail_verdict,
        sample_expected_blocked_by_infra_verdict, sample_fail_verdict, sample_pass_verdict,
        sample_skipped_not_proven_verdict,
    };

    #[test]
    fn sample_verdicts_validate() {
        for verdict in [
            sample_pass_verdict(),
            sample_fail_verdict(),
            sample_skipped_not_proven_verdict(),
            sample_expected_blocked_by_infra_verdict(),
        ] {
            verdict
                .validate()
                .unwrap_or_else(|error| panic!("{} should validate: {error}", verdict.scenario_id));
        }
    }

    #[test]
    fn missing_proof_level_is_rejected_by_schema_deserialization() {
        let mut value = json!(sample_pass_verdict());
        value
            .as_object_mut()
            .expect("sample serializes as object")
            .remove("proof_level");

        let error = serde_json::from_value::<ResourcePressureChaosVerdict>(value)
            .expect_err("proof_level is required");
        assert!(error.to_string().contains("proof_level"));
    }

    #[test]
    fn skipped_and_infra_blocked_verdicts_require_skip_reason() {
        let mut skipped = sample_skipped_not_proven_verdict();
        skipped.skip_reason = None;
        let skipped_error = skipped
            .validate()
            .expect_err("skipped_not_proven without reason must fail");
        assert!(skipped_error.to_string().contains("skip_reason"));

        let mut blocked = sample_expected_blocked_by_infra_verdict();
        blocked.skip_reason = Some(" ".into());
        let blocked_error = blocked
            .validate()
            .expect_err("expected infra block without reason must fail");
        assert!(blocked_error.to_string().contains("skip_reason"));
    }

    #[test]
    fn high_scale_pass_requires_real_hardware_predicates() {
        let mut simulated = sample_pass_verdict();
        simulated.proof_level = ResourcePressureProofLevel::SimulatedHighScale;
        let simulated_error = simulated
            .validate()
            .expect_err("high-scale pass cannot use simulated proof");
        assert!(simulated_error.to_string().contains("proof_level"));

        let mut insufficient = sample_pass_verdict();
        insufficient.hardware_evidence = Some(HighScaleHardwareEvidence::skipped(
            "hardware predicates not met",
        ));
        let insufficient_error = insufficient
            .validate()
            .expect_err("real high-scale proof requires satisfied hardware predicates");
        assert!(insufficient_error.to_string().contains("hardware_evidence"));
    }

    #[test]
    fn reduced_runs_cannot_claim_real_high_scale_proof() {
        let mut verdict = sample_fail_verdict();
        verdict.status = ResourcePressureChaosStatus::Pass;
        verdict.mode = ResourcePressureChaosMode::Reduced;
        verdict.proof_level = ResourcePressureProofLevel::RealHighScale;
        verdict.hardware_evidence = Some(HighScaleHardwareEvidence::satisfied(
            "hardware predicates met elsewhere",
        ));

        let error = verdict
            .validate()
            .expect_err("reduced runs cannot claim real_high_scale");
        assert!(error.to_string().contains("reduced runs cannot claim"));
    }

    #[test]
    fn default_matrix_covers_required_pressure_rows() {
        let matrix = ResourcePressureCoverageMatrix::default();
        let classes = matrix
            .rows
            .iter()
            .map(|row| row.pressure_class)
            .collect::<Vec<_>>();

        assert_eq!(
            matrix.schema_version,
            RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION
        );
        assert!(classes.contains(&ResourcePressureClass::CpuAdmission));
        assert!(classes.contains(&ResourcePressureClass::MemoryTiering));
        assert!(classes.contains(&ResourcePressureClass::StorageIoSearch));
        assert!(classes.contains(&ResourcePressureClass::ExternalServiceMcpSearchDaemonStall));
        assert!(classes.contains(&ResourcePressureClass::QueueSaturation));
        assert!(classes.contains(&ResourcePressureClass::ClockTimerAnomaly));
        assert!(
            matrix
                .rows
                .iter()
                .all(|row| row.required_for_parent_completion)
        );
    }

    #[test]
    fn parent_completion_requires_pass_for_all_matrix_rows() {
        let matrix = ResourcePressureCoverageMatrix::default();
        let mut verdicts = matrix
            .rows
            .iter()
            .take(4)
            .map(pass_for_row)
            .collect::<Vec<_>>();

        let incomplete = matrix.assess_parent_completion(&verdicts);
        assert!(!incomplete.parent_completion_ready);
        assert!(
            incomplete
                .blocking_pressure_classes
                .contains(&ResourcePressureClass::QueueSaturation)
        );
        assert!(
            incomplete
                .blocking_pressure_classes
                .contains(&ResourcePressureClass::ClockTimerAnomaly)
        );

        verdicts.extend(matrix.rows.iter().skip(4).map(skipped_for_row));
        let skipped_is_still_incomplete = matrix.assess_parent_completion(&verdicts);
        assert!(
            !skipped_is_still_incomplete.parent_completion_ready,
            "{skipped_is_still_incomplete:?}"
        );
        assert!(
            skipped_is_still_incomplete
                .blocking_pressure_classes
                .contains(&ResourcePressureClass::QueueSaturation)
        );
        assert!(
            skipped_is_still_incomplete
                .blocking_pressure_classes
                .contains(&ResourcePressureClass::ClockTimerAnomaly)
        );

        let all_passed = matrix.rows.iter().map(pass_for_row).collect::<Vec<_>>();
        let complete = matrix.assess_parent_completion(&all_passed);
        assert!(complete.parent_completion_ready, "{complete:?}");
        assert!(complete.blocking_pressure_classes.is_empty());
    }

    #[test]
    fn coverage_row_requires_common_assertion_vocabulary() {
        let matrix = ResourcePressureCoverageMatrix::default();
        let row = matrix
            .rows
            .iter()
            .find(|row| row.pressure_class == ResourcePressureClass::CpuAdmission)
            .expect("cpu row");
        let mut verdict = pass_for_row(row);
        verdict.assertions = vec![ResourcePressureAssertion::DiagnosticEmitted];

        let assessment = matrix.assess_parent_completion(&[verdict]);
        let cpu_status = assessment
            .row_statuses
            .iter()
            .find(|status| status.pressure_class == ResourcePressureClass::CpuAdmission)
            .expect("cpu row status");

        assert!(!cpu_status.satisfied);
        assert!(cpu_status.reason.contains("missing required assertions"));
    }

    #[test]
    fn cpu_admission_reduced_fixture_records_degrade_decision() {
        let verdict = cpu_admission_reduced_pass_verdict();
        verdict.validate().expect("CPU admission fixture validates");

        let observation = verdict
            .admission_observation
            .as_ref()
            .expect("CPU fixture records admission observation");
        assert_eq!(observation.admission_action, AdmissionAction::Degrade);
        assert_eq!(
            observation.mitigation_reason_code,
            AdmissionReasonCode::QueueSaturated
        );
        assert!(
            observation
                .admission_reason_codes
                .contains(&AdmissionReasonCode::QueueSaturated)
        );
        assert!(observation.queue.bounded_after_injection());
    }

    #[test]
    fn queue_saturation_reduced_fixture_records_shed_decision() {
        let verdict = queue_saturation_reduced_pass_verdict();
        verdict
            .validate()
            .expect("queue saturation fixture validates");

        let observation = verdict
            .admission_observation
            .as_ref()
            .expect("queue fixture records admission observation");
        assert_eq!(observation.admission_action, AdmissionAction::Shed);
        assert_eq!(
            observation.mitigation_reason_code,
            AdmissionReasonCode::QueueOverCapacity
        );
        assert_eq!(observation.queue.queue_depth_after, 16);
        assert_eq!(observation.queue.queue_depth_bound, 16);
    }

    #[test]
    fn negative_unbounded_queue_fixture_is_valid_fail_not_coverage() {
        let verdict = queue_saturation_unbounded_fail_verdict();
        verdict.validate().expect("negative fail fixture validates");

        let observation = verdict
            .admission_observation
            .as_ref()
            .expect("negative fixture records admission observation");
        assert!(!observation.queue.bounded_after_injection());

        let assessment =
            ResourcePressureCoverageMatrix::default().assess_parent_completion(&[verdict]);
        let queue_status = assessment
            .row_statuses
            .iter()
            .find(|status| status.pressure_class == ResourcePressureClass::QueueSaturation)
            .expect("queue row status");
        assert!(!queue_status.satisfied);
        assert!(queue_status.reason.contains("FAIL"));
    }

    #[test]
    fn high_scale_cpu_fixture_skips_without_topology_evidence() {
        let verdict = cpu_admission_high_scale_skipped_not_proven_verdict();
        verdict
            .validate()
            .expect("high-scale skipped CPU fixture validates");
        assert_eq!(
            verdict.status,
            ResourcePressureChaosStatus::SkippedNotProven
        );
        assert_eq!(
            verdict.proof_level,
            ResourcePressureProofLevel::SimulatedHighScale
        );
        assert!(
            verdict
                .hardware_evidence
                .as_ref()
                .expect("hardware evidence")
                .observed_logical_cores
                .is_none()
        );

        let matrix = ResourcePressureCoverageMatrix::default();
        let assessment = matrix.assess_parent_completion(&[verdict]);
        let cpu_status = assessment
            .row_statuses
            .iter()
            .find(|status| status.pressure_class == ResourcePressureClass::CpuAdmission)
            .expect("CPU row status");
        assert!(!cpu_status.satisfied);
        assert!(cpu_status.reason.contains("SKIPPED_NOT_PROVEN"));
    }

    #[test]
    fn skipped_cpu_and_queue_verdicts_do_not_require_admission_observation() {
        let mut verdict = cpu_admission_high_scale_skipped_not_proven_verdict();
        verdict.admission_observation = None;

        verdict
            .validate()
            .expect("skipped CPU proof without execution evidence should validate");
    }

    #[test]
    fn cpu_and_queue_pass_verdicts_require_admission_observation() {
        let mut verdict = cpu_admission_reduced_pass_verdict();
        verdict.admission_observation = None;

        let error = verdict
            .validate()
            .expect_err("missing CPU observation must be rejected");
        assert!(error.to_string().contains("admission_observation"));
    }

    #[test]
    fn cpu_and_queue_pass_verdicts_reject_unbounded_growth() {
        let mut verdict = queue_saturation_reduced_pass_verdict();
        let observation = verdict
            .admission_observation
            .as_mut()
            .expect("queue fixture has observation");
        observation.queue.queue_depth_after = observation.queue.queue_depth_bound + 1;

        let error = verdict
            .validate()
            .expect_err("unbounded pass verdict must be rejected");
        assert!(error.to_string().contains("queue_depth_after"));
    }

    #[test]
    fn high_scale_hardware_evidence_checks_core_and_memory_predicates() {
        let satisfied = HighScaleHardwareEvidence::satisfied("met");
        assert!(satisfied.predicates_met());
        assert_eq!(
            satisfied.required_logical_cores,
            HIGH_SCALE_REQUIRED_LOGICAL_CORES
        );
        assert_eq!(
            satisfied.required_memory_bytes,
            HIGH_SCALE_REQUIRED_MEMORY_BYTES
        );

        let mut insufficient_memory = satisfied.clone();
        insufficient_memory.observed_memory_bytes = Some(HIGH_SCALE_REQUIRED_MEMORY_BYTES - 1);
        assert!(!insufficient_memory.predicates_met());
    }

    fn pass_for_row(row: &ResourcePressureCoverageRow) -> ResourcePressureChaosVerdict {
        let mut verdict = sample_pass_verdict();
        verdict.scenario_id = format!("ft-lmg3g.test.{}.pass", row.pressure_class.as_str());
        verdict.pressure_class = row.pressure_class;
        verdict.mode = ResourcePressureChaosMode::Reduced;
        verdict.proof_level = ResourcePressureProofLevel::ReducedLocal;
        verdict.hardware_evidence = None;
        verdict.assertions = row.required_assertions.clone();
        verdict.admission_observation = match row.pressure_class {
            ResourcePressureClass::CpuAdmission => {
                cpu_admission_reduced_pass_verdict().admission_observation
            }
            ResourcePressureClass::QueueSaturation => {
                queue_saturation_reduced_pass_verdict().admission_observation
            }
            _ => None,
        };
        verdict.diagnostics = match row.pressure_class {
            ResourcePressureClass::CpuAdmission => cpu_admission_reduced_pass_verdict().diagnostics,
            ResourcePressureClass::QueueSaturation => {
                queue_saturation_reduced_pass_verdict().diagnostics
            }
            _ => verdict.diagnostics,
        };
        verdict
    }

    fn skipped_for_row(row: &ResourcePressureCoverageRow) -> ResourcePressureChaosVerdict {
        let mut verdict = pass_for_row(row);
        verdict.scenario_id = format!("ft-lmg3g.test.{}.skipped", row.pressure_class.as_str());
        verdict.mode = ResourcePressureChaosMode::HighScale;
        verdict.proof_level = ResourcePressureProofLevel::SimulatedHighScale;
        verdict.status = ResourcePressureChaosStatus::SkippedNotProven;
        verdict.logs_path = None;
        verdict.skip_reason = Some("real high-scale hardware predicates not met".into());
        verdict.hardware_evidence = Some(HighScaleHardwareEvidence::skipped(
            "real high-scale hardware predicates not met",
        ));
        verdict
    }
}
