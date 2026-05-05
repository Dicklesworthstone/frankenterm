//! Adaptive capture/indexing tier proof model for massive-swarm replay labs.
//!
//! This module is intentionally pure and deterministic. It models the receipts
//! that live ingest/search code must eventually emit when a large pane set is
//! tiered down, without mutating the active runtime, storage, or search paths.

use serde::{Deserialize, Serialize};

/// Schema version for adaptive capture/indexing tier reports.
pub const ADAPTIVE_CAPTURE_TIER_SCHEMA_VERSION: u32 = 1;

/// Capture and indexing tier assigned to a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureIndexTier {
    /// Real-time capture and indexing.
    Hot,
    /// Buffered capture with near-real-time indexing.
    Warm,
    /// Retained summary/checkpoint fidelity for idle panes.
    Cold,
    /// Capture/indexing is intentionally deferred with an explicit gap receipt.
    Deferred,
}

impl CaptureIndexTier {
    /// Stable key for summaries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Warm => "warm",
            Self::Cold => "cold",
            Self::Deferred => "deferred",
        }
    }
}

/// Search coverage disclosure attached to a tier decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchCoverageDisclosure {
    /// Search sees raw captured output immediately.
    FullRealtime,
    /// Search is complete after buffered indexing catches up.
    BufferedCatchup,
    /// Search sees retained summaries/checkpoints, not every raw delta.
    SummaryOnly,
    /// Search coverage is explicitly deferred and has a gap receipt.
    DeferredWithGap,
}

/// Stable reason code for a tier decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureTierReason {
    /// Pane is producing a burst and should remain hot.
    PaneBurst,
    /// Pane has live-ish activity but does not need hot treatment.
    BufferedWarm,
    /// Idle panes cool to summary/checkpoint fidelity.
    IdlePaneCooling,
    /// Previously degraded pane recovered after pressure dropped.
    RecoveryAfterPressureDrop,
    /// Capture backlog requires explicit deferred fidelity.
    CaptureBacklogPressure,
    /// Indexing backlog requires explicit deferred search coverage.
    IndexingSaturation,
    /// Memory pressure requires deferred capture/indexing.
    MemoryPressure,
}

impl CaptureTierReason {
    /// Stable key for receipts and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PaneBurst => "pane_burst",
            Self::BufferedWarm => "buffered_warm",
            Self::IdlePaneCooling => "idle_pane_cooling",
            Self::RecoveryAfterPressureDrop => "recovery_after_pressure_drop",
            Self::CaptureBacklogPressure => "capture_backlog_pressure",
            Self::IndexingSaturation => "indexing_saturation",
            Self::MemoryPressure => "memory_pressure",
        }
    }
}

/// Thresholds used by the deterministic tier planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureTierPolicy {
    /// Bytes/sec at or above this threshold stay hot.
    pub hot_bytes_per_sec: u64,
    /// Bytes/sec at or above this threshold stay warm.
    pub warm_bytes_per_sec: u64,
    /// Idle duration after which a quiet pane can cool.
    pub cold_idle_ms: u64,
    /// Capture backlog that forces deferred fidelity.
    pub deferred_backlog_segments: u64,
    /// Pending index segments that force deferred search coverage.
    pub indexing_saturation_segments: u64,
    /// Memory pressure percentage that forces deferred fidelity.
    pub deferred_memory_pressure_pct: u8,
    /// Memory pressure percentage below which deferred panes can recover.
    pub recovery_memory_pressure_pct: u8,
}

impl Default for CaptureTierPolicy {
    fn default() -> Self {
        Self {
            hot_bytes_per_sec: 1_000_000,
            warm_bytes_per_sec: 64_000,
            cold_idle_ms: 300_000,
            deferred_backlog_segments: 4_096,
            indexing_saturation_segments: 1_024,
            deferred_memory_pressure_pct: 90,
            recovery_memory_pressure_pct: 70,
        }
    }
}

/// Shared environment snapshot for one planning pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureTierEnvironment {
    /// Host memory pressure percentage.
    pub memory_pressure_pct: u8,
    /// Pending global capture backlog.
    pub global_capture_backlog_segments: u64,
    /// Pending global indexing backlog.
    pub global_index_backlog_segments: u64,
}

impl CaptureTierEnvironment {
    /// Low-pressure fixture used by deterministic tests and docs.
    #[must_use]
    pub const fn low_pressure() -> Self {
        Self {
            memory_pressure_pct: 42,
            global_capture_backlog_segments: 0,
            global_index_backlog_segments: 0,
        }
    }
}

/// Per-pane input to the tier planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTierInput {
    /// Stable pane id.
    pub pane_id: u64,
    /// Recent output rate in bytes/sec.
    pub bytes_per_sec: u64,
    /// Time since meaningful output.
    pub idle_ms: u64,
    /// Unpersisted capture backlog for this pane.
    pub capture_backlog_segments: u64,
    /// Unindexed segment backlog for this pane.
    pub pending_index_segments: u64,
    /// Retained bytes attributed to this pane.
    pub retained_bytes: u64,
    /// Previous tier, when known.
    pub previous_tier: Option<CaptureIndexTier>,
}

/// Explicit receipt for degraded or deferred fidelity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DegradedFidelityReceipt {
    /// Stable receipt id.
    pub receipt_id: String,
    /// Whether the gap/degradation is explicit.
    pub explicit_gap: bool,
    /// Machine-readable fidelity class.
    pub fidelity: String,
    /// Operator-facing explanation.
    pub message: String,
}

/// One pane's deterministic tier decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureTierDecision {
    /// Pane id.
    pub pane_id: u64,
    /// Previous tier, when known.
    pub previous_tier: Option<CaptureIndexTier>,
    /// Assigned tier.
    pub tier: CaptureIndexTier,
    /// Stable reason codes.
    pub reasons: Vec<String>,
    /// Search coverage users can expect from this pane.
    pub search_coverage: SearchCoverageDisclosure,
    /// Degraded-fidelity receipt, required for deferred coverage.
    pub degraded_fidelity: Option<DegradedFidelityReceipt>,
}

impl CaptureTierDecision {
    /// Whether this decision is allowed to drop or defer raw deltas.
    #[must_use]
    pub fn has_explicit_gap_receipt(&self) -> bool {
        self.degraded_fidelity
            .as_ref()
            .is_some_and(|receipt| receipt.explicit_gap)
    }
}

/// Compact report summary suitable for checked golden fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptiveCaptureTierSummary {
    /// Report schema version.
    pub schema_version: u32,
    /// Total pane decisions.
    pub total_panes: usize,
    /// Hot pane count.
    pub hot: usize,
    /// Warm pane count.
    pub warm: usize,
    /// Cold pane count.
    pub cold: usize,
    /// Deferred pane count.
    pub deferred: usize,
    /// Explicit degraded-fidelity receipts.
    pub degraded_receipts: usize,
    /// Full real-time search coverage count.
    pub search_full_realtime: usize,
    /// Buffered search coverage count.
    pub search_buffered_catchup: usize,
    /// Summary-only search coverage count.
    pub search_summary_only: usize,
    /// Deferred-with-gap search coverage count.
    pub search_deferred_with_gap: usize,
    /// Concise operator summary.
    pub operator_summary: String,
}

/// Full deterministic report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptiveCaptureTierReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Decisions sorted by pane id.
    pub decisions: Vec<CaptureTierDecision>,
}

impl AdaptiveCaptureTierReport {
    /// Build a compact summary from pane decisions.
    #[must_use]
    pub fn summary(&self) -> AdaptiveCaptureTierSummary {
        let mut summary = AdaptiveCaptureTierSummary {
            schema_version: self.schema_version,
            total_panes: self.decisions.len(),
            hot: 0,
            warm: 0,
            cold: 0,
            deferred: 0,
            degraded_receipts: 0,
            search_full_realtime: 0,
            search_buffered_catchup: 0,
            search_summary_only: 0,
            search_deferred_with_gap: 0,
            operator_summary: String::new(),
        };

        for decision in &self.decisions {
            match decision.tier {
                CaptureIndexTier::Hot => summary.hot += 1,
                CaptureIndexTier::Warm => summary.warm += 1,
                CaptureIndexTier::Cold => summary.cold += 1,
                CaptureIndexTier::Deferred => summary.deferred += 1,
            }
            if decision.degraded_fidelity.is_some() {
                summary.degraded_receipts += 1;
            }
            match decision.search_coverage {
                SearchCoverageDisclosure::FullRealtime => summary.search_full_realtime += 1,
                SearchCoverageDisclosure::BufferedCatchup => {
                    summary.search_buffered_catchup += 1;
                }
                SearchCoverageDisclosure::SummaryOnly => summary.search_summary_only += 1,
                SearchCoverageDisclosure::DeferredWithGap => {
                    summary.search_deferred_with_gap += 1;
                }
            }
        }

        summary.operator_summary = format!(
            "adaptive_capture_indexing: {} hot, {} warm, {} cold, {} deferred; {} explicit degraded-fidelity receipts",
            summary.hot, summary.warm, summary.cold, summary.deferred, summary.degraded_receipts
        );
        summary
    }

    /// Validate that deferred decisions have explicit receipts.
    #[must_use]
    pub fn deferred_without_receipts(&self) -> Vec<u64> {
        self.decisions
            .iter()
            .filter(|decision| decision.tier == CaptureIndexTier::Deferred)
            .filter(|decision| !decision.has_explicit_gap_receipt())
            .map(|decision| decision.pane_id)
            .collect()
    }
}

/// Evaluate pane capture/indexing tiers deterministically.
#[must_use]
pub fn evaluate_adaptive_capture_tiers(
    policy: &CaptureTierPolicy,
    environment: &CaptureTierEnvironment,
    panes: &[PaneTierInput],
) -> AdaptiveCaptureTierReport {
    let mut sorted = panes.to_vec();
    sorted.sort_by_key(|pane| pane.pane_id);

    let decisions = sorted
        .iter()
        .map(|pane| decide_pane_tier(policy, environment, pane))
        .collect();

    AdaptiveCaptureTierReport {
        schema_version: ADAPTIVE_CAPTURE_TIER_SCHEMA_VERSION,
        decisions,
    }
}

fn decide_pane_tier(
    policy: &CaptureTierPolicy,
    environment: &CaptureTierEnvironment,
    pane: &PaneTierInput,
) -> CaptureTierDecision {
    let memory_forces_defer =
        environment.memory_pressure_pct >= policy.deferred_memory_pressure_pct;
    let capture_forces_defer = pane.capture_backlog_segments >= policy.deferred_backlog_segments
        || environment.global_capture_backlog_segments >= policy.deferred_backlog_segments;
    let index_forces_defer = pane.pending_index_segments >= policy.indexing_saturation_segments
        || environment.global_index_backlog_segments >= policy.indexing_saturation_segments;

    if memory_forces_defer || capture_forces_defer || index_forces_defer {
        let mut reasons = Vec::new();
        if memory_forces_defer {
            reasons.push(CaptureTierReason::MemoryPressure.as_str().to_string());
        }
        if capture_forces_defer {
            reasons.push(
                CaptureTierReason::CaptureBacklogPressure
                    .as_str()
                    .to_string(),
            );
        }
        if index_forces_defer {
            reasons.push(CaptureTierReason::IndexingSaturation.as_str().to_string());
        }
        return build_decision(
            pane,
            CaptureIndexTier::Deferred,
            reasons,
            SearchCoverageDisclosure::DeferredWithGap,
            Some(degraded_receipt(pane, &environment_reason(environment))),
        );
    }

    if pane.previous_tier == Some(CaptureIndexTier::Deferred)
        && environment.memory_pressure_pct <= policy.recovery_memory_pressure_pct
        && pane.capture_backlog_segments == 0
        && pane.pending_index_segments < policy.indexing_saturation_segments
    {
        return build_decision(
            pane,
            CaptureIndexTier::Warm,
            vec![
                CaptureTierReason::RecoveryAfterPressureDrop
                    .as_str()
                    .to_string(),
            ],
            SearchCoverageDisclosure::BufferedCatchup,
            None,
        );
    }

    if pane.bytes_per_sec >= policy.hot_bytes_per_sec {
        return build_decision(
            pane,
            CaptureIndexTier::Hot,
            vec![CaptureTierReason::PaneBurst.as_str().to_string()],
            SearchCoverageDisclosure::FullRealtime,
            None,
        );
    }

    if pane.bytes_per_sec >= policy.warm_bytes_per_sec || pane.capture_backlog_segments > 0 {
        return build_decision(
            pane,
            CaptureIndexTier::Warm,
            vec![CaptureTierReason::BufferedWarm.as_str().to_string()],
            SearchCoverageDisclosure::BufferedCatchup,
            None,
        );
    }

    if pane.idle_ms >= policy.cold_idle_ms {
        return build_decision(
            pane,
            CaptureIndexTier::Cold,
            vec![CaptureTierReason::IdlePaneCooling.as_str().to_string()],
            SearchCoverageDisclosure::SummaryOnly,
            None,
        );
    }

    build_decision(
        pane,
        CaptureIndexTier::Warm,
        vec![CaptureTierReason::BufferedWarm.as_str().to_string()],
        SearchCoverageDisclosure::BufferedCatchup,
        None,
    )
}

fn build_decision(
    pane: &PaneTierInput,
    tier: CaptureIndexTier,
    reasons: Vec<String>,
    search_coverage: SearchCoverageDisclosure,
    degraded_fidelity: Option<DegradedFidelityReceipt>,
) -> CaptureTierDecision {
    CaptureTierDecision {
        pane_id: pane.pane_id,
        previous_tier: pane.previous_tier,
        tier,
        reasons,
        search_coverage,
        degraded_fidelity,
    }
}

fn degraded_receipt(pane: &PaneTierInput, reason: &str) -> DegradedFidelityReceipt {
    DegradedFidelityReceipt {
        receipt_id: format!("capture-tier-gap-pane-{}", pane.pane_id),
        explicit_gap: true,
        fidelity: "raw_delta_deferred".to_string(),
        message: format!(
            "pane {} raw capture/indexing deferred because {reason}; search coverage must disclose deferred fidelity",
            pane.pane_id
        ),
    }
}

fn environment_reason(environment: &CaptureTierEnvironment) -> String {
    format!(
        "memory={}%, global_capture_backlog={}, global_index_backlog={}",
        environment.memory_pressure_pct,
        environment.global_capture_backlog_segments,
        environment.global_index_backlog_segments
    )
}

/// Deterministic six-pane fixture used by the golden status summary.
#[must_use]
pub fn adaptive_capture_tier_golden_panes() -> Vec<PaneTierInput> {
    vec![
        PaneTierInput {
            pane_id: 1,
            bytes_per_sec: 2_000_000,
            idle_ms: 0,
            capture_backlog_segments: 0,
            pending_index_segments: 0,
            retained_bytes: 32_768,
            previous_tier: Some(CaptureIndexTier::Warm),
        },
        PaneTierInput {
            pane_id: 2,
            bytes_per_sec: 96_000,
            idle_ms: 2_000,
            capture_backlog_segments: 8,
            pending_index_segments: 64,
            retained_bytes: 65_536,
            previous_tier: Some(CaptureIndexTier::Warm),
        },
        PaneTierInput {
            pane_id: 3,
            bytes_per_sec: 512,
            idle_ms: 600_000,
            capture_backlog_segments: 0,
            pending_index_segments: 0,
            retained_bytes: 8_192,
            previous_tier: Some(CaptureIndexTier::Warm),
        },
        PaneTierInput {
            pane_id: 4,
            bytes_per_sec: 8_000,
            idle_ms: 1_000,
            capture_backlog_segments: 4_500,
            pending_index_segments: 64,
            retained_bytes: 524_288,
            previous_tier: Some(CaptureIndexTier::Warm),
        },
        PaneTierInput {
            pane_id: 5,
            bytes_per_sec: 32_000,
            idle_ms: 5_000,
            capture_backlog_segments: 32,
            pending_index_segments: 1_500,
            retained_bytes: 262_144,
            previous_tier: Some(CaptureIndexTier::Hot),
        },
        PaneTierInput {
            pane_id: 6,
            bytes_per_sec: 80_000,
            idle_ms: 1_000,
            capture_backlog_segments: 0,
            pending_index_segments: 32,
            retained_bytes: 131_072,
            previous_tier: Some(CaptureIndexTier::Deferred),
        },
    ]
}

/// Deterministic larger synthetic pane set for scale proof tests.
#[must_use]
pub fn synthetic_capture_tier_panes(pane_count: u64) -> Vec<PaneTierInput> {
    (1..=pane_count)
        .map(|pane_id| {
            let burst = pane_id % 17 == 0;
            let deferred = pane_id % 29 == 0;
            let indexing_hotspot = pane_id % 31 == 0;
            let idle = pane_id % 7 == 0;
            PaneTierInput {
                pane_id,
                bytes_per_sec: if burst {
                    1_500_000
                } else if indexing_hotspot {
                    96_000
                } else if idle {
                    512
                } else {
                    32_000
                },
                idle_ms: if idle { 900_000 } else { 1_000 },
                capture_backlog_segments: if deferred { 4_500 } else { pane_id % 11 },
                pending_index_segments: if indexing_hotspot {
                    1_500
                } else {
                    pane_id % 97
                },
                retained_bytes: pane_id.saturating_mul(4_096),
                previous_tier: if pane_id % 43 == 0 {
                    Some(CaptureIndexTier::Deferred)
                } else {
                    Some(CaptureIndexTier::Warm)
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden_report() -> AdaptiveCaptureTierReport {
        evaluate_adaptive_capture_tiers(
            &CaptureTierPolicy::default(),
            &CaptureTierEnvironment::low_pressure(),
            &adaptive_capture_tier_golden_panes(),
        )
    }

    #[test]
    fn pane_bursts_stay_hot_with_realtime_search() {
        let report = golden_report();
        let decision = report
            .decisions
            .iter()
            .find(|decision| decision.pane_id == 1)
            .unwrap();

        assert_eq!(decision.tier, CaptureIndexTier::Hot);
        assert_eq!(
            decision.search_coverage,
            SearchCoverageDisclosure::FullRealtime
        );
        assert!(decision.reasons.contains(&"pane_burst".to_string()));
        assert!(decision.degraded_fidelity.is_none());
    }

    #[test]
    fn idle_panes_cool_to_summary_only_search() {
        let report = golden_report();
        let decision = report
            .decisions
            .iter()
            .find(|decision| decision.pane_id == 3)
            .unwrap();

        assert_eq!(decision.tier, CaptureIndexTier::Cold);
        assert_eq!(
            decision.search_coverage,
            SearchCoverageDisclosure::SummaryOnly
        );
        assert!(decision.reasons.contains(&"idle_pane_cooling".to_string()));
    }

    #[test]
    fn backlogs_and_index_saturation_emit_explicit_gap_receipts() {
        let report = golden_report();
        let deferred = report
            .decisions
            .iter()
            .filter(|decision| decision.tier == CaptureIndexTier::Deferred)
            .collect::<Vec<_>>();

        assert_eq!(deferred.len(), 2);
        for decision in deferred {
            assert_eq!(
                decision.search_coverage,
                SearchCoverageDisclosure::DeferredWithGap
            );
            assert!(decision.has_explicit_gap_receipt());
        }
        assert!(report.deferred_without_receipts().is_empty());
    }

    #[test]
    fn recovered_deferred_panes_promote_to_warm_after_pressure_drops() {
        let report = golden_report();
        let decision = report
            .decisions
            .iter()
            .find(|decision| decision.pane_id == 6)
            .unwrap();

        assert_eq!(decision.previous_tier, Some(CaptureIndexTier::Deferred));
        assert_eq!(decision.tier, CaptureIndexTier::Warm);
        assert!(
            decision
                .reasons
                .contains(&"recovery_after_pressure_drop".to_string())
        );
        assert_eq!(
            decision.search_coverage,
            SearchCoverageDisclosure::BufferedCatchup
        );
    }

    #[test]
    fn large_synthetic_pane_set_has_no_silent_deferred_gaps() {
        let panes = synthetic_capture_tier_panes(2_048);
        let environment = CaptureTierEnvironment {
            memory_pressure_pct: 72,
            global_capture_backlog_segments: 0,
            global_index_backlog_segments: 0,
        };
        let report =
            evaluate_adaptive_capture_tiers(&CaptureTierPolicy::default(), &environment, &panes);
        let summary = report.summary();

        assert_eq!(summary.total_panes, 2_048);
        assert!(summary.hot > 0);
        assert!(summary.warm > 0);
        assert!(summary.cold > 0);
        assert!(summary.deferred > 0);
        assert_eq!(summary.deferred, summary.degraded_receipts);
        assert!(report.deferred_without_receipts().is_empty());
    }

    #[test]
    fn golden_status_summary_matches_checked_fixture() {
        let expected: AdaptiveCaptureTierSummary = serde_json::from_str(include_str!(
            "../../../fixtures/scale-lab/adaptive-capture-tier-summary.v1.json"
        ))
        .unwrap();

        assert_eq!(golden_report().summary(), expected);
    }
}
