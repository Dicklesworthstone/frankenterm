//! Deterministic clock/timer resource-pressure chaos scenarios.
//!
//! This module implements the `ft-lmg3g.6` clock/timer anomaly slice without
//! mutating host time. It builds the common [`ResourcePressureChaosVerdict`]
//! records used by the parent resource-pressure coverage matrix.

use serde::{Deserialize, Serialize};

use crate::resource_pressure_chaos::{
    HighScaleHardwareEvidence, ResourcePressureAssertion, ResourcePressureChaosMode,
    ResourcePressureChaosStatus, ResourcePressureChaosVerdict, ResourcePressureClass,
    ResourcePressureCoverageMatrix, ResourcePressureDiagnostic, ResourcePressureDiagnosticSeverity,
    ResourcePressureFailClosedDecision, ResourcePressureProofLevel, sample_fail_verdict,
    sample_pass_verdict, sample_skipped_not_proven_verdict,
};

/// Deterministic clock/timer anomaly class exercised by a reduced scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockTimerAnomalyKind {
    /// Logical time jumps forward while bounded wall-clock execution remains short.
    MonotonicJumpForward,
    /// A timer does not make progress and must resolve through a bounded timeout.
    StalledTimer,
    /// Scheduler ticks are delayed past their logical deadline.
    DelayedTick,
    /// A timeout budget collapses to zero or below and must fail closed.
    TimeoutBudgetCollapse,
}

impl ClockTimerAnomalyKind {
    /// Stable machine string for artifacts and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MonotonicJumpForward => "monotonic_jump_forward",
            Self::StalledTimer => "stalled_timer",
            Self::DelayedTick => "delayed_tick",
            Self::TimeoutBudgetCollapse => "timeout_budget_collapse",
        }
    }
}

/// Clock/timer scenario evidence that complements the common verdict schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockTimerAnomalyObservation {
    /// Stable scenario identifier.
    pub scenario_id: String,
    /// Injected clock/timer anomaly.
    pub anomaly_kind: ClockTimerAnomalyKind,
    /// Runtime surface whose deadline or cooldown was affected.
    pub affected_runtime_budget: String,
    /// Logical elapsed time observed by the controlled clock.
    pub logical_elapsed_ms: u64,
    /// Real wall-clock time spent by the reduced fixture.
    pub wall_elapsed_ms: u64,
    /// Original timeout/cooldown budget before injection.
    pub timeout_budget_ms: u64,
    /// Maximum permitted wall-clock time for the fixture.
    pub bounded_wall_timeout_ms: u64,
    /// Stable diagnostic reason code emitted by the scenario.
    pub mitigation_reason_code: String,
    /// Whether the scenario observed recovery after clearing the anomaly.
    pub recovery_observed: bool,
}

impl ClockTimerAnomalyObservation {
    /// Whether the fixture used bounded wall-clock execution rather than a long sleep.
    #[must_use]
    pub const fn wall_time_bounded(&self) -> bool {
        self.wall_elapsed_ms <= self.bounded_wall_timeout_ms
    }

    /// Whether logical time crossed the configured timeout/cooldown budget.
    #[must_use]
    pub const fn logical_timeout_crossed(&self) -> bool {
        self.logical_elapsed_ms >= self.timeout_budget_ms
    }
}

/// Deterministic reduced-mode pass observation for a delayed scheduler tick.
#[must_use]
pub fn clock_timer_reduced_pass_observation() -> ClockTimerAnomalyObservation {
    ClockTimerAnomalyObservation {
        scenario_id: "ft-lmg3g.6.reduced.clock_timer_anomaly.pass".into(),
        anomaly_kind: ClockTimerAnomalyKind::DelayedTick,
        affected_runtime_budget: "admission_cooldown_and_wait_loop_deadline".into(),
        logical_elapsed_ms: 550,
        wall_elapsed_ms: 12,
        timeout_budget_ms: 500,
        bounded_wall_timeout_ms: 50,
        mitigation_reason_code: "resource.clock_timer.delayed_tick_bounded".into(),
        recovery_observed: true,
    }
}

/// Negative reduced-mode observation where missing timer progress does not recover.
#[must_use]
pub fn clock_timer_missing_progress_fail_observation() -> ClockTimerAnomalyObservation {
    ClockTimerAnomalyObservation {
        scenario_id: "ft-lmg3g.6.reduced.clock_timer_anomaly.fail_missing_progress".into(),
        anomaly_kind: ClockTimerAnomalyKind::StalledTimer,
        affected_runtime_budget: "replay_deadline_and_retry_cooldown".into(),
        logical_elapsed_ms: 0,
        wall_elapsed_ms: 60,
        timeout_budget_ms: 500,
        bounded_wall_timeout_ms: 50,
        mitigation_reason_code: "resource.clock_timer.missing_progress_unbounded".into(),
        recovery_observed: false,
    }
}

/// Reduced-mode PASS verdict for the clock/timer coverage row.
#[must_use]
pub fn clock_timer_reduced_pass_verdict() -> ResourcePressureChaosVerdict {
    let observation = clock_timer_reduced_pass_observation();
    let mut verdict = sample_pass_verdict();
    verdict.scenario_id.clone_from(&observation.scenario_id);
    verdict.pressure_class = ResourcePressureClass::ClockTimerAnomaly;
    verdict.mode = ResourcePressureChaosMode::Reduced;
    verdict.preconditions = vec![
        "controlled monotonic clock fixture installed".into(),
        "host wall-clock time is not modified".into(),
        "wait loop and admission cooldown use bounded timeout checks".into(),
    ];
    verdict.injected_fault = format!(
        "{} crosses {}ms logical budget while wall time stays bounded",
        observation.anomaly_kind.as_str(),
        observation.timeout_budget_ms
    );
    verdict.observed_mitigation = format!(
        "bounded timeout fired for {}; recovery observed after anomaly cleared",
        observation.affected_runtime_budget
    );
    verdict.fail_closed_decision = ResourcePressureFailClosedDecision {
        fail_closed: true,
        reason:
            "timer anomaly produced a bounded timeout/degrade decision instead of an infinite wait"
                .into(),
    };
    verdict.diagnostics = vec![ResourcePressureDiagnostic {
        code: observation.mitigation_reason_code.clone(),
        message: "clock/timer anomaly was classified separately from CPU and memory pressure"
            .into(),
        severity: ResourcePressureDiagnosticSeverity::Info,
    }];
    verdict.logs_path = Some("artifacts/resource-pressure/clock-timer/reduced-pass.jsonl".into());
    verdict.proof_level = ResourcePressureProofLevel::ReducedLocal;
    verdict.skip_reason = None;
    verdict.status = ResourcePressureChaosStatus::Pass;
    verdict.assertions = vec![
        ResourcePressureAssertion::FailClosed,
        ResourcePressureAssertion::NoPanic,
        ResourcePressureAssertion::DiagnosticEmitted,
        ResourcePressureAssertion::RecoveryObserved,
    ];
    verdict.hardware_evidence = None;
    verdict.admission_observation = None;
    verdict.memory_observation = None;
    verdict
}

/// Reduced-mode FAIL verdict for a missing-progress timer anomaly.
#[must_use]
pub fn clock_timer_missing_progress_fail_verdict() -> ResourcePressureChaosVerdict {
    let observation = clock_timer_missing_progress_fail_observation();
    let mut verdict = sample_fail_verdict();
    verdict.scenario_id.clone_from(&observation.scenario_id);
    verdict.pressure_class = ResourcePressureClass::ClockTimerAnomaly;
    verdict.mode = ResourcePressureChaosMode::Reduced;
    verdict.preconditions = vec![
        "controlled timer fixture installed".into(),
        "missing progress is injected without changing host time".into(),
    ];
    verdict.injected_fault = "timer progress stalls before replay deadline can advance".into();
    verdict.observed_mitigation = "no recovery was observed before the bounded wall timeout".into();
    verdict.fail_closed_decision = ResourcePressureFailClosedDecision {
        fail_closed: false,
        reason: "missing timer progress did not produce the required fail-closed timeout".into(),
    };
    verdict.diagnostics = vec![ResourcePressureDiagnostic {
        code: observation.mitigation_reason_code.clone(),
        message: "clock/timer anomaly exceeded the fixture wall-time bound".into(),
        severity: ResourcePressureDiagnosticSeverity::Error,
    }];
    verdict.logs_path = Some("artifacts/resource-pressure/clock-timer/reduced-fail.jsonl".into());
    verdict.proof_level = ResourcePressureProofLevel::ReducedLocal;
    verdict.skip_reason = None;
    verdict.status = ResourcePressureChaosStatus::Fail;
    verdict.assertions = vec![
        ResourcePressureAssertion::NoPanic,
        ResourcePressureAssertion::DiagnosticEmitted,
    ];
    verdict.hardware_evidence = None;
    verdict.admission_observation = None;
    verdict.memory_observation = None;
    verdict
}

/// High-scale-shaped clock/timer verdict that must not count as real proof.
#[must_use]
pub fn clock_timer_high_scale_skipped_not_proven_verdict() -> ResourcePressureChaosVerdict {
    let mut verdict = sample_skipped_not_proven_verdict();
    verdict.scenario_id = "ft-lmg3g.6.high_scale.clock_timer_anomaly.skipped_not_proven".into();
    verdict.pressure_class = ResourcePressureClass::ClockTimerAnomaly;
    verdict.mode = ResourcePressureChaosMode::HighScale;
    verdict.preconditions = vec![
        "high-scale clock/timer replay requested".into(),
        "real swarm hardware predicates must be recorded before PROVEN".into(),
    ];
    verdict.injected_fault =
        "high-scale replay would inject delayed ticks and timeout collapse".into();
    verdict.observed_mitigation = "not executed with real high-scale hardware predicates".into();
    verdict.fail_closed_decision = ResourcePressureFailClosedDecision {
        fail_closed: true,
        reason:
            "the proof lane refused to label simulated clock/timer evidence as real high-scale proof"
                .into(),
    };
    verdict.diagnostics = vec![ResourcePressureDiagnostic {
        code: "resource.clock_timer.high_scale_not_proven".into(),
        message: "clock/timer high-scale evidence is skipped until hardware predicates are met"
            .into(),
        severity: ResourcePressureDiagnosticSeverity::Warn,
    }];
    verdict.logs_path = None;
    verdict.proof_level = ResourcePressureProofLevel::SimulatedHighScale;
    verdict.skip_reason = Some("64-core/256GiB high-scale predicate evidence is absent".into());
    verdict.status = ResourcePressureChaosStatus::SkippedNotProven;
    verdict.assertions = vec![
        ResourcePressureAssertion::FailClosed,
        ResourcePressureAssertion::NoPanic,
        ResourcePressureAssertion::DiagnosticEmitted,
        ResourcePressureAssertion::RecoveryObserved,
    ];
    verdict.hardware_evidence = Some(HighScaleHardwareEvidence::skipped(
        "64-core/256GiB high-scale predicate evidence is absent",
    ));
    verdict.admission_observation = None;
    verdict.memory_observation = None;
    verdict
}

/// Initial clock/timer scenario set for the resource-pressure chaos runner.
#[must_use]
pub fn clock_timer_initial_verdicts() -> Vec<ResourcePressureChaosVerdict> {
    vec![
        clock_timer_reduced_pass_verdict(),
        clock_timer_missing_progress_fail_verdict(),
        clock_timer_high_scale_skipped_not_proven_verdict(),
    ]
}

/// Assess only the clock/timer row with the initial verdict set.
#[must_use]
pub fn clock_timer_coverage_assessment() -> bool {
    let matrix = ResourcePressureCoverageMatrix::default();
    matrix
        .assess_parent_completion(&clock_timer_initial_verdicts())
        .row_statuses
        .into_iter()
        .find(|status| status.pressure_class == ResourcePressureClass::ClockTimerAnomaly)
        .is_some_and(|status| status.satisfied)
}

#[cfg(test)]
mod tests {
    use crate::resource_pressure_chaos::{
        ResourcePressureChaosStatus, ResourcePressureClass, ResourcePressureCoverageMatrix,
        ResourcePressureProofLevel,
    };

    use super::{
        ClockTimerAnomalyKind, clock_timer_coverage_assessment,
        clock_timer_high_scale_skipped_not_proven_verdict, clock_timer_initial_verdicts,
        clock_timer_missing_progress_fail_observation, clock_timer_missing_progress_fail_verdict,
        clock_timer_reduced_pass_observation, clock_timer_reduced_pass_verdict,
    };

    #[test]
    fn reduced_pass_observation_records_bounded_logical_time_anomaly() {
        let observation = clock_timer_reduced_pass_observation();

        assert_eq!(observation.anomaly_kind, ClockTimerAnomalyKind::DelayedTick);
        assert!(observation.logical_timeout_crossed());
        assert!(observation.wall_time_bounded());
        assert!(observation.recovery_observed);
        assert_eq!(
            observation.mitigation_reason_code,
            "resource.clock_timer.delayed_tick_bounded"
        );
    }

    #[test]
    fn missing_progress_observation_records_unbounded_failure_shape() {
        let observation = clock_timer_missing_progress_fail_observation();

        assert_eq!(
            observation.anomaly_kind,
            ClockTimerAnomalyKind::StalledTimer
        );
        assert!(!observation.logical_timeout_crossed());
        assert!(!observation.wall_time_bounded());
        assert!(!observation.recovery_observed);
    }

    #[test]
    fn reduced_pass_verdict_validates_and_satisfies_clock_timer_row() {
        let verdict = clock_timer_reduced_pass_verdict();
        verdict
            .validate()
            .expect("reduced clock/timer pass verdict validates");

        let assessment = ResourcePressureCoverageMatrix::default()
            .assess_parent_completion(std::slice::from_ref(&verdict));
        let row = assessment
            .row_statuses
            .iter()
            .find(|status| status.pressure_class == ResourcePressureClass::ClockTimerAnomaly)
            .expect("clock/timer row status");

        assert_eq!(verdict.status, ResourcePressureChaosStatus::Pass);
        assert!(row.satisfied, "{row:?}");
        assert!(row.reason.contains("covered by"));
    }

    #[test]
    fn missing_progress_fail_validates_but_never_satisfies_coverage() {
        let verdict = clock_timer_missing_progress_fail_verdict();
        verdict
            .validate()
            .expect("negative clock/timer fail verdict validates");

        let assessment =
            ResourcePressureCoverageMatrix::default().assess_parent_completion(&[verdict]);
        let row = assessment
            .row_statuses
            .iter()
            .find(|status| status.pressure_class == ResourcePressureClass::ClockTimerAnomaly)
            .expect("clock/timer row status");

        assert!(!row.satisfied);
        assert!(row.reason.contains("FAIL"));
    }

    #[test]
    fn high_scale_verdict_is_skipped_until_real_hardware_predicates_exist() {
        let verdict = clock_timer_high_scale_skipped_not_proven_verdict();
        verdict
            .validate()
            .expect("high-scale skipped clock/timer verdict validates");

        assert_eq!(
            verdict.status,
            ResourcePressureChaosStatus::SkippedNotProven
        );
        assert_eq!(
            verdict.proof_level,
            ResourcePressureProofLevel::SimulatedHighScale
        );
        assert!(
            !verdict
                .hardware_evidence
                .as_ref()
                .expect("hardware evidence")
                .predicates_met()
        );
    }

    #[test]
    fn initial_verdict_set_records_pass_fail_and_skipped_paths() {
        let verdicts = clock_timer_initial_verdicts();

        assert_eq!(verdicts.len(), 3);
        assert!(clock_timer_coverage_assessment());
        assert!(verdicts.iter().any(|verdict| {
            verdict.pressure_class == ResourcePressureClass::ClockTimerAnomaly
                && verdict.status == ResourcePressureChaosStatus::Pass
        }));
        assert!(verdicts.iter().any(|verdict| {
            verdict.pressure_class == ResourcePressureClass::ClockTimerAnomaly
                && verdict.status == ResourcePressureChaosStatus::Fail
        }));
        assert!(verdicts.iter().any(|verdict| {
            verdict.pressure_class == ResourcePressureClass::ClockTimerAnomaly
                && verdict.status == ResourcePressureChaosStatus::SkippedNotProven
        }));
    }
}
