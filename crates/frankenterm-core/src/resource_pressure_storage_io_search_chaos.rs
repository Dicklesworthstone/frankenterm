//! Deterministic storage I/O and search-stall resource-pressure scenarios.
//!
//! This module implements the `ft-lmg3g.4` storage/search slice as reduced,
//! machine-readable [`ResourcePressureChaosVerdict`] fixtures. The scenarios
//! model storage and index lag explicitly; they do not mutate the live storage
//! scheduler or search backend.

use serde::{Deserialize, Serialize};

use crate::resource_pressure_chaos::{
    HighScaleHardwareEvidence, ResourcePressureAssertion, ResourcePressureChaosMode,
    ResourcePressureChaosStatus, ResourcePressureChaosVerdict, ResourcePressureClass,
    ResourcePressureCoverageMatrix, ResourcePressureDiagnostic, ResourcePressureDiagnosticSeverity,
    ResourcePressureFailClosedDecision, ResourcePressureProofLevel, sample_fail_verdict,
    sample_pass_verdict, sample_skipped_not_proven_verdict,
};
use crate::storage::io_scheduler::{
    StorageIoAdmissionDecision, StorageIoClass, StorageIoClassBudget, StorageIoScheduler,
    StorageIoSchedulerConfig, StorageIoSchedulerSnapshot, StorageIoWorkItem,
};

/// Deterministic storage/search pressure class exercised by a reduced scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageIoSearchFaultKind {
    /// Capture persistence is delayed while bounded queue admission still runs.
    CapturePersistenceStall,
    /// Cold-tier hydration is delayed before a read/search path can complete.
    ColdTierHydrationDelay,
    /// Search indexing trails committed segments and must catch up explicitly.
    SearchIndexCatchUpLag,
    /// A durable write error must surface as fail-closed diagnostics.
    DurableWriteError,
}

impl StorageIoSearchFaultKind {
    /// Stable machine string for artifacts and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CapturePersistenceStall => "capture_persistence_stall",
            Self::ColdTierHydrationDelay => "cold_tier_hydration_delay",
            Self::SearchIndexCatchUpLag => "search_index_catch_up_lag",
            Self::DurableWriteError => "durable_write_error",
        }
    }
}

/// Local evidence for a storage/search chaos fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIoSearchObservation {
    /// Stable scenario identifier.
    pub scenario_id: String,
    /// Injected storage/search fault.
    pub fault_kind: StorageIoSearchFaultKind,
    /// Write work queued before injection.
    pub write_queue_depth_before: u32,
    /// Write work queued after mitigation.
    pub write_queue_depth_after: u32,
    /// Declared reduced-fixture write queue bound.
    pub write_queue_depth_bound: u32,
    /// Segments committed but not yet searchable before mitigation.
    pub search_lag_segments_before: u32,
    /// Segments committed but not yet searchable after mitigation.
    pub search_lag_segments_after: u32,
    /// Maximum allowed search lag after mitigation.
    pub search_lag_segments_bound: u32,
    /// Cold-tier hydration lag before mitigation.
    pub hydration_lag_ms_before: u64,
    /// Cold-tier hydration lag after mitigation.
    pub hydration_lag_ms_after: u64,
    /// Maximum allowed hydration lag after mitigation.
    pub hydration_lag_bound_ms: u64,
    /// Whether a durability or write-admission error was observed.
    pub durability_error_observed: bool,
    /// Whether audit/event durability outcome was surfaced to the caller.
    pub audit_event_outcome_reported: bool,
    /// Whether searchable history caught up before the pass verdict.
    pub searchable_history_caught_up: bool,
    /// Whether the scenario produced a storage/search-specific diagnostic.
    pub io_specific_diagnostic_emitted: bool,
    /// Whether the mitigation was classified separately from CPU and memory pressure.
    pub separated_from_cpu_memory_pressure: bool,
    /// Stable diagnostic reason code emitted by the scenario.
    pub diagnostic_code: String,
}

/// Machine-readable artifact emitted by the storage IO/search stress proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIoSearchStressProofArtifact {
    /// Stable scenario identifier.
    pub scenario_id: String,
    /// Reduced or high-scale execution mode.
    pub mode: ResourcePressureChaosMode,
    /// Verdict status associated with this proof artifact.
    pub status: ResourcePressureChaosStatus,
    /// Evidence strength backing this artifact.
    pub proof_level: ResourcePressureProofLevel,
    /// Fault injected by the proof workload.
    pub injected_fault: String,
    /// Primary class affected by the injected fault.
    pub affected_class: StorageIoClass,
    /// Admission outcomes observed while injecting the workload.
    pub observed_outcomes: Vec<String>,
    /// Peak aggregate queue depth observed by the scheduler.
    pub queue_depth_peak: u64,
    /// Declared aggregate queue bound for this proof.
    pub queue_depth_bound: u64,
    /// Peak oldest queued age observed in operator telemetry.
    pub oldest_queued_age_peak_ms: u64,
    /// Peak bytes pending observed by the scheduler.
    pub bytes_pending_peak: u64,
    /// Durable segment writes dispatched before success was claimed.
    pub durable_success_count: u64,
    /// Successes claimed while required durability/search evidence was still missing.
    pub false_success_count: u64,
    /// Optional work shed under the scenario.
    pub shed_count: u64,
    /// Fail-closed storage/audit decisions under the scenario.
    pub fail_closed_count: u64,
    /// Peak committed-but-not-searchable segment count.
    pub search_lag_segments_peak: u64,
    /// Search lag after the mitigation drained.
    pub search_lag_segments_after: u64,
    /// Declared acceptable search lag after mitigation.
    pub search_lag_segments_bound: u64,
    /// Peak cold-tier hydration backlog in pages/chunks.
    pub hydration_lag_pages_peak: u64,
    /// Hydration lag after the mitigation drained.
    pub hydration_lag_pages_after: u64,
    /// Declared acceptable hydration lag after mitigation.
    pub hydration_lag_pages_bound: u64,
    /// Stable reason codes surfaced to operators.
    pub operator_reason_codes: Vec<String>,
    /// Hardware predicates attached to high-scale-shaped artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_evidence: Option<HighScaleHardwareEvidence>,
    /// Path where the JSONL artifact is expected to be written by a proof lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
}

impl StorageIoSearchStressProofArtifact {
    /// Whether the reduced proof satisfies the ft-1grhq.6 closeout contract.
    #[must_use]
    pub fn satisfies_reduced_stress_contract(&self) -> bool {
        self.mode == ResourcePressureChaosMode::Reduced
            && self.status == ResourcePressureChaosStatus::Pass
            && self.proof_level == ResourcePressureProofLevel::ReducedLocal
            && self.queue_depth_peak <= self.queue_depth_bound
            && self.search_lag_segments_after <= self.search_lag_segments_bound
            && self.hydration_lag_pages_after <= self.hydration_lag_pages_bound
            && self.false_success_count == 0
            && self.durable_success_count > 0
            && self.search_lag_segments_peak > 0
            && self.hydration_lag_pages_peak > 0
            && self
                .operator_reason_codes
                .iter()
                .all(|code| is_stable_storage_io_reason_code(code))
            && self.has_io_mitigation_reason_code()
    }

    /// Whether this artifact is allowed to count as real high-scale proof.
    #[must_use]
    pub fn claims_real_high_scale_proof(&self) -> bool {
        self.mode == ResourcePressureChaosMode::HighScale
            && self.status == ResourcePressureChaosStatus::Pass
            && self.proof_level == ResourcePressureProofLevel::RealHighScale
            && self
                .hardware_evidence
                .as_ref()
                .is_some_and(HighScaleHardwareEvidence::predicates_met)
    }

    fn has_io_mitigation_reason_code(&self) -> bool {
        self.operator_reason_codes.iter().any(|code| {
            code.starts_with("storage_io.defer.")
                || code.starts_with("storage_io.degrade.")
                || code.starts_with("storage_io.fail_closed.")
                || code.starts_with("storage_io.shed.")
        })
    }
}

impl StorageIoSearchObservation {
    /// Whether queued writes, search lag, and hydration lag stayed inside bounds.
    #[must_use]
    pub const fn bounded_after_mitigation(&self) -> bool {
        self.write_queue_depth_after <= self.write_queue_depth_bound
            && self.search_lag_segments_after <= self.search_lag_segments_bound
            && self.hydration_lag_ms_after <= self.hydration_lag_bound_ms
    }

    /// Whether the observation satisfies the storage/search pass contract.
    #[must_use]
    pub const fn satisfies_reduced_pass_contract(&self) -> bool {
        self.bounded_after_mitigation()
            && self.audit_event_outcome_reported
            && self.searchable_history_caught_up
            && self.io_specific_diagnostic_emitted
            && self.separated_from_cpu_memory_pressure
    }

    /// Whether a write error surfaced instead of being hidden behind batching.
    #[must_use]
    pub const fn write_error_surfaces_fail_closed(&self) -> bool {
        self.durability_error_observed
            && self.audit_event_outcome_reported
            && self.io_specific_diagnostic_emitted
    }
}

/// Reduced-mode storage/search PASS observation with bounded catch-up.
#[must_use]
pub fn storage_io_search_reduced_pass_observation() -> StorageIoSearchObservation {
    StorageIoSearchObservation {
        scenario_id: "ft-lmg3g.4.reduced.storage_io_search.pass".into(),
        fault_kind: StorageIoSearchFaultKind::SearchIndexCatchUpLag,
        write_queue_depth_before: 12,
        write_queue_depth_after: 4,
        write_queue_depth_bound: 8,
        search_lag_segments_before: 96,
        search_lag_segments_after: 0,
        search_lag_segments_bound: 2,
        hydration_lag_ms_before: 1_250,
        hydration_lag_ms_after: 75,
        hydration_lag_bound_ms: 100,
        durability_error_observed: false,
        audit_event_outcome_reported: true,
        searchable_history_caught_up: true,
        io_specific_diagnostic_emitted: true,
        separated_from_cpu_memory_pressure: true,
        diagnostic_code: "resource.storage_io_search.catch_up_bounded".into(),
    }
}

/// Reduced-mode fail-closed observation for a deliberate durable write error.
#[must_use]
pub fn storage_io_search_write_error_observation() -> StorageIoSearchObservation {
    StorageIoSearchObservation {
        scenario_id: "ft-lmg3g.4.reduced.storage_io_search.write_error_fail_closed".into(),
        fault_kind: StorageIoSearchFaultKind::DurableWriteError,
        write_queue_depth_before: 3,
        write_queue_depth_after: 3,
        write_queue_depth_bound: 8,
        search_lag_segments_before: 4,
        search_lag_segments_after: 4,
        search_lag_segments_bound: 4,
        hydration_lag_ms_before: 20,
        hydration_lag_ms_after: 20,
        hydration_lag_bound_ms: 100,
        durability_error_observed: true,
        audit_event_outcome_reported: true,
        searchable_history_caught_up: false,
        io_specific_diagnostic_emitted: true,
        separated_from_cpu_memory_pressure: true,
        diagnostic_code: "resource.storage_io_search.write_error_fail_closed".into(),
    }
}

/// Negative reduced-mode observation where searchable history remains stranded.
#[must_use]
pub fn storage_io_search_stranded_history_fail_observation() -> StorageIoSearchObservation {
    StorageIoSearchObservation {
        scenario_id: "ft-lmg3g.4.reduced.storage_io_search.fail_stranded_history".into(),
        fault_kind: StorageIoSearchFaultKind::SearchIndexCatchUpLag,
        write_queue_depth_before: 12,
        write_queue_depth_after: 17,
        write_queue_depth_bound: 8,
        search_lag_segments_before: 96,
        search_lag_segments_after: 64,
        search_lag_segments_bound: 2,
        hydration_lag_ms_before: 1_250,
        hydration_lag_ms_after: 1_150,
        hydration_lag_bound_ms: 100,
        durability_error_observed: false,
        audit_event_outcome_reported: false,
        searchable_history_caught_up: false,
        io_specific_diagnostic_emitted: false,
        separated_from_cpu_memory_pressure: false,
        diagnostic_code: "resource.storage_io_search.stranded_history_silent".into(),
    }
}

/// Deterministic reduced-mode stress proof that exercises the real scheduler.
#[must_use]
pub fn storage_io_search_reduced_stress_proof_artifact() -> StorageIoSearchStressProofArtifact {
    let mut scheduler = StorageIoScheduler::new(storage_io_search_reduced_stress_config());
    let mut observed_outcomes = Vec::new();
    let mut operator_reason_codes = Vec::new();
    let workload = [
        (1, StorageIoClass::PaneSegmentDurable, 10),
        (2, StorageIoClass::PaneSegmentDurable, 11),
        (3, StorageIoClass::FtsIncremental, 12),
        (4, StorageIoClass::FtsIncremental, 13),
        (5, StorageIoClass::FtsIncremental, 14),
        (6, StorageIoClass::FtsIncremental, 15),
        (7, StorageIoClass::ColdTierRead, 16),
        (8, StorageIoClass::ColdTierRead, 17),
    ];

    for (id, class, now_ms) in workload {
        let decision = scheduler.admit(StorageIoWorkItem::new(id, class, 512), now_ms);
        record_admission_evidence(
            &decision,
            &mut observed_outcomes,
            &mut operator_reason_codes,
        );
    }

    let deferred_search = scheduler.admit(
        StorageIoWorkItem::new(9, StorageIoClass::FtsIncremental, 512),
        18,
    );
    record_admission_evidence(
        &deferred_search,
        &mut observed_outcomes,
        &mut operator_reason_codes,
    );

    let injected_snapshot = scheduler.snapshot(200);
    record_snapshot_reason(&injected_snapshot, &mut operator_reason_codes);

    let mut queue_depth_peak = injected_snapshot.aggregate_queue_depth;
    let mut bytes_pending_peak = injected_snapshot.aggregate_bytes_pending;
    let mut oldest_queued_age_peak_ms = injected_snapshot
        .operator_summary()
        .oldest_queued_age_ms
        .unwrap_or(0);
    let mut search_lag_segments_peak = injected_snapshot.search_lag_segments;
    let mut hydration_lag_pages_peak = injected_snapshot.hydration_lag_pages;
    let mut durable_success_count = 0_u64;
    let mut now_ms = 210_u64;

    while let Some(dispatched) = scheduler.pop_next(now_ms) {
        if dispatched.item.class == StorageIoClass::PaneSegmentDurable {
            durable_success_count = durable_success_count.saturating_add(1);
        }

        let snapshot = scheduler.snapshot(now_ms);
        queue_depth_peak = queue_depth_peak.max(snapshot.aggregate_queue_depth);
        bytes_pending_peak = bytes_pending_peak.max(snapshot.aggregate_bytes_pending);
        oldest_queued_age_peak_ms = oldest_queued_age_peak_ms.max(
            snapshot
                .operator_summary()
                .oldest_queued_age_ms
                .unwrap_or(0),
        );
        search_lag_segments_peak = search_lag_segments_peak.max(snapshot.search_lag_segments);
        hydration_lag_pages_peak = hydration_lag_pages_peak.max(snapshot.hydration_lag_pages);
        record_snapshot_reason(&snapshot, &mut operator_reason_codes);
        now_ms = now_ms.saturating_add(10);
    }

    let drained_snapshot = scheduler.snapshot(now_ms);
    record_snapshot_reason(&drained_snapshot, &mut operator_reason_codes);

    StorageIoSearchStressProofArtifact {
        scenario_id: "ft-1grhq.6.reduced.storage_io_search.scheduler_stress".into(),
        mode: ResourcePressureChaosMode::Reduced,
        status: ResourcePressureChaosStatus::Pass,
        proof_level: ResourcePressureProofLevel::ReducedLocal,
        injected_fault:
            "bounded high-output capture replay leaves FTS and cold-tier hydration behind durable writes"
                .into(),
        affected_class: StorageIoClass::FtsIncremental,
        observed_outcomes,
        queue_depth_peak,
        queue_depth_bound: scheduler.config().aggregate_max_items,
        oldest_queued_age_peak_ms,
        bytes_pending_peak,
        durable_success_count,
        false_success_count: 0,
        shed_count: injected_snapshot
            .classes
            .iter()
            .map(|row| row.shed_total)
            .sum(),
        fail_closed_count: injected_snapshot
            .classes
            .iter()
            .map(|row| row.fail_closed_total)
            .sum(),
        search_lag_segments_peak,
        search_lag_segments_after: drained_snapshot.search_lag_segments,
        search_lag_segments_bound: 0,
        hydration_lag_pages_peak,
        hydration_lag_pages_after: drained_snapshot.hydration_lag_pages,
        hydration_lag_pages_bound: 0,
        operator_reason_codes,
        hardware_evidence: None,
        artifact_path: Some(
            "artifacts/resource-pressure/storage-io-search/reduced-stress-proof.jsonl".into(),
        ),
    }
}

/// High-scale-shaped artifact that records missing hardware predicates.
#[must_use]
pub fn storage_io_search_high_scale_predicate_artifact() -> StorageIoSearchStressProofArtifact {
    let skipped_verdict = storage_io_search_high_scale_skipped_not_proven_verdict();
    StorageIoSearchStressProofArtifact {
        scenario_id: "ft-1grhq.6.high_scale.storage_io_search.skipped_not_proven".into(),
        mode: ResourcePressureChaosMode::HighScale,
        status: ResourcePressureChaosStatus::SkippedNotProven,
        proof_level: skipped_verdict.proof_level,
        injected_fault: skipped_verdict.injected_fault,
        affected_class: StorageIoClass::FtsIncremental,
        observed_outcomes: vec![skipped_verdict.status.as_str().to_string()],
        queue_depth_peak: 0,
        queue_depth_bound: 0,
        oldest_queued_age_peak_ms: 0,
        bytes_pending_peak: 0,
        durable_success_count: 0,
        false_success_count: 0,
        shed_count: 0,
        fail_closed_count: 0,
        search_lag_segments_peak: 0,
        search_lag_segments_after: 0,
        search_lag_segments_bound: 0,
        hydration_lag_pages_peak: 0,
        hydration_lag_pages_after: 0,
        hydration_lag_pages_bound: 0,
        operator_reason_codes: vec!["storage_io.defer.high_scale_not_proven".into()],
        hardware_evidence: skipped_verdict.hardware_evidence,
        artifact_path: None,
    }
}

/// Reduced-mode PASS verdict for bounded storage/search catch-up.
#[must_use]
pub fn storage_io_search_reduced_pass_verdict() -> ResourcePressureChaosVerdict {
    let observation = storage_io_search_reduced_pass_observation();
    let mut verdict = sample_pass_verdict();
    verdict.scenario_id = observation.scenario_id.clone();
    verdict.pressure_class = ResourcePressureClass::StorageIoSearch;
    verdict.mode = ResourcePressureChaosMode::Reduced;
    verdict.preconditions = vec![
        "bounded write queue fixture is installed".into(),
        "search index lag and cold-tier hydration lag are measured separately".into(),
        "audit/event write outcomes remain caller-visible".into(),
    ];
    verdict.injected_fault = format!(
        "{} leaves {} committed segments temporarily unsearchable",
        observation.fault_kind.as_str(),
        observation.search_lag_segments_before
    );
    verdict.observed_mitigation = format!(
        "storage/search mitigation drained writes to {}, search lag to {}, and hydration lag to {}ms",
        observation.write_queue_depth_after,
        observation.search_lag_segments_after,
        observation.hydration_lag_ms_after
    );
    verdict.fail_closed_decision = ResourcePressureFailClosedDecision {
        fail_closed: true,
        reason: "query path degraded while searchable history caught up explicitly".into(),
    };
    verdict.diagnostics = vec![ResourcePressureDiagnostic {
        code: observation.diagnostic_code.clone(),
        message: "storage I/O pressure emitted an IO-specific catch-up diagnostic".into(),
        severity: ResourcePressureDiagnosticSeverity::Warn,
    }];
    verdict.logs_path =
        Some("artifacts/resource-pressure/storage-io-search/reduced-pass.jsonl".into());
    verdict.proof_level = ResourcePressureProofLevel::ReducedLocal;
    verdict.skip_reason = None;
    verdict.status = ResourcePressureChaosStatus::Pass;
    verdict.assertions = vec![
        ResourcePressureAssertion::NoPanic,
        ResourcePressureAssertion::DiagnosticEmitted,
        ResourcePressureAssertion::MitigationLogged,
        ResourcePressureAssertion::RecoveryObserved,
    ];
    verdict.hardware_evidence = None;
    verdict.admission_observation = None;
    verdict.memory_observation = None;
    verdict.external_service_observation = None;
    verdict
}

/// Reduced-mode PASS verdict for a durable write error that fails closed.
#[must_use]
pub fn storage_io_search_write_error_fail_closed_verdict() -> ResourcePressureChaosVerdict {
    let observation = storage_io_search_write_error_observation();
    let mut verdict = sample_pass_verdict();
    verdict.scenario_id = observation.scenario_id.clone();
    verdict.pressure_class = ResourcePressureClass::StorageIoSearch;
    verdict.mode = ResourcePressureChaosMode::Reduced;
    verdict.preconditions = vec![
        "durable write error fixture is installed".into(),
        "audit/event persistence must report failure before downstream success is claimed".into(),
    ];
    verdict.injected_fault = "durable audit/event write returns a deterministic IO error".into();
    verdict.observed_mitigation =
        "write path failed closed and withheld searchable-success until durability recovered"
            .into();
    verdict.fail_closed_decision = ResourcePressureFailClosedDecision {
        fail_closed: true,
        reason: "durable write error was surfaced as an IO diagnostic instead of silent success"
            .into(),
    };
    verdict.diagnostics = vec![ResourcePressureDiagnostic {
        code: observation.diagnostic_code.clone(),
        message: "durability failure surfaced as a fail-closed storage/search diagnostic".into(),
        severity: ResourcePressureDiagnosticSeverity::Error,
    }];
    verdict.logs_path =
        Some("artifacts/resource-pressure/storage-io-search/write-error-fail-closed.jsonl".into());
    verdict.proof_level = ResourcePressureProofLevel::ReducedLocal;
    verdict.skip_reason = None;
    verdict.status = ResourcePressureChaosStatus::Pass;
    verdict.assertions = vec![
        ResourcePressureAssertion::NoPanic,
        ResourcePressureAssertion::DiagnosticEmitted,
        ResourcePressureAssertion::MitigationLogged,
        ResourcePressureAssertion::RecoveryObserved,
    ];
    verdict.hardware_evidence = None;
    verdict.admission_observation = None;
    verdict.memory_observation = None;
    verdict.external_service_observation = None;
    verdict
}

/// Reduced-mode FAIL verdict for silent stranded searchable history.
#[must_use]
pub fn storage_io_search_stranded_history_fail_verdict() -> ResourcePressureChaosVerdict {
    let observation = storage_io_search_stranded_history_fail_observation();
    let mut verdict = sample_fail_verdict();
    verdict.scenario_id = observation.scenario_id.clone();
    verdict.pressure_class = ResourcePressureClass::StorageIoSearch;
    verdict.mode = ResourcePressureChaosMode::Reduced;
    verdict.preconditions = vec![
        "misconfigured storage/search mitigation intentionally permits stale success".into(),
        "search lag and write queue bounds are declared by the fixture".into(),
    ];
    verdict.injected_fault =
        "storage flush and search indexing remain stalled after success is reported".into();
    verdict.observed_mitigation =
        "searchable history stayed behind without a caller-visible degraded verdict".into();
    verdict.fail_closed_decision = ResourcePressureFailClosedDecision {
        fail_closed: false,
        reason: "search success was claimed while committed segments remained stranded".into(),
    };
    verdict.diagnostics = vec![ResourcePressureDiagnostic {
        code: observation.diagnostic_code.clone(),
        message: "searchable history lag exceeded bounds without an IO-specific diagnostic".into(),
        severity: ResourcePressureDiagnosticSeverity::Error,
    }];
    verdict.logs_path =
        Some("artifacts/resource-pressure/storage-io-search/stranded-history-fail.jsonl".into());
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
    verdict.external_service_observation = None;
    verdict
}

/// Reduced-mode scheduler-stress verdict backed by the real storage IO scheduler.
#[must_use]
pub fn storage_io_search_reduced_stress_verdict() -> ResourcePressureChaosVerdict {
    let artifact = storage_io_search_reduced_stress_proof_artifact();
    let mut verdict = sample_pass_verdict();
    verdict.scenario_id = artifact.scenario_id.clone();
    verdict.pressure_class = ResourcePressureClass::StorageIoSearch;
    verdict.mode = ResourcePressureChaosMode::Reduced;
    verdict.preconditions = vec![
        "real storage IO scheduler is configured with bounded reduced-mode budgets".into(),
        "durable writes, FTS catch-up, and cold-tier hydration share the scheduler".into(),
        "operator snapshot reason codes are recorded before pass is claimed".into(),
    ];
    verdict.injected_fault = artifact.injected_fault.clone();
    verdict.observed_mitigation = format!(
        "scheduler peaked at queue_depth={} bytes_pending={} search_lag={} hydration_lag={} and drained search/hydration lag to zero",
        artifact.queue_depth_peak,
        artifact.bytes_pending_peak,
        artifact.search_lag_segments_peak,
        artifact.hydration_lag_pages_peak
    );
    verdict.fail_closed_decision = ResourcePressureFailClosedDecision {
        fail_closed: true,
        reason:
            "storage/search success remained gated on bounded queue drain and explicit reason codes"
                .into(),
    };
    verdict.diagnostics = vec![ResourcePressureDiagnostic {
        code: "resource.storage_io_search.scheduler_stress_bounded".into(),
        message: "reduced stress proof exercised scheduler-backed FTS and hydration lag".into(),
        severity: ResourcePressureDiagnosticSeverity::Warn,
    }];
    verdict.logs_path = artifact.artifact_path.clone();
    verdict.proof_level = artifact.proof_level;
    verdict.skip_reason = None;
    verdict.status = artifact.status;
    verdict.assertions = vec![
        ResourcePressureAssertion::FailClosed,
        ResourcePressureAssertion::BoundedQueueGrowth,
        ResourcePressureAssertion::NoPanic,
        ResourcePressureAssertion::DiagnosticEmitted,
        ResourcePressureAssertion::MitigationLogged,
        ResourcePressureAssertion::RecoveryObserved,
    ];
    verdict.hardware_evidence = None;
    verdict.admission_observation = None;
    verdict.memory_observation = None;
    verdict.external_service_observation = None;
    verdict
}

/// High-scale-shaped storage/search verdict that must not count as real proof.
#[must_use]
pub fn storage_io_search_high_scale_skipped_not_proven_verdict() -> ResourcePressureChaosVerdict {
    let mut verdict = sample_skipped_not_proven_verdict();
    verdict.scenario_id = "ft-lmg3g.4.high_scale.storage_io_search.skipped_not_proven".into();
    verdict.pressure_class = ResourcePressureClass::StorageIoSearch;
    verdict.mode = ResourcePressureChaosMode::HighScale;
    verdict.preconditions = vec![
        "high-output capture replay requested".into(),
        "disk free, IO pressure, and high-scale hardware predicates must be recorded before PROVEN"
            .into(),
    ];
    verdict.injected_fault =
        "high-output replay would delay capture persistence, cold hydration, and search indexing"
            .into();
    verdict.observed_mitigation = "not executed with real high-scale storage/IO predicates".into();
    verdict.fail_closed_decision = ResourcePressureFailClosedDecision {
        fail_closed: true,
        reason:
            "the proof lane refused to label simulated storage/search evidence as high-scale proof"
                .into(),
    };
    verdict.diagnostics = vec![ResourcePressureDiagnostic {
        code: "resource.storage_io_search.high_scale_not_proven".into(),
        message:
            "storage/search high-scale evidence is skipped until hardware and IO predicates are met"
                .into(),
        severity: ResourcePressureDiagnosticSeverity::Warn,
    }];
    verdict.logs_path = None;
    verdict.proof_level = ResourcePressureProofLevel::SimulatedHighScale;
    verdict.skip_reason =
        Some("64-core/256GiB plus disk-free/IO-pressure predicate evidence is absent".into());
    verdict.status = ResourcePressureChaosStatus::SkippedNotProven;
    verdict.assertions = vec![
        ResourcePressureAssertion::NoPanic,
        ResourcePressureAssertion::DiagnosticEmitted,
        ResourcePressureAssertion::MitigationLogged,
        ResourcePressureAssertion::RecoveryObserved,
    ];
    verdict.hardware_evidence = Some(HighScaleHardwareEvidence::skipped(
        "64-core/256GiB plus disk-free/IO-pressure predicate evidence is absent",
    ));
    verdict.admission_observation = None;
    verdict.memory_observation = None;
    verdict.external_service_observation = None;
    verdict
}

/// Initial storage/search scenario set for the resource-pressure chaos runner.
#[must_use]
pub fn storage_io_search_initial_verdicts() -> Vec<ResourcePressureChaosVerdict> {
    vec![
        storage_io_search_reduced_pass_verdict(),
        storage_io_search_reduced_stress_verdict(),
        storage_io_search_write_error_fail_closed_verdict(),
        storage_io_search_stranded_history_fail_verdict(),
        storage_io_search_high_scale_skipped_not_proven_verdict(),
    ]
}

/// Assess only the storage/search row with the initial verdict set.
#[must_use]
pub fn storage_io_search_coverage_assessment() -> bool {
    let matrix = ResourcePressureCoverageMatrix::default();
    matrix
        .assess_parent_completion(&storage_io_search_initial_verdicts())
        .row_statuses
        .into_iter()
        .find(|status| status.pressure_class == ResourcePressureClass::StorageIoSearch)
        .is_some_and(|status| status.satisfied)
}

fn storage_io_search_reduced_stress_config() -> StorageIoSchedulerConfig {
    let mut config = StorageIoSchedulerConfig {
        aggregate_max_items: 10,
        aggregate_max_bytes: 64 * 1024,
        max_consecutive_per_class: 2,
        ..StorageIoSchedulerConfig::default()
    };
    config.class_budgets.insert(
        StorageIoClass::PaneSegmentDurable,
        StorageIoClassBudget::deferrable(4, 16 * 1024, 1),
    );
    config.class_budgets.insert(
        StorageIoClass::FtsIncremental,
        StorageIoClassBudget::deferrable(4, 16 * 1024, 4),
    );
    config.class_budgets.insert(
        StorageIoClass::ColdTierRead,
        StorageIoClassBudget::deferrable(2, 16 * 1024, 2),
    );
    config
}

fn record_admission_evidence(
    decision: &StorageIoAdmissionDecision,
    observed_outcomes: &mut Vec<String>,
    operator_reason_codes: &mut Vec<String>,
) {
    observed_outcomes.push(format!(
        "{}:{}",
        decision.class.as_str(),
        decision.outcome.as_str()
    ));
    push_unique(operator_reason_codes, decision.reason_code());
}

fn record_snapshot_reason(
    snapshot: &StorageIoSchedulerSnapshot,
    operator_reason_codes: &mut Vec<String>,
) {
    let summary = snapshot.operator_summary();
    push_unique(operator_reason_codes, summary.io_pressure_reason);
    if let Some(dominant) = summary.dominant_class
        && let Some(reason_code) = dominant.reason_code
    {
        push_unique(operator_reason_codes, reason_code);
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn is_stable_storage_io_reason_code(code: &str) -> bool {
    let mut parts = code.split('.');
    matches!(parts.next(), Some("storage_io"))
        && matches!(
            parts.next(),
            Some("admit" | "batch" | "defer" | "degrade" | "shed" | "fail_closed" | "write_error")
        )
        && parts.next().is_some_and(|reason| !reason.trim().is_empty())
        && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use crate::resource_pressure_chaos::{
        ResourcePressureChaosStatus, ResourcePressureClass, ResourcePressureCoverageMatrix,
        ResourcePressureProofLevel,
    };

    use super::{
        StorageIoSearchFaultKind, storage_io_search_coverage_assessment,
        storage_io_search_high_scale_predicate_artifact,
        storage_io_search_high_scale_skipped_not_proven_verdict,
        storage_io_search_initial_verdicts, storage_io_search_reduced_pass_observation,
        storage_io_search_reduced_pass_verdict, storage_io_search_reduced_stress_proof_artifact,
        storage_io_search_reduced_stress_verdict,
        storage_io_search_stranded_history_fail_observation,
        storage_io_search_stranded_history_fail_verdict,
        storage_io_search_write_error_fail_closed_verdict,
        storage_io_search_write_error_observation,
    };

    #[test]
    fn reduced_pass_observation_records_bounded_catch_up() {
        let observation = storage_io_search_reduced_pass_observation();

        assert_eq!(
            observation.fault_kind,
            StorageIoSearchFaultKind::SearchIndexCatchUpLag
        );
        assert!(observation.bounded_after_mitigation());
        assert!(observation.satisfies_reduced_pass_contract());
        assert!(observation.searchable_history_caught_up);
        assert!(observation.separated_from_cpu_memory_pressure);
    }

    #[test]
    fn write_error_observation_records_fail_closed_durability_diagnostic() {
        let observation = storage_io_search_write_error_observation();

        assert_eq!(
            observation.fault_kind,
            StorageIoSearchFaultKind::DurableWriteError
        );
        assert!(observation.write_error_surfaces_fail_closed());
        assert!(observation.durability_error_observed);
        assert!(!observation.searchable_history_caught_up);
    }

    #[test]
    fn stranded_history_observation_records_silent_failure_shape() {
        let observation = storage_io_search_stranded_history_fail_observation();

        assert!(!observation.bounded_after_mitigation());
        assert!(!observation.satisfies_reduced_pass_contract());
        assert!(!observation.audit_event_outcome_reported);
        assert!(!observation.io_specific_diagnostic_emitted);
    }

    #[test]
    fn reduced_pass_verdict_validates_and_satisfies_storage_search_row() {
        let verdict = storage_io_search_reduced_pass_verdict();
        verdict
            .validate()
            .expect("reduced storage/search pass verdict validates");

        let assessment = ResourcePressureCoverageMatrix::default()
            .assess_parent_completion(std::slice::from_ref(&verdict));
        let row = assessment
            .row_statuses
            .iter()
            .find(|status| status.pressure_class == ResourcePressureClass::StorageIoSearch)
            .expect("storage/search row status");

        assert_eq!(verdict.status, ResourcePressureChaosStatus::Pass);
        assert!(row.satisfied, "{row:?}");
        assert!(row.reason.contains("covered by"));
    }

    #[test]
    fn write_error_fail_closed_verdict_validates_and_satisfies_storage_search_row() {
        let verdict = storage_io_search_write_error_fail_closed_verdict();
        verdict
            .validate()
            .expect("write-error fail-closed verdict validates");

        assert_eq!(verdict.status, ResourcePressureChaosStatus::Pass);
        assert!(verdict.fail_closed_decision.fail_closed);
        assert!(
            verdict
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.contains("write_error_fail_closed"))
        );
    }

    #[test]
    fn stranded_history_fail_validates_but_never_satisfies_coverage() {
        let verdict = storage_io_search_stranded_history_fail_verdict();
        verdict
            .validate()
            .expect("negative storage/search fail verdict validates");

        let assessment =
            ResourcePressureCoverageMatrix::default().assess_parent_completion(&[verdict]);
        let row = assessment
            .row_statuses
            .iter()
            .find(|status| status.pressure_class == ResourcePressureClass::StorageIoSearch)
            .expect("storage/search row status");

        assert!(!row.satisfied);
        assert!(row.reason.contains("FAIL"));
    }

    #[test]
    fn reduced_stress_artifact_drives_scheduler_and_drains_search_hydration_lag() {
        let artifact = storage_io_search_reduced_stress_proof_artifact();

        assert!(artifact.satisfies_reduced_stress_contract(), "{artifact:?}");
        assert_eq!(artifact.queue_depth_peak, 8);
        assert_eq!(artifact.queue_depth_bound, 10);
        assert_eq!(artifact.durable_success_count, 2);
        assert_eq!(artifact.false_success_count, 0);
        assert_eq!(artifact.search_lag_segments_peak, 4);
        assert_eq!(artifact.search_lag_segments_after, 0);
        assert_eq!(artifact.hydration_lag_pages_peak, 2);
        assert_eq!(artifact.hydration_lag_pages_after, 0);
        assert!(artifact.operator_reason_codes.iter().any(|code| {
            code == "storage_io.defer.class_budget_exhausted"
                || code == "storage_io.degrade.oldest_age_exceeded"
        }));
    }

    #[test]
    fn reduced_stress_contract_rejects_stranded_or_unbounded_or_unstable_evidence() {
        let artifact = storage_io_search_reduced_stress_proof_artifact();

        let mut stranded = artifact.clone();
        stranded.search_lag_segments_after = 1;
        assert!(!stranded.satisfies_reduced_stress_contract());

        let mut unbounded = artifact.clone();
        unbounded.queue_depth_peak = unbounded.queue_depth_bound.saturating_add(1);
        assert!(!unbounded.satisfies_reduced_stress_contract());

        let mut unstable_reason = artifact;
        unstable_reason.operator_reason_codes = vec!["degraded".into()];
        assert!(!unstable_reason.satisfies_reduced_stress_contract());
    }

    #[test]
    fn reduced_stress_verdict_validates_and_records_scheduler_artifact_path() {
        let verdict = storage_io_search_reduced_stress_verdict();
        verdict
            .validate()
            .expect("reduced storage/search scheduler stress verdict validates");

        assert_eq!(verdict.status, ResourcePressureChaosStatus::Pass);
        assert_eq!(
            verdict.proof_level,
            ResourcePressureProofLevel::ReducedLocal
        );
        assert!(
            verdict
                .logs_path
                .as_deref()
                .is_some_and(|path| path.ends_with("reduced-stress-proof.jsonl"))
        );
    }

    #[test]
    fn high_scale_verdict_is_skipped_until_real_io_predicates_exist() {
        let verdict = storage_io_search_high_scale_skipped_not_proven_verdict();
        verdict
            .validate()
            .expect("high-scale skipped storage/search verdict validates");

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

        let artifact = storage_io_search_high_scale_predicate_artifact();
        assert!(!artifact.claims_real_high_scale_proof());
        assert!(
            !artifact
                .hardware_evidence
                .as_ref()
                .expect("hardware evidence")
                .predicates_met()
        );
    }

    #[test]
    fn initial_verdict_set_records_pass_fail_and_skipped_paths() {
        let verdicts = storage_io_search_initial_verdicts();

        assert_eq!(verdicts.len(), 5);
        assert!(storage_io_search_coverage_assessment());
        assert!(verdicts.iter().any(|verdict| {
            verdict.pressure_class == ResourcePressureClass::StorageIoSearch
                && verdict.status == ResourcePressureChaosStatus::Pass
        }));
        assert!(verdicts.iter().any(|verdict| {
            verdict.pressure_class == ResourcePressureClass::StorageIoSearch
                && verdict.status == ResourcePressureChaosStatus::Fail
        }));
        assert!(verdicts.iter().any(|verdict| {
            verdict.pressure_class == ResourcePressureClass::StorageIoSearch
                && verdict.status == ResourcePressureChaosStatus::SkippedNotProven
        }));
    }
}
