//! Deterministic storage/indexing IO heat-map proof model.
//!
//! The live storage and indexing paths are intentionally not touched here. This
//! module models the operator receipts those paths must eventually emit when a
//! massive swarm becomes write-bound, indexing-bound, search-bound, or blocked
//! on compaction debt.

use crate::replay_capture_tiering::AdaptiveCaptureTierSummary;
use serde::{Deserialize, Serialize};

/// Schema version for storage/indexing heat-map reports.
pub const STORAGE_INDEX_HEATMAP_SCHEMA_VERSION: u32 = 1;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Aggregate heat level for a workload class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoHeatLevel {
    /// No meaningful pressure.
    Cool,
    /// Work is active but below intervention thresholds.
    Warm,
    /// Work is hot enough to need an admission decision.
    Hot,
    /// Work has exceeded a correctness/freshness threshold.
    Saturated,
}

impl IoHeatLevel {
    /// Stable key for reports and fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cool => "cool",
            Self::Warm => "warm",
            Self::Hot => "hot",
            Self::Saturated => "saturated",
        }
    }
}

/// Admission action for storage, indexing, search, or compaction work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageAdmissionAction {
    /// Admit immediately.
    RunNow,
    /// Defer non-critical work until pressure drops.
    Defer,
    /// Throttle producers to protect persistence and replay freshness.
    Throttle,
    /// Shard or split work across indexing/search lanes.
    Shard,
    /// Mark search/capture coverage degraded with explicit receipts.
    MarkCoverageDegraded,
}

impl StorageAdmissionAction {
    /// Stable key for reports and fixtures.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunNow => "run_now",
            Self::Defer => "defer",
            Self::Throttle => "throttle",
            Self::Shard => "shard",
            Self::MarkCoverageDegraded => "mark_coverage_degraded",
        }
    }
}

/// Reason code behind a heat-map admission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageAdmissionReason {
    /// Capture writes are the dominant source of IO pressure.
    WriteHeavyCapture,
    /// FTS or semantic indexing backlog is beyond the freshness threshold.
    IndexingBacklog,
    /// Search queries are hot enough to require sharding.
    SearchBurst,
    /// Compaction work is consuming or waiting on too much storage IO.
    CompactionDebt,
    /// Replay artifact reads are hot enough to compete with capture writes.
    ReplayArtifactIo,
    /// Upstream capture tiering already disclosed deferred coverage.
    CoverageAlreadyDeferred,
    /// Previously deferred work can run after pressure drops.
    RecoveryAfterPressureDrops,
}

impl StorageAdmissionReason {
    /// Stable key for receipts and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WriteHeavyCapture => "write_heavy_capture",
            Self::IndexingBacklog => "indexing_backlog",
            Self::SearchBurst => "search_burst",
            Self::CompactionDebt => "compaction_debt",
            Self::ReplayArtifactIo => "replay_artifact_io",
            Self::CoverageAlreadyDeferred => "coverage_already_deferred",
            Self::RecoveryAfterPressureDrops => "recovery_after_pressure_drops",
        }
    }
}

/// Freshness and coverage impact disclosed to operators and robot surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageFreshnessImpact {
    /// Coverage is complete and real-time.
    RealtimeComplete,
    /// Coverage is complete after bounded buffering.
    BufferedFresh,
    /// Coverage is complete, but freshness is delayed.
    DelayedFreshness,
    /// Coverage is explicitly summary/checkpoint-only.
    SummaryOnly,
    /// Coverage is degraded and must be disclosed to readers.
    CoverageDegraded,
}

/// Thresholds used by the deterministic heat-map planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIndexHeatPolicy {
    /// Write throughput that makes capture storage hot.
    pub hot_write_bytes_per_sec: u64,
    /// Write throughput that saturates capture storage.
    pub saturated_write_bytes_per_sec: u64,
    /// Pending index segments that mark coverage degraded.
    pub indexing_saturation_segments: u64,
    /// Search queries/sec that require sharding.
    pub search_burst_queries_per_sec: u64,
    /// Compaction debt that should be deferred.
    pub compaction_debt_bytes: u64,
    /// Replay artifact IO that should be sharded away from capture IO.
    pub replay_artifact_hot_bytes_per_sec: u64,
    /// Low-pressure write ceiling for recovery.
    pub recovery_write_bytes_per_sec: u64,
    /// Low-pressure pending-index ceiling for recovery.
    pub recovery_index_segments: u64,
    /// Estimated lag per pending index segment.
    pub freshness_lag_ms_per_index_segment: u64,
    /// Estimated lag per GiB of compaction debt.
    pub compaction_lag_ms_per_gib: u64,
    /// Estimated lag for each hot-write multiple.
    pub write_lag_ms_per_hot_multiple: u64,
    /// Estimated lag for a search burst that is sharded.
    pub search_burst_lag_ms: u64,
    /// Estimated lag for each upstream deferred capture tier.
    pub deferred_tier_lag_ms: u64,
}

impl Default for StorageIndexHeatPolicy {
    fn default() -> Self {
        Self {
            hot_write_bytes_per_sec: 8 * MIB,
            saturated_write_bytes_per_sec: 32 * MIB,
            indexing_saturation_segments: 1_024,
            search_burst_queries_per_sec: 120,
            compaction_debt_bytes: 2 * GIB,
            replay_artifact_hot_bytes_per_sec: 16 * MIB,
            recovery_write_bytes_per_sec: 2 * MIB,
            recovery_index_segments: 128,
            freshness_lag_ms_per_index_segment: 10,
            compaction_lag_ms_per_gib: 7_500,
            write_lag_ms_per_hot_multiple: 2_000,
            search_burst_lag_ms: 1_500,
            deferred_tier_lag_ms: 5_000,
        }
    }
}

/// Workload input for one heat-map cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIndexWorkloadInput {
    /// Stable workload id.
    pub workload_id: String,
    /// Operator-facing workload class.
    pub workload_class: String,
    /// Number of panes represented by this workload.
    pub pane_count: u64,
    /// Capture write pressure in bytes/sec.
    pub write_bytes_per_sec: u64,
    /// Pending FTS/semantic index segments.
    pub pending_index_segments: u64,
    /// Pending compaction debt in bytes.
    pub compaction_debt_bytes: u64,
    /// Replay artifact read pressure in bytes/sec.
    pub replay_artifact_read_bytes_per_sec: u64,
    /// Search query pressure in queries/sec.
    pub search_queries_per_sec: u64,
    /// Whether the previous sample shows pressure dropping.
    pub pressure_drop_observed: bool,
    /// Previous admission action, when known.
    pub previous_action: Option<StorageAdmissionAction>,
    /// Upstream adaptive capture tier summary for coverage disclosure.
    pub capture_tier_summary: AdaptiveCaptureTierSummary,
}

/// One heat-map decision cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIndexHeatMapCell {
    /// Stable workload id.
    pub workload_id: String,
    /// Operator-facing workload class.
    pub workload_class: String,
    /// Number of panes represented by this workload.
    pub pane_count: u64,
    /// Aggregate heat level.
    pub heat_level: IoHeatLevel,
    /// Admission action.
    pub admission_action: StorageAdmissionAction,
    /// Stable reason codes.
    pub reasons: Vec<String>,
    /// Freshness and coverage impact.
    pub freshness_impact: CoverageFreshnessImpact,
    /// Estimated freshness lag in milliseconds.
    pub estimated_freshness_lag_ms: u64,
    /// Concise operator note.
    pub operator_note: String,
}

/// Compact heat-map summary suitable for checked fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIndexHeatMapSummary {
    /// Report schema version.
    pub schema_version: u32,
    /// Total workload cells.
    pub total_workloads: usize,
    /// Cool workload count.
    pub cool: usize,
    /// Warm workload count.
    pub warm: usize,
    /// Hot workload count.
    pub hot: usize,
    /// Saturated workload count.
    pub saturated: usize,
    /// Run-now admission count.
    pub run_now: usize,
    /// Deferred admission count.
    pub defer: usize,
    /// Throttled admission count.
    pub throttle: usize,
    /// Sharded admission count.
    pub shard: usize,
    /// Degraded-coverage admission count.
    pub mark_coverage_degraded: usize,
    /// Maximum estimated freshness lag across workloads.
    pub max_estimated_freshness_lag_ms: u64,
    /// Concise operator summary.
    pub operator_summary: String,
}

impl StorageIndexHeatMapSummary {
    /// Render a stable compact TOON-like fixture.
    #[must_use]
    pub fn to_toon(&self) -> String {
        format!(
            "schema_version: {}\ntotal_workloads: {}\nheat_levels: cool={} warm={} hot={} saturated={}\nadmissions: run_now={} defer={} throttle={} shard={} mark_coverage_degraded={}\nmax_estimated_freshness_lag_ms: {}\noperator_summary: {}\n",
            self.schema_version,
            self.total_workloads,
            self.cool,
            self.warm,
            self.hot,
            self.saturated,
            self.run_now,
            self.defer,
            self.throttle,
            self.shard,
            self.mark_coverage_degraded,
            self.max_estimated_freshness_lag_ms,
            self.operator_summary
        )
    }
}

/// Full deterministic heat-map report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIndexHeatMapReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Decisions sorted by workload id.
    pub cells: Vec<StorageIndexHeatMapCell>,
}

impl StorageIndexHeatMapReport {
    /// Build a compact summary from workload cells.
    #[must_use]
    pub fn summary(&self) -> StorageIndexHeatMapSummary {
        let mut summary = StorageIndexHeatMapSummary {
            schema_version: self.schema_version,
            total_workloads: self.cells.len(),
            cool: 0,
            warm: 0,
            hot: 0,
            saturated: 0,
            run_now: 0,
            defer: 0,
            throttle: 0,
            shard: 0,
            mark_coverage_degraded: 0,
            max_estimated_freshness_lag_ms: 0,
            operator_summary: String::new(),
        };

        for cell in &self.cells {
            match cell.heat_level {
                IoHeatLevel::Cool => summary.cool += 1,
                IoHeatLevel::Warm => summary.warm += 1,
                IoHeatLevel::Hot => summary.hot += 1,
                IoHeatLevel::Saturated => summary.saturated += 1,
            }
            match cell.admission_action {
                StorageAdmissionAction::RunNow => summary.run_now += 1,
                StorageAdmissionAction::Defer => summary.defer += 1,
                StorageAdmissionAction::Throttle => summary.throttle += 1,
                StorageAdmissionAction::Shard => summary.shard += 1,
                StorageAdmissionAction::MarkCoverageDegraded => {
                    summary.mark_coverage_degraded += 1;
                }
            }
            summary.max_estimated_freshness_lag_ms = summary
                .max_estimated_freshness_lag_ms
                .max(cell.estimated_freshness_lag_ms);
        }

        summary.operator_summary = format!(
            "storage_index_heatmap: {} cool, {} warm, {} hot, {} saturated; admissions run_now={} defer={} throttle={} shard={} degraded={}; max_lag={}ms",
            summary.cool,
            summary.warm,
            summary.hot,
            summary.saturated,
            summary.run_now,
            summary.defer,
            summary.throttle,
            summary.shard,
            summary.mark_coverage_degraded,
            summary.max_estimated_freshness_lag_ms
        );
        summary
    }
}

/// Evaluate storage/indexing heat-map cells deterministically.
#[must_use]
pub fn evaluate_storage_index_heatmap(
    policy: &StorageIndexHeatPolicy,
    workloads: &[StorageIndexWorkloadInput],
) -> StorageIndexHeatMapReport {
    let mut sorted = workloads.to_vec();
    sorted.sort_by(|left, right| left.workload_id.cmp(&right.workload_id));

    let cells = sorted
        .iter()
        .map(|workload| decide_storage_index_cell(policy, workload))
        .collect();

    StorageIndexHeatMapReport {
        schema_version: STORAGE_INDEX_HEATMAP_SCHEMA_VERSION,
        cells,
    }
}

fn decide_storage_index_cell(
    policy: &StorageIndexHeatPolicy,
    workload: &StorageIndexWorkloadInput,
) -> StorageIndexHeatMapCell {
    let indexing_saturated = workload.pending_index_segments >= policy.indexing_saturation_segments;
    let upstream_deferred = workload.capture_tier_summary.deferred > 0;
    let write_hot = workload.write_bytes_per_sec >= policy.hot_write_bytes_per_sec;
    let write_saturated = workload.write_bytes_per_sec >= policy.saturated_write_bytes_per_sec;
    let search_burst = workload.search_queries_per_sec >= policy.search_burst_queries_per_sec;
    let replay_hot =
        workload.replay_artifact_read_bytes_per_sec >= policy.replay_artifact_hot_bytes_per_sec;
    let compaction_debt = workload.compaction_debt_bytes >= policy.compaction_debt_bytes;
    let recovering = workload.pressure_drop_observed
        && workload.previous_action.is_some()
        && workload.write_bytes_per_sec <= policy.recovery_write_bytes_per_sec
        && workload.pending_index_segments <= policy.recovery_index_segments
        && workload.compaction_debt_bytes < policy.compaction_debt_bytes
        && !upstream_deferred;
    let signals = IoHeatSignals {
        write_hot,
        write_saturated,
        indexing_saturated,
        upstream_deferred,
        search_burst,
        replay_hot,
        compaction_debt,
        recovering,
    };

    let heat_level = classify_heat(signals, workload);
    let (admission_action, reasons, freshness_impact) = decide_admission(signals);
    let estimated_freshness_lag_ms = estimate_freshness_lag_ms(policy, workload, admission_action);
    let operator_note = format!(
        "{} -> {} ({})",
        workload.workload_id,
        admission_action.as_str(),
        reasons.join("+")
    );

    StorageIndexHeatMapCell {
        workload_id: workload.workload_id.clone(),
        workload_class: workload.workload_class.clone(),
        pane_count: workload.pane_count,
        heat_level,
        admission_action,
        reasons,
        freshness_impact,
        estimated_freshness_lag_ms,
        operator_note,
    }
}

#[derive(Debug, Clone, Copy)]
struct IoHeatSignals {
    write_hot: bool,
    write_saturated: bool,
    indexing_saturated: bool,
    upstream_deferred: bool,
    search_burst: bool,
    replay_hot: bool,
    compaction_debt: bool,
    recovering: bool,
}

fn classify_heat(signals: IoHeatSignals, workload: &StorageIndexWorkloadInput) -> IoHeatLevel {
    if signals.write_saturated || signals.indexing_saturated {
        return IoHeatLevel::Saturated;
    }
    if signals.write_hot || signals.search_burst || signals.replay_hot || signals.compaction_debt {
        return IoHeatLevel::Hot;
    }
    if signals.recovering {
        return IoHeatLevel::Cool;
    }
    if workload.write_bytes_per_sec > 0
        || workload.pending_index_segments > 0
        || workload.compaction_debt_bytes > 0
        || workload.search_queries_per_sec > 0
    {
        return IoHeatLevel::Warm;
    }
    IoHeatLevel::Cool
}

fn decide_admission(
    signals: IoHeatSignals,
) -> (StorageAdmissionAction, Vec<String>, CoverageFreshnessImpact) {
    if signals.indexing_saturated || signals.upstream_deferred {
        let mut reasons = Vec::new();
        if signals.indexing_saturated {
            reasons.push(StorageAdmissionReason::IndexingBacklog.as_str().to_string());
        }
        if signals.upstream_deferred {
            reasons.push(
                StorageAdmissionReason::CoverageAlreadyDeferred
                    .as_str()
                    .to_string(),
            );
        }
        return (
            StorageAdmissionAction::MarkCoverageDegraded,
            reasons,
            CoverageFreshnessImpact::CoverageDegraded,
        );
    }

    if signals.write_hot {
        return (
            StorageAdmissionAction::Throttle,
            vec![
                StorageAdmissionReason::WriteHeavyCapture
                    .as_str()
                    .to_string(),
            ],
            CoverageFreshnessImpact::DelayedFreshness,
        );
    }

    if signals.search_burst || signals.replay_hot {
        let mut reasons = Vec::new();
        if signals.search_burst {
            reasons.push(StorageAdmissionReason::SearchBurst.as_str().to_string());
        }
        if signals.replay_hot {
            reasons.push(
                StorageAdmissionReason::ReplayArtifactIo
                    .as_str()
                    .to_string(),
            );
        }
        return (
            StorageAdmissionAction::Shard,
            reasons,
            CoverageFreshnessImpact::BufferedFresh,
        );
    }

    if signals.compaction_debt {
        return (
            StorageAdmissionAction::Defer,
            vec![StorageAdmissionReason::CompactionDebt.as_str().to_string()],
            CoverageFreshnessImpact::DelayedFreshness,
        );
    }

    if signals.recovering {
        return (
            StorageAdmissionAction::RunNow,
            vec![
                StorageAdmissionReason::RecoveryAfterPressureDrops
                    .as_str()
                    .to_string(),
            ],
            CoverageFreshnessImpact::BufferedFresh,
        );
    }

    (
        StorageAdmissionAction::RunNow,
        Vec::new(),
        CoverageFreshnessImpact::RealtimeComplete,
    )
}

fn estimate_freshness_lag_ms(
    policy: &StorageIndexHeatPolicy,
    workload: &StorageIndexWorkloadInput,
    admission_action: StorageAdmissionAction,
) -> u64 {
    match admission_action {
        StorageAdmissionAction::RunNow => 0,
        StorageAdmissionAction::Throttle => {
            ceil_div(workload.write_bytes_per_sec, policy.hot_write_bytes_per_sec)
                * policy.write_lag_ms_per_hot_multiple
        }
        StorageAdmissionAction::Shard => policy.search_burst_lag_ms,
        StorageAdmissionAction::Defer => {
            ceil_div(workload.compaction_debt_bytes, GIB) * policy.compaction_lag_ms_per_gib
        }
        StorageAdmissionAction::MarkCoverageDegraded => workload
            .pending_index_segments
            .saturating_mul(policy.freshness_lag_ms_per_index_segment)
            .saturating_add(
                (workload.capture_tier_summary.deferred as u64)
                    .saturating_mul(policy.deferred_tier_lag_ms),
            ),
    }
}

fn ceil_div(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        0
    } else {
        1 + (value - 1) / divisor
    }
}

/// Deterministic fixture inputs for JSON/TOON heat-map summaries.
#[must_use]
pub fn storage_index_heatmap_golden_workloads() -> Vec<StorageIndexWorkloadInput> {
    vec![
        StorageIndexWorkloadInput {
            workload_id: "capture-heavy".to_string(),
            workload_class: "write_heavy_capture".to_string(),
            pane_count: 800,
            write_bytes_per_sec: 20 * MIB,
            pending_index_segments: 128,
            compaction_debt_bytes: 512 * MIB,
            replay_artifact_read_bytes_per_sec: MIB,
            search_queries_per_sec: 12,
            pressure_drop_observed: false,
            previous_action: None,
            capture_tier_summary: capture_summary(800, 600, 180, 20, 0),
        },
        StorageIndexWorkloadInput {
            workload_id: "compaction-debt".to_string(),
            workload_class: "compaction_backlog".to_string(),
            pane_count: 360,
            write_bytes_per_sec: 2 * MIB,
            pending_index_segments: 96,
            compaction_debt_bytes: 3 * GIB,
            replay_artifact_read_bytes_per_sec: MIB,
            search_queries_per_sec: 8,
            pressure_drop_observed: false,
            previous_action: Some(StorageAdmissionAction::RunNow),
            capture_tier_summary: capture_summary(360, 200, 120, 40, 0),
        },
        StorageIndexWorkloadInput {
            workload_id: "indexing-backlog".to_string(),
            workload_class: "indexing_backlog".to_string(),
            pane_count: 1_200,
            write_bytes_per_sec: 4 * MIB,
            pending_index_segments: 1_500,
            compaction_debt_bytes: 256 * MIB,
            replay_artifact_read_bytes_per_sec: 2 * MIB,
            search_queries_per_sec: 48,
            pressure_drop_observed: false,
            previous_action: Some(StorageAdmissionAction::Throttle),
            capture_tier_summary: capture_summary(1_200, 420, 500, 278, 2),
        },
        StorageIndexWorkloadInput {
            workload_id: "recovery".to_string(),
            workload_class: "recovery_after_pressure_drop".to_string(),
            pane_count: 256,
            write_bytes_per_sec: MIB,
            pending_index_segments: 32,
            compaction_debt_bytes: 256 * MIB,
            replay_artifact_read_bytes_per_sec: 512 * 1024,
            search_queries_per_sec: 4,
            pressure_drop_observed: true,
            previous_action: Some(StorageAdmissionAction::Defer),
            capture_tier_summary: capture_summary(256, 64, 160, 32, 0),
        },
        StorageIndexWorkloadInput {
            workload_id: "search-burst".to_string(),
            workload_class: "search_burst".to_string(),
            pane_count: 600,
            write_bytes_per_sec: 2 * MIB,
            pending_index_segments: 64,
            compaction_debt_bytes: 128 * MIB,
            replay_artifact_read_bytes_per_sec: MIB,
            search_queries_per_sec: 240,
            pressure_drop_observed: false,
            previous_action: Some(StorageAdmissionAction::RunNow),
            capture_tier_summary: capture_summary(600, 240, 300, 60, 0),
        },
    ]
}

fn capture_summary(
    total_panes: usize,
    hot: usize,
    warm: usize,
    cold: usize,
    deferred: usize,
) -> AdaptiveCaptureTierSummary {
    AdaptiveCaptureTierSummary {
        schema_version: 1,
        total_panes,
        hot,
        warm,
        cold,
        deferred,
        degraded_receipts: deferred,
        search_full_realtime: hot,
        search_buffered_catchup: warm,
        search_summary_only: cold,
        search_deferred_with_gap: deferred,
        operator_summary: format!(
            "adaptive_capture_indexing: {hot} hot, {warm} warm, {cold} cold, {deferred} deferred; {deferred} explicit degraded-fidelity receipts"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden_report() -> StorageIndexHeatMapReport {
        evaluate_storage_index_heatmap(
            &StorageIndexHeatPolicy::default(),
            &storage_index_heatmap_golden_workloads(),
        )
    }

    fn cell<'a>(report: &'a StorageIndexHeatMapReport, id: &str) -> &'a StorageIndexHeatMapCell {
        report
            .cells
            .iter()
            .find(|cell| cell.workload_id == id)
            .unwrap()
    }

    #[test]
    fn heatmap_write_heavy_capture_throttles_with_lag() {
        let report = golden_report();
        let cell = cell(&report, "capture-heavy");

        assert_eq!(cell.heat_level, IoHeatLevel::Hot);
        assert_eq!(cell.admission_action, StorageAdmissionAction::Throttle);
        assert_eq!(
            cell.freshness_impact,
            CoverageFreshnessImpact::DelayedFreshness
        );
        assert_eq!(cell.estimated_freshness_lag_ms, 6_000);
        assert!(cell.reasons.contains(&"write_heavy_capture".to_string()));
    }

    #[test]
    fn heatmap_indexing_backlog_marks_coverage_degraded() {
        let report = golden_report();
        let cell = cell(&report, "indexing-backlog");

        assert_eq!(cell.heat_level, IoHeatLevel::Saturated);
        assert_eq!(
            cell.admission_action,
            StorageAdmissionAction::MarkCoverageDegraded
        );
        assert_eq!(
            cell.freshness_impact,
            CoverageFreshnessImpact::CoverageDegraded
        );
        assert_eq!(cell.estimated_freshness_lag_ms, 25_000);
        assert!(cell.reasons.contains(&"indexing_backlog".to_string()));
        assert!(
            cell.reasons
                .contains(&"coverage_already_deferred".to_string())
        );
    }

    #[test]
    fn heatmap_search_burst_shards_queries() {
        let report = golden_report();
        let cell = cell(&report, "search-burst");

        assert_eq!(cell.heat_level, IoHeatLevel::Hot);
        assert_eq!(cell.admission_action, StorageAdmissionAction::Shard);
        assert_eq!(
            cell.freshness_impact,
            CoverageFreshnessImpact::BufferedFresh
        );
        assert_eq!(cell.estimated_freshness_lag_ms, 1_500);
        assert!(cell.reasons.contains(&"search_burst".to_string()));
    }

    #[test]
    fn heatmap_compaction_debt_defers_until_pressure_drops() {
        let report = golden_report();
        let cell = cell(&report, "compaction-debt");

        assert_eq!(cell.heat_level, IoHeatLevel::Hot);
        assert_eq!(cell.admission_action, StorageAdmissionAction::Defer);
        assert_eq!(
            cell.freshness_impact,
            CoverageFreshnessImpact::DelayedFreshness
        );
        assert_eq!(cell.estimated_freshness_lag_ms, 22_500);
        assert!(cell.reasons.contains(&"compaction_debt".to_string()));
    }

    #[test]
    fn heatmap_recovery_after_pressure_drops_runs_now() {
        let report = golden_report();
        let cell = cell(&report, "recovery");

        assert_eq!(cell.heat_level, IoHeatLevel::Cool);
        assert_eq!(cell.admission_action, StorageAdmissionAction::RunNow);
        assert_eq!(
            cell.freshness_impact,
            CoverageFreshnessImpact::BufferedFresh
        );
        assert_eq!(cell.estimated_freshness_lag_ms, 0);
        assert!(
            cell.reasons
                .contains(&"recovery_after_pressure_drops".to_string())
        );
    }

    #[test]
    fn heatmap_golden_json_and_toon_fixtures_match() {
        let expected_json: StorageIndexHeatMapSummary = serde_json::from_str(include_str!(
            "../../../fixtures/scale-lab/storage-index-heatmap-summary.v1.json"
        ))
        .unwrap();
        let expected_toon =
            include_str!("../../../fixtures/scale-lab/storage-index-heatmap-summary.v1.toon");
        let summary = golden_report().summary();

        assert_eq!(summary, expected_json);
        assert_eq!(summary.to_toon(), expected_toon);
    }
}
