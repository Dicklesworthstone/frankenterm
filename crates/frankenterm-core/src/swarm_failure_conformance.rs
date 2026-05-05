//! Swarm failure-mode conformance matrix for chaos/recovery proof lanes.
//!
//! This module is deliberately a small report surface. It maps the failure
//! modes from `ft-bsfb9.4` to stable scenario identifiers, receipt/error codes,
//! recovery behavior, and proof commands. Live destructive cases stay
//! `SKIPPED_NOT_PROVEN` until a real fixture is attached; the reduced
//! event-storm row is anchored to the existing resource-pressure chaos runner.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::resource_pressure_chaos::ResourcePressureClass;
use crate::resource_pressure_chaos_runner::resource_pressure_chaos_reduced_report;

/// Current schema version for the swarm failure conformance report.
pub const SWARM_FAILURE_CONFORMANCE_SCHEMA_VERSION: u32 = 1;

/// Focused RCH command for the reduced conformance lab.
pub const SWARM_FAILURE_CONFORMANCE_RCH_COMMAND: &str = "rch exec -- bash -lc 'CARGO_TARGET_DIR=/tmp/ft-bsfb9-chaos-recovery cargo test -p frankenterm-core --test swarm_failure_conformance -- --nocapture'";

/// Failure mode covered by the conformance matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmFailureMode {
    /// An agent process exits while assigned work is still live.
    AgentProcessExit,
    /// An agent enters a rate-limit or quota stall.
    RateLimitStall,
    /// A pane disappears before pending work finishes.
    PaneDisappearance,
    /// The mux runtime is unavailable or restarting.
    MuxUnavailable,
    /// Storage writes are blocked by a lock or writer stall.
    StorageLockContention,
    /// Agent Mail is degraded, red, or read-only.
    AgentMailDegraded,
    /// RCH worker execution drops mid-proof or before dispatch.
    RchWorkerDrop,
    /// Runtime events saturate a queue or fanout path.
    EventStormSaturation,
}

impl SwarmFailureMode {
    /// Stable machine string for the failure mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentProcessExit => "agent_process_exit",
            Self::RateLimitStall => "rate_limit_stall",
            Self::PaneDisappearance => "pane_disappearance",
            Self::MuxUnavailable => "mux_unavailable",
            Self::StorageLockContention => "storage_lock_contention",
            Self::AgentMailDegraded => "agent_mail_degraded",
            Self::RchWorkerDrop => "rch_worker_drop",
            Self::EventStormSaturation => "event_storm_saturation",
        }
    }
}

impl fmt::Display for SwarmFailureMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Scenario execution status for one failure-mode row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmFailureScenarioStatus {
    /// Scenario executed and its conformance assertions passed.
    Pass,
    /// Scenario is intentionally skipped because required live predicates are absent.
    SkippedNotProven,
    /// Known infrastructure blocks execution; this is not a code pass.
    ExpectedBlockedByInfra,
    /// Scenario executed and failed its required assertions.
    Fail,
}

impl SwarmFailureScenarioStatus {
    /// Stable operator label for this status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::SkippedNotProven => "SKIPPED_NOT_PROVEN",
            Self::ExpectedBlockedByInfra => "EXPECTED_BLOCKED_BY_INFRA",
            Self::Fail => "FAIL",
        }
    }
}

impl fmt::Display for SwarmFailureScenarioStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Evidence strength backing a conformance row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmFailureProofLevel {
    /// Reduced deterministic in-process evidence.
    ReducedInProcess,
    /// Recorded fixture or transcript evidence.
    RecordedFixture,
    /// Requires a live external dependency to execute.
    LiveExternalDependency,
    /// Requires target-class high-scale hardware.
    RealHighScale,
}

/// Expected recovery behavior for a conformance row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmFailureRecoveryBehavior {
    /// The fault clears and the scenario observes recovery.
    RecoverAfterFaultClear,
    /// The system exposes a degraded, read-only, or cached result.
    DegradedReadOnly,
    /// The system denies or blocks unsafe work.
    FailClosed,
    /// Retry/backoff remains bounded.
    BoundedRetry,
    /// Live fixture is still needed before recovery can be claimed.
    SkippedUntilLiveFixture,
}

/// Structured log phase emitted by a scenario row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmFailureLogPhase {
    /// Scenario setup phase.
    Setup,
    /// Fault injection phase.
    InjectedFault,
    /// Expected degraded behavior phase.
    ExpectedDegradedBehavior,
    /// Recovery signal phase.
    RecoverySignal,
    /// Final invariant check phase.
    FinalInvariantCheck,
}

/// One conformance scenario row, including structured log fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmFailureConformanceScenario {
    /// Stable scenario identifier.
    pub scenario_id: String,
    /// Failure mode covered by this row.
    pub failure_mode: SwarmFailureMode,
    /// Scenario execution/proof status.
    pub status: SwarmFailureScenarioStatus,
    /// Evidence strength backing this row.
    pub proof_level: SwarmFailureProofLevel,
    /// Short setup description emitted into structured logs.
    pub setup: String,
    /// Injected fault emitted into structured logs.
    pub injected_fault: String,
    /// Expected degraded behavior emitted into structured logs.
    pub expected_degraded_behavior: String,
    /// Recovery signal emitted into structured logs.
    pub recovery_signal: String,
    /// Final invariant checks emitted into structured logs.
    pub final_invariant_checks: Vec<String>,
    /// Stable receipt code expected from this scenario.
    pub expected_receipt_code: String,
    /// Stable error code for degraded or blocked behavior.
    pub expected_error_code: Option<String>,
    /// Expected recovery behavior.
    pub recovery_behavior: SwarmFailureRecoveryBehavior,
    /// Exact proof command to run for this row.
    pub proof_command: String,
    /// Artifact path or payload expected from the proof.
    pub artifact_hint: String,
    /// Existing proof surface this row reuses, when any.
    pub evidence_anchor: Option<String>,
    /// Reason the row is skipped or blocked, when applicable.
    pub skip_reason: Option<String>,
}

/// One machine-readable structured log record for a conformance scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmFailureStructuredLogRecord {
    /// Report schema version.
    pub schema_version: u32,
    /// Stable scenario identifier.
    pub scenario_id: String,
    /// Failure mode covered by the scenario.
    pub failure_mode: SwarmFailureMode,
    /// Log phase.
    pub phase: SwarmFailureLogPhase,
    /// Scenario status.
    pub status: SwarmFailureScenarioStatus,
    /// Stable receipt code.
    pub receipt_code: String,
    /// Typed degraded/blocking error code.
    pub error_code: Option<String>,
    /// Human-readable structured message for the phase.
    pub message: String,
}

impl SwarmFailureConformanceScenario {
    /// Whether the row has enough fields to emit structured setup/fault/recovery logs.
    #[must_use]
    pub fn has_structured_log_fields(&self) -> bool {
        !self.setup.trim().is_empty()
            && !self.injected_fault.trim().is_empty()
            && !self.expected_degraded_behavior.trim().is_empty()
            && !self.recovery_signal.trim().is_empty()
            && !self.final_invariant_checks.is_empty()
            && self
                .final_invariant_checks
                .iter()
                .all(|check| !check.trim().is_empty())
    }

    /// Return validation errors for this scenario.
    #[must_use]
    pub fn validation_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if self.scenario_id.trim().is_empty() {
            errors.push(format!("{} scenario_id is blank", self.failure_mode));
        }
        if !self.expected_receipt_code.starts_with("swarm.failure.") {
            errors.push(format!(
                "{} receipt code must start with swarm.failure.",
                self.failure_mode
            ));
        }
        if self
            .expected_error_code
            .as_deref()
            .is_none_or(str::is_empty)
        {
            errors.push(format!(
                "{} must carry a typed degraded/blocking error code",
                self.failure_mode
            ));
        }
        if !self.proof_command.contains("rch exec")
            || !self
                .proof_command
                .contains("cargo test -p frankenterm-core")
        {
            errors.push(format!(
                "{} proof command must route through RCH cargo execution",
                self.failure_mode
            ));
        }
        if !self.has_structured_log_fields() {
            errors.push(format!(
                "{} must declare setup, injected fault, degraded behavior, recovery, and invariants",
                self.failure_mode
            ));
        }
        if self.status == SwarmFailureScenarioStatus::Pass {
            if self.skip_reason.is_some() {
                errors.push(format!(
                    "{} pass row cannot have skip_reason",
                    self.failure_mode
                ));
            }
            if self.evidence_anchor.is_none() {
                errors.push(format!(
                    "{} pass row must reference an existing evidence anchor",
                    self.failure_mode
                ));
            }
        } else if self.skip_reason.as_deref().is_none_or(str::is_empty) {
            errors.push(format!(
                "{} non-pass row must explain why it is not proven",
                self.failure_mode
            ));
        }

        errors
    }

    /// Whether this row validates cleanly.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.validation_errors().is_empty()
    }

    /// Emit structured log records for setup, fault, degradation, recovery, and invariants.
    #[must_use]
    pub fn structured_log_records(&self) -> Vec<SwarmFailureStructuredLogRecord> {
        let base = |phase, message: String| SwarmFailureStructuredLogRecord {
            schema_version: SWARM_FAILURE_CONFORMANCE_SCHEMA_VERSION,
            scenario_id: self.scenario_id.clone(),
            failure_mode: self.failure_mode,
            phase,
            status: self.status,
            receipt_code: self.expected_receipt_code.clone(),
            error_code: self.expected_error_code.clone(),
            message,
        };

        let mut records = vec![
            base(SwarmFailureLogPhase::Setup, self.setup.clone()),
            base(
                SwarmFailureLogPhase::InjectedFault,
                self.injected_fault.clone(),
            ),
            base(
                SwarmFailureLogPhase::ExpectedDegradedBehavior,
                self.expected_degraded_behavior.clone(),
            ),
            base(
                SwarmFailureLogPhase::RecoverySignal,
                self.recovery_signal.clone(),
            ),
        ];
        records.extend(
            self.final_invariant_checks
                .iter()
                .cloned()
                .map(|check| base(SwarmFailureLogPhase::FinalInvariantCheck, check)),
        );
        records
    }
}

/// Human-readable coverage row included in the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmFailureCoverageRow {
    /// Failure mode covered by this row.
    pub failure_mode: SwarmFailureMode,
    /// Stable scenario identifier for the row.
    pub scenario_id: String,
    /// Current row status.
    pub status: SwarmFailureScenarioStatus,
    /// Receipt code expected for the row.
    pub expected_receipt_code: String,
    /// Error code expected for degraded or blocked behavior.
    pub expected_error_code: Option<String>,
    /// Expected recovery behavior.
    pub recovery_behavior: SwarmFailureRecoveryBehavior,
    /// Evidence strength backing the row.
    pub proof_level: SwarmFailureProofLevel,
    /// Existing proof surface reused by the row, when any.
    pub evidence_anchor: Option<String>,
    /// Reason this row is skipped or blocked.
    pub skip_reason: Option<String>,
}

/// Aggregated swarm failure conformance report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmFailureConformanceReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Full scenario rows.
    pub scenarios: Vec<SwarmFailureConformanceScenario>,
    /// Matrix rows suitable for operator docs or Robot Mode fixtures.
    pub coverage_matrix: Vec<SwarmFailureCoverageRow>,
    /// Structured log records emitted by each scenario.
    pub structured_logs: Vec<SwarmFailureStructuredLogRecord>,
    /// Exact command for the focused reduced proof.
    pub reduced_proof_command: String,
    /// Number of reduced rows that actually pass.
    pub reduced_pass_count: usize,
    /// Whether every matrix row has a scenario.
    pub all_failure_modes_mapped: bool,
    /// Whether any row incorrectly claims high-scale proof.
    pub high_scale_proven: bool,
    /// Report validation errors.
    pub validation_errors: Vec<String>,
    /// Top-level diagnostics for operators.
    pub diagnostics: Vec<String>,
}

impl SwarmFailureConformanceReport {
    /// Whether this report is valid reduced closeout evidence for the conformance lab.
    #[must_use]
    pub fn reduced_lab_ready(&self) -> bool {
        self.validation_errors.is_empty()
            && self.all_failure_modes_mapped
            && self.reduced_pass_count >= 1
            && !self.high_scale_proven
    }
}

/// Build the default conformance report.
#[must_use]
pub fn swarm_failure_conformance_report() -> SwarmFailureConformanceReport {
    let scenarios = swarm_failure_conformance_scenarios();
    let validation_errors = scenarios
        .iter()
        .flat_map(SwarmFailureConformanceScenario::validation_errors)
        .collect::<Vec<_>>();
    let coverage_matrix = scenarios
        .iter()
        .map(|scenario| SwarmFailureCoverageRow {
            failure_mode: scenario.failure_mode,
            scenario_id: scenario.scenario_id.clone(),
            status: scenario.status,
            expected_receipt_code: scenario.expected_receipt_code.clone(),
            expected_error_code: scenario.expected_error_code.clone(),
            recovery_behavior: scenario.recovery_behavior,
            proof_level: scenario.proof_level,
            evidence_anchor: scenario.evidence_anchor.clone(),
            skip_reason: scenario.skip_reason.clone(),
        })
        .collect::<Vec<_>>();
    let reduced_pass_count = scenarios
        .iter()
        .filter(|scenario| {
            scenario.status == SwarmFailureScenarioStatus::Pass
                && scenario.proof_level == SwarmFailureProofLevel::ReducedInProcess
        })
        .count();
    let all_failure_modes_mapped = all_failure_modes_mapped(&scenarios);
    let high_scale_proven = scenarios.iter().any(|scenario| {
        scenario.status == SwarmFailureScenarioStatus::Pass
            && scenario.proof_level == SwarmFailureProofLevel::RealHighScale
    });
    let structured_logs = scenarios
        .iter()
        .flat_map(SwarmFailureConformanceScenario::structured_log_records)
        .collect::<Vec<_>>();
    let diagnostics = report_diagnostics(
        &scenarios,
        all_failure_modes_mapped,
        reduced_pass_count,
        high_scale_proven,
    );

    SwarmFailureConformanceReport {
        schema_version: SWARM_FAILURE_CONFORMANCE_SCHEMA_VERSION,
        scenarios,
        coverage_matrix,
        structured_logs,
        reduced_proof_command: SWARM_FAILURE_CONFORMANCE_RCH_COMMAND.into(),
        reduced_pass_count,
        all_failure_modes_mapped,
        high_scale_proven,
        validation_errors,
        diagnostics,
    }
}

/// Build all scenario rows for the conformance matrix.
#[must_use]
pub fn swarm_failure_conformance_scenarios() -> Vec<SwarmFailureConformanceScenario> {
    vec![
        skipped_live_scenario(
            SwarmFailureMode::AgentProcessExit,
            "ft-bsfb9.4.agent_process_exit.live.skipped",
            "spawned agent process with owned work and heartbeat stream",
            "agent process exits before work completion receipt",
            "ownership must become stale/claimable with an explicit recovery receipt",
            "requires live subprocess fixture that can exit without disrupting other agents",
            "swarm.failure.agent_process_exit.recovery_required",
            "SWARM-AGENT-EXIT-SKIPPED",
            SwarmFailureRecoveryBehavior::SkippedUntilLiveFixture,
            "live process-exit harness is not yet attached",
        ),
        skipped_live_scenario(
            SwarmFailureMode::RateLimitStall,
            "ft-bsfb9.4.rate_limit_stall.fixture.skipped",
            "agent transcript contains a rate-limit stall marker and pending work",
            "rate-limit state persists beyond the bounded progress window",
            "scheduler must pause or reassign without marking the work successful",
            "requires recorded stall transcript or live detector fixture",
            "swarm.failure.rate_limit_stall.degraded",
            "SWARM-RATE-LIMIT-SKIPPED",
            SwarmFailureRecoveryBehavior::BoundedRetry,
            "recorded rate-limit transcript fixture is not yet attached",
        ),
        skipped_live_scenario(
            SwarmFailureMode::PaneDisappearance,
            "ft-bsfb9.4.pane_disappearance.live.skipped",
            "mux pane exists with pending read/write expectations",
            "pane disappears before acknowledgement or text read completes",
            "caller must receive pane-gone receipt instead of silent success",
            "requires live pane lifecycle fixture",
            "swarm.failure.pane_disappearance.fail_closed",
            "SWARM-PANE-GONE-SKIPPED",
            SwarmFailureRecoveryBehavior::FailClosed,
            "live pane disappearance fixture is not yet attached",
        ),
        skipped_live_scenario(
            SwarmFailureMode::MuxUnavailable,
            "ft-bsfb9.4.mux_unavailable.live.skipped",
            "mux client is reachable before the fault",
            "mux read/write surface becomes unavailable or restarts",
            "Robot/MCP callers must receive typed mux-unavailable receipts",
            "requires isolated mux fixture; do not restart the shared GUI/runtime",
            "swarm.failure.mux_unavailable.fail_closed",
            "SWARM-MUX-UNAVAILABLE-SKIPPED",
            SwarmFailureRecoveryBehavior::FailClosed,
            "isolated mux-unavailable fixture is not yet attached",
        ),
        skipped_live_scenario(
            SwarmFailureMode::StorageLockContention,
            "ft-bsfb9.4.storage_lock_contention.live.skipped",
            "isolated storage database accepts writes before contention",
            "writer lock stays held past the bounded operation budget",
            "writes must fail closed or degrade with a storage-lock receipt",
            "requires isolated storage lock fixture",
            "swarm.failure.storage_lock_contention.fail_closed",
            "SWARM-STORAGE-LOCK-SKIPPED",
            SwarmFailureRecoveryBehavior::FailClosed,
            "isolated storage lock fixture is not yet attached",
        ),
        skipped_live_scenario(
            SwarmFailureMode::AgentMailDegraded,
            "ft-bsfb9.4.agent_mail_degraded.live.skipped",
            "Agent Mail contact or inbox check is available before the fault",
            "Agent Mail returns degraded, red, or read-only state",
            "coordination must degrade to Beads/local receipts without repair attempts",
            "requires live Agent Mail fixture and must not restart the shared service",
            "swarm.failure.agent_mail_degraded.read_only",
            "SWARM-AM-DEGRADED-SKIPPED",
            SwarmFailureRecoveryBehavior::DegradedReadOnly,
            "safe Agent Mail degraded-mode fixture is not yet attached",
        ),
        skipped_live_scenario(
            SwarmFailureMode::RchWorkerDrop,
            "ft-bsfb9.4.rch_worker_drop.live.skipped",
            "RCH worker is reachable before proof dispatch",
            "worker drops or rejects a dispatched proof command",
            "proof status must be infra-blocked or skipped, never local fallback success",
            "requires controlled RCH worker-drop fixture",
            "swarm.failure.rch_worker_drop.infra_blocked",
            "SWARM-RCH-WORKER-DROP-SKIPPED",
            SwarmFailureRecoveryBehavior::FailClosed,
            "controlled RCH worker-drop fixture is not yet attached",
        ),
        event_storm_saturation_scenario(),
    ]
}

fn skipped_live_scenario(
    failure_mode: SwarmFailureMode,
    scenario_id: &str,
    setup: &str,
    injected_fault: &str,
    expected_degraded_behavior: &str,
    recovery_signal: &str,
    receipt_code: &str,
    error_code: &str,
    recovery_behavior: SwarmFailureRecoveryBehavior,
    skip_reason: &str,
) -> SwarmFailureConformanceScenario {
    SwarmFailureConformanceScenario {
        scenario_id: scenario_id.into(),
        failure_mode,
        status: SwarmFailureScenarioStatus::SkippedNotProven,
        proof_level: SwarmFailureProofLevel::LiveExternalDependency,
        setup: setup.into(),
        injected_fault: injected_fault.into(),
        expected_degraded_behavior: expected_degraded_behavior.into(),
        recovery_signal: recovery_signal.into(),
        final_invariant_checks: vec![
            "typed receipt required before this row can pass".into(),
            "silent success is forbidden while the live fixture is absent".into(),
        ],
        expected_receipt_code: receipt_code.into(),
        expected_error_code: Some(error_code.into()),
        recovery_behavior,
        proof_command: SWARM_FAILURE_CONFORMANCE_RCH_COMMAND.into(),
        artifact_hint: format!(
            "artifacts/swarm-failure/{}/reduced.jsonl",
            failure_mode.as_str()
        ),
        evidence_anchor: None,
        skip_reason: Some(skip_reason.into()),
    }
}

fn event_storm_saturation_scenario() -> SwarmFailureConformanceScenario {
    let report = resource_pressure_chaos_reduced_report();
    let queue_row = report
        .coverage_matrix
        .iter()
        .find(|row| row.pressure_class == ResourcePressureClass::QueueSaturation);
    let queue_satisfied = queue_row.is_some_and(|row| row.satisfied);
    let queue_reason = queue_row.map_or_else(
        || "resource-pressure queue_saturation row missing".to_string(),
        |row| row.reason.clone(),
    );

    SwarmFailureConformanceScenario {
        scenario_id: "ft-bsfb9.4.event_storm_saturation.reduced.pass".into(),
        failure_mode: SwarmFailureMode::EventStormSaturation,
        status: if queue_satisfied {
            SwarmFailureScenarioStatus::Pass
        } else {
            SwarmFailureScenarioStatus::Fail
        },
        proof_level: SwarmFailureProofLevel::ReducedInProcess,
        setup: "resource-pressure reduced suite creates bounded queue-saturation pressure".into(),
        injected_fault: "event fanout exceeds the reduced queue bound".into(),
        expected_degraded_behavior: "admission degrades or sheds work with bounded queue growth"
            .into(),
        recovery_signal: "queue pressure drains and the row remains satisfied after mitigation"
            .into(),
        final_invariant_checks: vec![
            queue_reason,
            "no silent success while queue saturation is active".into(),
            "operator diagnostic and mitigation reason code are present".into(),
        ],
        expected_receipt_code: "swarm.failure.event_storm_saturation.bounded_degrade".into(),
        expected_error_code: Some("SWARM-EVENT-STORM-DEGRADED".into()),
        recovery_behavior: SwarmFailureRecoveryBehavior::RecoverAfterFaultClear,
        proof_command: SWARM_FAILURE_CONFORMANCE_RCH_COMMAND.into(),
        artifact_hint: "artifacts/swarm-failure/event_storm_saturation/reduced.jsonl".into(),
        evidence_anchor: Some("resource_pressure_chaos.queue_saturation".into()),
        skip_reason: None,
    }
}

fn all_failure_modes_mapped(scenarios: &[SwarmFailureConformanceScenario]) -> bool {
    let present = scenarios
        .iter()
        .map(|scenario| scenario.failure_mode)
        .collect::<BTreeSet<_>>();
    let expected = all_failure_modes().into_iter().collect::<BTreeSet<_>>();
    present == expected
}

fn all_failure_modes() -> [SwarmFailureMode; 8] {
    [
        SwarmFailureMode::AgentProcessExit,
        SwarmFailureMode::RateLimitStall,
        SwarmFailureMode::PaneDisappearance,
        SwarmFailureMode::MuxUnavailable,
        SwarmFailureMode::StorageLockContention,
        SwarmFailureMode::AgentMailDegraded,
        SwarmFailureMode::RchWorkerDrop,
        SwarmFailureMode::EventStormSaturation,
    ]
}

fn report_diagnostics(
    scenarios: &[SwarmFailureConformanceScenario],
    all_failure_modes_mapped: bool,
    reduced_pass_count: usize,
    high_scale_proven: bool,
) -> Vec<String> {
    let mut diagnostics = Vec::new();

    if !all_failure_modes_mapped {
        diagnostics.push("one or more required swarm failure modes are missing".into());
    }
    if reduced_pass_count == 0 {
        diagnostics.push("no bounded reduced scenario has passed yet".into());
    }
    if high_scale_proven {
        diagnostics.push("unexpected high-scale proof claim in reduced conformance report".into());
    }
    diagnostics.extend(
        scenarios
            .iter()
            .filter(|scenario| scenario.status != SwarmFailureScenarioStatus::Pass)
            .map(|scenario| {
                format!(
                    "{} remains {}: {}",
                    scenario.failure_mode,
                    scenario.status,
                    scenario
                        .skip_reason
                        .as_deref()
                        .unwrap_or("no reason recorded")
                )
            }),
    );

    diagnostics
}
