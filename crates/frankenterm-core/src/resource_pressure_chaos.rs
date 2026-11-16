//! Machine-readable resource-pressure chaos verdicts and coverage accounting.
//!
//! This module is the schema contract for the `ft-lmg3g` resource-pressure
//! chaos family. Fault injection remains in [`crate::chaos`]; this module
//! records what a scenario proved, what hardware evidence backed it, and
//! whether the parent coverage matrix is complete.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::backpressure::BackpressureTier;
use crate::fleet_memory_controller::{
    FleetMemoryAction, FleetMemoryConfig, FleetMemoryController, FleetMemoryTier,
    FleetMemoryTierBudgetRecord, FleetMemoryTierBudgetSnapshot, FleetMemoryTierReclamationTarget,
    FleetPressureTier, PressureSignals,
};
use crate::hardware_profile::HardwareProofStatus;
use crate::latency_stages::{LatencyStage, StagePressure};
use crate::memory_budget::BudgetLevel;
use crate::memory_pressure::MemoryPressureTier;
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

/// Memory-tier accounting before and after a memory-pressure mitigation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureMemoryBytesObservation {
    /// Hot resident bytes before the injected memory pressure.
    pub hot_resident_before_bytes: u64,
    /// Hot resident bytes after the mitigation ran.
    pub hot_resident_after_bytes: u64,
    /// Warm compressed bytes before mitigation.
    pub warm_compressed_before_bytes: u64,
    /// Warm compressed bytes after mitigation.
    pub warm_compressed_after_bytes: u64,
    /// Cold disk-backed bytes before mitigation.
    pub cold_disk_before_bytes: u64,
    /// Cold disk-backed bytes after mitigation.
    pub cold_disk_after_bytes: u64,
    /// Maximum allowed cold-tier growth for the reduced fixture.
    pub cold_disk_growth_bound_bytes: u64,
    /// Search/index cache bytes before mitigation.
    pub search_index_cache_before_bytes: u64,
    /// Search/index cache bytes after mitigation.
    pub search_index_cache_after_bytes: u64,
    /// Allocator-pool bytes before mitigation.
    pub allocator_pool_before_bytes: u64,
    /// Allocator-pool bytes after mitigation.
    pub allocator_pool_after_bytes: u64,
    /// Resident bytes after mitigation, excluding cold disk.
    pub resident_after_bytes: u64,
}

impl ResourcePressureMemoryBytesObservation {
    /// Cold-tier byte growth caused by warm-to-cold eviction.
    #[must_use]
    pub const fn cold_disk_growth_bytes(&self) -> u64 {
        self.cold_disk_after_bytes
            .saturating_sub(self.cold_disk_before_bytes)
    }

    /// Whether cold-tier growth stayed within the scenario bound.
    #[must_use]
    pub const fn cold_disk_growth_bounded(&self) -> bool {
        self.cold_disk_growth_bytes() <= self.cold_disk_growth_bound_bytes
    }
}

/// Resource-cockpit memory telemetry captured for a memory-tier scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureMemoryCockpitTelemetry {
    /// System memory pressure tier supplied to the fleet memory controller.
    pub memory_pressure_tier: MemoryPressureTier,
    /// Worst per-pane memory budget level supplied to the controller.
    pub worst_budget_level: BudgetLevel,
    /// Fleet tier derived from the tier-budget snapshot alone.
    pub tier_budget_pressure_tier: FleetPressureTier,
    /// Compound fleet pressure tier after the controller evaluated all signals.
    pub compound_pressure_tier: FleetPressureTier,
    /// Resident budget before mitigation.
    pub resident_budget_bytes: u64,
    /// Resident bytes before mitigation.
    pub resident_before_bytes: u64,
    /// Resident bytes after mitigation.
    pub resident_after_bytes: u64,
    /// Resident bytes over budget before mitigation.
    pub resident_over_budget_before_bytes: u64,
    /// Resident bytes still over budget after mitigation.
    pub resident_over_budget_after_bytes: u64,
    /// Refused bytes recorded in the tier-budget snapshot.
    pub refused_bytes: u64,
}

/// Memory-tier and allocator-pressure observation required for memory chaos verdicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureMemoryTierObservation {
    /// Operator-visible tier-budget snapshot consumed by the real memory governor.
    pub tier_budget: FleetMemoryTierBudgetSnapshot,
    /// Byte counters before and after mitigation.
    pub bytes: ResourcePressureMemoryBytesObservation,
    /// Reclamation targets computed by the tier-budget controller.
    pub reclamation_targets: Vec<FleetMemoryTierReclamationTarget>,
    /// Fleet memory actions returned by `FleetMemoryController`.
    pub controller_actions: Vec<FleetMemoryAction>,
    /// Resource-cockpit telemetry surfaced with the decision.
    pub resource_cockpit: ResourcePressureMemoryCockpitTelemetry,
    /// Stable memory-specific mitigation reason.
    pub mitigation_reason_code: String,
}

impl ResourcePressureMemoryTierObservation {
    /// Build an observation from the real fleet memory controller and tier-budget snapshot.
    #[must_use]
    pub fn from_tier_budget_decision(
        tier_budget: FleetMemoryTierBudgetSnapshot,
        bytes: ResourcePressureMemoryBytesObservation,
        memory_pressure_tier: MemoryPressureTier,
        worst_budget_level: BudgetLevel,
        pane_count: usize,
        mitigation_reason_code: impl Into<String>,
    ) -> Self {
        let mut controller = FleetMemoryController::new(FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        });
        let signals = PressureSignals {
            backpressure: BackpressureTier::Green,
            memory_pressure: memory_pressure_tier,
            worst_budget: worst_budget_level,
            pane_count,
            paused_pane_count: 0,
        };
        let controller_actions =
            controller.evaluate_with_tier_budget(&signals, tier_budget.clone());
        let snapshot = controller.snapshot();
        let tier_budget_pressure_tier = tier_budget.pressure_tier();
        let resident_budget_bytes = tier_budget.totals.resident_budget_bytes;
        let resident_before_bytes = tier_budget.totals.resident_actual_bytes;
        let resident_after_bytes = bytes.resident_after_bytes;
        let resident_over_budget_after_bytes =
            resident_after_bytes.saturating_sub(resident_budget_bytes);

        Self {
            reclamation_targets: tier_budget.reclamation_targets(),
            tier_budget,
            resource_cockpit: ResourcePressureMemoryCockpitTelemetry {
                memory_pressure_tier,
                worst_budget_level,
                tier_budget_pressure_tier,
                compound_pressure_tier: snapshot.compound_tier,
                resident_budget_bytes,
                resident_before_bytes,
                resident_after_bytes,
                resident_over_budget_before_bytes: resident_before_bytes
                    .saturating_sub(resident_budget_bytes),
                resident_over_budget_after_bytes,
                refused_bytes: snapshot
                    .tier_budget
                    .as_ref()
                    .map_or(0, |snapshot| snapshot.totals.refused_bytes),
            },
            bytes,
            controller_actions,
            mitigation_reason_code: mitigation_reason_code.into(),
        }
    }

    fn validate(
        &self,
        status: ResourcePressureChaosStatus,
        violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
    ) {
        if self.tier_budget.tiers.is_empty() {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "memory_observation.tier_budget.tiers",
                "must not be empty",
            ));
        }
        if self.controller_actions.is_empty() {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "memory_observation.controller_actions",
                "must not be empty",
            ));
        }
        if self.mitigation_reason_code.trim().is_empty() {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "memory_observation.mitigation_reason_code",
                "must not be blank",
            ));
        }

        if self.resource_cockpit.resident_budget_bytes
            != self.tier_budget.totals.resident_budget_bytes
            || self.resource_cockpit.resident_before_bytes
                != self.tier_budget.totals.resident_actual_bytes
            || self.resource_cockpit.resident_after_bytes != self.bytes.resident_after_bytes
            || self.resource_cockpit.tier_budget_pressure_tier != self.tier_budget.pressure_tier()
        {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "memory_observation.resource_cockpit",
                "must mirror tier-budget and resident-byte evidence",
            ));
        }

        if status == ResourcePressureChaosStatus::Pass {
            if self.tier_budget.totals.resident_over_budget_bytes == 0 {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "memory_observation.tier_budget.totals.resident_over_budget_bytes",
                    "pass verdicts require pre-mitigation resident memory pressure",
                ));
            }
            if self.resource_cockpit.compound_pressure_tier == FleetPressureTier::Normal {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "memory_observation.resource_cockpit.compound_pressure_tier",
                    "pass verdicts require a non-normal fleet memory pressure decision",
                ));
            }
            if !self.controller_actions.iter().any(|action| {
                matches!(
                    action,
                    FleetMemoryAction::EvictWarmScrollback
                        | FleetMemoryAction::PauseIdlePanes
                        | FleetMemoryAction::EmergencyCleanup
                )
            }) {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "memory_observation.controller_actions",
                    "pass verdicts require an explicit memory mitigation action",
                ));
            }
            if self.reclamation_targets.is_empty() {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "memory_observation.reclamation_targets",
                    "pass verdicts require at least one memory reclamation target",
                ));
            }
            if self.bytes.resident_after_bytes > self.tier_budget.totals.resident_budget_bytes {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "memory_observation.bytes.resident_after_bytes",
                    "pass verdicts must bring resident bytes within the declared budget",
                ));
            }
            if !self.bytes.cold_disk_growth_bounded() {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "memory_observation.bytes.cold_disk_after_bytes",
                    "pass verdicts must bound cold-tier growth",
                ));
            }
        }
    }
}

/// External dependency covered by an external-service stall scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureExternalDependencyKind {
    /// MCP proxy discovery, mount, or per-call dispatch path.
    McpProxy,
    /// Search daemon query or health-check path.
    SearchDaemon,
    /// Policy/audit persistence dependency required before unsafe actions.
    PolicyAuditStore,
    /// Control-plane service dependency outside the local scheduler.
    ControlPlane,
}

impl ResourcePressureExternalDependencyKind {
    /// Stable machine string for this dependency kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::McpProxy => "mcp_proxy",
            Self::SearchDaemon => "search_daemon",
            Self::PolicyAuditStore => "policy_audit_store",
            Self::ControlPlane => "control_plane",
        }
    }
}

impl fmt::Display for ResourcePressureExternalDependencyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Retry/backoff decision recorded by an external-service stall scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureRetryBackoffDecision {
    /// Attempts made before the system chose the final mitigation.
    pub attempts_before_decision: u32,
    /// Scenario-declared maximum attempts.
    pub max_attempts: u32,
    /// Backoff delay used between retry attempts.
    pub backoff_delay_ms: u64,
    /// Total retry budget before fail-closed/degrade must happen.
    pub retry_budget_ms: u64,
    /// Whether the injected dependency call timed out.
    pub timed_out: bool,
    /// Whether the retry budget was exhausted before recovery.
    pub retry_budget_exhausted: bool,
    /// Stable final decision code reported to operators.
    pub final_decision_code: String,
}

impl ResourcePressureRetryBackoffDecision {
    fn validate(
        &self,
        violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
        field_prefix: &str,
    ) {
        if self.max_attempts == 0 {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                format!("{field_prefix}.max_attempts"),
                "must be greater than zero",
            ));
        }
        if self.attempts_before_decision == 0 {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                format!("{field_prefix}.attempts_before_decision"),
                "must be greater than zero",
            ));
        }
        if self.attempts_before_decision > self.max_attempts {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                format!("{field_prefix}.attempts_before_decision"),
                "must not exceed max_attempts",
            ));
        }
        push_blank_violation(
            violations,
            &format!("{field_prefix}.final_decision_code"),
            &self.final_decision_code,
        );
    }
}

/// Policy/audit decision recorded at an external-service boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressurePolicyAuditOutcome {
    /// Whether the action required policy/audit persistence before proceeding.
    pub audit_required: bool,
    /// Whether the policy/audit dependency was available.
    pub audit_available: bool,
    /// Whether the system failed closed when the dependency was unavailable.
    pub fail_closed: bool,
    /// Whether a remote action was allowed to proceed.
    pub remote_action_allowed: bool,
    /// Whether stale or cached read-only data was explicitly allowed.
    pub stale_cached_response_allowed: bool,
    /// Whether a mutating action was blocked.
    pub blocked_mutating_action: bool,
    /// Stable policy/audit reason code.
    pub reason_code: String,
}

impl ResourcePressurePolicyAuditOutcome {
    fn validate(
        &self,
        status: ResourcePressureChaosStatus,
        violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
        field_prefix: &str,
    ) {
        push_blank_violation(
            violations,
            &format!("{field_prefix}.reason_code"),
            &self.reason_code,
        );

        if status == ResourcePressureChaosStatus::Pass
            && self.audit_required
            && !self.audit_available
        {
            if !self.fail_closed {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    format!("{field_prefix}.fail_closed"),
                    "pass verdicts requiring unavailable audit storage must fail closed",
                ));
            }
            if self.remote_action_allowed {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    format!("{field_prefix}.remote_action_allowed"),
                    "pass verdicts must block remote actions when required audit storage is unavailable",
                ));
            }
            if !self.stale_cached_response_allowed && !self.blocked_mutating_action {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    field_prefix,
                    "pass verdicts must record an explicit cached degrade or blocked mutating action",
                ));
            }
        }
    }
}

/// Queue and fanout evidence for external dependency calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureExternalCallQueueObservation {
    /// In-flight external calls before the injected fault.
    pub in_flight_before: u32,
    /// In-flight external calls after mitigation.
    pub in_flight_after: u32,
    /// Maximum allowed in-flight calls for the scenario.
    pub in_flight_bound: u32,
    /// Queued external calls before the injected fault.
    pub queued_before: u32,
    /// Queued external calls after mitigation.
    pub queued_after: u32,
    /// Maximum allowed queued calls for the scenario.
    pub queued_bound: u32,
    /// Concurrent-agent fanout represented by the scenario, when known.
    pub concurrent_agent_fanout: Option<u32>,
}

impl ResourcePressureExternalCallQueueObservation {
    /// Whether in-flight and queued calls stayed within the declared bounds.
    #[must_use]
    pub const fn bounded_after_injection(&self) -> bool {
        self.in_flight_after <= self.in_flight_bound && self.queued_after <= self.queued_bound
    }

    fn validate(
        &self,
        status: ResourcePressureChaosStatus,
        violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
        field_prefix: &str,
    ) {
        if self.in_flight_bound == 0 {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                format!("{field_prefix}.in_flight_bound"),
                "must be greater than zero",
            ));
        }
        if self.queued_bound == 0 {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                format!("{field_prefix}.queued_bound"),
                "must be greater than zero",
            ));
        }
        if status == ResourcePressureChaosStatus::Pass && !self.bounded_after_injection() {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                field_prefix,
                "pass verdicts must keep external-call fanout and queued work within declared bounds",
            ));
        }
    }
}

/// External-service/MCP/search-daemon stall evidence for a chaos verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureExternalServiceObservation {
    /// Human-readable dependency path, such as `mcp_proxy.remote_call`.
    pub dependency_name: String,
    /// Kind of external dependency represented by this observation.
    pub dependency_kind: ResourcePressureExternalDependencyKind,
    /// Injected deterministic latency.
    pub injected_latency_ms: u64,
    /// Timeout threshold enforced by the boundary.
    pub timeout_threshold_ms: u64,
    /// Injected error code, if the script returned an error instead of only latency.
    pub injected_error_code: Option<String>,
    /// Total bounded wait observed before fail-closed/degrade/recovery.
    pub bounded_wait_ms: u64,
    /// Retry/backoff decision emitted by the boundary.
    pub retry_backoff: ResourcePressureRetryBackoffDecision,
    /// Policy/audit outcome for the dependency boundary.
    pub policy_audit_outcome: ResourcePressurePolicyAuditOutcome,
    /// External-call queue and fanout evidence.
    pub call_queue: ResourcePressureExternalCallQueueObservation,
    /// MCP proxy failure counter before injection, if the scenario covers MCP.
    pub mcp_proxy_failure_counter_before: Option<u64>,
    /// MCP proxy failure counter after mitigation, if the scenario covers MCP.
    pub mcp_proxy_failure_counter_after: Option<u64>,
    /// Whether stale/cached/local-only degraded behavior was used.
    pub degraded_to_stale_or_cached: bool,
    /// Whether recovery was observed after the injected dependency recovered.
    pub recovered_after_fault_clear: bool,
    /// Operator diagnostic code paired with this observation.
    pub operator_diagnostic_code: String,
}

impl ResourcePressureExternalServiceObservation {
    /// Declared upper bound for wait time before mitigation must decide.
    #[must_use]
    pub fn declared_wait_bound_ms(&self) -> u64 {
        let attempt_window = self
            .timeout_threshold_ms
            .saturating_mul(u64::from(self.retry_backoff.attempts_before_decision));
        attempt_window.saturating_add(self.retry_backoff.retry_budget_ms)
    }

    /// Whether the observed wait stayed within the declared retry/timeout budget.
    #[must_use]
    pub fn bounded_wait_observed(&self) -> bool {
        self.bounded_wait_ms <= self.declared_wait_bound_ms()
    }

    /// Whether the MCP proxy failure counter moved for an MCP dependency.
    #[must_use]
    pub fn records_mcp_proxy_failure(&self) -> bool {
        self.dependency_kind != ResourcePressureExternalDependencyKind::McpProxy
            || matches!(
                (
                    self.mcp_proxy_failure_counter_before,
                    self.mcp_proxy_failure_counter_after,
                ),
                (Some(before), Some(after)) if after > before
            )
    }

    fn validate(
        &self,
        status: ResourcePressureChaosStatus,
        violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
    ) {
        push_blank_violation(
            violations,
            "external_service_observation.dependency_name",
            &self.dependency_name,
        );
        push_blank_violation(
            violations,
            "external_service_observation.operator_diagnostic_code",
            &self.operator_diagnostic_code,
        );
        if self.timeout_threshold_ms == 0 {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "external_service_observation.timeout_threshold_ms",
                "must be greater than zero",
            ));
        }
        if self.injected_latency_ms == 0 && self.injected_error_code.is_none() {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "external_service_observation",
                "must inject latency or an error code",
            ));
        }
        if self
            .injected_error_code
            .as_deref()
            .is_some_and(|code| code.trim().is_empty())
        {
            violations.push(ResourcePressureChaosSchemaViolation::new(
                "external_service_observation.injected_error_code",
                "must not be blank when present",
            ));
        }

        self.retry_backoff
            .validate(violations, "external_service_observation.retry_backoff");
        self.policy_audit_outcome.validate(
            status,
            violations,
            "external_service_observation.policy_audit_outcome",
        );
        self.call_queue.validate(
            status,
            violations,
            "external_service_observation.call_queue",
        );

        if status == ResourcePressureChaosStatus::Pass {
            if !self.bounded_wait_observed() {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "external_service_observation.bounded_wait_ms",
                    "pass verdicts must resolve within the declared retry/timeout budget",
                ));
            }
            if !self.records_mcp_proxy_failure() {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "external_service_observation.mcp_proxy_failure_counter_after",
                    "MCP proxy pass verdicts must record the proxy failure counter movement",
                ));
            }
            if !self.degraded_to_stale_or_cached
                && !self.policy_audit_outcome.blocked_mutating_action
                && !self.recovered_after_fault_clear
            {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "external_service_observation",
                    "pass verdicts must record cached degrade, fail-closed block, or recovery",
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
    /// Memory-tier observation, required for executed memory/tiering scenarios.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_observation: Option<ResourcePressureMemoryTierObservation>,
    /// External-service/MCP/search-daemon observation, required for executed external stalls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_service_observation: Option<ResourcePressureExternalServiceObservation>,
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

        if self.pressure_class == ResourcePressureClass::MemoryTiering {
            self.validate_memory_tiering_observation(&mut violations);
        }

        if self.pressure_class == ResourcePressureClass::ExternalServiceMcpSearchDaemonStall {
            self.validate_external_service_observation(&mut violations);
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

    fn validate_memory_tiering_observation(
        &self,
        violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
    ) {
        let Some(observation) = self.memory_observation.as_ref() else {
            if matches!(
                self.status,
                ResourcePressureChaosStatus::Pass | ResourcePressureChaosStatus::Fail
            ) {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "memory_observation",
                    "executed memory/tiering verdicts require tier-budget and mitigation evidence",
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
                "memory pass diagnostics must identify memory/tiering pressure",
            ));
        }
    }

    fn validate_external_service_observation(
        &self,
        violations: &mut Vec<ResourcePressureChaosSchemaViolation>,
    ) {
        let Some(observation) = self.external_service_observation.as_ref() else {
            if matches!(
                self.status,
                ResourcePressureChaosStatus::Pass | ResourcePressureChaosStatus::Fail
            ) {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "external_service_observation",
                    "executed external-service/MCP/search-daemon stall verdicts require dependency observation evidence",
                ));
            }
            return;
        };

        observation.validate(self.status, violations);

        if self.status == ResourcePressureChaosStatus::Pass {
            if !self.fail_closed_decision.fail_closed {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "fail_closed_decision.fail_closed",
                    "external-service pass verdicts require fail-closed or explicit degrade",
                ));
            }
            if !self.diagnostics.iter().any(|diagnostic| {
                diagnostic_matches_pressure_class(diagnostic, self.pressure_class)
                    && diagnostic.code == observation.operator_diagnostic_code
            }) {
                violations.push(ResourcePressureChaosSchemaViolation::new(
                    "diagnostics",
                    "external-service pass diagnostics must identify the dependency pressure and match the observation diagnostic code",
                ));
            }
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
        memory_observation: None,
        external_service_observation: None,
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
        memory_observation: Some(memory_tiering_unbounded_fail_observation()),
        external_service_observation: None,
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
        memory_observation: None,
        external_service_observation: None,
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
        memory_observation: None,
        external_service_observation: None,
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
        memory_observation: None,
        external_service_observation: None,
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
        memory_observation: None,
        external_service_observation: None,
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
        memory_observation: None,
        external_service_observation: None,
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

/// Reduced memory-tier fixture derived from the real fleet memory controller.
pub fn memory_tiering_reduced_pass_verdict() -> ResourcePressureChaosVerdict {
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.3.memory_tiering.reduced".into(),
        pressure_class: ResourcePressureClass::MemoryTiering,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec![
            "synthetic hot/warm/search/allocator tier budgets force memory pressure".into(),
            "fleet memory controller evaluates the tier-budget snapshot".into(),
            "cold disk tier remains queryable and bounded after warm eviction".into(),
        ],
        injected_fault:
            "hot resident, warm compressed, search-cache, and allocator-pool tiers exceed budget"
                .into(),
        observed_mitigation:
            "fleet memory controller throttled work, evicted warm scrollback, paused idle panes, and selected bounded reclamation targets"
                .into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: true,
            reason:
                "resident memory pressure degraded capture/search work until bytes returned to budget"
                    .into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.memory_tiering.reclaim_budget".into(),
            message:
                "memory/tiering pressure produced explicit reclaim, eviction, and pause mitigation"
                    .into(),
            severity: ResourcePressureDiagnosticSeverity::Warn,
        }],
        logs_path: Some("artifacts/resource-pressure/memory-tiering/reduced.jsonl".into()),
        proof_level: ResourcePressureProofLevel::ReducedLocal,
        skip_reason: None,
        status: ResourcePressureChaosStatus::Pass,
        assertions: vec![
            ResourcePressureAssertion::FailClosed,
            ResourcePressureAssertion::BoundedQueueGrowth,
            ResourcePressureAssertion::DiagnosticEmitted,
            ResourcePressureAssertion::MitigationLogged,
            ResourcePressureAssertion::RecoveryObserved,
        ],
        hardware_evidence: None,
        admission_observation: None,
        memory_observation: Some(memory_tiering_reduced_observation()),
        external_service_observation: None,
    }
}

/// Negative reduced fixture: memory pressure remained unbounded and must stay FAIL.
pub fn memory_tiering_unbounded_fail_verdict() -> ResourcePressureChaosVerdict {
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.3.memory_tiering.unbounded_fail".into(),
        pressure_class: ResourcePressureClass::MemoryTiering,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec!["misconfigured memory-tier mitigation intentionally disabled".into()],
        injected_fault: "resident tiers exceed budget while warm eviction is disabled".into(),
        observed_mitigation:
            "memory pressure diagnostic was emitted but resident bytes and cold growth stayed unbounded"
                .into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: false,
            reason: "capture/search admission continued while resident memory exceeded budget".into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.memory_tiering.unbounded_resident_bytes".into(),
            message:
                "resident memory remained over budget and cold-tier growth exceeded the declared bound"
                    .into(),
            severity: ResourcePressureDiagnosticSeverity::Error,
        }],
        logs_path: Some("artifacts/resource-pressure/memory-tiering/unbounded-fail.jsonl".into()),
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
        memory_observation: Some(memory_tiering_unbounded_fail_observation()),
        external_service_observation: None,
    }
}

/// High-scale memory fixture that cannot claim proof without 256 GiB evidence.
pub fn memory_tiering_high_scale_skipped_not_proven_verdict() -> ResourcePressureChaosVerdict {
    let mut verdict = memory_tiering_reduced_pass_verdict();
    verdict.scenario_id = "ft-lmg3g.3.memory_tiering.high_scale.skipped".into();
    verdict.mode = ResourcePressureChaosMode::HighScale;
    verdict.proof_level = ResourcePressureProofLevel::SimulatedHighScale;
    verdict.logs_path = None;
    verdict.status = ResourcePressureChaosStatus::SkippedNotProven;
    verdict.skip_reason =
        Some("256 GiB memory predicate absent; high-scale memory proof not claimed".into());
    verdict.hardware_evidence = Some(HighScaleHardwareEvidence::skipped(
        "256 GiB memory predicate absent",
    ));
    verdict.memory_observation = None;
    verdict
}

/// Reduced external-service fixture for recoverable MCP proxy stalls.
pub fn external_service_mcp_recoverable_stall_reduced_pass_verdict() -> ResourcePressureChaosVerdict
{
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.5.external_service.mcp_recoverable_stall.reduced".into(),
        pressure_class: ResourcePressureClass::ExternalServiceMcpSearchDaemonStall,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec![
            "in-process MCP proxy fake has deterministic delay and error scripting".into(),
            "read-only stale response fallback is explicitly policy-allowed".into(),
            "MCP proxy failure counters are captured before and after injection".into(),
        ],
        injected_fault:
            "MCP proxy remote_call sleeps past the timeout twice and returns a timeout error"
                .into(),
        observed_mitigation:
            "remote mutation stayed blocked, read-only cached response was served, and the proxy recovered after the scripted delay cleared"
                .into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: true,
            reason:
                "remote MCP mutation was blocked while stale cached read-only data was allowed by policy"
                    .into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.external_service.mcp_proxy.cached_degrade_recovered".into(),
            message:
                "MCP proxy stall used bounded retry/backoff, emitted diagnostics, and recovered"
                    .into(),
            severity: ResourcePressureDiagnosticSeverity::Warn,
        }],
        logs_path: Some("artifacts/resource-pressure/external-service/mcp-recoverable.jsonl".into()),
        proof_level: ResourcePressureProofLevel::ReducedLocal,
        skip_reason: None,
        status: ResourcePressureChaosStatus::Pass,
        assertions: vec![
            ResourcePressureAssertion::FailClosed,
            ResourcePressureAssertion::BoundedQueueGrowth,
            ResourcePressureAssertion::DiagnosticEmitted,
            ResourcePressureAssertion::MitigationLogged,
            ResourcePressureAssertion::RecoveryObserved,
        ],
        hardware_evidence: None,
        admission_observation: None,
        memory_observation: None,
        external_service_observation: Some(external_service_mcp_recoverable_observation()),
    }
}

/// Reduced external-service fixture for required audit storage fail-closed behavior.
pub fn external_service_policy_audit_fail_closed_reduced_pass_verdict()
-> ResourcePressureChaosVerdict {
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.5.external_service.policy_audit_fail_closed.reduced".into(),
        pressure_class: ResourcePressureClass::ExternalServiceMcpSearchDaemonStall,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec![
            "in-process policy/audit fake denies persistence after a deterministic delay".into(),
            "remote MCP write requires durable policy/audit evidence before dispatch".into(),
        ],
        injected_fault:
            "policy/audit dependency times out while a mutating MCP proxy action is pending".into(),
        observed_mitigation:
            "mutating MCP proxy action was denied until audit storage recovered and the denial was diagnosed"
                .into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: true,
            reason: "required audit persistence was unavailable, so the remote action was denied"
                .into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.external_service.policy_audit.fail_closed".into(),
            message: "required policy/audit dependency timed out and remote action was blocked"
                .into(),
            severity: ResourcePressureDiagnosticSeverity::Warn,
        }],
        logs_path: Some(
            "artifacts/resource-pressure/external-service/policy-audit-fail-closed.jsonl".into(),
        ),
        proof_level: ResourcePressureProofLevel::ReducedLocal,
        skip_reason: None,
        status: ResourcePressureChaosStatus::Pass,
        assertions: vec![
            ResourcePressureAssertion::FailClosed,
            ResourcePressureAssertion::BoundedQueueGrowth,
            ResourcePressureAssertion::DiagnosticEmitted,
            ResourcePressureAssertion::MitigationLogged,
            ResourcePressureAssertion::RecoveryObserved,
        ],
        hardware_evidence: None,
        admission_observation: None,
        memory_observation: None,
        external_service_observation: Some(external_service_policy_audit_fail_closed_observation()),
    }
}

/// Negative reduced fixture: external dependency waits escaped the declared budget.
pub fn external_service_unbounded_wait_fail_verdict() -> ResourcePressureChaosVerdict {
    ResourcePressureChaosVerdict {
        schema_version: RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        scenario_id: "ft-lmg3g.5.external_service.unbounded_wait_fail".into(),
        pressure_class: ResourcePressureClass::ExternalServiceMcpSearchDaemonStall,
        mode: ResourcePressureChaosMode::Reduced,
        preconditions: vec![
            "external dependency timeout guard intentionally disabled".into(),
            "policy/audit dependency is required for the pending remote action".into(),
        ],
        injected_fault:
            "search-daemon query stalls past the declared timeout while audit storage is unavailable"
                .into(),
        observed_mitigation:
            "remote action was allowed after an unbounded wait and no fail-closed denial occurred"
                .into(),
        fail_closed_decision: ResourcePressureFailClosedDecision {
            fail_closed: false,
            reason: "required policy/audit storage was unavailable but the remote action proceeded"
                .into(),
        },
        diagnostics: vec![ResourcePressureDiagnostic {
            code: "resource.search_daemon.unbounded_wait_fail_open".into(),
            message:
                "external search-daemon stall exceeded timeout/backoff budget and failed open".into(),
            severity: ResourcePressureDiagnosticSeverity::Error,
        }],
        logs_path: Some("artifacts/resource-pressure/external-service/unbounded-fail.jsonl".into()),
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
        memory_observation: None,
        external_service_observation: Some(external_service_unbounded_wait_observation()),
    }
}

/// High-scale external-service fixture that cannot claim proof without scale evidence.
pub fn external_service_high_scale_skipped_not_proven_verdict() -> ResourcePressureChaosVerdict {
    let mut verdict = external_service_mcp_recoverable_stall_reduced_pass_verdict();
    verdict.scenario_id = "ft-lmg3g.5.external_service.high_scale.skipped".into();
    verdict.mode = ResourcePressureChaosMode::HighScale;
    verdict.proof_level = ResourcePressureProofLevel::SimulatedHighScale;
    verdict.logs_path = None;
    verdict.status = ResourcePressureChaosStatus::SkippedNotProven;
    verdict.skip_reason = Some(
        "concurrent-agent fanout plus 64-core/256 GiB predicates absent; high-scale external-service proof not claimed"
            .into(),
    );
    verdict.hardware_evidence = Some(HighScaleHardwareEvidence::skipped(
        "concurrent-agent fanout and 64-core/256 GiB predicates absent",
    ));
    if let Some(observation) = verdict.external_service_observation.as_mut() {
        observation.call_queue.in_flight_before = 512;
        observation.call_queue.in_flight_after = 1_024;
        observation.call_queue.in_flight_bound = 2_048;
        observation.call_queue.queued_before = 1_024;
        observation.call_queue.queued_after = 1_536;
        observation.call_queue.queued_bound = 2_048;
        observation.call_queue.concurrent_agent_fanout = Some(4_096);
    }
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

fn memory_tiering_reduced_observation() -> ResourcePressureMemoryTierObservation {
    let tier_budget = memory_tiering_pressure_budget();
    ResourcePressureMemoryTierObservation::from_tier_budget_decision(
        tier_budget,
        ResourcePressureMemoryBytesObservation {
            hot_resident_before_bytes: 2_300,
            hot_resident_after_bytes: 2_100,
            warm_compressed_before_bytes: 2_400,
            warm_compressed_after_bytes: 2_100,
            cold_disk_before_bytes: 2_000,
            cold_disk_after_bytes: 2_300,
            cold_disk_growth_bound_bytes: 512,
            search_index_cache_before_bytes: 1_300,
            search_index_cache_after_bytes: 1_100,
            allocator_pool_before_bytes: 1_100,
            allocator_pool_after_bytes: 700,
            resident_after_bytes: 6_000,
        },
        MemoryPressureTier::Orange,
        BudgetLevel::OverBudget,
        128,
        "memory_tiering.reclaim_to_budget",
    )
}

fn memory_tiering_unbounded_fail_observation() -> ResourcePressureMemoryTierObservation {
    let tier_budget = memory_tiering_pressure_budget();
    ResourcePressureMemoryTierObservation::from_tier_budget_decision(
        tier_budget,
        ResourcePressureMemoryBytesObservation {
            hot_resident_before_bytes: 2_300,
            hot_resident_after_bytes: 2_450,
            warm_compressed_before_bytes: 2_400,
            warm_compressed_after_bytes: 2_650,
            cold_disk_before_bytes: 2_000,
            cold_disk_after_bytes: 3_200,
            cold_disk_growth_bound_bytes: 512,
            search_index_cache_before_bytes: 1_300,
            search_index_cache_after_bytes: 1_300,
            allocator_pool_before_bytes: 1_100,
            allocator_pool_after_bytes: 1_100,
            resident_after_bytes: 7_500,
        },
        MemoryPressureTier::Orange,
        BudgetLevel::OverBudget,
        128,
        "memory_tiering.mitigation_missing",
    )
}

fn memory_tiering_pressure_budget() -> FleetMemoryTierBudgetSnapshot {
    FleetMemoryTierBudgetSnapshot::from_tiers([
        FleetMemoryTierBudgetRecord::new(FleetMemoryTier::HotResident, 2_000, 2_300)
            .with_reclaimable_bytes(500),
        FleetMemoryTierBudgetRecord::new(FleetMemoryTier::WarmCompressed, 2_000, 2_400)
            .with_reclaimable_bytes(300),
        FleetMemoryTierBudgetRecord::new(FleetMemoryTier::ColdDisk, 10_000, 2_000)
            .with_reclaimable_bytes(0),
        FleetMemoryTierBudgetRecord::new(FleetMemoryTier::SearchIndexCache, 1_000, 1_300)
            .with_reclaimable_bytes(200),
        FleetMemoryTierBudgetRecord::new(FleetMemoryTier::AllocatorPools, 1_000, 1_100)
            .with_reclaimable_bytes(400),
    ])
}

fn external_service_mcp_recoverable_observation() -> ResourcePressureExternalServiceObservation {
    ResourcePressureExternalServiceObservation {
        dependency_name: "mcp_proxy.remote_call".into(),
        dependency_kind: ResourcePressureExternalDependencyKind::McpProxy,
        injected_latency_ms: 750,
        timeout_threshold_ms: 250,
        injected_error_code: Some("mcp_proxy.remote_timeout".into()),
        bounded_wait_ms: 820,
        retry_backoff: ResourcePressureRetryBackoffDecision {
            attempts_before_decision: 3,
            max_attempts: 3,
            backoff_delay_ms: 35,
            retry_budget_ms: 100,
            timed_out: true,
            retry_budget_exhausted: false,
            final_decision_code: "mcp_proxy.cached_degrade_recover".into(),
        },
        policy_audit_outcome: ResourcePressurePolicyAuditOutcome {
            audit_required: true,
            audit_available: true,
            fail_closed: true,
            remote_action_allowed: false,
            stale_cached_response_allowed: true,
            blocked_mutating_action: true,
            reason_code: "external_service.mcp_proxy.cached_read_only_degrade".into(),
        },
        call_queue: ResourcePressureExternalCallQueueObservation {
            in_flight_before: 4,
            in_flight_after: 5,
            in_flight_bound: 8,
            queued_before: 6,
            queued_after: 6,
            queued_bound: 8,
            concurrent_agent_fanout: Some(128),
        },
        mcp_proxy_failure_counter_before: Some(7),
        mcp_proxy_failure_counter_after: Some(8),
        degraded_to_stale_or_cached: true,
        recovered_after_fault_clear: true,
        operator_diagnostic_code: "resource.external_service.mcp_proxy.cached_degrade_recovered"
            .into(),
    }
}

fn external_service_policy_audit_fail_closed_observation()
-> ResourcePressureExternalServiceObservation {
    ResourcePressureExternalServiceObservation {
        dependency_name: "policy_audit_store.persist_mcp_decision".into(),
        dependency_kind: ResourcePressureExternalDependencyKind::PolicyAuditStore,
        injected_latency_ms: 500,
        timeout_threshold_ms: 200,
        injected_error_code: Some("policy_audit_store.timeout".into()),
        bounded_wait_ms: 420,
        retry_backoff: ResourcePressureRetryBackoffDecision {
            attempts_before_decision: 2,
            max_attempts: 2,
            backoff_delay_ms: 20,
            retry_budget_ms: 50,
            timed_out: true,
            retry_budget_exhausted: true,
            final_decision_code: "policy_audit.fail_closed_deny_remote_action".into(),
        },
        policy_audit_outcome: ResourcePressurePolicyAuditOutcome {
            audit_required: true,
            audit_available: false,
            fail_closed: true,
            remote_action_allowed: false,
            stale_cached_response_allowed: false,
            blocked_mutating_action: true,
            reason_code: "policy_audit.required_store_unavailable".into(),
        },
        call_queue: ResourcePressureExternalCallQueueObservation {
            in_flight_before: 2,
            in_flight_after: 2,
            in_flight_bound: 4,
            queued_before: 3,
            queued_after: 3,
            queued_bound: 4,
            concurrent_agent_fanout: Some(64),
        },
        mcp_proxy_failure_counter_before: None,
        mcp_proxy_failure_counter_after: None,
        degraded_to_stale_or_cached: false,
        recovered_after_fault_clear: true,
        operator_diagnostic_code: "resource.external_service.policy_audit.fail_closed".into(),
    }
}

fn external_service_unbounded_wait_observation() -> ResourcePressureExternalServiceObservation {
    ResourcePressureExternalServiceObservation {
        dependency_name: "search_daemon.query".into(),
        dependency_kind: ResourcePressureExternalDependencyKind::SearchDaemon,
        injected_latency_ms: 1_500,
        timeout_threshold_ms: 250,
        injected_error_code: Some("search_daemon.timeout".into()),
        bounded_wait_ms: 1_400,
        retry_backoff: ResourcePressureRetryBackoffDecision {
            attempts_before_decision: 2,
            max_attempts: 2,
            backoff_delay_ms: 50,
            retry_budget_ms: 100,
            timed_out: true,
            retry_budget_exhausted: true,
            final_decision_code: "search_daemon.fail_open_after_unbounded_wait".into(),
        },
        policy_audit_outcome: ResourcePressurePolicyAuditOutcome {
            audit_required: true,
            audit_available: false,
            fail_closed: false,
            remote_action_allowed: true,
            stale_cached_response_allowed: false,
            blocked_mutating_action: false,
            reason_code: "policy_audit.required_store_unavailable_but_allowed".into(),
        },
        call_queue: ResourcePressureExternalCallQueueObservation {
            in_flight_before: 8,
            in_flight_after: 12,
            in_flight_bound: 8,
            queued_before: 16,
            queued_after: 24,
            queued_bound: 16,
            concurrent_agent_fanout: Some(256),
        },
        mcp_proxy_failure_counter_before: None,
        mcp_proxy_failure_counter_after: None,
        degraded_to_stale_or_cached: false,
        recovered_after_fault_clear: false,
        operator_diagnostic_code: "resource.search_daemon.unbounded_wait_fail_open".into(),
    }
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
        ResourcePressureClass::ExternalServiceMcpSearchDaemonStall => {
            diagnostic.code.contains("external_service")
                || diagnostic.code.contains("mcp_proxy")
                || diagnostic.code.contains("search_daemon")
        }
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

    use crate::fleet_memory_controller::{
        FleetMemoryAction, FleetMemoryTier, FleetMemoryTierReclamationAction, FleetPressureTier,
    };
    use crate::memory_budget::BudgetLevel;
    use crate::memory_pressure::MemoryPressureTier;
    use crate::swarm_scheduler::{AdmissionAction, AdmissionReasonCode};

    use super::{
        HIGH_SCALE_REQUIRED_LOGICAL_CORES, HIGH_SCALE_REQUIRED_MEMORY_BYTES,
        HighScaleHardwareEvidence, RESOURCE_PRESSURE_CHAOS_SCHEMA_VERSION,
        ResourcePressureAssertion, ResourcePressureChaosMode, ResourcePressureChaosStatus,
        ResourcePressureChaosVerdict, ResourcePressureClass, ResourcePressureCoverageMatrix,
        ResourcePressureCoverageRow, ResourcePressureProofLevel,
        cpu_admission_high_scale_skipped_not_proven_verdict, cpu_admission_reduced_pass_verdict,
        external_service_high_scale_skipped_not_proven_verdict,
        external_service_mcp_recoverable_stall_reduced_pass_verdict,
        external_service_policy_audit_fail_closed_reduced_pass_verdict,
        external_service_unbounded_wait_fail_verdict,
        memory_tiering_high_scale_skipped_not_proven_verdict, memory_tiering_reduced_pass_verdict,
        memory_tiering_unbounded_fail_verdict, queue_saturation_reduced_pass_verdict,
        queue_saturation_unbounded_fail_verdict, sample_expected_blocked_by_infra_verdict,
        sample_fail_verdict, sample_pass_verdict, sample_skipped_not_proven_verdict,
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
    fn memory_tiering_reduced_fixture_records_reclaiming_decision() {
        let verdict = memory_tiering_reduced_pass_verdict();
        verdict
            .validate()
            .expect("memory-tiering fixture validates");

        let observation = verdict
            .memory_observation
            .as_ref()
            .expect("memory fixture records memory observation");
        assert_eq!(
            observation.resource_cockpit.memory_pressure_tier,
            MemoryPressureTier::Orange
        );
        assert_eq!(
            observation.resource_cockpit.worst_budget_level,
            BudgetLevel::OverBudget
        );
        assert_eq!(
            observation.resource_cockpit.compound_pressure_tier,
            FleetPressureTier::Critical
        );
        assert!(
            observation
                .controller_actions
                .contains(&FleetMemoryAction::EvictWarmScrollback)
        );
        assert!(
            observation
                .controller_actions
                .contains(&FleetMemoryAction::PauseIdlePanes)
        );
        assert!(
            observation
                .reclamation_targets
                .iter()
                .any(|target| target.tier == FleetMemoryTier::AllocatorPools
                    && target.action == FleetMemoryTierReclamationAction::TrimAllocatorPools),
            "allocator pressure should be represented by a real reclamation target"
        );
        assert_eq!(observation.bytes.resident_after_bytes, 6_000);
        assert_eq!(
            observation.bytes.resident_after_bytes,
            observation.tier_budget.totals.resident_budget_bytes
        );
        assert!(observation.bytes.cold_disk_growth_bounded());
    }

    #[test]
    fn negative_memory_tiering_fixture_is_valid_fail_not_coverage() {
        let verdict = memory_tiering_unbounded_fail_verdict();
        verdict
            .validate()
            .expect("negative memory fixture validates");

        let observation = verdict
            .memory_observation
            .as_ref()
            .expect("negative fixture records memory observation");
        assert!(
            observation.bytes.resident_after_bytes
                > observation.tier_budget.totals.resident_budget_bytes
        );
        assert!(!observation.bytes.cold_disk_growth_bounded());

        let assessment =
            ResourcePressureCoverageMatrix::default().assess_parent_completion(&[verdict]);
        let memory_status = assessment
            .row_statuses
            .iter()
            .find(|status| status.pressure_class == ResourcePressureClass::MemoryTiering)
            .expect("memory row status");
        assert!(!memory_status.satisfied);
        assert!(memory_status.reason.contains("FAIL"));
    }

    #[test]
    fn high_scale_memory_fixture_skips_without_256gb_evidence() {
        let verdict = memory_tiering_high_scale_skipped_not_proven_verdict();
        verdict
            .validate()
            .expect("high-scale skipped memory fixture validates");
        assert_eq!(
            verdict.status,
            ResourcePressureChaosStatus::SkippedNotProven
        );
        assert_eq!(
            verdict.proof_level,
            ResourcePressureProofLevel::SimulatedHighScale
        );
        let evidence = verdict
            .hardware_evidence
            .as_ref()
            .expect("hardware evidence");
        assert_eq!(
            evidence.required_memory_bytes,
            HIGH_SCALE_REQUIRED_MEMORY_BYTES
        );
        assert!(evidence.observed_memory_bytes.is_none());
        assert!(
            verdict.memory_observation.is_none(),
            "simulated high-scale skips must not masquerade as executed memory evidence"
        );

        let matrix = ResourcePressureCoverageMatrix::default();
        let assessment = matrix.assess_parent_completion(&[verdict]);
        let memory_status = assessment
            .row_statuses
            .iter()
            .find(|status| status.pressure_class == ResourcePressureClass::MemoryTiering)
            .expect("memory row status");
        assert!(!memory_status.satisfied);
        assert!(memory_status.reason.contains("SKIPPED_NOT_PROVEN"));
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
    fn skipped_memory_verdicts_do_not_require_memory_observation() {
        let mut verdict = memory_tiering_high_scale_skipped_not_proven_verdict();
        verdict.memory_observation = None;

        verdict
            .validate()
            .expect("skipped memory proof without execution evidence should validate");
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
    fn memory_pass_verdicts_require_memory_observation() {
        let mut verdict = memory_tiering_reduced_pass_verdict();
        verdict.memory_observation = None;

        let error = verdict
            .validate()
            .expect_err("missing memory observation must be rejected");
        assert!(error.to_string().contains("memory_observation"));
    }

    #[test]
    fn memory_pass_verdicts_reject_unbounded_resident_or_cold_growth() {
        let mut resident = memory_tiering_reduced_pass_verdict();
        let observation = resident
            .memory_observation
            .as_mut()
            .expect("memory fixture has observation");
        observation.bytes.resident_after_bytes =
            observation.tier_budget.totals.resident_budget_bytes + 1;
        observation.resource_cockpit.resident_after_bytes = observation.bytes.resident_after_bytes;
        observation
            .resource_cockpit
            .resident_over_budget_after_bytes = 1;

        let resident_error = resident
            .validate()
            .expect_err("over-budget resident memory must be rejected");
        assert!(resident_error.to_string().contains("resident_after_bytes"));

        let mut cold = memory_tiering_reduced_pass_verdict();
        let observation = cold
            .memory_observation
            .as_mut()
            .expect("memory fixture has observation");
        observation.bytes.cold_disk_after_bytes = observation.bytes.cold_disk_before_bytes
            + observation.bytes.cold_disk_growth_bound_bytes
            + 1;

        let cold_error = cold
            .validate()
            .expect_err("unbounded cold-tier growth must be rejected");
        assert!(cold_error.to_string().contains("cold_disk_after_bytes"));
    }

    #[test]
    fn external_service_recoverable_fixture_records_cached_degrade_and_recovery() {
        let verdict = external_service_mcp_recoverable_stall_reduced_pass_verdict();
        verdict
            .validate()
            .expect("external MCP stall fixture validates");

        let observation = verdict
            .external_service_observation
            .as_ref()
            .expect("external fixture records dependency observation");
        assert_eq!(observation.dependency_name, "mcp_proxy.remote_call");
        assert!(observation.bounded_wait_observed());
        assert!(observation.records_mcp_proxy_failure());
        assert!(observation.call_queue.bounded_after_injection());
        assert!(observation.degraded_to_stale_or_cached);
        assert!(observation.recovered_after_fault_clear);
        assert!(!observation.policy_audit_outcome.remote_action_allowed);
    }

    #[test]
    fn external_service_policy_audit_fixture_blocks_required_remote_action() {
        let verdict = external_service_policy_audit_fail_closed_reduced_pass_verdict();
        verdict
            .validate()
            .expect("policy/audit external fixture validates");

        let observation = verdict
            .external_service_observation
            .as_ref()
            .expect("policy fixture records dependency observation");
        assert!(observation.policy_audit_outcome.audit_required);
        assert!(!observation.policy_audit_outcome.audit_available);
        assert!(observation.policy_audit_outcome.fail_closed);
        assert!(observation.policy_audit_outcome.blocked_mutating_action);
        assert!(!observation.policy_audit_outcome.remote_action_allowed);
    }

    #[test]
    fn negative_external_service_fixture_is_valid_fail_not_coverage() {
        let verdict = external_service_unbounded_wait_fail_verdict();
        verdict
            .validate()
            .expect("negative external-service fixture validates");

        let observation = verdict
            .external_service_observation
            .as_ref()
            .expect("negative fixture records dependency observation");
        assert!(!observation.bounded_wait_observed());
        assert!(!observation.call_queue.bounded_after_injection());
        assert!(observation.policy_audit_outcome.remote_action_allowed);

        let assessment =
            ResourcePressureCoverageMatrix::default().assess_parent_completion(&[verdict]);
        let external_status = assessment
            .row_statuses
            .iter()
            .find(|status| {
                status.pressure_class == ResourcePressureClass::ExternalServiceMcpSearchDaemonStall
            })
            .expect("external row status");
        assert!(!external_status.satisfied);
        assert!(external_status.reason.contains("FAIL"));
    }

    #[test]
    fn high_scale_external_service_fixture_skips_without_fanout_hardware_proof() {
        let verdict = external_service_high_scale_skipped_not_proven_verdict();
        verdict
            .validate()
            .expect("high-scale skipped external fixture validates");
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
                .skip_reason
                .as_deref()
                .expect("skip reason")
                .contains("concurrent-agent fanout")
        );
        assert_eq!(
            verdict
                .external_service_observation
                .as_ref()
                .and_then(|observation| observation.call_queue.concurrent_agent_fanout),
            Some(4_096)
        );

        let assessment =
            ResourcePressureCoverageMatrix::default().assess_parent_completion(&[verdict]);
        let external_status = assessment
            .row_statuses
            .iter()
            .find(|status| {
                status.pressure_class == ResourcePressureClass::ExternalServiceMcpSearchDaemonStall
            })
            .expect("external row status");
        assert!(!external_status.satisfied);
        assert!(external_status.reason.contains("SKIPPED_NOT_PROVEN"));
    }

    #[test]
    fn skipped_external_service_verdicts_do_not_require_dependency_observation() {
        let mut verdict = external_service_high_scale_skipped_not_proven_verdict();
        verdict.external_service_observation = None;

        verdict
            .validate()
            .expect("skipped external proof without execution evidence should validate");
    }

    #[test]
    fn external_service_pass_verdicts_require_dependency_observation() {
        let mut verdict = external_service_mcp_recoverable_stall_reduced_pass_verdict();
        verdict.external_service_observation = None;

        let error = verdict
            .validate()
            .expect_err("missing external-service observation must be rejected");
        assert!(error.to_string().contains("external_service_observation"));
    }

    #[test]
    fn external_service_pass_verdicts_reject_unbounded_wait_or_audit_fail_open() {
        let mut unbounded = external_service_mcp_recoverable_stall_reduced_pass_verdict();
        let observation = unbounded
            .external_service_observation
            .as_mut()
            .expect("external fixture has observation");
        observation.bounded_wait_ms = observation.declared_wait_bound_ms() + 1;

        let unbounded_error = unbounded
            .validate()
            .expect_err("unbounded external wait must be rejected");
        assert!(unbounded_error.to_string().contains("bounded_wait_ms"));

        let mut fail_open = external_service_policy_audit_fail_closed_reduced_pass_verdict();
        let observation = fail_open
            .external_service_observation
            .as_mut()
            .expect("policy fixture has observation");
        observation.policy_audit_outcome.fail_closed = false;
        observation.policy_audit_outcome.remote_action_allowed = true;
        observation.policy_audit_outcome.blocked_mutating_action = false;

        let fail_open_error = fail_open
            .validate()
            .expect_err("required audit storage fail-open must be rejected");
        assert!(
            fail_open_error
                .to_string()
                .contains("remote_action_allowed")
        );
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
        verdict.memory_observation = match row.pressure_class {
            ResourcePressureClass::MemoryTiering => {
                memory_tiering_reduced_pass_verdict().memory_observation
            }
            _ => None,
        };
        verdict.external_service_observation = match row.pressure_class {
            ResourcePressureClass::ExternalServiceMcpSearchDaemonStall => {
                external_service_mcp_recoverable_stall_reduced_pass_verdict()
                    .external_service_observation
            }
            _ => None,
        };
        verdict.diagnostics = match row.pressure_class {
            ResourcePressureClass::CpuAdmission => cpu_admission_reduced_pass_verdict().diagnostics,
            ResourcePressureClass::MemoryTiering => {
                memory_tiering_reduced_pass_verdict().diagnostics
            }
            ResourcePressureClass::ExternalServiceMcpSearchDaemonStall => {
                external_service_mcp_recoverable_stall_reduced_pass_verdict().diagnostics
            }
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
