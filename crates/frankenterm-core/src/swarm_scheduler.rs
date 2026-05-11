// =============================================================================
// Scheduler/rebalancer/autoscaler for live fleets (ft-3681t.3.2)
//
// Runtime scheduling that rebalances work and resizes fleets based on queue
// pressure, rate limits, failures, and policy constraints. Designed to avoid
// cascade failure patterns common in ad-hoc swarms.
//
// # Architecture
//
// ```text
// SwarmWorkQueue ──► QueuePressure ──► SwarmScheduler.evaluate()
//                                              │
//              LifecycleRegistry ──────────────►│
//                                              ▼
//                                     SchedulerDecision
//                                              │
//                    ┌────────┬────────┬────────┼─────────┐
//                    ▼        ▼        ▼        ▼         ▼
//                 Noop    Assign   Rebalance  ScaleUp  ScaleDown
//                                              │         │
//                                     FleetLauncher   drain/close
//                                              │
//                                  Anti-cascade guards:
//                                  - cooldown timers
//                                  - circuit breaker
//                                  - grace periods
// ```
// =============================================================================

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fleet_memory_controller::{FleetMemoryTierBudgetSnapshot, FleetPressureTier};
use crate::latency_stages::StagePressure;
use crate::priority::PanePriority;
use crate::swarm_work_queue::{AgentSlotId, QueueStats, SwarmWorkQueue, WorkItemId};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the swarm scheduler/rebalancer/autoscaler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchedulerConfig {
    /// Minimum time between scale-up operations (ms).
    pub scale_up_cooldown_ms: u64,
    /// Minimum time between scale-down operations (ms).
    pub scale_down_cooldown_ms: u64,
    /// Minimum fleet size (never scale below this).
    pub min_fleet_size: u32,
    /// Maximum fleet size (never scale above this).
    pub max_fleet_size: u32,
    /// Queue utilization ratio above which scale-up is triggered (0.0..1.0).
    pub scale_up_threshold: f64,
    /// Queue utilization ratio below which scale-down is triggered (0.0..1.0).
    pub scale_down_threshold: f64,
    /// Load imbalance ratio above which rebalancing is triggered (0.0..1.0).
    pub rebalance_imbalance_threshold: f64,
    /// Maximum consecutive scale operations before circuit breaker trips.
    pub max_consecutive_scale_ops: u32,
    /// Grace period (ms) before new agents are evaluated for scale-down.
    pub agent_startup_grace_ms: u64,
    /// Circuit breaker reset time (ms) after tripping.
    pub circuit_breaker_reset_ms: u64,
    /// Maximum scale-up step size (agents added per operation).
    pub max_scale_step: u32,
    /// Failure rate (0.0..1.0) above which scale-down is suppressed.
    pub failure_rate_suppress_threshold: f64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            scale_up_cooldown_ms: 60_000,
            scale_down_cooldown_ms: 120_000,
            min_fleet_size: 1,
            max_fleet_size: 64,
            scale_up_threshold: 0.85,
            scale_down_threshold: 0.20,
            rebalance_imbalance_threshold: 0.40,
            max_consecutive_scale_ops: 5,
            agent_startup_grace_ms: 30_000,
            circuit_breaker_reset_ms: 300_000,
            max_scale_step: 4,
            failure_rate_suppress_threshold: 0.50,
        }
    }
}

// =============================================================================
// Pressure / metrics types
// =============================================================================

/// Computed queue pressure metrics for scheduling decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuePressure {
    /// Ratio of ready items to total non-terminal items (0.0..1.0).
    pub ready_ratio: f64,
    /// Ratio of in-progress items to total agent capacity (0.0..1.0).
    pub utilization: f64,
    /// Number of items past the starvation threshold.
    pub starvation_count: u32,
    /// Recent failure rate (failures / total completions, 0.0..1.0).
    pub failure_rate: f64,
    /// Total non-terminal items in queue.
    pub pending_items: u32,
    /// Active agent count.
    pub active_agents: u32,
    /// Total agent capacity (active_agents * max_concurrent_per_agent).
    pub total_capacity: u32,
}

/// Synchronized event family that can create a fleet-wide herd wave.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HerdWaveEventKind {
    /// Many agents compact/context-rotate together.
    Compaction,
    /// Many agents retry a failed operation together.
    Retry,
    /// Many agents recover from a rate limit or quota window together.
    RateLimitRecovery,
    /// Many agents issue search/index work together.
    SearchBurst,
    /// Workflow fanout produced many near-simultaneous actions.
    WorkflowFanout,
    /// Many idle agents woke up at nearly the same time.
    Wake,
    /// Known herd signal that does not fit a narrower family yet.
    Other,
}

/// One timestamped signal used for herd-wave detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWaveSignal {
    /// Pane that emitted the signal, when pane-scoped.
    pub pane_id: Option<u64>,
    /// Synchronized event family.
    pub kind: HerdWaveEventKind,
    /// Event timestamp in epoch milliseconds.
    pub timestamp_ms: u64,
}

impl HerdWaveSignal {
    /// Build a pane-scoped signal.
    #[must_use]
    pub const fn pane(pane_id: u64, kind: HerdWaveEventKind, timestamp_ms: u64) -> Self {
        Self {
            pane_id: Some(pane_id),
            kind,
            timestamp_ms,
        }
    }
}

/// Deterministic policy for detecting and staggering herd waves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWaveDetectionConfig {
    /// Sliding window used to group synchronized signals.
    pub detection_window_ms: u64,
    /// Minimum distinct pane count before a burst is treated as a herd wave.
    pub min_distinct_panes: u32,
    /// Distinct pane count that maps to elevated pressure.
    pub elevated_distinct_panes: u32,
    /// Distinct pane count that maps to critical pressure.
    pub critical_distinct_panes: u32,
    /// Distinct pane count that maps to emergency pressure.
    pub emergency_distinct_panes: u32,
    /// Delay between adjacent actions when staggering a detected cohort.
    pub base_stagger_ms: u64,
    /// Maximum delay assigned to the tail of a staggered cohort.
    pub max_stagger_ms: u64,
}

impl Default for HerdWaveDetectionConfig {
    fn default() -> Self {
        Self {
            detection_window_ms: 30_000,
            min_distinct_panes: 3,
            elevated_distinct_panes: 3,
            critical_distinct_panes: 8,
            emergency_distinct_panes: 16,
            base_stagger_ms: 750,
            max_stagger_ms: 30_000,
        }
    }
}

/// Operator-facing summary for a synchronized herd-wave cohort.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWavePressureSummary {
    /// Synthesized pressure tier contributed by the herd wave.
    pub pressure_tier: FleetPressureTier,
    /// Whether the configured distinct-pane threshold was reached.
    pub detected: bool,
    /// Signals considered inside the active window.
    pub event_count: u32,
    /// Distinct pane count inside the active window.
    pub distinct_panes: u32,
    /// Detection window used for this summary.
    pub window_ms: u64,
    /// First signal timestamp in the active window.
    pub first_seen_ms: Option<u64>,
    /// Last signal timestamp in the active window.
    pub last_seen_ms: Option<u64>,
    /// Most common event family in the active window.
    pub dominant_kind: Option<HerdWaveEventKind>,
    /// Count of the dominant family.
    pub dominant_kind_count: u32,
    /// Recommended delay between adjacent cohort actions.
    pub recommended_stagger_ms: u64,
    /// Maximum delay assigned to the final action in this cohort.
    pub cohort_max_stagger_ms: u64,
}

/// One pane action scheduled after smoothing a synchronized herd wave.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWaveStaggeredAction {
    /// Pane whose synchronized signal is being staggered.
    pub pane_id: u64,
    /// Event family that put this pane into the active cohort.
    pub kind: HerdWaveEventKind,
    /// Original signal timestamp in epoch milliseconds.
    pub observed_at_ms: u64,
    /// Deterministic cohort order used for delay computation.
    pub cohort_rank: u32,
    /// Delay applied before this pane's follow-up action may run.
    pub delay_ms: u64,
    /// Absolute scheduled timestamp after smoothing.
    pub scheduled_at_ms: u64,
}

/// Replayable herd-wave smoothing plan with operator-facing diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWaveStaggerPlan {
    /// Detection summary that produced this plan.
    pub summary: HerdWavePressureSummary,
    /// Deterministically staggered pane actions for the detected cohort.
    pub actions: Vec<HerdWaveStaggeredAction>,
}

/// Contract id for the v1 herd-wave operator snapshot.
pub const HERD_WAVE_CONTRACT_ID: &str = "ft.herd_wave.v1";

/// Schema version for the v1 herd-wave operator snapshot.
pub const HERD_WAVE_SCHEMA_VERSION: u16 = 1;

/// Evidence posture for a herd-wave source row.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HerdWaveEvidenceState {
    /// Fresh evidence collected from the represented workspace/run.
    Measured,
    /// Derived from measured counters or event history.
    Inferred,
    /// Fixture, replay, synthetic, or model-only evidence.
    Simulated,
    /// Evidence exists but exceeded its freshness budget.
    Stale,
    /// Required evidence was absent or unwired.
    Unavailable,
    /// Root object combines domains with different states.
    Mixed,
}

/// Source class for a telemetry row.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HerdWaveTelemetrySourceKind {
    /// Live runtime source.
    Live,
    /// Deterministic fixture or replay source.
    Fixture,
    /// Source was present but degraded.
    Degraded,
    /// Source was not available.
    Unavailable,
}

/// Root operator state for the herd-wave contract.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HerdWaveOverallState {
    Normal,
    Elevated,
    Critical,
    Emergency,
    MissingTelemetry,
    StaleEvidence,
    PriorityProtected,
    OperatorOverride,
    CooldownActive,
    CircuitBreakerActive,
    Unknown,
}

/// Freshness and provenance for one telemetry input domain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWaveSourceFreshness {
    /// Stable source name.
    pub source: String,
    /// Source class.
    pub source_kind: HerdWaveTelemetrySourceKind,
    /// Source evidence posture.
    pub evidence_state: HerdWaveEvidenceState,
    /// When the source sample was generated.
    pub generated_at_ms: Option<u64>,
    /// Source age at root generation time.
    pub freshness_ms: Option<u64>,
    /// Maximum accepted source age.
    pub max_age_ms: u64,
    /// Stable reasons for unavailable/stale/degraded rows.
    pub reason_codes: Vec<String>,
}

impl HerdWaveSourceFreshness {
    /// Build a live source row and mark it stale when it exceeds `max_age_ms`.
    #[must_use]
    pub fn live(
        source: impl Into<String>,
        root_generated_at_ms: u64,
        source_generated_at_ms: u64,
        max_age_ms: u64,
    ) -> Self {
        let freshness_ms = root_generated_at_ms.saturating_sub(source_generated_at_ms);
        let evidence_state = if freshness_ms > max_age_ms {
            HerdWaveEvidenceState::Stale
        } else {
            HerdWaveEvidenceState::Measured
        };
        let mut reason_codes = Vec::new();
        if evidence_state == HerdWaveEvidenceState::Stale {
            reason_codes.push("herd_wave.telemetry.stale".to_string());
        }
        Self {
            source: source.into(),
            source_kind: HerdWaveTelemetrySourceKind::Live,
            evidence_state,
            generated_at_ms: Some(source_generated_at_ms),
            freshness_ms: Some(freshness_ms),
            max_age_ms,
            reason_codes,
        }
    }

    /// Build a fixture/source row for deterministic tests and replay.
    #[must_use]
    pub fn fixture(source: impl Into<String>, generated_at_ms: u64) -> Self {
        Self {
            source: source.into(),
            source_kind: HerdWaveTelemetrySourceKind::Fixture,
            evidence_state: HerdWaveEvidenceState::Simulated,
            generated_at_ms: Some(generated_at_ms),
            freshness_ms: Some(0),
            max_age_ms: 0,
            reason_codes: vec!["herd_wave.telemetry.fixture".to_string()],
        }
    }

    /// Build a degraded source row.
    #[must_use]
    pub fn degraded(
        source: impl Into<String>,
        generated_at_ms: Option<u64>,
        max_age_ms: u64,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            source_kind: HerdWaveTelemetrySourceKind::Degraded,
            evidence_state: HerdWaveEvidenceState::Inferred,
            generated_at_ms,
            freshness_ms: None,
            max_age_ms,
            reason_codes: vec![reason.into()],
        }
    }

    /// Build an unavailable source row.
    #[must_use]
    pub fn unavailable(source: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            source_kind: HerdWaveTelemetrySourceKind::Unavailable,
            evidence_state: HerdWaveEvidenceState::Unavailable,
            generated_at_ms: None,
            freshness_ms: None,
            max_age_ms: 0,
            reason_codes: vec![reason.into()],
        }
    }

    const fn is_unavailable(&self) -> bool {
        matches!(self.evidence_state, HerdWaveEvidenceState::Unavailable)
    }

    const fn is_stale(&self) -> bool {
        matches!(self.evidence_state, HerdWaveEvidenceState::Stale)
    }
}

/// One unavailable source projected into the v1 contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWaveUnavailableSource {
    pub source: String,
    pub evidence_state: HerdWaveEvidenceState,
    pub freshness_ms: Option<u64>,
    pub max_age_ms: u64,
    pub reason_codes: Vec<String>,
}

impl From<&HerdWaveSourceFreshness> for HerdWaveUnavailableSource {
    fn from(source: &HerdWaveSourceFreshness) -> Self {
        Self {
            source: source.source.clone(),
            evidence_state: source.evidence_state,
            freshness_ms: source.freshness_ms,
            max_age_ms: source.max_age_ms,
            reason_codes: source.reason_codes.clone(),
        }
    }
}

/// Admission controller state mirrored for herd-wave evidence without depending
/// on pane text or mutation surfaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWaveCapacityControllerSnapshot {
    pub admission_stage: String,
    pub last_pressure_action: Option<String>,
    pub last_pressure_action_at_ms: Option<u64>,
    pub cooldown_or_pressure_active: bool,
    pub reason_codes: Vec<String>,
}

/// Priority-protection projection for the v1 contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWavePriorityProtectionSnapshot {
    pub protected: bool,
    pub protection_units: u8,
    pub operator_override_active: bool,
    pub reason_codes: Vec<String>,
}

impl HerdWavePriorityProtectionSnapshot {
    fn from_decision(decision: Option<&ResourceAdmissionDecisionSummary>) -> Self {
        let Some(decision) = decision else {
            return Self {
                protected: false,
                protection_units: 0,
                operator_override_active: false,
                reason_codes: Vec::new(),
            };
        };
        let protected = decision.priority_protection_units > 0
            || decision
                .reason_codes
                .contains(&AdmissionReasonCode::PriorityProtected);
        let operator_override_active = decision
            .reason_codes
            .contains(&AdmissionReasonCode::OperatorOverride);
        let mut reason_codes = Vec::new();
        if protected {
            reason_codes.push("herd_wave.priority.protected".to_string());
        }
        if operator_override_active {
            reason_codes.push("herd_wave.priority.operator_override".to_string());
        }
        Self {
            protected,
            protection_units: decision.priority_protection_units,
            operator_override_active,
            reason_codes,
        }
    }
}

/// Contract-shaped read-only snapshot used by later robot/doctor/MCP surfaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HerdWaveContractSnapshot {
    pub schema_version: u16,
    pub contract_id: &'static str,
    pub generated_at_ms: u64,
    pub source: String,
    pub source_freshness: Vec<HerdWaveSourceFreshness>,
    pub evidence_state: HerdWaveEvidenceState,
    pub overall_state: HerdWaveOverallState,
    pub dominant_kind: Option<HerdWaveEventKind>,
    pub event_count: u32,
    pub distinct_panes: u32,
    pub window_ms: u64,
    pub pressure_tier: FleetPressureTier,
    pub admission_action: Option<AdmissionAction>,
    pub reason_codes: Vec<String>,
    pub recommended_stagger_ms: u64,
    pub cohort_max_stagger_ms: u64,
    pub wave_summary: HerdWavePressureSummary,
    pub priority_protection: HerdWavePriorityProtectionSnapshot,
    pub unavailable_sources: Vec<HerdWaveUnavailableSource>,
    pub raw_pane_content_stored: bool,
    pub artifact_paths: Vec<String>,
}

impl HerdWaveContractSnapshot {
    /// Build a v1 snapshot from admission telemetry and an optional admission decision.
    #[must_use]
    pub fn from_telemetry(
        generated_at_ms: u64,
        source: impl Into<String>,
        telemetry: &SwarmAdmissionTelemetry,
        admission_decision: Option<&ResourceAdmissionDecisionSummary>,
        telemetry_generated_at_ms: Option<u64>,
        max_age_ms: u64,
    ) -> Self {
        let source_freshness = herd_wave_source_freshness_from_telemetry(
            generated_at_ms,
            telemetry,
            telemetry_generated_at_ms,
            max_age_ms,
        );
        let wave_summary = telemetry
            .herd_wave_pressure
            .clone()
            .unwrap_or_else(missing_herd_wave_summary);
        Self::from_parts(
            generated_at_ms,
            source,
            wave_summary,
            admission_decision,
            source_freshness,
        )
    }

    /// Build a v1 snapshot from already-computed pieces.
    #[must_use]
    pub fn from_parts(
        generated_at_ms: u64,
        source: impl Into<String>,
        wave_summary: HerdWavePressureSummary,
        admission_decision: Option<&ResourceAdmissionDecisionSummary>,
        source_freshness: Vec<HerdWaveSourceFreshness>,
    ) -> Self {
        let priority_protection =
            HerdWavePriorityProtectionSnapshot::from_decision(admission_decision);
        let unavailable_sources: Vec<_> = source_freshness
            .iter()
            .filter(|source| source.is_unavailable() || source.is_stale())
            .map(HerdWaveUnavailableSource::from)
            .collect();
        let evidence_state = root_evidence_state(&source_freshness);
        let overall_state =
            root_overall_state(&wave_summary, &priority_protection, &source_freshness);
        let admission_action = admission_decision.map(|decision| decision.action);
        let mut reason_codes = root_reason_codes(&wave_summary, &source_freshness);
        if let Some(decision) = admission_decision {
            for reason in &decision.reason_codes {
                push_string_reason(&mut reason_codes, admission_reason_code(*reason));
            }
        }
        for reason in &priority_protection.reason_codes {
            push_string_reason(&mut reason_codes, reason);
        }

        Self {
            schema_version: HERD_WAVE_SCHEMA_VERSION,
            contract_id: HERD_WAVE_CONTRACT_ID,
            generated_at_ms,
            source: source.into(),
            source_freshness,
            evidence_state,
            overall_state,
            dominant_kind: wave_summary.dominant_kind,
            event_count: wave_summary.event_count,
            distinct_panes: wave_summary.distinct_panes,
            window_ms: wave_summary.window_ms,
            pressure_tier: wave_summary.pressure_tier,
            admission_action,
            reason_codes,
            recommended_stagger_ms: wave_summary.recommended_stagger_ms,
            cohort_max_stagger_ms: wave_summary.cohort_max_stagger_ms,
            wave_summary,
            priority_protection,
            unavailable_sources,
            raw_pane_content_stored: false,
            artifact_paths: Vec::new(),
        }
    }
}

/// Compute a bounded per-rank stagger delay for a herd-wave cohort.
#[must_use]
pub fn herd_wave_stagger_delay_ms(cohort_rank: u32, config: &HerdWaveDetectionConfig) -> u64 {
    u64::from(cohort_rank)
        .saturating_mul(config.base_stagger_ms.max(1))
        .min(config.max_stagger_ms)
}

/// Detect synchronized herd-wave pressure from timestamped pane signals.
#[must_use]
pub fn detect_herd_wave_pressure(
    signals: &[HerdWaveSignal],
    config: &HerdWaveDetectionConfig,
) -> HerdWavePressureSummary {
    let Some(latest_ms) = signals.iter().map(|signal| signal.timestamp_ms).max() else {
        return HerdWavePressureSummary {
            pressure_tier: FleetPressureTier::Normal,
            detected: false,
            event_count: 0,
            distinct_panes: 0,
            window_ms: config.detection_window_ms,
            first_seen_ms: None,
            last_seen_ms: None,
            dominant_kind: None,
            dominant_kind_count: 0,
            recommended_stagger_ms: 0,
            cohort_max_stagger_ms: 0,
        };
    };

    let window_start_ms = latest_ms.saturating_sub(config.detection_window_ms);
    let mut event_count = 0_usize;
    let mut distinct_panes = BTreeSet::new();
    let mut kind_counts: BTreeMap<HerdWaveEventKind, u32> = BTreeMap::new();
    let mut first_seen_ms: Option<u64> = None;

    for signal in signals
        .iter()
        .filter(|signal| signal.timestamp_ms >= window_start_ms && signal.timestamp_ms <= latest_ms)
    {
        event_count = event_count.saturating_add(1);
        if let Some(pane_id) = signal.pane_id {
            distinct_panes.insert(pane_id);
        }
        let count = kind_counts.entry(signal.kind).or_insert(0);
        *count = count.saturating_add(1);
        first_seen_ms =
            Some(first_seen_ms.map_or(signal.timestamp_ms, |first| first.min(signal.timestamp_ms)));
    }

    let distinct_panes = saturating_usize_to_u32(distinct_panes.len());
    let event_count = saturating_usize_to_u32(event_count);
    let (dominant_kind, dominant_kind_count) = dominant_herd_wave_kind(&kind_counts);
    let pressure_tier = herd_wave_pressure_tier(distinct_panes, config);
    let detected = pressure_tier > FleetPressureTier::Normal;
    let recommended_stagger_ms = if detected {
        herd_wave_stagger_delay_ms(1, config)
    } else {
        0
    };
    let cohort_max_stagger_ms = if detected && distinct_panes > 0 {
        herd_wave_stagger_delay_ms(distinct_panes.saturating_sub(1), config)
    } else {
        0
    };

    HerdWavePressureSummary {
        pressure_tier,
        detected,
        event_count,
        distinct_panes,
        window_ms: config.detection_window_ms,
        first_seen_ms,
        last_seen_ms: Some(latest_ms),
        dominant_kind,
        dominant_kind_count,
        recommended_stagger_ms,
        cohort_max_stagger_ms,
    }
}

/// Per-agent load snapshot for rebalancing decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLoadSnapshot {
    /// Agent slot identifier.
    pub agent_id: AgentSlotId,
    /// Number of currently assigned work items.
    pub active_items: u32,
    /// Max concurrent items this agent supports.
    pub max_items: u32,
    /// Total items completed by this agent.
    pub completed_count: u32,
    /// Total items failed by this agent.
    pub failed_count: u32,
    /// Timestamp (epoch ms) when agent was first seen.
    pub first_seen_ms: u64,
}

// =============================================================================
// Scheduling decisions
// =============================================================================

/// A scheduling decision produced by `SwarmScheduler::evaluate()`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SchedulerDecision {
    /// No action needed — fleet is healthy and balanced.
    Noop { reason: String },
    /// Pull work from the queue and assign to underutilized agents.
    AssignWork { assignments: Vec<WorkAssignment> },
    /// Rebalance work across agents to reduce load imbalance.
    Rebalance { moves: Vec<RebalanceMove> },
    /// Scale fleet up to handle increased queue pressure.
    ScaleUp {
        additional_agents: u32,
        reason: String,
    },
    /// Scale fleet down to reduce idle capacity.
    ScaleDown {
        remove_agents: Vec<AgentSlotId>,
        reason: String,
    },
    /// Reclaim work items from timed-out agents.
    ReclaimStale { reclaimed_items: Vec<WorkItemId> },
}

/// A work item → agent assignment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkAssignment {
    pub item_id: WorkItemId,
    pub agent_id: AgentSlotId,
}

/// A rebalance operation: move work from an overloaded agent to an underloaded one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebalanceMove {
    pub item_id: WorkItemId,
    pub from_agent: AgentSlotId,
    pub to_agent: AgentSlotId,
    pub reason: String,
}

// =============================================================================
// Scale events (audit trail)
// =============================================================================

/// A recorded scale event for audit and debugging.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScaleEvent {
    pub event_type: ScaleEventType,
    pub timestamp_ms: u64,
    pub reason: String,
    pub fleet_size_before: u32,
    pub fleet_size_after: u32,
    pub decision: SchedulerDecision,
}

/// Type of scale event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScaleEventType {
    ScaleUp,
    ScaleDown,
    Rebalance,
    Assignment,
    Reclaim,
    CircuitBreakerTripped,
    CircuitBreakerReset,
}

// =============================================================================
// Scheduler errors
// =============================================================================

/// Errors from scheduler operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SchedulerError {
    /// Circuit breaker is tripped — no scale operations allowed.
    CircuitBreakerActive { tripped_at: u64, resets_at: u64 },
    /// Fleet is already at maximum size.
    AtMaxCapacity { current: u32, max: u32 },
    /// Fleet is already at minimum size.
    AtMinCapacity { current: u32, min: u32 },
    /// Cooldown period has not elapsed.
    CooldownActive {
        operation: String,
        remaining_ms: u64,
    },
    /// No agents available for the requested operation.
    NoAgentsAvailable,
    /// No ready work items to assign.
    NoReadyWork,
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CircuitBreakerActive {
                tripped_at,
                resets_at,
            } => write!(
                f,
                "circuit breaker active (tripped at {tripped_at}, resets at {resets_at})"
            ),
            Self::AtMaxCapacity { current, max } => {
                write!(f, "fleet at max capacity ({current}/{max})")
            }
            Self::AtMinCapacity { current, min } => {
                write!(f, "fleet at min capacity ({current}/{min})")
            }
            Self::CooldownActive {
                operation,
                remaining_ms,
            } => write!(
                f,
                "{operation} cooldown active ({remaining_ms}ms remaining)"
            ),
            Self::NoAgentsAvailable => write!(f, "no agents available"),
            Self::NoReadyWork => write!(f, "no ready work items"),
        }
    }
}

impl std::error::Error for SchedulerError {}

// =============================================================================
// Scheduler snapshot (for checkpoint/restore)
// =============================================================================

/// Serializable snapshot of scheduler state for checkpoint/restore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchedulerSnapshot {
    pub config: SchedulerConfig,
    pub last_scale_up_ms: u64,
    pub last_scale_down_ms: u64,
    pub last_evaluation_ms: u64,
    pub consecutive_scale_ops: u32,
    pub circuit_breaker_tripped_at: Option<u64>,
    pub scale_history: Vec<ScaleEvent>,
    pub agent_first_seen: BTreeMap<AgentSlotId, u64>,
    pub agent_completed: BTreeMap<AgentSlotId, u32>,
    pub agent_failed: BTreeMap<AgentSlotId, u32>,
    pub sequence: u64,
}

// =============================================================================
// Main scheduler
// =============================================================================

/// Runtime scheduler, rebalancer, and autoscaler for live swarm fleets.
///
/// Evaluates queue pressure and agent utilization to make scheduling decisions:
/// - **Assign**: Pull work from the queue and dispatch to available agents
/// - **Rebalance**: Move work from overloaded to underloaded agents
/// - **Scale up**: Add agents when queue pressure exceeds threshold
/// - **Scale down**: Remove idle agents when pressure drops
/// - **Reclaim**: Reclaim work from timed-out agents
///
/// Anti-cascade safety:
/// - Cooldown timers prevent rapid scale oscillation
/// - Circuit breaker trips after too many consecutive scale ops
/// - Agent startup grace period prevents premature scale-down of new agents
pub struct SwarmScheduler {
    config: SchedulerConfig,
    last_scale_up_ms: u64,
    last_scale_down_ms: u64,
    last_evaluation_ms: u64,
    consecutive_scale_ops: u32,
    circuit_breaker_tripped_at: Option<u64>,
    scale_history: Vec<ScaleEvent>,
    agent_first_seen: HashMap<AgentSlotId, u64>,
    agent_completed: HashMap<AgentSlotId, u32>,
    agent_failed: HashMap<AgentSlotId, u32>,
    sequence: u64,
    max_history_entries: usize,
}

impl SwarmScheduler {
    /// Create a new scheduler with the given configuration.
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            last_scale_up_ms: 0,
            last_scale_down_ms: 0,
            last_evaluation_ms: 0,
            consecutive_scale_ops: 0,
            circuit_breaker_tripped_at: None,
            scale_history: Vec::new(),
            agent_first_seen: HashMap::new(),
            agent_completed: HashMap::new(),
            agent_failed: HashMap::new(),
            sequence: 0,
            max_history_entries: 1000,
        }
    }

    /// Create a scheduler with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(SchedulerConfig::default())
    }

    /// Restore scheduler from a checkpoint snapshot.
    pub fn restore(snapshot: SchedulerSnapshot) -> Self {
        Self {
            config: snapshot.config,
            last_scale_up_ms: snapshot.last_scale_up_ms,
            last_scale_down_ms: snapshot.last_scale_down_ms,
            last_evaluation_ms: snapshot.last_evaluation_ms,
            consecutive_scale_ops: snapshot.consecutive_scale_ops,
            circuit_breaker_tripped_at: snapshot.circuit_breaker_tripped_at,
            scale_history: snapshot.scale_history,
            agent_first_seen: snapshot.agent_first_seen.into_iter().collect(),
            agent_completed: snapshot.agent_completed.into_iter().collect(),
            agent_failed: snapshot.agent_failed.into_iter().collect(),
            sequence: snapshot.sequence,
            max_history_entries: 1000,
        }
    }

    /// Take a checkpoint snapshot of the scheduler state.
    pub fn snapshot(&self) -> SchedulerSnapshot {
        SchedulerSnapshot {
            config: self.config.clone(),
            last_scale_up_ms: self.last_scale_up_ms,
            last_scale_down_ms: self.last_scale_down_ms,
            last_evaluation_ms: self.last_evaluation_ms,
            consecutive_scale_ops: self.consecutive_scale_ops,
            circuit_breaker_tripped_at: self.circuit_breaker_tripped_at,
            scale_history: self.scale_history.clone(),
            agent_first_seen: self
                .agent_first_seen
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            agent_completed: self
                .agent_completed
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            agent_failed: self
                .agent_failed
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            sequence: self.sequence,
        }
    }

    /// Read-only access to the scheduler configuration.
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Read-only access to the scale event history.
    pub fn scale_history(&self) -> &[ScaleEvent] {
        &self.scale_history
    }

    /// Current monotonic sequence counter.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Whether the circuit breaker is currently tripped.
    pub fn circuit_breaker_active(&self, now_ms: u64) -> bool {
        match self.circuit_breaker_tripped_at {
            Some(tripped_at) => {
                now_ms < tripped_at.saturating_add(self.config.circuit_breaker_reset_ms)
            }
            None => false,
        }
    }

    // =========================================================================
    // Agent tracking
    // =========================================================================

    /// Register an agent with the scheduler (records first-seen time).
    pub fn register_agent(&mut self, agent_id: &AgentSlotId, now_ms: u64) {
        self.agent_first_seen
            .entry(agent_id.clone())
            .or_insert(now_ms);
        self.agent_completed.entry(agent_id.clone()).or_insert(0);
        self.agent_failed.entry(agent_id.clone()).or_insert(0);
    }

    /// Record a completion by an agent.
    pub fn record_completion(&mut self, agent_id: &AgentSlotId) {
        *self.agent_completed.entry(agent_id.clone()).or_insert(0) += 1;
    }

    /// Record a failure by an agent.
    pub fn record_failure(&mut self, agent_id: &AgentSlotId) {
        *self.agent_failed.entry(agent_id.clone()).or_insert(0) += 1;
    }

    /// Remove an agent from tracking.
    pub fn deregister_agent(&mut self, agent_id: &AgentSlotId) {
        self.agent_first_seen.remove(agent_id);
        self.agent_completed.remove(agent_id);
        self.agent_failed.remove(agent_id);
    }

    /// Get load snapshots for all tracked agents.
    pub fn agent_snapshots(
        &self,
        queue: &SwarmWorkQueue,
        max_concurrent: u32,
    ) -> Vec<AgentLoadSnapshot> {
        let mut snapshots = Vec::new();
        for (agent_id, &first_seen) in &self.agent_first_seen {
            let active = saturating_usize_to_u32(queue.agent_items(agent_id).len());
            let completed = self.agent_completed.get(agent_id).copied().unwrap_or(0);
            let failed = self.agent_failed.get(agent_id).copied().unwrap_or(0);
            snapshots.push(AgentLoadSnapshot {
                agent_id: agent_id.clone(),
                active_items: active,
                max_items: max_concurrent,
                completed_count: completed,
                failed_count: failed,
                first_seen_ms: first_seen,
            });
        }
        snapshots.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        snapshots
    }

    // =========================================================================
    // Queue pressure computation
    // =========================================================================

    /// Compute queue pressure metrics from the current queue state.
    pub fn compute_pressure(
        &self,
        stats: &QueueStats,
        max_concurrent_per_agent: u32,
    ) -> QueuePressure {
        let non_terminal = stats
            .total_items
            .saturating_sub(stats.completed + stats.failed + stats.cancelled);
        let ready_ratio = if non_terminal > 0 {
            stats.ready as f64 / non_terminal as f64
        } else {
            0.0
        };

        let active = saturating_usize_to_u32(stats.active_agents);
        let capacity = active.saturating_mul(max_concurrent_per_agent);
        let utilization_capacity =
            (stats.active_agents as f64) * f64::from(max_concurrent_per_agent);
        let utilization = if utilization_capacity > 0.0 {
            stats.in_progress as f64 / utilization_capacity
        } else if stats.ready > 0 || stats.in_progress > 0 {
            // No schedulable capacity while work is waiting/running: treat as
            // saturated so autoscaling can recover from zero-capacity stalls.
            1.0
        } else {
            0.0
        };

        let total_completions = stats.completed + stats.failed;
        let failure_rate = if total_completions > 0 {
            stats.failed as f64 / total_completions as f64
        } else {
            0.0
        };

        QueuePressure {
            ready_ratio,
            utilization,
            starvation_count: 0, // computed externally from queue internals
            failure_rate,
            pending_items: saturating_usize_to_u32(non_terminal),
            active_agents: active,
            total_capacity: capacity,
        }
    }

    // =========================================================================
    // Core evaluation
    // =========================================================================

    /// Evaluate the current fleet state and produce a scheduling decision.
    ///
    /// Priority order:
    /// 1. Reclaim stale items (heartbeat timeout)
    /// 2. Assign ready work to underutilized agents
    /// 3. Rebalance overloaded agents
    /// 4. Scale up if pressure exceeds threshold
    /// 5. Scale down if pressure is very low
    /// 6. Noop if everything is healthy
    pub fn evaluate(&mut self, queue: &mut SwarmWorkQueue, now_ms: u64) -> SchedulerDecision {
        self.last_evaluation_ms = now_ms;
        self.sequence += 1;

        // Check circuit breaker reset
        if let Some(tripped_at) = self.circuit_breaker_tripped_at {
            if now_ms >= tripped_at.saturating_add(self.config.circuit_breaker_reset_ms) {
                self.circuit_breaker_tripped_at = None;
                self.consecutive_scale_ops = 0;
                self.record_event(
                    ScaleEventType::CircuitBreakerReset,
                    "circuit breaker auto-reset after cooldown".to_string(),
                    0,
                    0,
                    SchedulerDecision::Noop {
                        reason: "circuit breaker reset".to_string(),
                    },
                    now_ms,
                );
            }
        }

        // Step 1: Reclaim timed-out items
        let reclaimed = queue.reclaim_timed_out();
        if !reclaimed.is_empty() {
            let decision = SchedulerDecision::ReclaimStale {
                reclaimed_items: reclaimed,
            };
            return decision;
        }

        let stats = queue.stats();
        let max_concurrent = queue.config().max_concurrent_per_agent;
        let pressure = self.compute_pressure(&stats, max_concurrent);

        // Step 2: Assign ready work to agents with capacity
        if stats.ready > 0 {
            let mut assignments = Vec::new();
            let snapshots = self.agent_snapshots(queue, max_concurrent);
            for snap in &snapshots {
                if snap.active_items < snap.max_items {
                    // Agent has capacity — try to pull work
                    if let Ok(assignment) = queue.pull(&snap.agent_id) {
                        assignments.push(WorkAssignment {
                            item_id: assignment.work_item_id,
                            agent_id: snap.agent_id.clone(),
                        });
                    }
                }
            }
            if !assignments.is_empty() {
                let decision = SchedulerDecision::AssignWork { assignments };
                return decision;
            }
        }

        // Step 3: Check for load imbalance and rebalance
        if let Some(decision) = self.check_rebalance(queue, max_concurrent) {
            return decision;
        }

        // Step 4: Scale up if pressure exceeds threshold
        if pressure.utilization > self.config.scale_up_threshold
            && pressure.active_agents < self.config.max_fleet_size
        {
            if let Some(decision) = self.try_scale_up(&pressure, now_ms) {
                return decision;
            }
        }

        // Step 5: Scale down if pressure is very low
        if pressure.utilization < self.config.scale_down_threshold
            && pressure.active_agents > self.config.min_fleet_size
        {
            if let Some(decision) = self.try_scale_down(queue, &pressure, now_ms) {
                return decision;
            }
        }

        SchedulerDecision::Noop {
            reason: format!(
                "fleet healthy: util={:.2}, ready_ratio={:.2}, agents={}",
                pressure.utilization, pressure.ready_ratio, pressure.active_agents,
            ),
        }
    }

    /// Evaluate without mutating the queue (read-only analysis).
    pub fn evaluate_readonly(
        &self,
        stats: &QueueStats,
        max_concurrent_per_agent: u32,
        _now_ms: u64,
    ) -> QueuePressure {
        self.compute_pressure(stats, max_concurrent_per_agent)
    }

    // =========================================================================
    // Scale-up logic
    // =========================================================================

    fn try_scale_up(&mut self, pressure: &QueuePressure, now_ms: u64) -> Option<SchedulerDecision> {
        // Check cooldown
        if now_ms
            < self
                .last_scale_up_ms
                .saturating_add(self.config.scale_up_cooldown_ms)
        {
            return None;
        }

        // Check circuit breaker
        if self.circuit_breaker_active(now_ms) {
            return None;
        }

        // Check max capacity
        if pressure.active_agents >= self.config.max_fleet_size {
            return None;
        }

        // Suppress scale-up if failure rate is too high (scaling won't help)
        if pressure.failure_rate > self.config.failure_rate_suppress_threshold {
            return None;
        }

        // Calculate how many agents to add (proportional to pressure)
        let excess = pressure.utilization - self.config.scale_up_threshold;
        let scale_factor = (excess / (1.0 - self.config.scale_up_threshold)).clamp(0.0, 1.0);
        let raw_step = (scale_factor * self.config.max_scale_step as f64).ceil() as u32;
        // The `>= max_fleet_size` guard at the top of `try_scale_up`
        // makes this subtraction safe today, but `saturating_sub` keeps
        // the math correct if a future caller weakens or moves the
        // guard.
        let step = raw_step.max(1).min(self.config.max_scale_step).min(
            self.config
                .max_fleet_size
                .saturating_sub(pressure.active_agents),
        );

        let reason = format!(
            "queue pressure {:.2} exceeds threshold {:.2} (ready={}, capacity={})",
            pressure.utilization,
            self.config.scale_up_threshold,
            pressure.pending_items,
            pressure.total_capacity,
        );

        self.last_scale_up_ms = now_ms;
        self.consecutive_scale_ops += 1;
        self.check_circuit_breaker(now_ms);

        let decision = SchedulerDecision::ScaleUp {
            additional_agents: step,
            reason: reason.clone(),
        };

        self.record_event(
            ScaleEventType::ScaleUp,
            reason,
            pressure.active_agents,
            pressure.active_agents + step,
            decision.clone(),
            now_ms,
        );

        Some(decision)
    }

    // =========================================================================
    // Scale-down logic
    // =========================================================================

    fn try_scale_down(
        &mut self,
        queue: &SwarmWorkQueue,
        pressure: &QueuePressure,
        now_ms: u64,
    ) -> Option<SchedulerDecision> {
        // Check cooldown
        if now_ms
            < self
                .last_scale_down_ms
                .saturating_add(self.config.scale_down_cooldown_ms)
        {
            return None;
        }

        // Check circuit breaker
        if self.circuit_breaker_active(now_ms) {
            return None;
        }

        // Check min capacity
        if pressure.active_agents <= self.config.min_fleet_size {
            return None;
        }

        // Find agents eligible for removal (idle, past grace period)
        let mut removable: Vec<AgentSlotId> = Vec::new();
        for (agent_id, &first_seen) in &self.agent_first_seen {
            // Skip agents in startup grace period
            if now_ms < first_seen.saturating_add(self.config.agent_startup_grace_ms) {
                continue;
            }
            // Only remove agents with no active work
            let active = queue.agent_items(agent_id).len();
            if active == 0 {
                removable.push(agent_id.clone());
            }
        }

        if removable.is_empty() {
            return None;
        }

        // Sort by least productive first (fewest completions)
        removable.sort_by(|a, b| {
            let a_completed = self.agent_completed.get(a).copied().unwrap_or(0);
            let b_completed = self.agent_completed.get(b).copied().unwrap_or(0);
            a_completed.cmp(&b_completed)
        });

        // Only remove enough to stay above min and not remove too many at once.
        // The `<= min_fleet_size` early-return at the top of `try_scale_down`
        // makes the subtraction safe today, but `saturating_sub` keeps the
        // math correct if a future caller weakens or moves the guard.
        let max_remove = pressure
            .active_agents
            .saturating_sub(self.config.min_fleet_size)
            .min(self.config.max_scale_step);
        removable.truncate(max_remove as usize);

        if removable.is_empty() {
            return None;
        }

        let reason = format!(
            "queue pressure {:.2} below threshold {:.2}, removing {} idle agent(s)",
            pressure.utilization,
            self.config.scale_down_threshold,
            removable.len(),
        );

        self.last_scale_down_ms = now_ms;
        self.consecutive_scale_ops += 1;
        self.check_circuit_breaker(now_ms);

        // `removable` was truncated to `max_remove`, which is bounded by
        // `pressure.active_agents - min_fleet_size`, so the subtraction below
        // cannot underflow. The saturating helpers harden the path against
        // any future regression in the truncation ceiling.
        let new_size = pressure
            .active_agents
            .saturating_sub(saturating_usize_to_u32(removable.len()));
        let decision = SchedulerDecision::ScaleDown {
            remove_agents: removable,
            reason: reason.clone(),
        };

        self.record_event(
            ScaleEventType::ScaleDown,
            reason,
            pressure.active_agents,
            new_size,
            decision.clone(),
            now_ms,
        );

        Some(decision)
    }

    // =========================================================================
    // Rebalance logic
    // =========================================================================

    fn check_rebalance(
        &self,
        queue: &SwarmWorkQueue,
        max_concurrent: u32,
    ) -> Option<SchedulerDecision> {
        let snapshots = self.agent_snapshots(queue, max_concurrent);
        if snapshots.len() < 2 {
            return None;
        }

        let loads: Vec<f64> = snapshots
            .iter()
            .map(|s| {
                if s.max_items > 0 {
                    s.active_items as f64 / s.max_items as f64
                } else {
                    0.0
                }
            })
            .collect();

        let max_load = loads.iter().copied().fold(0.0_f64, f64::max);
        let min_load = loads.iter().copied().fold(1.0_f64, f64::min);
        let imbalance = max_load - min_load;

        if imbalance < self.config.rebalance_imbalance_threshold {
            return None;
        }

        // Find overloaded and underloaded agents
        let avg_load: f64 = loads.iter().sum::<f64>() / loads.len() as f64;
        let mut moves = Vec::new();

        let overloaded: Vec<_> = snapshots
            .iter()
            .zip(loads.iter())
            .filter(|entry| *entry.1 > avg_load + self.config.rebalance_imbalance_threshold / 2.0)
            .map(|(s, _)| s)
            .collect();

        let underloaded: Vec<_> = snapshots
            .iter()
            .zip(loads.iter())
            .filter(|entry| *entry.1 < avg_load - self.config.rebalance_imbalance_threshold / 2.0)
            .map(|(s, _)| s)
            .collect();

        // Suggest moves from overloaded to underloaded (advisory only)
        let mut target_idx = 0;
        for over in &overloaded {
            if target_idx >= underloaded.len() {
                break;
            }
            let items = queue.agent_items(&over.agent_id);
            // Suggest moving the most recently assigned item
            if let Some(assignment) = items.last() {
                let under = &underloaded[target_idx];
                if under.active_items < under.max_items {
                    moves.push(RebalanceMove {
                        item_id: assignment.work_item_id.clone(),
                        from_agent: over.agent_id.clone(),
                        to_agent: under.agent_id.clone(),
                        reason: format!(
                            "load imbalance {:.2}: {}/{} -> {}/{}",
                            imbalance,
                            over.active_items,
                            over.max_items,
                            under.active_items,
                            under.max_items,
                        ),
                    });
                    target_idx += 1;
                }
            }
        }

        if moves.is_empty() {
            return None;
        }

        Some(SchedulerDecision::Rebalance { moves })
    }

    // =========================================================================
    // Circuit breaker
    // =========================================================================

    fn check_circuit_breaker(&mut self, now_ms: u64) {
        if self.consecutive_scale_ops >= self.config.max_consecutive_scale_ops {
            self.circuit_breaker_tripped_at = Some(now_ms);
            self.record_event(
                ScaleEventType::CircuitBreakerTripped,
                format!(
                    "circuit breaker tripped after {} consecutive scale ops",
                    self.consecutive_scale_ops,
                ),
                0,
                0,
                SchedulerDecision::Noop {
                    reason: "circuit breaker tripped".to_string(),
                },
                now_ms,
            );
        }
    }

    /// Manually reset the circuit breaker.
    pub fn reset_circuit_breaker(&mut self) {
        self.circuit_breaker_tripped_at = None;
        self.consecutive_scale_ops = 0;
    }

    // =========================================================================
    // Event recording
    // =========================================================================

    fn record_event(
        &mut self,
        event_type: ScaleEventType,
        reason: String,
        before: u32,
        after: u32,
        decision: SchedulerDecision,
        now_ms: u64,
    ) {
        self.scale_history.push(ScaleEvent {
            event_type,
            timestamp_ms: now_ms,
            reason,
            fleet_size_before: before,
            fleet_size_after: after,
            decision,
        });
        // Evict oldest 10% when history is full
        if self.scale_history.len() > self.max_history_entries {
            let drain_count = self.max_history_entries / 10;
            self.scale_history.drain(..drain_count);
        }
    }

    // =========================================================================
    // Helpers
    // =========================================================================

    /// Get the current wall-clock time in milliseconds since the Unix epoch.
    #[allow(dead_code)]
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

// =============================================================================
// Convenience: compute pressure from queue directly
// =============================================================================

/// Compute queue pressure from a SwarmWorkQueue snapshot.
pub fn compute_queue_pressure(queue: &SwarmWorkQueue) -> QueuePressure {
    let stats = queue.stats();
    let max_concurrent = queue.config().max_concurrent_per_agent;
    let scheduler = SwarmScheduler::with_defaults();
    scheduler.compute_pressure(&stats, max_concurrent)
}

// =============================================================================
// Global admission controller
// =============================================================================

/// How much mission-level protection a work request should receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionCriticality {
    /// Opportunistic work that may be stopped first under pressure.
    Background,
    /// Normal operator work.
    Standard,
    /// Important work that should degrade before being shed.
    Critical,
    /// Work explicitly tied to keeping the swarm usable.
    MissionCritical,
}

impl MissionCriticality {
    const fn protection_units(self) -> u8 {
        match self {
            Self::Background | Self::Standard => 0,
            Self::Critical => 1,
            Self::MissionCritical => 2,
        }
    }
}

/// Requested work item being admitted against current resource pressure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionRequest {
    /// Pane associated with the request, when the request is pane scoped.
    pub pane_id: Option<u64>,
    /// Pane resource priority from the existing priority classifier.
    pub pane_priority: PanePriority,
    /// Mission criticality supplied by the caller.
    pub mission_criticality: MissionCriticality,
    /// Work-queue priority. Lower numbers are more important.
    pub work_priority: u32,
    /// Estimated effort units from the work queue.
    pub estimated_effort: u32,
    /// Explicit operator override allowing priority protection to exceed normal caps.
    pub operator_priority_override: bool,
}

impl AdmissionRequest {
    /// Build a standard request for non-pane work.
    #[must_use]
    pub const fn standard(work_priority: u32, estimated_effort: u32) -> Self {
        Self {
            pane_id: None,
            pane_priority: PanePriority::Medium,
            mission_criticality: MissionCriticality::Standard,
            work_priority,
            estimated_effort,
            operator_priority_override: false,
        }
    }
}

/// Live telemetry consumed by the global admission controller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwarmAdmissionTelemetry {
    /// Queue pressure derived from [`SwarmWorkQueue`] statistics.
    pub queue_pressure: Option<QueuePressure>,
    /// Compound fleet pressure from [`crate::fleet_memory_controller`].
    pub fleet_pressure: Option<FleetPressureTier>,
    /// Optional memory tier budget snapshot; absent data fails closed.
    pub memory_tier_budget: Option<FleetMemoryTierBudgetSnapshot>,
    /// Per-latency-stage budget pressure.
    pub latency_stage_pressures: Option<Vec<StagePressure>>,
    /// Optional herd-wave pressure from synchronized agent actions.
    pub herd_wave_pressure: Option<HerdWavePressureSummary>,
}

impl SwarmAdmissionTelemetry {
    /// Construct telemetry from known queue and fleet pressure surfaces.
    #[must_use]
    pub fn new(
        queue_pressure: QueuePressure,
        fleet_pressure: FleetPressureTier,
        memory_tier_budget: FleetMemoryTierBudgetSnapshot,
        latency_stage_pressures: Vec<StagePressure>,
    ) -> Self {
        Self {
            queue_pressure: Some(queue_pressure),
            fleet_pressure: Some(fleet_pressure),
            memory_tier_budget: Some(memory_tier_budget),
            latency_stage_pressures: Some(latency_stage_pressures),
            herd_wave_pressure: None,
        }
    }

    /// Attach a herd-wave pressure summary to this telemetry bundle.
    #[must_use]
    pub fn with_herd_wave_pressure(mut self, summary: HerdWavePressureSummary) -> Self {
        self.herd_wave_pressure = Some(summary);
        self
    }
}

/// Admission result severity. Ordered from least to most disruptive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionAction {
    /// Admit immediately.
    Admit,
    /// Defer until pressure drops or more telemetry arrives.
    Defer,
    /// Admit only in a reduced-quality mode.
    Degrade,
    /// Shed the request without scheduling work.
    Shed,
}

impl AdmissionAction {
    /// Numeric severity for counters, tests, and dashboards.
    #[must_use]
    pub const fn severity(self) -> u8 {
        match self {
            Self::Admit => 0,
            Self::Defer => 1,
            Self::Degrade => 2,
            Self::Shed => 3,
        }
    }
}

/// Stable reason codes emitted by the admission controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionReasonCode {
    /// All pressure inputs were below configured thresholds.
    Healthy,
    /// Queue utilization crossed the defer threshold.
    QueueElevated,
    /// Queue utilization crossed the degrade threshold.
    QueueSaturated,
    /// Queue utilization or backlog crossed the shed threshold.
    QueueOverCapacity,
    /// Failure rate is high enough that more admission would amplify churn.
    FailureRateHigh,
    /// Compound fleet pressure is above normal.
    FleetPressure,
    /// Memory-tier budget pressure is above normal.
    MemoryTierPressure,
    /// At least one latency stage is over its current budget.
    LatencyStageOverBudget,
    /// Synchronized agent activity is likely to amplify pressure.
    HerdWavePressure,
    /// Queue telemetry was missing.
    MissingQueueTelemetry,
    /// Fleet-pressure telemetry was missing.
    MissingFleetTelemetry,
    /// Memory-tier telemetry was missing.
    MissingMemoryTierTelemetry,
    /// Latency-stage telemetry was missing.
    MissingLatencyTelemetry,
    /// Telemetry contained non-finite values and was treated as unsafe.
    NonFiniteTelemetry,
    /// Latency telemetry contained impossible numeric values.
    InvalidLatencyTelemetry,
    /// Pane/work priority reduced the requested disruption severity.
    PriorityProtected,
    /// Operator priority override expanded protection beyond normal caps.
    OperatorOverride,
    /// Missing telemetry prevented an otherwise-admitted decision.
    FailClosedMissingTelemetry,
}

/// Per-evaluation counters for operator-facing telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionDecisionCounters {
    /// Number of admitted requests represented by this summary.
    pub admitted: u64,
    /// Number of deferred requests represented by this summary.
    pub deferred: u64,
    /// Number of degraded requests represented by this summary.
    pub degraded: u64,
    /// Number of shed requests represented by this summary.
    pub shed: u64,
}

impl AdmissionDecisionCounters {
    const fn from_action(action: AdmissionAction) -> Self {
        match action {
            AdmissionAction::Admit => Self {
                admitted: 1,
                deferred: 0,
                degraded: 0,
                shed: 0,
            },
            AdmissionAction::Defer => Self {
                admitted: 0,
                deferred: 1,
                degraded: 0,
                shed: 0,
            },
            AdmissionAction::Degrade => Self {
                admitted: 0,
                deferred: 0,
                degraded: 1,
                shed: 0,
            },
            AdmissionAction::Shed => Self {
                admitted: 0,
                deferred: 0,
                degraded: 0,
                shed: 1,
            },
        }
    }
}

/// Operator-facing result for a single admission evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResourceAdmissionDecisionSummary {
    /// Final admission action.
    pub action: AdmissionAction,
    /// Stable reason codes explaining the action.
    pub reason_codes: Vec<AdmissionReasonCode>,
    /// Per-action counters for dashboards.
    pub counters: AdmissionDecisionCounters,
    /// Raw resource pressure severity before priority protection.
    pub raw_pressure_severity: u8,
    /// Final pressure severity after priority protection and fail-closed gates.
    pub effective_pressure_severity: u8,
    /// Protection units applied from pane priority, work priority, and mission criticality.
    pub priority_protection_units: u8,
    /// Queue utilization seen by the controller.
    pub queue_utilization: Option<f64>,
    /// Pending queue items seen by the controller.
    pub pending_items: Option<u32>,
    /// Compound fleet pressure input.
    pub fleet_pressure: Option<FleetPressureTier>,
    /// Pressure derived from the memory-tier budget snapshot.
    pub memory_tier_pressure: Option<FleetPressureTier>,
    /// Maximum latency-stage over-budget ratio, if latency telemetry was available.
    pub max_latency_over_budget_ratio: Option<f64>,
    /// Herd-wave pressure tier from synchronized agent activity, if available.
    pub herd_wave_pressure: Option<FleetPressureTier>,
    /// Recommended adjacent-action stagger for the active herd-wave cohort.
    pub herd_wave_recommended_stagger_ms: Option<u64>,
    /// Maximum cohort delay implied by the active herd-wave cohort.
    pub herd_wave_cohort_max_stagger_ms: Option<u64>,
}

/// Deterministic threshold policy for global admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdmissionControllerConfig {
    /// Queue utilization at which non-protected requests are deferred.
    pub defer_queue_utilization: f64,
    /// Queue utilization at which non-protected requests are degraded.
    pub degrade_queue_utilization: f64,
    /// Queue utilization at which non-protected requests are shed.
    pub shed_queue_utilization: f64,
    /// Failure rate at which requests are degraded.
    pub degrade_failure_rate: f64,
    /// Failure rate at which requests are shed.
    pub shed_failure_rate: f64,
    /// Latency over-budget ratio that starts deferral.
    pub defer_stage_over_budget_ratio: f64,
    /// Latency over-budget ratio that starts degradation.
    pub degrade_stage_over_budget_ratio: f64,
    /// Latency over-budget ratio that starts shedding.
    pub shed_stage_over_budget_ratio: f64,
    /// Minimum severity assigned when mandatory telemetry is absent.
    pub missing_telemetry_severity: u8,
}

impl Default for AdmissionControllerConfig {
    fn default() -> Self {
        Self {
            defer_queue_utilization: 0.80,
            degrade_queue_utilization: 0.90,
            shed_queue_utilization: 1.0,
            degrade_failure_rate: 0.50,
            shed_failure_rate: 0.80,
            defer_stage_over_budget_ratio: 0.05,
            degrade_stage_over_budget_ratio: 0.25,
            shed_stage_over_budget_ratio: 1.0,
            missing_telemetry_severity: 1,
        }
    }
}

/// Global admission controller that converts live pressure into one decision.
#[derive(Debug, Clone)]
pub struct SwarmAdmissionController {
    config: AdmissionControllerConfig,
}

impl SwarmAdmissionController {
    /// Create a controller from a deterministic threshold config.
    #[must_use]
    pub const fn new(config: AdmissionControllerConfig) -> Self {
        Self { config }
    }

    /// Read-only access to the current admission policy.
    #[must_use]
    pub const fn config(&self) -> &AdmissionControllerConfig {
        &self.config
    }

    /// Evaluate a request against the current resource-pressure telemetry.
    #[must_use]
    pub fn evaluate(
        &self,
        request: &AdmissionRequest,
        telemetry: &SwarmAdmissionTelemetry,
    ) -> ResourceAdmissionDecisionSummary {
        let mut reasons = Vec::new();
        let mut missing_telemetry = false;
        let mut raw_severity = 0_u8;

        let queue_utilization = telemetry
            .queue_pressure
            .as_ref()
            .map(|pressure| pressure.utilization);
        let pending_items = telemetry
            .queue_pressure
            .as_ref()
            .map(|pressure| pressure.pending_items);

        match telemetry.queue_pressure.as_ref() {
            Some(queue) => {
                raw_severity = raw_severity.max(self.queue_severity(queue, &mut reasons));
            }
            None => {
                missing_telemetry = true;
                push_reason(&mut reasons, AdmissionReasonCode::MissingQueueTelemetry);
                raw_severity = raw_severity.max(self.config.missing_telemetry_severity.min(3));
            }
        }

        match telemetry.fleet_pressure {
            Some(fleet_pressure) => {
                raw_severity =
                    raw_severity.max(fleet_pressure_severity(fleet_pressure, &mut reasons));
            }
            None => {
                missing_telemetry = true;
                push_reason(&mut reasons, AdmissionReasonCode::MissingFleetTelemetry);
                raw_severity = raw_severity.max(self.config.missing_telemetry_severity.min(3));
            }
        }

        let memory_tier_pressure = match telemetry.memory_tier_budget.as_ref() {
            Some(snapshot) => {
                let tier = snapshot.pressure_tier();
                raw_severity = raw_severity.max(fleet_pressure_severity(tier, &mut reasons));
                if tier > FleetPressureTier::Normal {
                    push_reason(&mut reasons, AdmissionReasonCode::MemoryTierPressure);
                }
                Some(tier)
            }
            None => {
                missing_telemetry = true;
                push_reason(
                    &mut reasons,
                    AdmissionReasonCode::MissingMemoryTierTelemetry,
                );
                raw_severity = raw_severity.max(self.config.missing_telemetry_severity.min(3));
                None
            }
        };

        let max_latency_over_budget_ratio = match telemetry.latency_stage_pressures.as_ref() {
            Some(pressures) if !pressures.is_empty() => {
                let (severity, ratio) = self.latency_severity(pressures, &mut reasons);
                raw_severity = raw_severity.max(severity);
                ratio
            }
            _ => {
                missing_telemetry = true;
                push_reason(&mut reasons, AdmissionReasonCode::MissingLatencyTelemetry);
                raw_severity = raw_severity.max(self.config.missing_telemetry_severity.min(3));
                None
            }
        };

        let (herd_wave_pressure, herd_wave_recommended_stagger_ms, herd_wave_cohort_max_stagger_ms) =
            match telemetry.herd_wave_pressure.as_ref() {
                Some(summary) => {
                    let severity = summary.pressure_tier.as_u8();
                    if severity > 0 {
                        raw_severity = raw_severity.max(severity);
                        push_reason(&mut reasons, AdmissionReasonCode::HerdWavePressure);
                    }
                    (
                        Some(summary.pressure_tier),
                        (summary.recommended_stagger_ms > 0)
                            .then_some(summary.recommended_stagger_ms),
                        (summary.cohort_max_stagger_ms > 0)
                            .then_some(summary.cohort_max_stagger_ms),
                    )
                }
                None => (None, None, None),
            };

        if raw_severity == 0 {
            push_reason(&mut reasons, AdmissionReasonCode::Healthy);
        }

        let priority_protection_units = priority_protection_units(request);
        let mut effective_severity = raw_severity.saturating_sub(priority_protection_units);
        if priority_protection_units > 0 && raw_severity > effective_severity {
            push_reason(&mut reasons, AdmissionReasonCode::PriorityProtected);
        }
        if request.operator_priority_override {
            push_reason(&mut reasons, AdmissionReasonCode::OperatorOverride);
        }

        let mut action = action_for_severity(effective_severity);
        if missing_telemetry && action == AdmissionAction::Admit {
            action = AdmissionAction::Defer;
            effective_severity = AdmissionAction::Defer.severity();
            push_reason(
                &mut reasons,
                AdmissionReasonCode::FailClosedMissingTelemetry,
            );
        }

        ResourceAdmissionDecisionSummary {
            action,
            reason_codes: reasons,
            counters: AdmissionDecisionCounters::from_action(action),
            raw_pressure_severity: raw_severity,
            effective_pressure_severity: effective_severity,
            priority_protection_units,
            queue_utilization,
            pending_items,
            fleet_pressure: telemetry.fleet_pressure,
            memory_tier_pressure,
            max_latency_over_budget_ratio,
            herd_wave_pressure,
            herd_wave_recommended_stagger_ms,
            herd_wave_cohort_max_stagger_ms,
        }
    }

    fn queue_severity(&self, queue: &QueuePressure, reasons: &mut Vec<AdmissionReasonCode>) -> u8 {
        if !queue.utilization.is_finite()
            || !queue.ready_ratio.is_finite()
            || !queue.failure_rate.is_finite()
        {
            push_reason(reasons, AdmissionReasonCode::NonFiniteTelemetry);
            return 2;
        }

        let mut severity = 0_u8;
        if queue.failure_rate >= self.config.shed_failure_rate {
            push_reason(reasons, AdmissionReasonCode::FailureRateHigh);
            severity = severity.max(3);
        } else if queue.failure_rate >= self.config.degrade_failure_rate {
            push_reason(reasons, AdmissionReasonCode::FailureRateHigh);
            severity = severity.max(2);
        }

        if queue.total_capacity == 0 && queue.pending_items > 0 {
            push_reason(reasons, AdmissionReasonCode::QueueSaturated);
            severity = severity.max(2);
        } else if queue.utilization >= self.config.shed_queue_utilization
            || (queue.total_capacity > 0
                && queue.pending_items > queue.total_capacity.saturating_mul(4))
        {
            push_reason(reasons, AdmissionReasonCode::QueueOverCapacity);
            severity = severity.max(3);
        } else if queue.utilization >= self.config.degrade_queue_utilization {
            push_reason(reasons, AdmissionReasonCode::QueueSaturated);
            severity = severity.max(2);
        } else if queue.utilization >= self.config.defer_queue_utilization
            || queue.starvation_count > 0
        {
            push_reason(reasons, AdmissionReasonCode::QueueElevated);
            severity = severity.max(1);
        }

        severity
    }

    fn latency_severity(
        &self,
        pressures: &[StagePressure],
        reasons: &mut Vec<AdmissionReasonCode>,
    ) -> (u8, Option<f64>) {
        let mut max_ratio = 0.0_f64;
        let mut severity = 0_u8;

        for pressure in pressures {
            if !pressure.observed_p95_us.is_finite() || !pressure.budget_p95_us.is_finite() {
                push_reason(reasons, AdmissionReasonCode::NonFiniteTelemetry);
                severity = severity.max(2);
                continue;
            }
            if pressure.observed_p95_us < 0.0 || pressure.budget_p95_us < 0.0 {
                push_reason(reasons, AdmissionReasonCode::InvalidLatencyTelemetry);
                severity = severity.max(2);
                continue;
            }

            let ratio = if pressure.budget_p95_us <= 0.0 {
                if pressure.observed_p95_us > 0.0 {
                    push_reason(reasons, AdmissionReasonCode::InvalidLatencyTelemetry);
                    severity = severity.max(2);
                    self.config.shed_stage_over_budget_ratio
                } else {
                    0.0
                }
            } else {
                (pressure.observed_p95_us - pressure.budget_p95_us) / pressure.budget_p95_us
            };

            if ratio > max_ratio {
                max_ratio = ratio;
            }
        }

        if max_ratio >= self.config.shed_stage_over_budget_ratio {
            push_reason(reasons, AdmissionReasonCode::LatencyStageOverBudget);
            severity = severity.max(3);
        } else if max_ratio >= self.config.degrade_stage_over_budget_ratio {
            push_reason(reasons, AdmissionReasonCode::LatencyStageOverBudget);
            severity = severity.max(2);
        } else if max_ratio >= self.config.defer_stage_over_budget_ratio {
            push_reason(reasons, AdmissionReasonCode::LatencyStageOverBudget);
            severity = severity.max(1);
        }

        (severity, Some(max_ratio.max(0.0)))
    }
}

impl Default for SwarmAdmissionController {
    fn default() -> Self {
        Self::new(AdmissionControllerConfig::default())
    }
}

impl SwarmScheduler {
    /// Evaluate resource admission using the same queue-pressure model as scheduling.
    #[must_use]
    pub fn evaluate_admission(
        &self,
        request: &AdmissionRequest,
        telemetry: &SwarmAdmissionTelemetry,
    ) -> ResourceAdmissionDecisionSummary {
        SwarmAdmissionController::default().evaluate(request, telemetry)
    }
}

fn priority_protection_units(request: &AdmissionRequest) -> u8 {
    let mut units = 0_u8;

    if request.pane_priority >= PanePriority::High {
        units = units.saturating_add(1);
    }
    if request.pane_priority == PanePriority::Critical {
        units = units.saturating_add(1);
    }
    units = units.saturating_add(request.mission_criticality.protection_units());
    if request.work_priority <= 1 {
        units = units.saturating_add(1);
    }

    let normal_cap = if request.operator_priority_override {
        3
    } else {
        2
    };
    units.min(normal_cap)
}

fn action_for_severity(severity: u8) -> AdmissionAction {
    match severity {
        0 => AdmissionAction::Admit,
        1 => AdmissionAction::Defer,
        2 => AdmissionAction::Degrade,
        _ => AdmissionAction::Shed,
    }
}

fn fleet_pressure_severity(tier: FleetPressureTier, reasons: &mut Vec<AdmissionReasonCode>) -> u8 {
    let severity = tier.as_u8();
    if severity > 0 {
        push_reason(reasons, AdmissionReasonCode::FleetPressure);
    }
    severity
}

fn push_reason(reasons: &mut Vec<AdmissionReasonCode>, reason: AdmissionReasonCode) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

/// Build a deterministic smoothing plan for the active herd-wave cohort.
///
/// The plan intentionally keeps one action per distinct pane in the active
/// window. Repeated signals from the same pane are diagnostic evidence for the
/// wave but must not duplicate follow-up work for that pane.
#[must_use]
pub fn plan_herd_wave_staggered_actions(
    signals: &[HerdWaveSignal],
    config: &HerdWaveDetectionConfig,
) -> HerdWaveStaggerPlan {
    let summary = detect_herd_wave_pressure(signals, config);
    if !summary.detected {
        return HerdWaveStaggerPlan {
            summary,
            actions: Vec::new(),
        };
    }

    let latest_ms = summary.last_seen_ms.unwrap_or(0);
    let window_start_ms = latest_ms.saturating_sub(config.detection_window_ms);
    let mut earliest_by_pane: BTreeMap<u64, (u64, HerdWaveEventKind)> = BTreeMap::new();

    for signal in signals
        .iter()
        .filter(|signal| signal.timestamp_ms >= window_start_ms && signal.timestamp_ms <= latest_ms)
    {
        let Some(pane_id) = signal.pane_id else {
            continue;
        };
        let candidate = (signal.timestamp_ms, signal.kind);
        earliest_by_pane
            .entry(pane_id)
            .and_modify(|existing| {
                if candidate < *existing {
                    *existing = candidate;
                }
            })
            .or_insert(candidate);
    }

    let mut cohort: Vec<_> = earliest_by_pane
        .into_iter()
        .map(|(pane_id, (observed_at_ms, kind))| (observed_at_ms, pane_id, kind))
        .collect();
    cohort.sort_unstable();

    let actions = cohort
        .into_iter()
        .enumerate()
        .map(|(rank, (observed_at_ms, pane_id, kind))| {
            let cohort_rank = saturating_usize_to_u32(rank);
            let delay_ms = herd_wave_stagger_delay_ms(cohort_rank, config);
            HerdWaveStaggeredAction {
                pane_id,
                kind,
                observed_at_ms,
                cohort_rank,
                delay_ms,
                scheduled_at_ms: latest_ms.saturating_add(delay_ms),
            }
        })
        .collect();

    HerdWaveStaggerPlan { summary, actions }
}

fn herd_wave_pressure_tier(
    distinct_panes: u32,
    config: &HerdWaveDetectionConfig,
) -> FleetPressureTier {
    let min = config.min_distinct_panes.max(2);
    if distinct_panes < min {
        return FleetPressureTier::Normal;
    }

    let elevated = config.elevated_distinct_panes.max(min);
    let critical = config
        .critical_distinct_panes
        .max(elevated.saturating_add(1));
    let emergency = config
        .emergency_distinct_panes
        .max(critical.saturating_add(1));

    if distinct_panes >= emergency {
        FleetPressureTier::Emergency
    } else if distinct_panes >= critical {
        FleetPressureTier::Critical
    } else {
        FleetPressureTier::Elevated
    }
}

fn dominant_herd_wave_kind(
    kind_counts: &BTreeMap<HerdWaveEventKind, u32>,
) -> (Option<HerdWaveEventKind>, u32) {
    let mut best_kind = None;
    let mut best_count = 0;
    for (&kind, &count) in kind_counts {
        if count > best_count {
            best_kind = Some(kind);
            best_count = count;
        }
    }
    (best_kind, best_count)
}

fn saturating_usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn herd_wave_source_freshness_from_telemetry(
    generated_at_ms: u64,
    telemetry: &SwarmAdmissionTelemetry,
    telemetry_generated_at_ms: Option<u64>,
    max_age_ms: u64,
) -> Vec<HerdWaveSourceFreshness> {
    let observed_at_ms = telemetry_generated_at_ms.unwrap_or(generated_at_ms);
    let live = |source: &'static str| {
        HerdWaveSourceFreshness::live(source, generated_at_ms, observed_at_ms, max_age_ms)
    };

    let mut sources = Vec::with_capacity(5);
    sources.push(if telemetry.queue_pressure.is_some() {
        live("swarm.queue_pressure")
    } else {
        HerdWaveSourceFreshness::unavailable(
            "swarm.queue_pressure",
            "herd_wave.telemetry.missing_queue",
        )
    });
    sources.push(if telemetry.fleet_pressure.is_some() {
        live("swarm.fleet_pressure")
    } else {
        HerdWaveSourceFreshness::unavailable(
            "swarm.fleet_pressure",
            "herd_wave.telemetry.missing_fleet",
        )
    });
    sources.push(if telemetry.memory_tier_budget.is_some() {
        live("swarm.memory_tier_budget")
    } else {
        HerdWaveSourceFreshness::unavailable(
            "swarm.memory_tier_budget",
            "herd_wave.telemetry.missing_memory_tier",
        )
    });
    sources.push(
        if telemetry
            .latency_stage_pressures
            .as_ref()
            .is_some_and(|pressures| !pressures.is_empty())
        {
            live("swarm.latency_stages")
        } else {
            HerdWaveSourceFreshness::unavailable(
                "swarm.latency_stages",
                "herd_wave.telemetry.missing_latency",
            )
        },
    );
    sources.push(if telemetry.herd_wave_pressure.is_some() {
        live("swarm.herd_wave_pressure")
    } else {
        HerdWaveSourceFreshness::unavailable(
            "swarm.herd_wave_pressure",
            "herd_wave.telemetry.missing_herd_wave",
        )
    });
    sources
}

fn missing_herd_wave_summary() -> HerdWavePressureSummary {
    HerdWavePressureSummary {
        pressure_tier: FleetPressureTier::Normal,
        detected: false,
        event_count: 0,
        distinct_panes: 0,
        window_ms: 0,
        first_seen_ms: None,
        last_seen_ms: None,
        dominant_kind: None,
        dominant_kind_count: 0,
        recommended_stagger_ms: 0,
        cohort_max_stagger_ms: 0,
    }
}

fn root_evidence_state(sources: &[HerdWaveSourceFreshness]) -> HerdWaveEvidenceState {
    if sources.is_empty() {
        return HerdWaveEvidenceState::Unavailable;
    }
    if sources.iter().any(HerdWaveSourceFreshness::is_unavailable) {
        return HerdWaveEvidenceState::Unavailable;
    }
    if sources.iter().any(HerdWaveSourceFreshness::is_stale) {
        return HerdWaveEvidenceState::Stale;
    }

    let Some(first) = sources.first().map(|source| source.evidence_state) else {
        return HerdWaveEvidenceState::Unavailable;
    };
    if sources.iter().all(|source| source.evidence_state == first) {
        first
    } else {
        HerdWaveEvidenceState::Mixed
    }
}

fn root_overall_state(
    wave_summary: &HerdWavePressureSummary,
    priority_protection: &HerdWavePriorityProtectionSnapshot,
    sources: &[HerdWaveSourceFreshness],
) -> HerdWaveOverallState {
    if sources.iter().any(HerdWaveSourceFreshness::is_unavailable) {
        return HerdWaveOverallState::MissingTelemetry;
    }
    if sources.iter().any(HerdWaveSourceFreshness::is_stale) {
        return HerdWaveOverallState::StaleEvidence;
    }
    if priority_protection.operator_override_active {
        return HerdWaveOverallState::OperatorOverride;
    }
    if priority_protection.protected {
        return HerdWaveOverallState::PriorityProtected;
    }
    match wave_summary.pressure_tier {
        FleetPressureTier::Normal => HerdWaveOverallState::Normal,
        FleetPressureTier::Elevated => HerdWaveOverallState::Elevated,
        FleetPressureTier::Critical => HerdWaveOverallState::Critical,
        FleetPressureTier::Emergency => HerdWaveOverallState::Emergency,
    }
}

fn root_reason_codes(
    wave_summary: &HerdWavePressureSummary,
    sources: &[HerdWaveSourceFreshness],
) -> Vec<String> {
    let mut reasons = Vec::new();
    for source in sources {
        for reason in &source.reason_codes {
            push_string_reason(&mut reasons, reason);
        }
    }
    if let Some(kind) = wave_summary.dominant_kind {
        push_string_reason(&mut reasons, herd_wave_event_reason_code(kind));
    }
    if wave_summary.detected {
        push_string_reason(&mut reasons, "herd_wave.threshold.distinct_panes");
    } else if reasons.is_empty() {
        push_string_reason(&mut reasons, "herd_wave.admission.healthy");
    }
    reasons
}

fn push_string_reason(reasons: &mut Vec<String>, reason: impl AsRef<str>) {
    let reason = reason.as_ref();
    if !reasons.iter().any(|existing| existing == reason) {
        reasons.push(reason.to_string());
    }
}

fn herd_wave_event_reason_code(kind: HerdWaveEventKind) -> &'static str {
    match kind {
        HerdWaveEventKind::Compaction => "herd_wave.kind.compaction",
        HerdWaveEventKind::Retry => "herd_wave.kind.retry",
        HerdWaveEventKind::RateLimitRecovery => "herd_wave.kind.rate_limit_recovery",
        HerdWaveEventKind::SearchBurst => "herd_wave.kind.search_burst",
        HerdWaveEventKind::WorkflowFanout => "herd_wave.kind.workflow_fanout",
        HerdWaveEventKind::Wake => "herd_wave.kind.wake",
        HerdWaveEventKind::Other => "herd_wave.kind.other",
    }
}

fn admission_reason_code(reason: AdmissionReasonCode) -> &'static str {
    match reason {
        AdmissionReasonCode::Healthy => "herd_wave.admission.healthy",
        AdmissionReasonCode::QueueElevated => "herd_wave.admission.queue_elevated",
        AdmissionReasonCode::QueueSaturated => "herd_wave.admission.queue_saturated",
        AdmissionReasonCode::QueueOverCapacity => "herd_wave.admission.queue_over_capacity",
        AdmissionReasonCode::FailureRateHigh => "herd_wave.admission.failure_rate_high",
        AdmissionReasonCode::FleetPressure => "herd_wave.admission.fleet_pressure",
        AdmissionReasonCode::MemoryTierPressure => "herd_wave.admission.memory_tier_pressure",
        AdmissionReasonCode::LatencyStageOverBudget => {
            "herd_wave.admission.latency_stage_over_budget"
        }
        AdmissionReasonCode::HerdWavePressure => "herd_wave.admission.herd_wave_pressure",
        AdmissionReasonCode::MissingQueueTelemetry => "herd_wave.telemetry.missing_queue",
        AdmissionReasonCode::MissingFleetTelemetry => "herd_wave.telemetry.missing_fleet",
        AdmissionReasonCode::MissingMemoryTierTelemetry => {
            "herd_wave.telemetry.missing_memory_tier"
        }
        AdmissionReasonCode::MissingLatencyTelemetry => "herd_wave.telemetry.missing_latency",
        AdmissionReasonCode::NonFiniteTelemetry => "herd_wave.telemetry.non_finite",
        AdmissionReasonCode::InvalidLatencyTelemetry => "herd_wave.telemetry.invalid_latency",
        AdmissionReasonCode::PriorityProtected => "herd_wave.priority.protected",
        AdmissionReasonCode::OperatorOverride => "herd_wave.priority.operator_override",
        AdmissionReasonCode::FailClosedMissingTelemetry => {
            "herd_wave.admission.fail_closed_missing_telemetry"
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_memory_controller::{FleetMemoryTier, FleetMemoryTierBudgetRecord};
    use crate::latency_stages::LatencyStage;
    use crate::swarm_work_queue::{WorkItem, WorkQueueConfig};
    use proptest::prelude::*;

    fn test_config() -> SchedulerConfig {
        SchedulerConfig {
            scale_up_cooldown_ms: 1000,
            scale_down_cooldown_ms: 2000,
            min_fleet_size: 1,
            max_fleet_size: 16,
            scale_up_threshold: 0.80,
            scale_down_threshold: 0.20,
            rebalance_imbalance_threshold: 0.40,
            max_consecutive_scale_ops: 3,
            agent_startup_grace_ms: 5000,
            circuit_breaker_reset_ms: 10_000,
            max_scale_step: 2,
            failure_rate_suppress_threshold: 0.50,
        }
    }

    fn make_queue() -> SwarmWorkQueue {
        SwarmWorkQueue::new(WorkQueueConfig {
            max_concurrent_per_agent: 3,
            heartbeat_timeout_ms: 30_000,
            max_retries: 2,
            anti_starvation: true,
            starvation_threshold_ms: 60_000,
        })
    }

    fn make_item(id: &str, priority: u32) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            title: format!("Work item {id}"),
            priority,
            depends_on: Vec::new(),
            effort: 1,
            labels: Vec::new(),
            preferred_program: None,
            metadata: HashMap::new(),
        }
    }

    fn admission_queue_pressure(utilization: f64) -> QueuePressure {
        QueuePressure {
            ready_ratio: 0.10,
            utilization,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 6,
            active_agents: 2,
            total_capacity: 6,
        }
    }

    fn healthy_tier_budget() -> FleetMemoryTierBudgetSnapshot {
        FleetMemoryTierBudgetSnapshot::from_tiers([FleetMemoryTierBudgetRecord::new(
            FleetMemoryTier::HotResident,
            1_000,
            900,
        )])
    }

    fn over_budget_tier_budget() -> FleetMemoryTierBudgetSnapshot {
        FleetMemoryTierBudgetSnapshot::from_tiers([FleetMemoryTierBudgetRecord::new(
            FleetMemoryTier::HotResident,
            1_000,
            1_800,
        )
        .with_counters(0, 0, 1)])
    }

    fn healthy_stage_pressure() -> Vec<StagePressure> {
        vec![StagePressure::compute(
            LatencyStage::PtyCapture,
            500.0,
            1_000.0,
        )]
    }

    fn over_budget_stage_pressure(ratio: f64) -> Vec<StagePressure> {
        let budget = 1_000.0;
        vec![StagePressure::compute(
            LatencyStage::StorageWrite,
            budget * (1.0 + ratio),
            budget,
        )]
    }

    fn admission_telemetry(
        utilization: f64,
        fleet_pressure: FleetPressureTier,
    ) -> SwarmAdmissionTelemetry {
        SwarmAdmissionTelemetry::new(
            admission_queue_pressure(utilization),
            fleet_pressure,
            healthy_tier_budget(),
            healthy_stage_pressure(),
        )
    }

    fn background_admission_request() -> AdmissionRequest {
        AdmissionRequest {
            pane_id: Some(42),
            pane_priority: PanePriority::Background,
            mission_criticality: MissionCriticality::Background,
            work_priority: 9,
            estimated_effort: 1,
            operator_priority_override: false,
        }
    }

    fn mission_critical_admission_request() -> AdmissionRequest {
        AdmissionRequest {
            pane_id: Some(7),
            pane_priority: PanePriority::Critical,
            mission_criticality: MissionCriticality::MissionCritical,
            work_priority: 0,
            estimated_effort: 1,
            operator_priority_override: false,
        }
    }

    #[allow(dead_code)]
    fn make_dep_item(id: &str, priority: u32, deps: Vec<&str>) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            title: format!("Work item {id}"),
            priority,
            depends_on: deps.into_iter().map(String::from).collect(),
            effort: 1,
            labels: Vec::new(),
            preferred_program: None,
            metadata: HashMap::new(),
        }
    }

    // =========================================================================
    // Global admission controller tests (ft-t1ktp)
    // =========================================================================

    #[test]
    fn admission_threshold_boundaries_map_to_admit_defer_degrade_shed_ft_t1ktp() {
        let controller = SwarmAdmissionController::default();
        let request = background_admission_request();

        let healthy = controller.evaluate(
            &request,
            &admission_telemetry(0.799, FleetPressureTier::Normal),
        );
        assert_eq!(healthy.action, AdmissionAction::Admit);
        assert!(healthy.reason_codes.contains(&AdmissionReasonCode::Healthy));

        let defer = controller.evaluate(
            &request,
            &admission_telemetry(0.800, FleetPressureTier::Normal),
        );
        assert_eq!(defer.action, AdmissionAction::Defer);
        assert!(
            defer
                .reason_codes
                .contains(&AdmissionReasonCode::QueueElevated)
        );

        let degrade = controller.evaluate(
            &request,
            &admission_telemetry(0.900, FleetPressureTier::Normal),
        );
        assert_eq!(degrade.action, AdmissionAction::Degrade);
        assert!(
            degrade
                .reason_codes
                .contains(&AdmissionReasonCode::QueueSaturated)
        );

        let shed = controller.evaluate(
            &request,
            &admission_telemetry(1.000, FleetPressureTier::Normal),
        );
        assert_eq!(shed.action, AdmissionAction::Shed);
        assert!(
            shed.reason_codes
                .contains(&AdmissionReasonCode::QueueOverCapacity)
        );
    }

    #[test]
    fn admission_fails_closed_when_live_telemetry_is_missing_ft_t1ktp() {
        let controller = SwarmAdmissionController::default();
        let telemetry = SwarmAdmissionTelemetry {
            queue_pressure: Some(admission_queue_pressure(0.0)),
            fleet_pressure: Some(FleetPressureTier::Normal),
            memory_tier_budget: None,
            latency_stage_pressures: Some(healthy_stage_pressure()),
            herd_wave_pressure: None,
        };
        let request = mission_critical_admission_request();

        let summary = controller.evaluate(&request, &telemetry);

        assert_eq!(summary.action, AdmissionAction::Defer);
        assert!(
            summary
                .reason_codes
                .contains(&AdmissionReasonCode::MissingMemoryTierTelemetry)
        );
        assert!(
            summary
                .reason_codes
                .contains(&AdmissionReasonCode::FailClosedMissingTelemetry)
        );
        assert_eq!(summary.counters.deferred, 1);
    }

    #[test]
    fn admission_priority_protection_prevents_low_priority_inversion_ft_t1ktp() {
        let controller = SwarmAdmissionController::default();
        let telemetry = admission_telemetry(0.0, FleetPressureTier::Emergency);

        let low = controller.evaluate(&background_admission_request(), &telemetry);
        let high = controller.evaluate(&mission_critical_admission_request(), &telemetry);

        assert_eq!(low.action, AdmissionAction::Shed);
        assert_eq!(high.action, AdmissionAction::Defer);
        assert!(low.action.severity() >= high.action.severity());
        assert!(
            high.reason_codes
                .contains(&AdmissionReasonCode::PriorityProtected)
        );
    }

    #[test]
    fn admission_decisions_are_monotonic_as_pressure_increases_ft_t1ktp() {
        let controller = SwarmAdmissionController::default();
        let request = background_admission_request();

        let mut previous = AdmissionAction::Admit;
        for tier in [
            FleetPressureTier::Normal,
            FleetPressureTier::Elevated,
            FleetPressureTier::Critical,
            FleetPressureTier::Emergency,
        ] {
            let summary = controller.evaluate(&request, &admission_telemetry(0.0, tier));
            assert!(
                summary.action.severity() >= previous.severity(),
                "tier {tier:?} produced non-monotonic action {:?} after {:?}",
                summary.action,
                previous
            );
            previous = summary.action;
        }

        let mut previous = AdmissionAction::Admit;
        for utilization in [0.0, 0.80, 0.90, 1.0] {
            let summary = controller.evaluate(
                &request,
                &admission_telemetry(utilization, FleetPressureTier::Normal),
            );
            assert!(
                summary.action.severity() >= previous.severity(),
                "utilization {utilization} produced non-monotonic action {:?} after {:?}",
                summary.action,
                previous
            );
            previous = summary.action;
        }
    }

    #[test]
    fn admission_high_load_summary_surfaces_shed_reasons_and_is_replay_stable_ft_t1ktp() {
        let controller = SwarmAdmissionController::default();
        let request = background_admission_request();
        let telemetry = SwarmAdmissionTelemetry::new(
            admission_queue_pressure(1.10),
            FleetPressureTier::Emergency,
            over_budget_tier_budget(),
            over_budget_stage_pressure(1.25),
        );

        let first = controller.evaluate(&request, &telemetry);
        let second = controller.evaluate(&request, &telemetry);

        assert_eq!(first, second);
        assert_eq!(first.action, AdmissionAction::Shed);
        assert_eq!(first.counters.shed, 1);
        assert!(
            first
                .reason_codes
                .contains(&AdmissionReasonCode::QueueOverCapacity)
        );
        assert!(
            first
                .reason_codes
                .contains(&AdmissionReasonCode::FleetPressure)
        );
        assert!(
            first
                .reason_codes
                .contains(&AdmissionReasonCode::MemoryTierPressure)
        );
        assert!(
            first
                .reason_codes
                .contains(&AdmissionReasonCode::LatencyStageOverBudget)
        );

        let json = serde_json::to_string(&first).expect("serialize admission summary");
        assert!(json.contains("\"action\":\"shed\""));
        assert!(json.contains("memory_tier_pressure"));
    }

    #[test]
    fn admission_zero_latency_budget_keeps_summary_ratio_finite() {
        let controller = SwarmAdmissionController::default();
        let request = background_admission_request();
        let telemetry = SwarmAdmissionTelemetry::new(
            admission_queue_pressure(0.10),
            FleetPressureTier::Normal,
            healthy_tier_budget(),
            vec![StagePressure::compute(LatencyStage::StorageWrite, 1.0, 0.0)],
        );

        let summary = controller.evaluate(&request, &telemetry);
        let ratio = summary
            .max_latency_over_budget_ratio
            .expect("latency telemetry should produce a ratio");

        assert!(ratio.is_finite());
        assert!((ratio - controller.config().shed_stage_over_budget_ratio).abs() <= f64::EPSILON);
        assert!(
            summary
                .reason_codes
                .contains(&AdmissionReasonCode::InvalidLatencyTelemetry)
        );
        assert!(
            summary
                .reason_codes
                .contains(&AdmissionReasonCode::LatencyStageOverBudget)
        );

        let json = serde_json::to_string(&summary).expect("serialize admission summary");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse summary");
        assert!(value["max_latency_over_budget_ratio"].is_number());
    }

    #[test]
    fn herd_wave_detection_requires_distinct_panes_not_repeated_single_pane_ft_wks87() {
        let config = HerdWaveDetectionConfig::default();
        let repeated_single_pane = vec![
            HerdWaveSignal::pane(7, HerdWaveEventKind::Retry, 1_000),
            HerdWaveSignal::pane(7, HerdWaveEventKind::Retry, 1_100),
            HerdWaveSignal::pane(7, HerdWaveEventKind::Retry, 1_200),
            HerdWaveSignal::pane(7, HerdWaveEventKind::Retry, 1_300),
        ];

        let false_positive = detect_herd_wave_pressure(&repeated_single_pane, &config);

        assert!(!false_positive.detected);
        assert_eq!(false_positive.pressure_tier, FleetPressureTier::Normal);
        assert_eq!(false_positive.distinct_panes, 1);
        assert_eq!(false_positive.event_count, 4);
        assert_eq!(false_positive.recommended_stagger_ms, 0);

        let synchronized_cohort = vec![
            HerdWaveSignal::pane(1, HerdWaveEventKind::Retry, 2_000),
            HerdWaveSignal::pane(2, HerdWaveEventKind::Retry, 2_100),
            HerdWaveSignal::pane(3, HerdWaveEventKind::Retry, 2_200),
        ];
        let detected = detect_herd_wave_pressure(&synchronized_cohort, &config);

        assert!(detected.detected);
        assert_eq!(detected.pressure_tier, FleetPressureTier::Elevated);
        assert_eq!(detected.distinct_panes, 3);
        assert_eq!(detected.dominant_kind, Some(HerdWaveEventKind::Retry));
        assert_eq!(detected.dominant_kind_count, 3);
        assert_eq!(detected.recommended_stagger_ms, config.base_stagger_ms);
        assert_eq!(detected.cohort_max_stagger_ms, config.base_stagger_ms * 2);
    }

    #[test]
    fn herd_wave_detection_respects_sliding_window_boundaries_ft_wks87() {
        let config = HerdWaveDetectionConfig {
            detection_window_ms: 100,
            ..HerdWaveDetectionConfig::default()
        };
        let below_threshold = vec![
            HerdWaveSignal::pane(1, HerdWaveEventKind::Compaction, 0),
            HerdWaveSignal::pane(2, HerdWaveEventKind::Compaction, 50),
            HerdWaveSignal::pane(3, HerdWaveEventKind::Compaction, 101),
        ];

        let summary = detect_herd_wave_pressure(&below_threshold, &config);

        assert!(!summary.detected);
        assert_eq!(summary.distinct_panes, 2);
        assert_eq!(summary.event_count, 2);
        assert_eq!(summary.first_seen_ms, Some(50));
        assert_eq!(summary.last_seen_ms, Some(101));

        let inside_window = vec![
            HerdWaveSignal::pane(1, HerdWaveEventKind::Compaction, 0),
            HerdWaveSignal::pane(2, HerdWaveEventKind::Compaction, 50),
            HerdWaveSignal::pane(3, HerdWaveEventKind::Compaction, 100),
            HerdWaveSignal::pane(4, HerdWaveEventKind::WorkflowFanout, 101),
        ];
        let detected = detect_herd_wave_pressure(&inside_window, &config);

        assert!(detected.detected);
        assert_eq!(detected.distinct_panes, 3);
        assert_eq!(detected.event_count, 3);
        assert_eq!(detected.dominant_kind, Some(HerdWaveEventKind::Compaction));
    }

    #[test]
    fn admission_uses_herd_wave_pressure_and_surfaces_stagger_guidance_ft_wks87() {
        let controller = SwarmAdmissionController::default();
        let wave_signals: Vec<_> = (0..8)
            .map(|pane| {
                HerdWaveSignal::pane(pane, HerdWaveEventKind::RateLimitRecovery, 10_000 + pane)
            })
            .collect();
        let wave = detect_herd_wave_pressure(&wave_signals, &HerdWaveDetectionConfig::default());
        let telemetry =
            admission_telemetry(0.10, FleetPressureTier::Normal).with_herd_wave_pressure(wave);

        let summary = controller.evaluate(&background_admission_request(), &telemetry);

        assert_eq!(summary.action, AdmissionAction::Degrade);
        assert_eq!(
            summary.herd_wave_pressure,
            Some(FleetPressureTier::Critical)
        );
        assert_eq!(
            summary.herd_wave_recommended_stagger_ms,
            Some(HerdWaveDetectionConfig::default().base_stagger_ms)
        );
        assert!(
            summary
                .reason_codes
                .contains(&AdmissionReasonCode::HerdWavePressure)
        );
        assert!(summary.herd_wave_cohort_max_stagger_ms.is_some());

        let json = serde_json::to_string(&summary).expect("serialize summary");
        assert!(json.contains("herd_wave_pressure"));
        assert!(json.contains("herd_wave_recommended_stagger_ms"));
    }

    #[test]
    fn herd_wave_contract_snapshot_maps_normal_live_telemetry_ft_5bwjf_2() {
        let controller = SwarmAdmissionController::default();
        let wave = detect_herd_wave_pressure(&[], &HerdWaveDetectionConfig::default());
        let telemetry =
            admission_telemetry(0.10, FleetPressureTier::Normal).with_herd_wave_pressure(wave);
        let decision = controller.evaluate(&background_admission_request(), &telemetry);

        let snapshot = HerdWaveContractSnapshot::from_telemetry(
            20_000,
            "unit.herd_wave",
            &telemetry,
            Some(&decision),
            Some(19_900),
            1_000,
        );

        assert_eq!(snapshot.contract_id, HERD_WAVE_CONTRACT_ID);
        assert_eq!(snapshot.schema_version, HERD_WAVE_SCHEMA_VERSION);
        assert_eq!(snapshot.evidence_state, HerdWaveEvidenceState::Measured);
        assert_eq!(snapshot.overall_state, HerdWaveOverallState::Normal);
        assert_eq!(snapshot.admission_action, Some(AdmissionAction::Admit));
        assert!(!snapshot.raw_pane_content_stored);
        assert!(snapshot.unavailable_sources.is_empty());

        let json = serde_json::to_value(&snapshot).expect("serialize herd-wave snapshot");
        assert_eq!(json["contract_id"], HERD_WAVE_CONTRACT_ID);
        assert_eq!(json["raw_pane_content_stored"], false);
    }

    #[test]
    fn herd_wave_contract_snapshot_distinguishes_elevated_and_critical_ft_5bwjf_2() {
        let controller = SwarmAdmissionController::default();
        let elevated_wave = detect_herd_wave_pressure(
            &[
                HerdWaveSignal::pane(1, HerdWaveEventKind::Retry, 1_000),
                HerdWaveSignal::pane(2, HerdWaveEventKind::Retry, 1_010),
                HerdWaveSignal::pane(3, HerdWaveEventKind::Retry, 1_020),
            ],
            &HerdWaveDetectionConfig::default(),
        );
        let elevated_telemetry = admission_telemetry(0.10, FleetPressureTier::Normal)
            .with_herd_wave_pressure(elevated_wave);
        let elevated_decision =
            controller.evaluate(&background_admission_request(), &elevated_telemetry);
        let elevated = HerdWaveContractSnapshot::from_telemetry(
            2_000,
            "unit.herd_wave",
            &elevated_telemetry,
            Some(&elevated_decision),
            Some(1_990),
            1_000,
        );

        assert_eq!(elevated.overall_state, HerdWaveOverallState::Elevated);
        assert_eq!(elevated.pressure_tier, FleetPressureTier::Elevated);
        assert_eq!(elevated.dominant_kind, Some(HerdWaveEventKind::Retry));
        assert!(
            elevated
                .reason_codes
                .contains(&"herd_wave.kind.retry".to_string())
        );

        let critical_signals: Vec<_> = (0..8)
            .map(|pane| HerdWaveSignal::pane(pane, HerdWaveEventKind::SearchBurst, 3_000 + pane))
            .collect();
        let critical_wave =
            detect_herd_wave_pressure(&critical_signals, &HerdWaveDetectionConfig::default());
        let critical_telemetry = admission_telemetry(0.10, FleetPressureTier::Normal)
            .with_herd_wave_pressure(critical_wave);
        let critical_decision =
            controller.evaluate(&background_admission_request(), &critical_telemetry);
        let critical = HerdWaveContractSnapshot::from_telemetry(
            4_000,
            "unit.herd_wave",
            &critical_telemetry,
            Some(&critical_decision),
            Some(3_990),
            1_000,
        );

        assert_eq!(critical.overall_state, HerdWaveOverallState::Critical);
        assert_eq!(critical.pressure_tier, FleetPressureTier::Critical);
        assert_eq!(critical.admission_action, Some(AdmissionAction::Degrade));
        assert!(
            critical
                .reason_codes
                .contains(&"herd_wave.kind.search_burst".to_string())
        );
    }

    #[test]
    fn herd_wave_contract_snapshot_fails_closed_for_missing_telemetry_ft_5bwjf_2() {
        let controller = SwarmAdmissionController::default();
        let telemetry = SwarmAdmissionTelemetry {
            queue_pressure: None,
            fleet_pressure: None,
            memory_tier_budget: None,
            latency_stage_pressures: None,
            herd_wave_pressure: None,
        };
        let decision = controller.evaluate(&mission_critical_admission_request(), &telemetry);

        let snapshot = HerdWaveContractSnapshot::from_telemetry(
            10_000,
            "unit.herd_wave",
            &telemetry,
            Some(&decision),
            None,
            1_000,
        );

        assert_eq!(snapshot.evidence_state, HerdWaveEvidenceState::Unavailable);
        assert_eq!(
            snapshot.overall_state,
            HerdWaveOverallState::MissingTelemetry
        );
        assert_eq!(snapshot.admission_action, Some(AdmissionAction::Defer));
        assert_eq!(snapshot.unavailable_sources.len(), 5);
        assert!(
            snapshot
                .reason_codes
                .contains(&"herd_wave.telemetry.missing_queue".to_string())
        );
        assert!(
            snapshot
                .reason_codes
                .contains(&"herd_wave.admission.fail_closed_missing_telemetry".to_string())
        );
    }

    #[test]
    fn herd_wave_contract_snapshot_marks_stale_freshness_ft_5bwjf_2() {
        let controller = SwarmAdmissionController::default();
        let wave = detect_herd_wave_pressure(
            &[
                HerdWaveSignal::pane(1, HerdWaveEventKind::Compaction, 1_000),
                HerdWaveSignal::pane(2, HerdWaveEventKind::Compaction, 1_010),
                HerdWaveSignal::pane(3, HerdWaveEventKind::Compaction, 1_020),
            ],
            &HerdWaveDetectionConfig::default(),
        );
        let telemetry =
            admission_telemetry(0.10, FleetPressureTier::Normal).with_herd_wave_pressure(wave);
        let decision = controller.evaluate(&background_admission_request(), &telemetry);

        let snapshot = HerdWaveContractSnapshot::from_telemetry(
            20_000,
            "unit.herd_wave",
            &telemetry,
            Some(&decision),
            Some(10_000),
            500,
        );

        assert_eq!(snapshot.evidence_state, HerdWaveEvidenceState::Stale);
        assert_eq!(snapshot.overall_state, HerdWaveOverallState::StaleEvidence);
        assert_eq!(snapshot.unavailable_sources.len(), 5);
        assert!(
            snapshot
                .source_freshness
                .iter()
                .all(|source| source.evidence_state == HerdWaveEvidenceState::Stale)
        );
    }

    #[test]
    fn herd_wave_contract_snapshot_surfaces_priority_and_controller_state_ft_5bwjf_2() {
        let controller = SwarmAdmissionController::default();
        let wave_signals: Vec<_> = (0..16)
            .map(|pane| HerdWaveSignal::pane(pane, HerdWaveEventKind::Wake, 5_000 + pane))
            .collect();
        let wave = detect_herd_wave_pressure(&wave_signals, &HerdWaveDetectionConfig::default());
        let telemetry =
            admission_telemetry(0.10, FleetPressureTier::Normal).with_herd_wave_pressure(wave);
        let decision = controller.evaluate(&mission_critical_admission_request(), &telemetry);

        let snapshot = HerdWaveContractSnapshot::from_telemetry(
            6_000,
            "unit.herd_wave",
            &telemetry,
            Some(&decision),
            Some(5_990),
            1_000,
        );

        assert_eq!(
            snapshot.overall_state,
            HerdWaveOverallState::PriorityProtected
        );
        assert!(snapshot.priority_protection.protected);
        assert!(snapshot.priority_protection.protection_units > 0);
        assert!(
            snapshot
                .reason_codes
                .contains(&"herd_wave.priority.protected".to_string())
        );

        let mut controller_state =
            crate::runtime_telemetry::SwarmCapacityAdmissionControllerState::default();
        controller_state.record_decision(
            crate::runtime_telemetry::SwarmCapacityAdmissionAction::Defer,
            6_000,
        );
        let mirrored = HerdWaveCapacityControllerSnapshot::from(&controller_state);

        assert_eq!(mirrored.admission_stage, "shadow");
        assert_eq!(mirrored.last_pressure_action.as_deref(), Some("defer"));
        assert!(mirrored.cooldown_or_pressure_active);
        assert!(
            mirrored
                .reason_codes
                .contains(&"herd_wave.admission_controller.pressure_active".to_string())
        );
    }

    proptest! {
        #[test]
        fn herd_wave_stagger_delay_is_bounded_and_monotonic_ft_wks87(rank in 0_u32..100_000) {
            let config = HerdWaveDetectionConfig::default();
            let delay = herd_wave_stagger_delay_ms(rank, &config);

            prop_assert!(delay <= config.max_stagger_ms);
            if rank > 0 {
                let previous = herd_wave_stagger_delay_ms(rank - 1, &config);
                prop_assert!(delay >= previous);
            }
        }

        #[test]
        fn detected_herd_wave_cohort_never_gets_unbounded_tail_delay_ft_wks87(
            distinct_panes in 3_u32..512,
        ) {
            let config = HerdWaveDetectionConfig::default();
            let signals: Vec<_> = (0..distinct_panes)
                .map(|pane| HerdWaveSignal::pane(u64::from(pane), HerdWaveEventKind::Wake, 20_000 + u64::from(pane)))
                .collect();

            let summary = detect_herd_wave_pressure(&signals, &config);

            prop_assert!(summary.detected);
            prop_assert!(summary.cohort_max_stagger_ms <= config.max_stagger_ms);
            prop_assert!(summary.recommended_stagger_ms <= config.max_stagger_ms);
        }
    }

    // =========================================================================
    // Config tests
    // =========================================================================

    #[test]
    fn default_config_has_sane_values() {
        let cfg = SchedulerConfig::default();
        assert!(cfg.scale_up_cooldown_ms > 0);
        assert!(cfg.scale_down_cooldown_ms > 0);
        assert!(cfg.min_fleet_size >= 1);
        assert!(cfg.max_fleet_size > cfg.min_fleet_size);
        assert!(cfg.scale_up_threshold > cfg.scale_down_threshold);
        assert!(cfg.max_consecutive_scale_ops > 0);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = test_config();
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: SchedulerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, restored);
    }

    // =========================================================================
    // Pressure computation tests
    // =========================================================================

    #[test]
    fn empty_queue_has_zero_pressure() {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 0,
            blocked: 0,
            ready: 0,
            in_progress: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: 0,
            completion_log_size: 0,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);
        assert!((pressure.ready_ratio - 0.0).abs() < f64::EPSILON);
        assert!((pressure.utilization - 0.0).abs() < f64::EPSILON);
        assert!((pressure.failure_rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pressure_with_full_utilization() {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 10,
            blocked: 0,
            ready: 1,
            in_progress: 9,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: 3,
            completion_log_size: 0,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);
        assert!((pressure.utilization - 1.0).abs() < f64::EPSILON); // 9 / (3*3)
        assert!(pressure.ready_ratio > 0.0);
    }

    #[test]
    fn pressure_with_failures() {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 10,
            blocked: 0,
            ready: 2,
            in_progress: 3,
            completed: 3,
            failed: 2,
            cancelled: 0,
            active_agents: 2,
            completion_log_size: 5,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);
        assert!((pressure.failure_rate - 2.0 / 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pressure_with_ready_work_and_zero_capacity_is_saturated() {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 3,
            blocked: 0,
            ready: 3,
            in_progress: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: 0,
            completion_log_size: 0,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);
        assert!((pressure.utilization - 1.0).abs() < f64::EPSILON);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn pressure_saturates_large_queue_counts_without_wrapping() {
        let scheduler = SwarmScheduler::with_defaults();
        let above_u32 = usize::try_from(u64::from(u32::MAX) + 5).unwrap();
        let stats = QueueStats {
            total_items: above_u32,
            blocked: 0,
            ready: above_u32,
            in_progress: above_u32,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: above_u32,
            completion_log_size: 0,
        };

        let pressure = scheduler.compute_pressure(&stats, 2);

        assert_eq!(pressure.pending_items, u32::MAX);
        assert_eq!(pressure.active_agents, u32::MAX);
        assert_eq!(pressure.total_capacity, u32::MAX);
        assert!(pressure.utilization.is_finite());
    }

    // =========================================================================
    // Agent tracking tests
    // =========================================================================

    #[test]
    fn register_and_deregister_agent() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.register_agent(&"agent-1".to_string(), 1000);
        assert!(scheduler.agent_first_seen.contains_key("agent-1"));

        scheduler.deregister_agent(&"agent-1".to_string());
        assert!(!scheduler.agent_first_seen.contains_key("agent-1"));
    }

    #[test]
    fn record_completion_increments_counter() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 1000);
        scheduler.record_completion(&agent);
        scheduler.record_completion(&agent);
        assert_eq!(scheduler.agent_completed[&agent], 2);
    }

    #[test]
    fn record_failure_increments_counter() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 1000);
        scheduler.record_failure(&agent);
        assert_eq!(scheduler.agent_failed[&agent], 1);
    }

    #[test]
    fn agent_snapshots_sorted_by_id() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();
        scheduler.register_agent(&"zebra".to_string(), 1000);
        scheduler.register_agent(&"alpha".to_string(), 1000);
        scheduler.register_agent(&"mid".to_string(), 1000);

        let snapshots = scheduler.agent_snapshots(&queue, 3);
        assert_eq!(snapshots[0].agent_id, "alpha");
        assert_eq!(snapshots[1].agent_id, "mid");
        assert_eq!(snapshots[2].agent_id, "zebra");
    }

    // =========================================================================
    // Evaluation tests
    // =========================================================================

    #[test]
    fn evaluate_noop_on_empty_queue() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        scheduler.register_agent(&"agent-1".to_string(), 1000);

        let decision = scheduler.evaluate(&mut queue, 2000);
        match decision {
            SchedulerDecision::Noop { .. } => {}
            other => panic!("expected Noop, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_assigns_ready_work() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 1000);

        queue.enqueue(make_item("w1", 0)).unwrap();
        queue.enqueue(make_item("w2", 1)).unwrap();

        let decision = scheduler.evaluate(&mut queue, 2000);
        match decision {
            SchedulerDecision::AssignWork { assignments } => {
                assert!(!assignments.is_empty());
                assert_eq!(assignments[0].agent_id, agent);
            }
            other => panic!("expected AssignWork, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_assigns_to_multiple_agents() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        scheduler.register_agent(&"a1".to_string(), 1000);
        scheduler.register_agent(&"a2".to_string(), 1000);

        for i in 0..6 {
            queue.enqueue(make_item(&format!("w{i}"), 0)).unwrap();
        }

        let decision = scheduler.evaluate(&mut queue, 2000);
        match decision {
            SchedulerDecision::AssignWork { assignments } => {
                // Should assign to both agents
                let agents: Vec<_> = assignments.iter().map(|a| &a.agent_id).collect();
                assert!(agents.contains(&&"a1".to_string()));
                assert!(agents.contains(&&"a2".to_string()));
            }
            other => panic!("expected AssignWork, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_scales_up_when_ready_work_exists_with_zero_capacity() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        queue.enqueue(make_item("w1", 0)).unwrap();

        match scheduler.evaluate(&mut queue, 5000) {
            SchedulerDecision::ScaleUp {
                additional_agents, ..
            } => assert!(additional_agents >= 1),
            other => panic!("expected ScaleUp, got {other:?}"),
        }
    }

    // =========================================================================
    // Scale-up tests
    // =========================================================================

    #[test]
    fn scale_up_when_utilization_exceeds_threshold() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 0);

        // Fill agent to capacity
        for i in 0..3 {
            queue.enqueue(make_item(&format!("w{i}"), 0)).unwrap();
            queue.assign(&format!("w{i}"), &agent).unwrap();
        }
        // Add more ready work (no agents to pull it)
        queue.enqueue(make_item("w3", 0)).unwrap();

        let decision = scheduler.evaluate(&mut queue, 5000);
        // First call will try to assign w3 but agent is at capacity, resulting in noop
        // or scale-up depending on utilization
        match &decision {
            SchedulerDecision::ScaleUp {
                additional_agents, ..
            } => {
                assert!(*additional_agents >= 1);
            }
            SchedulerDecision::Noop { .. } => {
                // Agent at capacity but utilization may not exceed threshold with 1 agent
                // This is OK — utilization = 3/3 = 1.0 which exceeds 0.80
                // But ready items exist, so assignment is tried first but fails
            }
            other => panic!("expected ScaleUp or Noop, got {other:?}"),
        }
    }

    #[test]
    fn scale_up_respects_cooldown() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.last_scale_up_ms = 1000;

        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.95,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 10,
            active_agents: 2,
            total_capacity: 6,
        };

        // Too soon — within 1000ms cooldown
        let result = scheduler.try_scale_up(&pressure, 1500);
        assert!(result.is_none());

        // After cooldown
        let result = scheduler.try_scale_up(&pressure, 2500);
        assert!(result.is_some());
    }

    #[test]
    fn scale_up_respects_max_fleet_size() {
        let mut scheduler = SwarmScheduler::new(test_config());

        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.95,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 10,
            active_agents: 16, // at max
            total_capacity: 48,
        };

        let result = scheduler.try_scale_up(&pressure, 5000);
        assert!(result.is_none());
    }

    #[test]
    fn scale_up_suppressed_by_high_failure_rate() {
        let mut scheduler = SwarmScheduler::new(test_config());

        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.95,
            starvation_count: 0,
            failure_rate: 0.60, // above 0.50 threshold
            pending_items: 10,
            active_agents: 4,
            total_capacity: 12,
        };

        let result = scheduler.try_scale_up(&pressure, 5000);
        assert!(result.is_none());
    }

    // =========================================================================
    // Scale-down tests
    // =========================================================================

    #[test]
    fn scale_down_removes_idle_agents() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        // Register agents well past grace period
        scheduler.register_agent(&"a1".to_string(), 0);
        scheduler.register_agent(&"a2".to_string(), 0);
        scheduler.register_agent(&"a3".to_string(), 0);

        // a1 has work, a2 and a3 are idle
        queue.enqueue(make_item("w1", 0)).unwrap();
        queue.assign(&"w1".to_string(), &"a1".to_string()).unwrap();

        let pressure = QueuePressure {
            ready_ratio: 0.0,
            utilization: 0.11, // 1/(3*3)
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 1,
            active_agents: 3,
            total_capacity: 9,
        };

        let result = scheduler.try_scale_down(&queue, &pressure, 10_000);
        assert!(result.is_some());
        match result.unwrap() {
            SchedulerDecision::ScaleDown { remove_agents, .. } => {
                assert!(!remove_agents.is_empty());
                // Should not remove a1 (has active work)
                assert!(!remove_agents.contains(&"a1".to_string()));
            }
            other => panic!("expected ScaleDown, got {other:?}"),
        }
    }

    #[test]
    fn scale_down_respects_grace_period() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();

        // Agent just started (within 5000ms grace)
        scheduler.register_agent(&"new-agent".to_string(), 8000);

        let pressure = QueuePressure {
            ready_ratio: 0.0,
            utilization: 0.0,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 0,
            active_agents: 2,
            total_capacity: 6,
        };

        let result = scheduler.try_scale_down(&queue, &pressure, 10_000);
        // Agent is within grace period (8000 + 5000 > 10000) — not removable
        assert!(result.is_none());
    }

    #[test]
    fn scale_down_respects_min_fleet_size() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();

        scheduler.register_agent(&"a1".to_string(), 0);

        let pressure = QueuePressure {
            ready_ratio: 0.0,
            utilization: 0.0,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 0,
            active_agents: 1, // at min
            total_capacity: 3,
        };

        let result = scheduler.try_scale_down(&queue, &pressure, 10_000);
        assert!(result.is_none());
    }

    // =========================================================================
    // Circuit breaker tests
    // =========================================================================

    #[test]
    fn circuit_breaker_trips_after_consecutive_scale_ops() {
        let mut scheduler = SwarmScheduler::new(test_config());

        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.95,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 10,
            active_agents: 4,
            total_capacity: 12,
        };

        // 3 consecutive scale-ups should trip the breaker
        for i in 0..3 {
            let t = (i + 1) as u64 * 2000;
            scheduler.try_scale_up(&pressure, t);
        }

        assert!(scheduler.circuit_breaker_tripped_at.is_some());
        assert!(scheduler.circuit_breaker_active(7000));
    }

    #[test]
    fn circuit_breaker_auto_resets() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.circuit_breaker_tripped_at = Some(1000);

        // Not yet reset (within 10_000ms window)
        assert!(scheduler.circuit_breaker_active(5000));

        // Reset after window
        assert!(!scheduler.circuit_breaker_active(12_000));
    }

    #[test]
    fn manual_circuit_breaker_reset() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.circuit_breaker_tripped_at = Some(1000);
        scheduler.consecutive_scale_ops = 5;

        scheduler.reset_circuit_breaker();
        assert!(scheduler.circuit_breaker_tripped_at.is_none());
        assert_eq!(scheduler.consecutive_scale_ops, 0);
    }

    // =========================================================================
    // Rebalance tests
    // =========================================================================

    #[test]
    fn rebalance_detects_imbalanced_load() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        scheduler.register_agent(&"a1".to_string(), 0);
        scheduler.register_agent(&"a2".to_string(), 0);

        // a1 has 3 items (full), a2 has 0 (empty) → imbalance = 1.0
        for i in 0..3 {
            queue.enqueue(make_item(&format!("w{i}"), 0)).unwrap();
            queue.assign(&format!("w{i}"), &"a1".to_string()).unwrap();
        }

        let result = scheduler.check_rebalance(&queue, 3);
        assert!(result.is_some());
        match result.unwrap() {
            SchedulerDecision::Rebalance { moves } => {
                assert!(!moves.is_empty());
                assert_eq!(moves[0].from_agent, "a1");
                assert_eq!(moves[0].to_agent, "a2");
            }
            other => panic!("expected Rebalance, got {other:?}"),
        }
    }

    #[test]
    fn no_rebalance_when_balanced() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        scheduler.register_agent(&"a1".to_string(), 0);
        scheduler.register_agent(&"a2".to_string(), 0);

        // Each agent has 1 item — balanced
        queue.enqueue(make_item("w1", 0)).unwrap();
        queue.assign(&"w1".to_string(), &"a1".to_string()).unwrap();
        queue.enqueue(make_item("w2", 0)).unwrap();
        queue.assign(&"w2".to_string(), &"a2".to_string()).unwrap();

        let result = scheduler.check_rebalance(&queue, 3);
        assert!(result.is_none());
    }

    // =========================================================================
    // Snapshot/restore tests
    // =========================================================================

    #[test]
    fn snapshot_restore_roundtrip() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.register_agent(&"a1".to_string(), 1000);
        scheduler.record_completion(&"a1".to_string());
        scheduler.record_failure(&"a1".to_string());
        scheduler.last_scale_up_ms = 5000;
        scheduler.consecutive_scale_ops = 2;

        let snap = scheduler.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let restored_snap: SchedulerSnapshot = serde_json::from_str(&json).unwrap();
        let restored = SwarmScheduler::restore(restored_snap);

        assert_eq!(restored.last_scale_up_ms, 5000);
        assert_eq!(restored.consecutive_scale_ops, 2);
        assert_eq!(restored.agent_completed[&"a1".to_string()], 1);
        assert_eq!(restored.agent_failed[&"a1".to_string()], 1);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let scheduler = SwarmScheduler::new(test_config());
        let snap = scheduler.snapshot();
        let json = serde_json::to_string_pretty(&snap).unwrap();
        let restored: SchedulerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, restored);
    }

    // =========================================================================
    // Decision type tests
    // =========================================================================

    #[test]
    fn decision_serde_roundtrip_noop() {
        let d = SchedulerDecision::Noop {
            reason: "healthy".to_string(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let restored: SchedulerDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored);
    }

    #[test]
    fn decision_serde_roundtrip_scale_up() {
        let d = SchedulerDecision::ScaleUp {
            additional_agents: 3,
            reason: "high pressure".to_string(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let restored: SchedulerDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored);
    }

    #[test]
    fn decision_serde_roundtrip_rebalance() {
        let d = SchedulerDecision::Rebalance {
            moves: vec![RebalanceMove {
                item_id: "w1".to_string(),
                from_agent: "a1".to_string(),
                to_agent: "a2".to_string(),
                reason: "imbalance".to_string(),
            }],
        };
        let json = serde_json::to_string(&d).unwrap();
        let restored: SchedulerDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored);
    }

    // =========================================================================
    // Error type tests
    // =========================================================================

    #[test]
    fn error_display_coverage() {
        let errors = vec![
            SchedulerError::CircuitBreakerActive {
                tripped_at: 1000,
                resets_at: 11_000,
            },
            SchedulerError::AtMaxCapacity {
                current: 16,
                max: 16,
            },
            SchedulerError::AtMinCapacity { current: 1, min: 1 },
            SchedulerError::CooldownActive {
                operation: "scale-up".to_string(),
                remaining_ms: 500,
            },
            SchedulerError::NoAgentsAvailable,
            SchedulerError::NoReadyWork,
        ];
        for e in &errors {
            let msg = format!("{e}");
            assert!(!msg.is_empty());
        }
    }

    #[test]
    fn error_serde_roundtrip() {
        let e = SchedulerError::CircuitBreakerActive {
            tripped_at: 1000,
            resets_at: 11_000,
        };
        let json = serde_json::to_string(&e).unwrap();
        let restored: SchedulerError = serde_json::from_str(&json).unwrap();
        assert_eq!(e, restored);
    }

    // =========================================================================
    // Convenience function tests
    // =========================================================================

    #[test]
    fn compute_queue_pressure_on_empty() {
        let queue = make_queue();
        let pressure = compute_queue_pressure(&queue);
        assert!((pressure.utilization - 0.0).abs() < f64::EPSILON);
        assert!((pressure.ready_ratio - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn evaluate_readonly_uses_caller_capacity_hint() {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 8,
            blocked: 0,
            ready: 0,
            in_progress: 4,
            completed: 2,
            failed: 0,
            cancelled: 0,
            active_agents: 2,
            completion_log_size: 0,
        };

        let pressure_tight = scheduler.evaluate_readonly(&stats, 2, 1000);
        let pressure_loose = scheduler.evaluate_readonly(&stats, 4, 1000);
        assert!(pressure_tight.utilization > pressure_loose.utilization);
        assert!((pressure_tight.utilization - 1.0).abs() < f64::EPSILON);
        assert!((pressure_loose.utilization - 0.5).abs() < f64::EPSILON);
    }

    // =========================================================================
    // History eviction tests
    // =========================================================================

    #[test]
    fn scale_history_evicts_oldest_when_full() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.max_history_entries = 10;

        for i in 0..15 {
            scheduler.record_event(
                ScaleEventType::ScaleUp,
                format!("event {i}"),
                i,
                i + 1,
                SchedulerDecision::Noop {
                    reason: "test".to_string(),
                },
                i as u64 * 1000,
            );
        }

        assert!(scheduler.scale_history.len() <= 10);
    }

    // =========================================================================
    // Integration: full evaluate cycle
    // =========================================================================

    #[test]
    fn full_evaluate_cycle_assign_and_complete() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        let a1 = "agent-1".to_string();
        scheduler.register_agent(&a1, 0);

        // Enqueue work
        queue.enqueue(make_item("task-1", 0)).unwrap();
        queue.enqueue(make_item("task-2", 1)).unwrap();

        // First evaluation should assign work
        let d1 = scheduler.evaluate(&mut queue, 1000);
        match &d1 {
            SchedulerDecision::AssignWork { assignments } => {
                assert!(!assignments.is_empty());
            }
            other => panic!("expected AssignWork, got {other:?}"),
        }

        // Complete first task
        queue
            .complete(&"task-1".to_string(), &a1, Some("done".to_string()))
            .unwrap();
        scheduler.record_completion(&a1);

        // Second evaluation should assign remaining work
        let d2 = scheduler.evaluate(&mut queue, 2000);
        match &d2 {
            SchedulerDecision::AssignWork { assignments } => {
                assert!(assignments.iter().any(|a| a.item_id == "task-2"));
            }
            SchedulerDecision::Noop { .. } => {
                // task-2 might already have been assigned in d1
            }
            other => panic!("expected AssignWork or Noop, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_reclaims_before_assigning() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = SwarmWorkQueue::new(WorkQueueConfig {
            max_concurrent_per_agent: 3,
            heartbeat_timeout_ms: 0, // immediate timeout
            max_retries: 2,
            anti_starvation: false,
            starvation_threshold_ms: 60_000,
        });

        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 0);

        queue.enqueue(make_item("w1", 0)).unwrap();
        queue.assign(&"w1".to_string(), &agent).unwrap();

        // Ensure at least 1ms passes so reclaim_timed_out detects elapsed > 0
        std::thread::sleep(std::time::Duration::from_millis(2));

        let decision = scheduler.evaluate(&mut queue, 5000);
        match decision {
            SchedulerDecision::ReclaimStale { reclaimed_items } => {
                assert!(reclaimed_items.contains(&"w1".to_string()));
            }
            other => panic!("expected ReclaimStale, got {other:?}"),
        }
    }

    #[test]
    fn sequence_increments_on_evaluation() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        assert_eq!(scheduler.sequence(), 0);
        scheduler.evaluate(&mut queue, 1000);
        assert_eq!(scheduler.sequence(), 1);
        scheduler.evaluate(&mut queue, 2000);
        assert_eq!(scheduler.sequence(), 2);
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn single_agent_no_rebalance() {
        let scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();

        // Can't rebalance with only one agent
        let result = scheduler.check_rebalance(&queue, 3);
        assert!(result.is_none());
    }

    #[test]
    fn scale_down_prefers_least_productive() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();

        scheduler.register_agent(&"productive".to_string(), 0);
        scheduler.register_agent(&"lazy".to_string(), 0);

        // productive has 10 completions, lazy has 0
        for _ in 0..10 {
            scheduler.record_completion(&"productive".to_string());
        }

        let pressure = QueuePressure {
            ready_ratio: 0.0,
            utilization: 0.0,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 0,
            active_agents: 2,
            total_capacity: 6,
        };

        let result = scheduler.try_scale_down(&queue, &pressure, 10_000);
        match result {
            Some(SchedulerDecision::ScaleDown { remove_agents, .. }) => {
                // Should prefer removing the lazy agent first
                assert_eq!(remove_agents[0], "lazy");
            }
            other => panic!("expected ScaleDown, got {other:?}"),
        }
    }

    #[test]
    fn scale_step_proportional_to_pressure() {
        let mut config = test_config();
        config.max_scale_step = 4;
        config.scale_up_threshold = 0.80;
        let mut scheduler = SwarmScheduler::new(config);

        // Moderate pressure: 0.85 → small step
        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.85,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 10,
            active_agents: 4,
            total_capacity: 12,
        };
        let result = scheduler.try_scale_up(&pressure, 5000);
        match result {
            Some(SchedulerDecision::ScaleUp {
                additional_agents, ..
            }) => {
                assert!(additional_agents >= 1);
                assert!(additional_agents <= 4);
            }
            other => panic!("expected ScaleUp, got {other:?}"),
        }
    }

    #[test]
    fn work_queue_config_accessible() {
        let queue = make_queue();
        let cfg = queue.config();
        assert_eq!(cfg.max_concurrent_per_agent, 3);
    }
}
