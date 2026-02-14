//! Global resize scheduler with frame-budget-aware work classes.
//!
//! This module provides the control-plane scheduler required by `wa-1u90p.2.3`.
//! It implements:
//! - bounded per-pane pending queueing (`<= 1` pending intent after coalescing)
//! - single-flight execution per pane (no concurrent active intents per pane)
//! - work-class-aware global selection (`interactive` vs `background`)
//! - frame-budget-aware scheduling
//! - starvation protection for background work via deferral aging
//! - input-first guardrails that reserve budget under interaction backlog
//! - cross-pane resize storm detection and deduplication (`wa-1u90p.5.3`)
//! - domain-aware throttling with per-domain budget caps (`wa-1u90p.5.3`)
//! - observability via scheduler metrics and snapshots

use std::collections::{HashMap, VecDeque};
use std::sync::{OnceLock, RwLock};

use crate::resize_invariants::{
    ResizeInvariantReport, ResizeInvariantTelemetry, ResizePhase, check_lifecycle_event_invariants,
    check_phase_transition, check_scheduler_invariants, check_scheduler_snapshot_invariants,
};
use serde::{Deserialize, Serialize};

/// Global scheduling class for a resize intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeWorkClass {
    /// Latency-biased work; should be preferred when budgets are tight.
    Interactive,
    /// Throughput-biased work; may be deferred behind interactive work.
    Background,
}

impl ResizeWorkClass {
    const fn base_priority(self) -> u32 {
        match self {
            Self::Interactive => 100,
            Self::Background => 10,
        }
    }
}

/// Domain classification for resize intents.
///
/// Each pane belongs to a domain representing its connection context.
/// Domains enable per-domain throttling and fair budget partitioning
/// so that remote-domain resize storms don't starve local panes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeDomain {
    /// Local panes on the same host.
    Local,
    /// Remote panes connected via SSH.
    Ssh { host: String },
    /// Remote panes connected via mux protocol.
    Mux { endpoint: String },
}

impl ResizeDomain {
    /// Return a domain key for grouping/throttling purposes.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Local => "local".to_string(),
            Self::Ssh { host } => format!("ssh:{host}"),
            Self::Mux { endpoint } => format!("mux:{endpoint}"),
        }
    }

    /// Default budget share weight for this domain type.
    ///
    /// Local panes get higher default weight since they are more latency-sensitive.
    #[allow(dead_code)]
    const fn default_weight(&self) -> u32 {
        match self {
            Self::Local => 4,
            Self::Ssh { .. } => 2,
            Self::Mux { .. } => 1,
        }
    }
}

impl Default for ResizeDomain {
    fn default() -> Self {
        Self::Local
    }
}

/// A resize intent admitted into the scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeIntent {
    /// Target pane.
    pub pane_id: u64,
    /// Monotonic sequence number for this pane's intent stream.
    pub intent_seq: u64,
    /// Scheduling class.
    pub scheduler_class: ResizeWorkClass,
    /// Estimated frame-budget work units for this intent.
    /// Values `0` are normalized to `1`.
    pub work_units: u32,
    /// Submission timestamp (epoch ms).
    pub submitted_at_ms: u64,
    /// Domain that owns this pane, used for fair budget partitioning.
    #[serde(default)]
    pub domain: ResizeDomain,
    /// Optional tab grouping ID for storm dedup.
    /// Intents from the same tab within a storm window are collapsed.
    #[serde(default)]
    pub tab_id: Option<u64>,
}

impl ResizeIntent {
    fn normalized_work_units(&self) -> u32 {
        if self.work_units == 0 {
            1
        } else {
            self.work_units
        }
    }
}

/// Configuration for frame-budget-aware resize scheduling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResizeSchedulerConfig {
    /// Master enable for control-plane scheduler behavior.
    pub control_plane_enabled: bool,
    /// Emergency kill-switch; when true control-plane behavior is suppressed.
    pub emergency_disable: bool,
    /// Whether callers should fall back to legacy resize behavior when disabled.
    pub legacy_fallback_enabled: bool,
    /// Default per-frame budget in work units.
    pub frame_budget_units: u32,
    /// Enable input-first guardrails that reserve a portion of frame budget.
    pub input_guardrail_enabled: bool,
    /// Minimum input backlog required before guardrails activate.
    pub input_backlog_threshold: u32,
    /// Work units reserved for input processing when guardrails activate.
    pub input_reserve_units: u32,
    /// Number of consecutive deferrals before background work is force-served.
    pub max_deferrals_before_force: u32,
    /// Per-frame aging credit added while a pane remains pending.
    pub aging_credit_per_frame: u32,
    /// Maximum aging credit for any pane.
    pub max_aging_credit: u32,
    /// Allow one oversubscribed pick when a frame starts empty.
    pub allow_single_oversubscription: bool,
    /// Maximum concurrently queued pending panes before overload policy is applied.
    pub max_pending_panes: usize,
    /// Maximum consecutive deferrals before pending work is dropped as stale.
    pub max_deferrals_before_drop: u32,
    /// Maximum number of lifecycle events retained for debug introspection.
    pub max_lifecycle_events: usize,
    /// Duration in milliseconds of the storm detection sliding window.
    /// Set to `0` to disable storm detection.
    pub storm_window_ms: u64,
    /// Number of intents from the same tab within the storm window to trigger storm mode.
    pub storm_threshold_intents: u32,
    /// Maximum picks allowed per tab per frame during storm conditions.
    pub max_storm_picks_per_tab: u32,
    /// Enable per-domain budget partitioning for fair cross-domain scheduling.
    pub domain_budget_enabled: bool,
}

impl Default for ResizeSchedulerConfig {
    fn default() -> Self {
        Self {
            control_plane_enabled: true,
            emergency_disable: false,
            legacy_fallback_enabled: true,
            frame_budget_units: 8,
            input_guardrail_enabled: true,
            input_backlog_threshold: 1,
            input_reserve_units: 2,
            max_deferrals_before_force: 3,
            aging_credit_per_frame: 5,
            max_aging_credit: 80,
            allow_single_oversubscription: true,
            max_pending_panes: 128,
            max_deferrals_before_drop: 12,
            max_lifecycle_events: 256,
            storm_window_ms: 50,
            storm_threshold_intents: 4,
            max_storm_picks_per_tab: 2,
            domain_budget_enabled: false,
        }
    }
}

/// Outcome of submitting a resize intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubmitOutcome {
    /// Intent accepted; may replace a previously pending intent for the pane.
    Accepted {
        /// Sequence ID of the pending intent that was superseded, if any.
        replaced_pending_seq: Option<u64>,
    },
    /// Intent rejected because per-pane sequence monotonicity was violated.
    RejectedNonMonotonic {
        /// Latest known sequence for this pane.
        latest_seq: u64,
    },
    /// Intent rejected because queue saturation policy denied admission.
    DroppedOverload {
        /// Pending pane count at decision time.
        pending_total: usize,
        /// Optional evicted pending entry when higher-priority work forced admission.
        evicted_pending: Option<(u64, u64)>,
    },
    /// Intent suppressed by feature gate / emergency kill-switch.
    SuppressedByKillSwitch {
        /// Whether legacy fallback path is configured.
        legacy_fallback: bool,
    },
}

/// One scheduled resize work item for the current frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledResizeWork {
    /// Target pane.
    pub pane_id: u64,
    /// Scheduled intent sequence.
    pub intent_seq: u64,
    /// Scheduling class of the intent.
    pub scheduler_class: ResizeWorkClass,
    /// Scheduled work-unit cost.
    pub work_units: u32,
    /// True when this pick consumed more work units than remaining frame budget.
    pub over_budget: bool,
    /// True when selected due to starvation forcing.
    pub forced_by_starvation: bool,
}

/// Result of a single frame scheduling round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScheduleFrameResult {
    /// Frame budget used for this round.
    pub frame_budget_units: u32,
    /// Effective resize budget after input-reserve guardrails are applied.
    pub effective_resize_budget_units: u32,
    /// Work units reserved for input handling in this frame.
    pub input_reserved_units: u32,
    /// Observed pending input backlog at scheduling time.
    pub pending_input_events: u32,
    /// Work units spent by scheduled picks.
    pub budget_spent_units: u32,
    /// Picks in scheduling order.
    pub scheduled: Vec<ScheduledResizeWork>,
    /// Pending intents still queued after this frame.
    pub pending_after: usize,
}

/// Scheduler metrics for observability and triage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResizeSchedulerMetrics {
    /// Number of scheduler rounds executed.
    pub frames: u64,
    /// Count of superseded pending intents.
    pub superseded_intents: u64,
    /// Count of rejected non-monotonic submissions.
    pub rejected_non_monotonic: u64,
    /// Count of starvation-forced background selections.
    pub forced_background_runs: u64,
    /// Count of over-budget selections.
    pub over_budget_runs: u64,
    /// Count of incoming intents rejected due to overload admission policy.
    pub overload_rejected: u64,
    /// Count of pending intents evicted due to overload policy.
    pub overload_evicted: u64,
    /// Count of pending intents dropped after excessive deferrals.
    pub dropped_after_deferrals: u64,
    /// Count of submitted intents suppressed by gate/kill-switch.
    pub suppressed_by_gate: u64,
    /// Count of frames where scheduling was suppressed by gate/kill-switch.
    pub suppressed_frames: u64,
    /// Count of frames where input guardrails reserved budget.
    pub input_guardrail_frames: u64,
    /// Count of candidate deferrals caused specifically by input guardrails.
    pub input_guardrail_deferrals: u64,
    /// Last frame budget units.
    pub last_frame_budget_units: u32,
    /// Last effective resize budget after guardrail reservation.
    pub last_effective_resize_budget_units: u32,
    /// Last observed pending input backlog.
    pub last_input_backlog: u32,
    /// Last frame consumed units.
    pub last_frame_spent_units: u32,
    /// Number of picks made in last frame.
    pub last_frame_scheduled: u32,
    /// Count of active transactions cancelled due to supersession.
    pub cancelled_active: u64,
    /// Count of active transactions completed successfully.
    pub completed_active: u64,
    /// Count of completion attempts rejected due to sequence mismatch.
    pub completion_rejected: u64,
    /// Count of storm conditions detected (tab exceeded storm threshold).
    pub storm_events_detected: u64,
    /// Count of candidate picks throttled by per-tab storm limit.
    pub storm_picks_throttled: u64,
    /// Count of candidate picks throttled by domain budget cap.
    pub domain_budget_throttled: u64,
}

/// Overload drop reason for pending/intent admission controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeOverloadReason {
    /// Pending queue capacity reached.
    QueueCapacity,
    /// Pending work exceeded maximum deferral threshold.
    DeferralTimeout,
}

/// High-level lifecycle stage for a resize transaction event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeLifecycleStage {
    /// Intent admitted and queued.
    Queued,
    /// Intent selected into active execution for a frame.
    Scheduled,
    /// Active intent entered prepare phase.
    Preparing,
    /// Active intent entered reflow phase.
    Reflowing,
    /// Active intent entered present phase.
    Presenting,
    /// Active work cancelled due to supersession.
    Cancelled,
    /// Active work committed.
    Committed,
    /// A lifecycle transition failed validation.
    Failed,
}

/// Fine-grained execution phase for an active resize transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeExecutionPhase {
    /// Staging prerequisites before reflow.
    Preparing,
    /// Performing reflow/layout work.
    Reflowing,
    /// Present/swap stage before commit.
    Presenting,
}

impl ResizeExecutionPhase {
    const fn lifecycle_stage(self) -> ResizeLifecycleStage {
        match self {
            Self::Preparing => ResizeLifecycleStage::Preparing,
            Self::Reflowing => ResizeLifecycleStage::Reflowing,
            Self::Presenting => ResizeLifecycleStage::Presenting,
        }
    }
}

/// Detailed lifecycle event kind for transaction introspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResizeLifecycleDetail {
    /// New intent accepted into per-pane pending slot.
    IntentSubmitted {
        /// Sequence ID of superseded pending intent, if any.
        replaced_pending_seq: Option<u64>,
    },
    /// Intent rejected due to non-monotonic sequence.
    IntentRejectedNonMonotonic {
        /// Latest known sequence at rejection time.
        latest_seq: u64,
    },
    /// Incoming intent rejected because overload policy denied admission.
    IntentRejectedOverload {
        /// Pending pane count at rejection time.
        pending_total: usize,
    },
    /// Incoming intent suppressed by control-plane gate.
    IntentSuppressedByGate {
        /// Whether emergency disable was active.
        emergency_disable: bool,
        /// Whether legacy fallback is configured.
        legacy_fallback: bool,
    },
    /// Existing pending intent dropped by overload/backpressure policy.
    PendingDroppedOverload {
        /// Why the pending work was dropped.
        reason: ResizeOverloadReason,
        /// Pane whose pending intent was removed.
        dropped_pane_id: u64,
        /// Pending sequence that was dropped.
        dropped_intent_seq: u64,
    },
    /// Intent moved from pending to active during scheduling.
    IntentScheduled {
        /// Selected scheduling class.
        scheduler_class: ResizeWorkClass,
        /// Scheduled work units.
        work_units: u32,
        /// Whether pick exceeded remaining frame budget.
        over_budget: bool,
        /// Whether starvation forcing selected this intent.
        forced_by_starvation: bool,
    },
    /// Active transaction cancelled because a newer sequence exists.
    ActiveCancelledSuperseded {
        /// Sequence that superseded the cancelled active work.
        superseded_by_seq: u64,
    },
    /// Active transaction completed normally.
    ActiveCompleted,
    /// Active transaction moved to a specific execution phase.
    ActivePhaseTransition {
        /// New active execution phase.
        phase: ResizeExecutionPhase,
    },
    /// Completion attempted against a non-active sequence.
    ActiveCompletionRejected {
        /// Active sequence at rejection time.
        active_seq: Option<u64>,
    },
    /// Phase transition attempted for a non-active sequence.
    ActivePhaseTransitionRejected {
        /// Active sequence at rejection time.
        active_seq: Option<u64>,
        /// Requested phase transition.
        requested_phase: ResizeExecutionPhase,
    },
}

/// One recorded lifecycle event for resize transaction diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeTransactionLifecycleEvent {
    /// Monotonic scheduler-local lifecycle event ID.
    pub event_seq: u64,
    /// Scheduler frame count at emission time.
    pub frame_seq: u64,
    /// Target pane ID.
    pub pane_id: u64,
    /// Intent sequence the event is about.
    pub intent_seq: u64,
    /// Event timestamp if known (typically submit-time epoch ms).
    pub observed_at_ms: Option<u64>,
    /// Latest known sequence for pane at emission time.
    pub latest_seq: Option<u64>,
    /// Pending sequence for pane at emission time.
    pub pending_seq: Option<u64>,
    /// Active sequence for pane at emission time.
    pub active_seq: Option<u64>,
    /// Lifecycle stage bucket.
    pub stage: ResizeLifecycleStage,
    /// Event-specific detail payload.
    pub detail: ResizeLifecycleDetail,
}

/// Per-pane scheduler snapshot for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeSchedulerPaneSnapshot {
    /// Pane identifier.
    pub pane_id: u64,
    /// Latest known intent sequence for this pane.
    pub latest_seq: Option<u64>,
    /// Pending sequence, if any.
    pub pending_seq: Option<u64>,
    /// Pending class, if any.
    pub pending_class: Option<ResizeWorkClass>,
    /// Active sequence currently in-flight, if any.
    pub active_seq: Option<u64>,
    /// Active execution phase for in-flight transaction, if known.
    pub active_phase: Option<ResizeExecutionPhase>,
    /// Timestamp when current active phase started (epoch ms), if known.
    pub active_phase_started_at_ms: Option<u64>,
    /// Current deferral count for pending work.
    pub deferrals: u32,
    /// Current aging credit.
    pub aging_credit: u32,
}

/// Full scheduler snapshot for telemetry surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeSchedulerSnapshot {
    /// Scheduler config.
    pub config: ResizeSchedulerConfig,
    /// Runtime metrics.
    pub metrics: ResizeSchedulerMetrics,
    /// Total pending intents.
    pub pending_total: usize,
    /// Total panes currently active.
    pub active_total: usize,
    /// Per-pane state rows.
    pub panes: Vec<ResizeSchedulerPaneSnapshot>,
}

/// Resolved feature-gate state for resize control-plane behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeControlPlaneGateState {
    /// Config-level enable.
    pub control_plane_enabled: bool,
    /// Emergency kill-switch state.
    pub emergency_disable: bool,
    /// Whether legacy fallback is configured.
    pub legacy_fallback_enabled: bool,
    /// Effective gate resolution (`enabled && !emergency_disable`).
    pub active: bool,
}

/// Debug snapshot bundle for resize transaction lifecycle introspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeSchedulerDebugSnapshot {
    /// Effective control-plane gate state.
    pub gate: ResizeControlPlaneGateState,
    /// Core scheduler snapshot.
    pub scheduler: ResizeSchedulerSnapshot,
    /// Recent lifecycle events (oldest first).
    pub lifecycle_events: Vec<ResizeTransactionLifecycleEvent>,
    /// Invariant report computed from scheduler + lifecycle state.
    #[serde(default)]
    pub invariants: ResizeInvariantReport,
    /// Aggregate invariant counters for quick health checks.
    #[serde(default)]
    pub invariant_telemetry: ResizeInvariantTelemetry,
}

/// Stalled active transaction summary derived from scheduler debug state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeStalledTransaction {
    /// Pane identifier with active work.
    pub pane_id: u64,
    /// Active intent sequence.
    pub intent_seq: u64,
    /// Current active phase, if known.
    pub active_phase: Option<ResizeExecutionPhase>,
    /// Age of active phase in milliseconds.
    pub age_ms: u64,
    /// Latest known pane sequence (helps diagnose supersession).
    pub latest_seq: Option<u64>,
}

static GLOBAL_RESIZE_DEBUG_SNAPSHOT: OnceLock<RwLock<Option<ResizeSchedulerDebugSnapshot>>> =
    OnceLock::new();

impl ResizeSchedulerDebugSnapshot {
    /// Update the process-global debug snapshot used by introspection surfaces.
    pub fn update_global(snapshot: Self) {
        let lock = GLOBAL_RESIZE_DEBUG_SNAPSHOT.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(snapshot);
        }
    }

    /// Get the latest process-global resize control-plane debug snapshot.
    #[must_use]
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_RESIZE_DEBUG_SNAPSHOT.get_or_init(|| RwLock::new(None));
        lock.read().ok().and_then(|guard| guard.clone())
    }

    /// Derive stalled active transactions above `threshold_ms` age.
    #[must_use]
    pub fn stalled_transactions(
        &self,
        now_ms: u64,
        threshold_ms: u64,
    ) -> Vec<ResizeStalledTransaction> {
        self.scheduler
            .panes
            .iter()
            .filter_map(|pane| {
                let active_seq = pane.active_seq?;
                let started_at = pane.active_phase_started_at_ms?;
                let age_ms = now_ms.saturating_sub(started_at);
                if age_ms < threshold_ms {
                    return None;
                }
                Some(ResizeStalledTransaction {
                    pane_id: pane.pane_id,
                    intent_seq: active_seq,
                    active_phase: pane.active_phase,
                    age_ms,
                    latest_seq: pane.latest_seq,
                })
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct PaneState {
    latest_seq: Option<u64>,
    pending: Option<ResizeIntent>,
    active_seq: Option<u64>,
    active_phase: Option<ResizeExecutionPhase>,
    active_phase_started_at_ms: Option<u64>,
    deferrals: u32,
    aging_credit: u32,
}

#[derive(Debug, Clone)]
struct Candidate {
    pane_id: u64,
    score: u32,
    forced_by_starvation: bool,
    intent_seq: u64,
    submitted_at_ms: u64,
    work_units: u32,
    domain_key: String,
    domain_weight: u32,
    tab_id: Option<u64>,
}

/// Global resize scheduler.
#[derive(Debug)]
pub struct ResizeScheduler {
    config: ResizeSchedulerConfig,
    panes: HashMap<u64, PaneState>,
    metrics: ResizeSchedulerMetrics,
    lifecycle_events: VecDeque<ResizeTransactionLifecycleEvent>,
    next_lifecycle_event_seq: u64,
    /// Per-tab submission timestamps for storm detection.
    tab_submit_history: HashMap<u64, VecDeque<u64>>,
}

impl ResizeScheduler {
    /// Create a scheduler with the supplied configuration.
    #[must_use]
    pub fn new(config: ResizeSchedulerConfig) -> Self {
        let scheduler = Self {
            config,
            panes: HashMap::new(),
            metrics: ResizeSchedulerMetrics::default(),
            lifecycle_events: VecDeque::new(),
            next_lifecycle_event_seq: 0,
            tab_submit_history: HashMap::new(),
        };
        scheduler.publish_debug_snapshot();
        scheduler
    }

    /// Read current scheduler config.
    #[must_use]
    pub const fn config(&self) -> &ResizeSchedulerConfig {
        &self.config
    }

    /// Whether control-plane scheduling behavior is currently active.
    #[must_use]
    pub const fn control_plane_active(&self) -> bool {
        self.config.control_plane_enabled && !self.config.emergency_disable
    }

    /// Effective gate state, including resolved active bool.
    #[must_use]
    pub const fn gate_state(&self) -> ResizeControlPlaneGateState {
        ResizeControlPlaneGateState {
            control_plane_enabled: self.config.control_plane_enabled,
            emergency_disable: self.config.emergency_disable,
            legacy_fallback_enabled: self.config.legacy_fallback_enabled,
            active: self.control_plane_active(),
        }
    }

    /// Toggle master enable for control-plane behavior.
    pub fn set_control_plane_enabled(&mut self, enabled: bool) {
        self.config.control_plane_enabled = enabled;
        self.publish_debug_snapshot();
    }

    /// Toggle emergency disable kill-switch.
    pub fn set_emergency_disable(&mut self, emergency_disable: bool) {
        self.config.emergency_disable = emergency_disable;
        self.publish_debug_snapshot();
    }

    /// Read scheduler metrics.
    #[must_use]
    pub const fn metrics(&self) -> &ResizeSchedulerMetrics {
        &self.metrics
    }

    /// Number of currently pending intents.
    #[must_use]
    pub fn pending_total(&self) -> usize {
        self.panes
            .values()
            .filter(|state| state.pending.is_some())
            .count()
    }

    /// Number of panes currently in active single-flight execution.
    #[must_use]
    pub fn active_total(&self) -> usize {
        self.panes
            .values()
            .filter(|state| state.active_seq.is_some())
            .count()
    }

    /// Submit a new resize intent.
    ///
    /// Per-pane sequence numbers must be strictly increasing.
    /// Pending queue depth is bounded to one entry by replacing older pending work.
    pub fn submit_intent(&mut self, mut intent: ResizeIntent) -> SubmitOutcome {
        let pane_id = intent.pane_id;
        let intent_seq = intent.intent_seq;
        let observed_at_ms = Some(intent.submitted_at_ms);
        let tab_id = intent.tab_id;
        let submitted_at_ms = intent.submitted_at_ms;
        if !self.control_plane_active() {
            self.metrics.suppressed_by_gate = self.metrics.suppressed_by_gate.saturating_add(1);
            self.push_lifecycle_event(
                pane_id,
                intent_seq,
                observed_at_ms,
                ResizeLifecycleStage::Failed,
                ResizeLifecycleDetail::IntentSuppressedByGate {
                    emergency_disable: self.config.emergency_disable,
                    legacy_fallback: self.config.legacy_fallback_enabled,
                },
            );
            self.publish_debug_snapshot();
            return SubmitOutcome::SuppressedByKillSwitch {
                legacy_fallback: self.config.legacy_fallback_enabled,
            };
        }
        intent.work_units = intent.normalized_work_units();
        let latest_seq = self.panes.get(&pane_id).and_then(|state| state.latest_seq);
        if let Some(latest) = latest_seq {
            if intent_seq <= latest {
                self.metrics.rejected_non_monotonic =
                    self.metrics.rejected_non_monotonic.saturating_add(1);
                self.push_lifecycle_event(
                    pane_id,
                    intent_seq,
                    observed_at_ms,
                    ResizeLifecycleStage::Failed,
                    ResizeLifecycleDetail::IntentRejectedNonMonotonic { latest_seq: latest },
                );
                self.publish_debug_snapshot();
                return SubmitOutcome::RejectedNonMonotonic { latest_seq: latest };
            }
        }

        let replaced_pending_seq = self
            .panes
            .get(&pane_id)
            .and_then(|state| state.pending.as_ref().map(|pending| pending.intent_seq));
        let mut evicted_pending = None;
        let pending_cap = self.config.max_pending_panes.max(1);
        let needs_new_pending_slot = replaced_pending_seq.is_none();
        if needs_new_pending_slot && self.pending_total() >= pending_cap {
            evicted_pending = if matches!(intent.scheduler_class, ResizeWorkClass::Interactive) {
                self.evict_oldest_background_pending()
            } else {
                None
            };

            if evicted_pending.is_none() {
                self.metrics.overload_rejected = self.metrics.overload_rejected.saturating_add(1);
                self.push_lifecycle_event(
                    pane_id,
                    intent_seq,
                    observed_at_ms,
                    ResizeLifecycleStage::Failed,
                    ResizeLifecycleDetail::IntentRejectedOverload {
                        pending_total: self.pending_total(),
                    },
                );
                self.publish_debug_snapshot();
                return SubmitOutcome::DroppedOverload {
                    pending_total: self.pending_total(),
                    evicted_pending: None,
                };
            }
        }

        if let Some((dropped_pane_id, dropped_intent_seq)) = evicted_pending {
            self.metrics.overload_evicted = self.metrics.overload_evicted.saturating_add(1);
            self.push_lifecycle_event(
                dropped_pane_id,
                dropped_intent_seq,
                observed_at_ms,
                ResizeLifecycleStage::Cancelled,
                ResizeLifecycleDetail::PendingDroppedOverload {
                    reason: ResizeOverloadReason::QueueCapacity,
                    dropped_pane_id,
                    dropped_intent_seq,
                },
            );
        }

        let state = self.panes.entry(pane_id).or_default();
        if replaced_pending_seq.is_some() {
            self.metrics.superseded_intents = self.metrics.superseded_intents.saturating_add(1);
        }

        state.latest_seq = Some(intent.intent_seq);
        state.pending = Some(intent);
        self.push_lifecycle_event(
            pane_id,
            intent_seq,
            observed_at_ms,
            ResizeLifecycleStage::Queued,
            ResizeLifecycleDetail::IntentSubmitted {
                replaced_pending_seq,
            },
        );

        // Storm detection: track per-tab submission rate.
        if let Some(tab) = tab_id {
            if self.config.storm_window_ms > 0 && self.config.storm_threshold_intents > 0 {
                let history = self.tab_submit_history.entry(tab).or_default();
                let cutoff = submitted_at_ms.saturating_sub(self.config.storm_window_ms);
                while history.front().is_some_and(|&ts| ts < cutoff) {
                    history.pop_front();
                }
                history.push_back(submitted_at_ms);
                if history.len() as u32 >= self.config.storm_threshold_intents {
                    self.metrics.storm_events_detected =
                        self.metrics.storm_events_detected.saturating_add(1);
                }
            }
        }

        self.publish_debug_snapshot();

        SubmitOutcome::Accepted {
            replaced_pending_seq,
        }
    }

    /// Returns true if the active intent is stale and should be cancelled at a phase boundary.
    #[must_use]
    pub fn active_is_superseded(&self, pane_id: u64) -> bool {
        let Some(state) = self.panes.get(&pane_id) else {
            return false;
        };

        matches!(
            (state.active_seq, state.latest_seq),
            (Some(active), Some(latest)) if latest > active
        )
    }

    /// Cancel active work for a pane if a newer intent exists.
    ///
    /// Returns true when cancellation happened.
    pub fn cancel_active_if_superseded(&mut self, pane_id: u64) -> bool {
        let Some(state) = self.panes.get_mut(&pane_id) else {
            return false;
        };

        if matches!(
            (state.active_seq, state.latest_seq),
            (Some(active), Some(latest)) if latest > active
        ) {
            let cancelled_seq = state
                .active_seq
                .expect("active_seq must exist under matched cancellation guard");
            let superseded_by_seq = state
                .latest_seq
                .expect("latest_seq must exist under matched cancellation guard");
            state.active_seq = None;
            state.active_phase = None;
            state.active_phase_started_at_ms = None;
            self.metrics.cancelled_active = self.metrics.cancelled_active.saturating_add(1);
            self.push_lifecycle_event(
                pane_id,
                cancelled_seq,
                None,
                ResizeLifecycleStage::Cancelled,
                ResizeLifecycleDetail::ActiveCancelledSuperseded { superseded_by_seq },
            );
            self.publish_debug_snapshot();
            return true;
        }

        false
    }

    /// Mark an active intent as complete.
    ///
    /// Returns true when the supplied sequence matched the current active sequence.
    pub fn complete_active(&mut self, pane_id: u64, intent_seq: u64) -> bool {
        let Some(state) = self.panes.get(&pane_id) else {
            return false;
        };
        let Some(active_seq) = state.active_seq else {
            return false;
        };
        let latest_seq = state.latest_seq;

        if active_seq != intent_seq {
            self.metrics.completion_rejected = self.metrics.completion_rejected.saturating_add(1);
            self.push_lifecycle_event(
                pane_id,
                intent_seq,
                None,
                ResizeLifecycleStage::Failed,
                ResizeLifecycleDetail::ActiveCompletionRejected {
                    active_seq: Some(active_seq),
                },
            );
            self.publish_debug_snapshot();
            return false;
        }

        if latest_seq.is_some_and(|latest| latest > active_seq) {
            self.metrics.completion_rejected = self.metrics.completion_rejected.saturating_add(1);
            self.push_lifecycle_event(
                pane_id,
                intent_seq,
                None,
                ResizeLifecycleStage::Failed,
                ResizeLifecycleDetail::ActiveCompletionRejected {
                    active_seq: Some(active_seq),
                },
            );
            self.publish_debug_snapshot();
            return false;
        }

        if let Some(state) = self.panes.get_mut(&pane_id) {
            state.active_seq = None;
            state.active_phase = None;
            state.active_phase_started_at_ms = None;
        }
        self.metrics.completed_active = self.metrics.completed_active.saturating_add(1);
        self.push_lifecycle_event(
            pane_id,
            intent_seq,
            None,
            ResizeLifecycleStage::Committed,
            ResizeLifecycleDetail::ActiveCompleted,
        );
        self.publish_debug_snapshot();
        true
    }

    /// Mark an active transaction phase transition for debug/lifecycle introspection.
    ///
    /// Returns true when the pane has matching active sequence and the phase update is recorded.
    pub fn mark_active_phase(
        &mut self,
        pane_id: u64,
        intent_seq: u64,
        phase: ResizeExecutionPhase,
        observed_at_ms: u64,
    ) -> bool {
        let active_seq = self.panes.get(&pane_id).and_then(|state| state.active_seq);
        if active_seq != Some(intent_seq) {
            self.push_lifecycle_event(
                pane_id,
                intent_seq,
                Some(observed_at_ms),
                ResizeLifecycleStage::Failed,
                ResizeLifecycleDetail::ActivePhaseTransitionRejected {
                    active_seq,
                    requested_phase: phase,
                },
            );
            self.publish_debug_snapshot();
            return false;
        }

        if let Some(state) = self.panes.get_mut(&pane_id) {
            state.active_phase = Some(phase);
            state.active_phase_started_at_ms = Some(observed_at_ms);
        }

        self.push_lifecycle_event(
            pane_id,
            intent_seq,
            Some(observed_at_ms),
            phase.lifecycle_stage(),
            ResizeLifecycleDetail::ActivePhaseTransition { phase },
        );
        self.publish_debug_snapshot();
        true
    }

    /// Schedule one frame using the default configured budget.
    pub fn schedule_frame(&mut self) -> ScheduleFrameResult {
        self.schedule_frame_with_budget(self.config.frame_budget_units)
    }

    /// Schedule one frame using the provided budget.
    ///
    /// Selection order is class-aware and aging-aware:
    /// - interactive intents are preferred by default
    /// - pending panes accumulate aging credit
    /// - background panes can be force-served after repeated deferrals
    pub fn schedule_frame_with_budget(&mut self, frame_budget_units: u32) -> ScheduleFrameResult {
        self.schedule_frame_with_input_backlog(frame_budget_units, 0)
    }

    /// Schedule one frame while accounting for input backlog pressure.
    ///
    /// When backlog crosses the configured threshold, this reserves a slice of
    /// frame budget for input processing and throttles resize picks accordingly.
    pub fn schedule_frame_with_input_backlog(
        &mut self,
        frame_budget_units: u32,
        pending_input_events: u32,
    ) -> ScheduleFrameResult {
        if !self.control_plane_active() {
            self.metrics.suppressed_frames = self.metrics.suppressed_frames.saturating_add(1);
            self.metrics.last_input_backlog = pending_input_events;
            self.publish_debug_snapshot();
            return ScheduleFrameResult {
                frame_budget_units: frame_budget_units.max(1),
                effective_resize_budget_units: frame_budget_units.max(1),
                input_reserved_units: 0,
                pending_input_events,
                budget_spent_units: 0,
                scheduled: Vec::new(),
                pending_after: self.pending_total(),
            };
        }

        self.metrics.frames = self.metrics.frames.saturating_add(1);
        self.drop_overdeferred_pending();

        let mut candidates = self.collect_candidates();
        candidates.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.submitted_at_ms.cmp(&right.submitted_at_ms))
                .then_with(|| left.intent_seq.cmp(&right.intent_seq))
                .then_with(|| left.pane_id.cmp(&right.pane_id))
        });

        let mut spent_units = 0u32;
        let mut scheduled = Vec::new();
        let mut deferred_panes = Vec::new();
        let budget_units = frame_budget_units.max(1);
        let (effective_budget_units, input_reserved_units) =
            self.resolve_resize_budget_with_input_guardrail(budget_units, pending_input_events);
        if input_reserved_units > 0 {
            self.metrics.input_guardrail_frames =
                self.metrics.input_guardrail_frames.saturating_add(1);
        }
        let mut forced_over_budget_served = false;

        // Domain budget partitioning: compute per-domain budget allocations.
        let domain_budgets: HashMap<String, u32> = if self.config.domain_budget_enabled {
            let mut domain_weights: HashMap<String, u32> = HashMap::new();
            for c in &candidates {
                domain_weights
                    .entry(c.domain_key.clone())
                    .or_insert(c.domain_weight);
            }
            let total_weight: u32 = domain_weights.values().sum();
            if total_weight > 0 {
                domain_weights
                    .into_iter()
                    .map(|(key, weight)| {
                        let share = (u64::from(effective_budget_units) * u64::from(weight)
                            / u64::from(total_weight)) as u32;
                        (key, share.max(1))
                    })
                    .collect()
            } else {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };
        let mut domain_spent: HashMap<String, u32> = HashMap::new();
        let mut tab_picks: HashMap<u64, u32> = HashMap::new();

        for candidate in candidates {
            // Storm per-tab throttle: limit picks from tabs in storm state.
            if let Some(tab) = candidate.tab_id {
                if self.is_tab_in_storm(tab) {
                    let picks = tab_picks.get(&tab).copied().unwrap_or(0);
                    if picks >= self.config.max_storm_picks_per_tab {
                        self.metrics.storm_picks_throttled =
                            self.metrics.storm_picks_throttled.saturating_add(1);
                        deferred_panes.push(candidate.pane_id);
                        continue;
                    }
                }
            }

            // Domain budget throttle: enforce per-domain budget caps.
            if self.config.domain_budget_enabled && !candidate.forced_by_starvation {
                if let Some(&budget) = domain_budgets.get(&candidate.domain_key) {
                    let spent = domain_spent
                        .get(&candidate.domain_key)
                        .copied()
                        .unwrap_or(0);
                    if spent.saturating_add(candidate.work_units) > budget {
                        self.metrics.domain_budget_throttled =
                            self.metrics.domain_budget_throttled.saturating_add(1);
                        deferred_panes.push(candidate.pane_id);
                        continue;
                    }
                }
            }

            let remaining_units = effective_budget_units.saturating_sub(spent_units);
            let remaining_total_units = budget_units.saturating_sub(spent_units);
            let fits_budget = candidate.work_units <= remaining_units;
            let deferred_by_input_guardrail =
                !fits_budget && candidate.work_units <= remaining_total_units;
            let allow_oversub = self.config.allow_single_oversubscription
                && scheduled.is_empty()
                && input_reserved_units == 0;
            let allow_forced_over_budget = candidate.forced_by_starvation
                && !fits_budget
                && !forced_over_budget_served
                && input_reserved_units == 0;

            if !(fits_budget || allow_oversub || allow_forced_over_budget) {
                if deferred_by_input_guardrail {
                    self.metrics.input_guardrail_deferrals =
                        self.metrics.input_guardrail_deferrals.saturating_add(1);
                }
                deferred_panes.push(candidate.pane_id);
                continue;
            }

            let Some(state) = self.panes.get_mut(&candidate.pane_id) else {
                continue;
            };
            let Some(intent) = state.pending.take() else {
                continue;
            };

            let over_budget = candidate.work_units > remaining_units;
            if over_budget {
                self.metrics.over_budget_runs = self.metrics.over_budget_runs.saturating_add(1);
            }
            if candidate.forced_by_starvation {
                self.metrics.forced_background_runs =
                    self.metrics.forced_background_runs.saturating_add(1);
            }
            if allow_forced_over_budget {
                forced_over_budget_served = true;
            }

            state.active_seq = Some(intent.intent_seq);
            state.active_phase = Some(ResizeExecutionPhase::Preparing);
            state.active_phase_started_at_ms = Some(intent.submitted_at_ms);
            state.deferrals = 0;
            state.aging_credit = 0;

            spent_units = spent_units.saturating_add(intent.normalized_work_units());
            scheduled.push(ScheduledResizeWork {
                pane_id: intent.pane_id,
                intent_seq: intent.intent_seq,
                scheduler_class: intent.scheduler_class,
                work_units: intent.normalized_work_units(),
                over_budget,
                forced_by_starvation: candidate.forced_by_starvation,
            });
            self.push_lifecycle_event(
                intent.pane_id,
                intent.intent_seq,
                Some(intent.submitted_at_ms),
                ResizeLifecycleStage::Scheduled,
                ResizeLifecycleDetail::IntentScheduled {
                    scheduler_class: intent.scheduler_class,
                    work_units: intent.normalized_work_units(),
                    over_budget,
                    forced_by_starvation: candidate.forced_by_starvation,
                },
            );
            self.push_lifecycle_event(
                intent.pane_id,
                intent.intent_seq,
                Some(intent.submitted_at_ms),
                ResizeLifecycleStage::Preparing,
                ResizeLifecycleDetail::ActivePhaseTransition {
                    phase: ResizeExecutionPhase::Preparing,
                },
            );

            // Track domain and tab usage for throttling within this frame.
            if self.config.domain_budget_enabled {
                *domain_spent.entry(candidate.domain_key).or_insert(0) += candidate.work_units;
            }
            if let Some(tab) = candidate.tab_id {
                *tab_picks.entry(tab).or_insert(0) += 1;
            }
        }

        self.apply_deferral_aging(&deferred_panes);

        self.metrics.last_frame_budget_units = budget_units;
        self.metrics.last_effective_resize_budget_units = effective_budget_units;
        self.metrics.last_input_backlog = pending_input_events;
        self.metrics.last_frame_spent_units = spent_units;
        self.metrics.last_frame_scheduled = u32::try_from(scheduled.len()).unwrap_or(u32::MAX);
        self.publish_debug_snapshot();

        ScheduleFrameResult {
            frame_budget_units: budget_units,
            effective_resize_budget_units: effective_budget_units,
            input_reserved_units,
            pending_input_events,
            budget_spent_units: spent_units,
            scheduled,
            pending_after: self.pending_total(),
        }
    }

    /// Produce a telemetry snapshot of scheduler internals.
    #[must_use]
    pub fn snapshot(&self) -> ResizeSchedulerSnapshot {
        let mut panes: Vec<ResizeSchedulerPaneSnapshot> = self
            .panes
            .iter()
            .map(|(pane_id, state)| ResizeSchedulerPaneSnapshot {
                pane_id: *pane_id,
                latest_seq: state.latest_seq,
                pending_seq: state.pending.as_ref().map(|intent| intent.intent_seq),
                pending_class: state.pending.as_ref().map(|intent| intent.scheduler_class),
                active_seq: state.active_seq,
                active_phase: state.active_phase,
                active_phase_started_at_ms: state.active_phase_started_at_ms,
                deferrals: state.deferrals,
                aging_credit: state.aging_credit,
            })
            .collect();
        panes.sort_by_key(|row| row.pane_id);

        ResizeSchedulerSnapshot {
            config: self.config.clone(),
            metrics: self.metrics.clone(),
            pending_total: self.pending_total(),
            active_total: self.active_total(),
            panes,
        }
    }

    /// Return recent lifecycle events for transaction introspection.
    #[must_use]
    pub fn lifecycle_events(&self, limit: usize) -> Vec<ResizeTransactionLifecycleEvent> {
        if self.lifecycle_events.is_empty() {
            return Vec::new();
        }
        let keep = if limit == 0 {
            self.lifecycle_events.len()
        } else {
            limit.min(self.lifecycle_events.len())
        };
        let start = self.lifecycle_events.len().saturating_sub(keep);
        self.lifecycle_events.iter().skip(start).cloned().collect()
    }

    /// Produce debug snapshot bundle with scheduler state and lifecycle events.
    #[must_use]
    pub fn debug_snapshot(&self, lifecycle_event_limit: usize) -> ResizeSchedulerDebugSnapshot {
        let scheduler = self.snapshot();
        let lifecycle_events = self.lifecycle_events(lifecycle_event_limit);
        let (invariants, invariant_telemetry) =
            self.evaluate_invariants(&scheduler, &lifecycle_events);
        ResizeSchedulerDebugSnapshot {
            gate: self.gate_state(),
            scheduler,
            lifecycle_events,
            invariants,
            invariant_telemetry,
        }
    }

    fn lifecycle_stage_to_invariant_phase(stage: ResizeLifecycleStage) -> Option<ResizePhase> {
        match stage {
            ResizeLifecycleStage::Queued => Some(ResizePhase::Queued),
            ResizeLifecycleStage::Preparing => Some(ResizePhase::Preparing),
            ResizeLifecycleStage::Reflowing => Some(ResizePhase::Reflowing),
            ResizeLifecycleStage::Presenting => Some(ResizePhase::Presenting),
            ResizeLifecycleStage::Committed => Some(ResizePhase::Committed),
            ResizeLifecycleStage::Cancelled => Some(ResizePhase::Cancelled),
            ResizeLifecycleStage::Scheduled | ResizeLifecycleStage::Failed => None,
        }
    }

    #[allow(clippy::unused_self)]
    fn evaluate_invariants(
        &self,
        snapshot: &ResizeSchedulerSnapshot,
        lifecycle_events: &[ResizeTransactionLifecycleEvent],
    ) -> (ResizeInvariantReport, ResizeInvariantTelemetry) {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_snapshot_invariants(&mut report, snapshot);
        check_lifecycle_event_invariants(&mut report, lifecycle_events);

        let mut last_phase_by_tx: HashMap<(u64, u64), ResizePhase> = HashMap::new();
        for event in lifecycle_events {
            if let Some(next_phase) = Self::lifecycle_stage_to_invariant_phase(event.stage) {
                let key = (event.pane_id, event.intent_seq);
                let prev_phase = last_phase_by_tx
                    .get(&key)
                    .copied()
                    .unwrap_or(ResizePhase::Idle);
                check_phase_transition(
                    &mut report,
                    Some(event.pane_id),
                    Some(event.intent_seq),
                    prev_phase,
                    next_phase,
                );
                last_phase_by_tx.insert(key, next_phase);
            }

            if matches!(event.stage, ResizeLifecycleStage::Committed) {
                check_scheduler_invariants(
                    &mut report,
                    event.pane_id,
                    Some(event.intent_seq),
                    event.latest_seq,
                    usize::from(event.pending_seq.is_some()),
                    true,
                );
            }
        }

        let mut telemetry = ResizeInvariantTelemetry::default();
        telemetry.absorb(&report);
        (report, telemetry)
    }

    fn collect_candidates(&self) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        for (pane_id, state) in &self.panes {
            let Some(intent) = state.pending.as_ref() else {
                continue;
            };
            if state.active_seq.is_some() {
                continue;
            }

            let forced_by_starvation = state.deferrals >= self.config.max_deferrals_before_force
                && matches!(intent.scheduler_class, ResizeWorkClass::Background);
            let force_bonus = if forced_by_starvation { 1_000 } else { 0 };
            let score = intent
                .scheduler_class
                .base_priority()
                .saturating_add(state.aging_credit)
                .saturating_add(force_bonus);

            candidates.push(Candidate {
                pane_id: *pane_id,
                score,
                forced_by_starvation,
                intent_seq: intent.intent_seq,
                submitted_at_ms: intent.submitted_at_ms,
                work_units: intent.normalized_work_units(),
                domain_key: intent.domain.key(),
                domain_weight: intent.domain.default_weight(),
                tab_id: intent.tab_id,
            });
        }
        candidates
    }

    fn resolve_resize_budget_with_input_guardrail(
        &self,
        budget_units: u32,
        pending_input_events: u32,
    ) -> (u32, u32) {
        if !self.config.input_guardrail_enabled {
            return (budget_units, 0);
        }
        if pending_input_events < self.config.input_backlog_threshold {
            return (budget_units, 0);
        }
        if budget_units <= 1 {
            return (budget_units, 0);
        }

        let reserve_units = self
            .config
            .input_reserve_units
            .max(1)
            .min(budget_units.saturating_sub(1));
        (budget_units.saturating_sub(reserve_units), reserve_units)
    }

    fn evict_oldest_background_pending(&mut self) -> Option<(u64, u64)> {
        let candidate = self
            .panes
            .iter()
            .filter_map(|(pane_id, state)| {
                let pending = state.pending.as_ref()?;
                if !matches!(pending.scheduler_class, ResizeWorkClass::Background) {
                    return None;
                }
                Some((*pane_id, pending.intent_seq, pending.submitted_at_ms))
            })
            .min_by_key(|(_, _, submitted_at_ms)| *submitted_at_ms)
            .map(|(pane_id, intent_seq, _)| (pane_id, intent_seq));

        let (pane_id, intent_seq) = candidate?;
        if let Some(state) = self.panes.get_mut(&pane_id) {
            state.pending = None;
            state.deferrals = 0;
            state.aging_credit = 0;
        }
        Some((pane_id, intent_seq))
    }

    fn drop_overdeferred_pending(&mut self) {
        if self.config.max_deferrals_before_drop == 0 {
            return;
        }

        let mut to_drop: Vec<(u64, u64)> = Vec::new();
        for (pane_id, state) in &self.panes {
            let Some(pending) = state.pending.as_ref() else {
                continue;
            };
            if state.deferrals >= self.config.max_deferrals_before_drop {
                to_drop.push((*pane_id, pending.intent_seq));
            }
        }

        for (pane_id, dropped_intent_seq) in to_drop {
            if let Some(state) = self.panes.get_mut(&pane_id) {
                state.pending = None;
                state.deferrals = 0;
                state.aging_credit = 0;
            }
            self.metrics.dropped_after_deferrals =
                self.metrics.dropped_after_deferrals.saturating_add(1);
            self.push_lifecycle_event(
                pane_id,
                dropped_intent_seq,
                None,
                ResizeLifecycleStage::Cancelled,
                ResizeLifecycleDetail::PendingDroppedOverload {
                    reason: ResizeOverloadReason::DeferralTimeout,
                    dropped_pane_id: pane_id,
                    dropped_intent_seq,
                },
            );
        }
    }

    fn apply_deferral_aging(&mut self, deferred_panes: &[u64]) {
        for pane_id in deferred_panes {
            let Some(state) = self.panes.get_mut(pane_id) else {
                continue;
            };
            let Some(intent) = state.pending.as_ref() else {
                continue;
            };

            state.deferrals = state.deferrals.saturating_add(1);
            let boost = match intent.scheduler_class {
                ResizeWorkClass::Interactive => self.config.aging_credit_per_frame / 2,
                ResizeWorkClass::Background => self.config.aging_credit_per_frame,
            };
            state.aging_credit = state
                .aging_credit
                .saturating_add(boost)
                .min(self.config.max_aging_credit);
        }
    }

    fn is_tab_in_storm(&self, tab_id: u64) -> bool {
        self.config.storm_window_ms > 0
            && self.config.storm_threshold_intents > 0
            && self
                .tab_submit_history
                .get(&tab_id)
                .is_some_and(|h| h.len() as u32 >= self.config.storm_threshold_intents)
    }

    fn push_lifecycle_event(
        &mut self,
        pane_id: u64,
        intent_seq: u64,
        observed_at_ms: Option<u64>,
        stage: ResizeLifecycleStage,
        detail: ResizeLifecycleDetail,
    ) {
        self.next_lifecycle_event_seq = self.next_lifecycle_event_seq.wrapping_add(1);
        let pane_state = self.panes.get(&pane_id);
        self.lifecycle_events
            .push_back(ResizeTransactionLifecycleEvent {
                event_seq: self.next_lifecycle_event_seq,
                frame_seq: self.metrics.frames,
                pane_id,
                intent_seq,
                observed_at_ms,
                latest_seq: pane_state.and_then(|s| s.latest_seq),
                pending_seq: pane_state.and_then(|s| s.pending.as_ref().map(|p| p.intent_seq)),
                active_seq: pane_state.and_then(|s| s.active_seq),
                stage,
                detail,
            });
        let max_events = self.config.max_lifecycle_events.max(1);
        while self.lifecycle_events.len() > max_events {
            let _ = self.lifecycle_events.pop_front();
        }
    }

    fn publish_debug_snapshot(&self) {
        ResizeSchedulerDebugSnapshot::update_global(
            self.debug_snapshot(self.config.max_lifecycle_events),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ResizeDomain, ResizeExecutionPhase, ResizeIntent, ResizeLifecycleDetail,
        ResizeLifecycleStage, ResizeScheduler, ResizeSchedulerConfig, ResizeSchedulerDebugSnapshot,
        ResizeWorkClass, SubmitOutcome,
    };
    use crate::resize_invariants::ResizeViolationKind;

    fn intent(
        pane_id: u64,
        intent_seq: u64,
        scheduler_class: ResizeWorkClass,
        work_units: u32,
        submitted_at_ms: u64,
    ) -> ResizeIntent {
        ResizeIntent {
            pane_id,
            intent_seq,
            scheduler_class,
            work_units,
            submitted_at_ms,
            domain: ResizeDomain::default(),
            tab_id: None,
        }
    }

    fn intent_with_tab(
        pane_id: u64,
        intent_seq: u64,
        scheduler_class: ResizeWorkClass,
        work_units: u32,
        submitted_at_ms: u64,
        tab_id: Option<u64>,
    ) -> ResizeIntent {
        ResizeIntent {
            pane_id,
            intent_seq,
            scheduler_class,
            work_units,
            submitted_at_ms,
            domain: ResizeDomain::default(),
            tab_id,
        }
    }

    fn intent_with_domain(
        pane_id: u64,
        intent_seq: u64,
        scheduler_class: ResizeWorkClass,
        work_units: u32,
        submitted_at_ms: u64,
        domain: ResizeDomain,
    ) -> ResizeIntent {
        ResizeIntent {
            pane_id,
            intent_seq,
            scheduler_class,
            work_units,
            submitted_at_ms,
            domain,
            tab_id: None,
        }
    }

    #[test]
    fn submit_rejects_non_monotonic_sequences() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

        let first = scheduler.submit_intent(intent(1, 10, ResizeWorkClass::Interactive, 2, 100));
        assert!(matches!(
            first,
            SubmitOutcome::Accepted {
                replaced_pending_seq: None
            }
        ));

        let duplicate =
            scheduler.submit_intent(intent(1, 10, ResizeWorkClass::Interactive, 2, 101));
        assert!(matches!(
            duplicate,
            SubmitOutcome::RejectedNonMonotonic { latest_seq: 10 }
        ));

        let older = scheduler.submit_intent(intent(1, 9, ResizeWorkClass::Interactive, 2, 102));
        assert!(matches!(
            older,
            SubmitOutcome::RejectedNonMonotonic { latest_seq: 10 }
        ));

        assert_eq!(scheduler.metrics().rejected_non_monotonic, 2);
    }

    #[test]
    fn coalesces_pending_to_latest_intent() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

        let _ = scheduler.submit_intent(intent(42, 1, ResizeWorkClass::Background, 2, 100));
        let second = scheduler.submit_intent(intent(42, 2, ResizeWorkClass::Background, 3, 101));

        assert!(matches!(
            second,
            SubmitOutcome::Accepted {
                replaced_pending_seq: Some(1)
            }
        ));
        assert_eq!(scheduler.pending_total(), 1);
        assert_eq!(scheduler.metrics().superseded_intents, 1);

        let snap = scheduler.snapshot();
        let pane = snap.panes.iter().find(|row| row.pane_id == 42).unwrap();
        assert_eq!(pane.pending_seq, Some(2));
        assert_eq!(pane.pending_class, Some(ResizeWorkClass::Background));
    }

    #[test]
    fn interactive_preempts_background_when_budget_tight() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 2,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 2, 100));
        let _ = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 2, 101));

        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 1);
        assert_eq!(frame.scheduled[0].pane_id, 2);
        assert_eq!(scheduler.pending_total(), 1);
        assert_eq!(
            scheduler
                .snapshot()
                .panes
                .iter()
                .find(|row| row.pane_id == 1)
                .unwrap()
                .deferrals,
            1
        );
    }

    #[test]
    fn single_flight_prevents_double_schedule_until_completion() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 4,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(7, 1, ResizeWorkClass::Interactive, 1, 100));
        let frame1 = scheduler.schedule_frame();
        assert_eq!(frame1.scheduled.len(), 1);
        assert_eq!(frame1.scheduled[0].intent_seq, 1);

        let _ = scheduler.submit_intent(intent(7, 2, ResizeWorkClass::Interactive, 1, 101));
        let frame2 = scheduler.schedule_frame();
        assert!(
            frame2.scheduled.is_empty(),
            "pane should remain single-flight"
        );

        // Active seq 1 is now superseded by pending seq 2 — cancel it rather
        // than complete, since complete_active rejects when latest_seq > active_seq.
        assert!(scheduler.active_is_superseded(7));
        assert!(scheduler.cancel_active_if_superseded(7));
        let frame3 = scheduler.schedule_frame();
        assert_eq!(frame3.scheduled.len(), 1);
        assert_eq!(frame3.scheduled[0].intent_seq, 2);
    }

    #[test]
    fn background_starvation_gets_forced_service() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 1,
            max_deferrals_before_force: 2,
            aging_credit_per_frame: 1,
            max_aging_credit: 10,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(10, 1, ResizeWorkClass::Background, 1, 100));
        let _ = scheduler.submit_intent(intent(11, 1, ResizeWorkClass::Interactive, 1, 101));

        let frame1 = scheduler.schedule_frame();
        assert_eq!(frame1.scheduled.len(), 1);
        assert_eq!(frame1.scheduled[0].pane_id, 11);
        assert!(scheduler.complete_active(11, 1));
        let _ = scheduler.submit_intent(intent(11, 2, ResizeWorkClass::Interactive, 1, 102));

        let frame2 = scheduler.schedule_frame();
        assert_eq!(frame2.scheduled.len(), 1);
        assert_eq!(frame2.scheduled[0].pane_id, 11);
        assert!(scheduler.complete_active(11, 2));
        let _ = scheduler.submit_intent(intent(11, 3, ResizeWorkClass::Interactive, 1, 103));

        let frame3 = scheduler.schedule_frame();
        assert_eq!(frame3.scheduled.len(), 1);
        assert_eq!(
            frame3.scheduled[0].pane_id, 10,
            "background pane should be forced after repeated deferrals"
        );
        assert!(frame3.scheduled[0].forced_by_starvation);
        assert_eq!(scheduler.metrics().forced_background_runs, 1);
    }

    #[test]
    fn forced_background_can_run_over_budget_once() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 1,
            max_deferrals_before_force: 0,
            aging_credit_per_frame: 1,
            max_aging_credit: 10,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 3, 100));
        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 1);
        assert!(frame.scheduled[0].over_budget);
        assert!(frame.scheduled[0].forced_by_starvation);
        assert_eq!(scheduler.metrics().over_budget_runs, 1);
        assert_eq!(scheduler.metrics().forced_background_runs, 1);
    }

    #[test]
    fn stale_active_can_be_cancelled_at_boundary() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

        let _ = scheduler.submit_intent(intent(77, 1, ResizeWorkClass::Interactive, 1, 100));
        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 1);
        assert_eq!(frame.scheduled[0].intent_seq, 1);

        let _ = scheduler.submit_intent(intent(77, 2, ResizeWorkClass::Interactive, 1, 101));
        assert!(scheduler.active_is_superseded(77));
        assert!(scheduler.cancel_active_if_superseded(77));
        assert!(!scheduler.active_is_superseded(77));

        let frame2 = scheduler.schedule_frame();
        assert_eq!(frame2.scheduled.len(), 1);
        assert_eq!(frame2.scheduled[0].intent_seq, 2);
    }

    #[test]
    fn stale_active_completion_is_rejected_when_newer_intent_exists() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

        let _ = scheduler.submit_intent(intent(81, 1, ResizeWorkClass::Interactive, 1, 100));
        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 1);
        assert_eq!(frame.scheduled[0].intent_seq, 1);

        let _ = scheduler.submit_intent(intent(81, 2, ResizeWorkClass::Interactive, 1, 101));
        assert!(scheduler.active_is_superseded(81));
        assert!(
            !scheduler.complete_active(81, 1),
            "stale completion should be rejected when latest_seq is newer"
        );
        assert_eq!(scheduler.metrics().completion_rejected, 1);

        assert!(scheduler.cancel_active_if_superseded(81));
        let frame2 = scheduler.schedule_frame();
        assert_eq!(frame2.scheduled.len(), 1);
        assert_eq!(frame2.scheduled[0].intent_seq, 2);
        assert!(scheduler.complete_active(81, 2));
    }

    #[test]
    fn lifecycle_events_capture_submit_schedule_cancel_and_commit() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

        let _ = scheduler.submit_intent(intent(9, 1, ResizeWorkClass::Interactive, 1, 100));
        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 1);
        assert_eq!(frame.scheduled[0].intent_seq, 1);

        let _ = scheduler.submit_intent(intent(9, 2, ResizeWorkClass::Interactive, 1, 101));
        assert!(scheduler.cancel_active_if_superseded(9));
        let frame2 = scheduler.schedule_frame();
        assert_eq!(frame2.scheduled.len(), 1);
        assert_eq!(frame2.scheduled[0].intent_seq, 2);
        assert!(scheduler.complete_active(9, 2));

        let events = scheduler.lifecycle_events(0);
        assert!(
            events.len() >= 6,
            "expected at least six lifecycle events, got {}",
            events.len()
        );
        let last = events.last().expect("events should not be empty");
        assert_eq!(last.pane_id, 9);
        assert_eq!(last.intent_seq, 2);
        assert_eq!(last.stage, ResizeLifecycleStage::Committed);
        assert!(matches!(
            last.detail,
            ResizeLifecycleDetail::ActiveCompleted
        ));

        assert_eq!(scheduler.metrics().cancelled_active, 1);
        assert_eq!(scheduler.metrics().completed_active, 1);
    }

    #[test]
    fn debug_snapshot_respects_lifecycle_event_limit() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            max_lifecycle_events: 3,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
        let _ = scheduler.submit_intent(intent(1, 2, ResizeWorkClass::Interactive, 1, 101));
        let _ = scheduler.submit_intent(intent(1, 3, ResizeWorkClass::Interactive, 1, 102));
        let _ = scheduler.schedule_frame();

        let snapshot = scheduler.debug_snapshot(0);
        assert_eq!(snapshot.lifecycle_events.len(), 3);
        assert_eq!(snapshot.lifecycle_events[0].intent_seq, 3);
        assert_eq!(snapshot.lifecycle_events[2].intent_seq, 3);
    }

    #[test]
    fn debug_snapshot_global_store_roundtrip() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let _ = scheduler.submit_intent(intent(77, 1, ResizeWorkClass::Background, 2, 10));
        let _ = scheduler.schedule_frame();

        let expected = scheduler.debug_snapshot(16);
        let mut matched = false;
        for _ in 0..8 {
            ResizeSchedulerDebugSnapshot::update_global(expected.clone());
            let stored =
                ResizeSchedulerDebugSnapshot::get_global().expect("global snapshot should be set");
            if stored
                .scheduler
                .panes
                .iter()
                .any(|pane| pane.pane_id == 77 && pane.active_seq == Some(1))
            {
                matched = true;
                break;
            }
        }
        assert!(
            matched,
            "global snapshot should include sentinel pane update"
        );
    }

    #[test]
    fn debug_snapshot_invariants_clean_for_nominal_transaction() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let _ = scheduler.submit_intent(intent(42, 1, ResizeWorkClass::Interactive, 1, 1_000));
        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 1);
        assert!(scheduler.mark_active_phase(42, 1, ResizeExecutionPhase::Reflowing, 1_010));
        assert!(scheduler.mark_active_phase(42, 1, ResizeExecutionPhase::Presenting, 1_015));
        assert!(scheduler.complete_active(42, 1));

        let snap = scheduler.debug_snapshot(64);
        assert!(
            snap.invariants.is_clean(),
            "expected no invariant violations, got {:?}",
            snap.invariants.violations
        );
        assert_eq!(snap.invariant_telemetry.critical_count, 0);
        assert_eq!(snap.invariant_telemetry.error_count, 0);
    }

    #[test]
    fn debug_snapshot_invariants_detect_pending_active_sequence_inversion() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let _ = scheduler.submit_intent(intent(9, 5, ResizeWorkClass::Interactive, 1, 100));

        let state = scheduler
            .panes
            .get_mut(&9)
            .expect("pane state should exist after submit");
        state.latest_seq = Some(5);
        state.active_seq = Some(5);
        state.active_phase = Some(ResizeExecutionPhase::Preparing);
        state.pending = Some(intent(9, 4, ResizeWorkClass::Background, 1, 101));

        let snap = scheduler.debug_snapshot(16);
        assert!(
            !snap.invariants.is_clean(),
            "expected invariant violations for inverted pending/active seq"
        );
        assert!(snap.invariants.violations.iter().any(|violation| {
            matches!(
                violation.kind,
                ResizeViolationKind::ConcurrentPaneTransaction
                    | ResizeViolationKind::IntentSequenceRegression
            )
        }));
        assert!(snap.invariant_telemetry.total_failures > 0);
    }

    #[test]
    fn active_phase_transitions_are_recorded_with_explicit_labels() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let _ = scheduler.submit_intent(intent(5, 1, ResizeWorkClass::Interactive, 1, 1_000));
        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 1);
        assert!(scheduler.mark_active_phase(5, 1, ResizeExecutionPhase::Reflowing, 1_010));
        assert!(scheduler.mark_active_phase(5, 1, ResizeExecutionPhase::Presenting, 1_020));

        let snap = scheduler.snapshot();
        let pane = snap.panes.iter().find(|row| row.pane_id == 5).unwrap();
        assert_eq!(pane.active_phase, Some(ResizeExecutionPhase::Presenting));
        assert_eq!(pane.active_phase_started_at_ms, Some(1_020));

        let events = scheduler.lifecycle_events(0);
        assert!(events.iter().any(|event| {
            event.stage == ResizeLifecycleStage::Reflowing
                && matches!(
                    event.detail,
                    ResizeLifecycleDetail::ActivePhaseTransition {
                        phase: ResizeExecutionPhase::Reflowing
                    }
                )
        }));
        assert!(events.iter().any(|event| {
            event.stage == ResizeLifecycleStage::Presenting
                && matches!(
                    event.detail,
                    ResizeLifecycleDetail::ActivePhaseTransition {
                        phase: ResizeExecutionPhase::Presenting
                    }
                )
        }));
    }

    #[test]
    fn stalled_transaction_heuristic_reports_old_active_phase() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let _ = scheduler.submit_intent(intent(11, 3, ResizeWorkClass::Background, 2, 5_000));
        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 1);
        assert!(scheduler.mark_active_phase(11, 3, ResizeExecutionPhase::Reflowing, 5_100));

        let debug = scheduler.debug_snapshot(64);
        let stalled = debug.stalled_transactions(8_200, 2_000);
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].pane_id, 11);
        assert_eq!(stalled[0].intent_seq, 3);
        assert_eq!(
            stalled[0].active_phase,
            Some(ResizeExecutionPhase::Reflowing)
        );
        assert_eq!(stalled[0].age_ms, 3_100);
    }

    #[test]
    fn overload_rejects_background_when_pending_cap_is_reached() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            max_pending_panes: 1,
            ..ResizeSchedulerConfig::default()
        });

        let first = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 1, 100));
        assert!(matches!(
            first,
            SubmitOutcome::Accepted {
                replaced_pending_seq: None
            }
        ));

        let second = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Background, 1, 101));
        assert!(matches!(
            second,
            SubmitOutcome::DroppedOverload {
                pending_total: 1,
                evicted_pending: None
            }
        ));
        assert_eq!(scheduler.metrics().overload_rejected, 1);
    }

    #[test]
    fn overload_allows_interactive_by_evicting_oldest_background_pending() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            max_pending_panes: 1,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 1, 100));
        let interactive =
            scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 1, 101));
        assert!(matches!(
            interactive,
            SubmitOutcome::Accepted {
                replaced_pending_seq: None
            }
        ));
        assert_eq!(scheduler.metrics().overload_evicted, 1);

        let snapshot = scheduler.snapshot();
        let pane1 = snapshot.panes.iter().find(|row| row.pane_id == 1).unwrap();
        let pane2 = snapshot.panes.iter().find(|row| row.pane_id == 2).unwrap();
        assert_eq!(pane1.pending_seq, None);
        assert_eq!(pane2.pending_seq, Some(1));
    }

    #[test]
    fn pending_is_dropped_after_excessive_deferrals() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 1,
            max_deferrals_before_force: 100,
            max_deferrals_before_drop: 2,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        // Background pane 1 is always deferred by interactive pane 2.
        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 1, 100));
        let _ = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 1, 101));
        let frame1 = scheduler.schedule_frame();
        assert_eq!(frame1.scheduled.len(), 1);
        assert_eq!(frame1.scheduled[0].pane_id, 2);
        assert!(scheduler.complete_active(2, 1));

        let _ = scheduler.submit_intent(intent(2, 2, ResizeWorkClass::Interactive, 1, 102));
        let frame2 = scheduler.schedule_frame();
        assert_eq!(frame2.scheduled.len(), 1);
        assert_eq!(frame2.scheduled[0].pane_id, 2);
        assert!(scheduler.complete_active(2, 2));

        // Third frame triggers drop before scheduling due to deferral threshold.
        let _ = scheduler.submit_intent(intent(2, 3, ResizeWorkClass::Interactive, 1, 103));
        let _ = scheduler.schedule_frame();

        let pane1 = scheduler
            .snapshot()
            .panes
            .iter()
            .find(|row| row.pane_id == 1)
            .unwrap()
            .pending_seq;
        assert_eq!(pane1, None);
        assert_eq!(scheduler.metrics().dropped_after_deferrals, 1);
    }

    #[test]
    fn kill_switch_suppresses_submit_and_schedule_with_legacy_hint() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            emergency_disable: true,
            legacy_fallback_enabled: true,
            ..ResizeSchedulerConfig::default()
        });

        let submit = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
        assert!(matches!(
            submit,
            SubmitOutcome::SuppressedByKillSwitch {
                legacy_fallback: true
            }
        ));
        let frame = scheduler.schedule_frame();
        assert!(frame.scheduled.is_empty());
        assert_eq!(scheduler.metrics().suppressed_by_gate, 1);
        assert_eq!(scheduler.metrics().suppressed_frames, 1);

        let debug = scheduler.debug_snapshot(8);
        assert!(!debug.gate.active);
        assert!(debug.gate.emergency_disable);
        assert!(debug.gate.legacy_fallback_enabled);
    }

    #[test]
    fn gate_toggle_reenables_control_plane_path() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            control_plane_enabled: false,
            emergency_disable: false,
            ..ResizeSchedulerConfig::default()
        });

        let submit = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 1, 100));
        assert!(matches!(
            submit,
            SubmitOutcome::SuppressedByKillSwitch { .. }
        ));

        scheduler.set_control_plane_enabled(true);
        let submit2 = scheduler.submit_intent(intent(2, 2, ResizeWorkClass::Interactive, 1, 101));
        assert!(matches!(submit2, SubmitOutcome::Accepted { .. }));
    }

    #[test]
    fn input_guardrail_reserves_budget_and_defers_resize_work() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 4,
            input_guardrail_enabled: true,
            input_backlog_threshold: 1,
            input_reserve_units: 2,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 4, 100));
        let frame = scheduler.schedule_frame_with_input_backlog(4, 3);
        assert!(frame.scheduled.is_empty());
        assert_eq!(frame.frame_budget_units, 4);
        assert_eq!(frame.effective_resize_budget_units, 2);
        assert_eq!(frame.input_reserved_units, 2);
        assert_eq!(frame.pending_input_events, 3);
        assert_eq!(scheduler.pending_total(), 1);
        assert_eq!(scheduler.metrics().input_guardrail_frames, 1);
        assert_eq!(scheduler.metrics().input_guardrail_deferrals, 1);
        assert_eq!(scheduler.metrics().last_effective_resize_budget_units, 2);
        assert_eq!(scheduler.metrics().last_input_backlog, 3);
    }

    #[test]
    fn input_guardrail_is_inactive_without_backlog() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 4,
            input_guardrail_enabled: true,
            input_backlog_threshold: 1,
            input_reserve_units: 2,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 4, 100));
        let frame = scheduler.schedule_frame_with_input_backlog(4, 0);
        assert_eq!(frame.scheduled.len(), 1);
        assert_eq!(frame.effective_resize_budget_units, 4);
        assert_eq!(frame.input_reserved_units, 0);
        assert_eq!(scheduler.metrics().input_guardrail_frames, 0);
        assert_eq!(scheduler.metrics().input_guardrail_deferrals, 0);
        assert_eq!(scheduler.metrics().last_effective_resize_budget_units, 4);
        assert_eq!(scheduler.metrics().last_input_backlog, 0);
    }

    #[test]
    fn input_guardrail_disables_oversubscription_when_backlog_is_present() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 2,
            input_guardrail_enabled: true,
            input_backlog_threshold: 1,
            input_reserve_units: 1,
            allow_single_oversubscription: true,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 2, 100));
        let frame = scheduler.schedule_frame_with_input_backlog(2, 1);
        assert!(frame.scheduled.is_empty());
        assert_eq!(frame.effective_resize_budget_units, 1);
        assert_eq!(frame.input_reserved_units, 1);
        assert_eq!(scheduler.metrics().input_guardrail_deferrals, 1);
    }

    #[test]
    fn storm_detection_throttles_per_tab_picks() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 20,
            storm_window_ms: 100,
            storm_threshold_intents: 3,
            max_storm_picks_per_tab: 1,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        // 4 panes in tab 1, all submit within storm window -> triggers storm.
        let _ = scheduler.submit_intent(intent_with_tab(
            1,
            1,
            ResizeWorkClass::Interactive,
            1,
            100,
            Some(1),
        ));
        let _ = scheduler.submit_intent(intent_with_tab(
            2,
            1,
            ResizeWorkClass::Interactive,
            1,
            110,
            Some(1),
        ));
        let _ = scheduler.submit_intent(intent_with_tab(
            3,
            1,
            ResizeWorkClass::Interactive,
            1,
            120,
            Some(1),
        ));
        let _ = scheduler.submit_intent(intent_with_tab(
            4,
            1,
            ResizeWorkClass::Interactive,
            1,
            130,
            Some(1),
        ));
        // 1 pane in tab 2 (no storm).
        let _ = scheduler.submit_intent(intent_with_tab(
            5,
            1,
            ResizeWorkClass::Interactive,
            1,
            140,
            Some(2),
        ));

        let frame = scheduler.schedule_frame();
        let tab1_picks = frame.scheduled.iter().filter(|s| s.pane_id <= 4).count();
        let tab2_picks = frame.scheduled.iter().filter(|s| s.pane_id == 5).count();
        assert_eq!(tab1_picks, 1, "storm should throttle tab 1 to 1 pick");
        assert_eq!(tab2_picks, 1, "tab 2 should not be throttled");
        assert!(scheduler.metrics().storm_events_detected > 0);
        assert!(scheduler.metrics().storm_picks_throttled > 0);
    }

    #[test]
    fn storm_detection_inactive_below_threshold() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 20,
            storm_window_ms: 100,
            storm_threshold_intents: 5,
            max_storm_picks_per_tab: 1,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        // Only 3 panes in tab 1 (threshold is 5), so no storm.
        let _ = scheduler.submit_intent(intent_with_tab(
            1,
            1,
            ResizeWorkClass::Interactive,
            1,
            100,
            Some(1),
        ));
        let _ = scheduler.submit_intent(intent_with_tab(
            2,
            1,
            ResizeWorkClass::Interactive,
            1,
            110,
            Some(1),
        ));
        let _ = scheduler.submit_intent(intent_with_tab(
            3,
            1,
            ResizeWorkClass::Interactive,
            1,
            120,
            Some(1),
        ));

        let frame = scheduler.schedule_frame();
        assert_eq!(
            frame.scheduled.len(),
            3,
            "all picks should be served without storm"
        );
        assert_eq!(scheduler.metrics().storm_events_detected, 0);
        assert_eq!(scheduler.metrics().storm_picks_throttled, 0);
    }

    #[test]
    fn storm_detection_disabled_when_window_is_zero() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 20,
            storm_window_ms: 0,
            storm_threshold_intents: 1,
            max_storm_picks_per_tab: 1,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        // Many panes in same tab, but storm disabled via window=0.
        for pane in 1..=5 {
            let _ = scheduler.submit_intent(intent_with_tab(
                pane,
                1,
                ResizeWorkClass::Interactive,
                1,
                100,
                Some(1),
            ));
        }

        let frame = scheduler.schedule_frame();
        assert_eq!(
            frame.scheduled.len(),
            5,
            "storm disabled, all should schedule"
        );
        assert_eq!(scheduler.metrics().storm_events_detected, 0);
        assert_eq!(scheduler.metrics().storm_picks_throttled, 0);
    }

    #[test]
    fn domain_budget_partitions_fairly() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 6,
            domain_budget_enabled: true,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        // 3 local panes (weight 4), 3 ssh panes (weight 2).
        // Total weight = 4 + 2 = 6.
        // Local budget share: 6 * 4/6 = 4 units.
        // SSH budget share: 6 * 2/6 = 2 units.
        let _ = scheduler.submit_intent(intent_with_domain(
            1,
            1,
            ResizeWorkClass::Interactive,
            2,
            100,
            ResizeDomain::Local,
        ));
        let _ = scheduler.submit_intent(intent_with_domain(
            2,
            1,
            ResizeWorkClass::Interactive,
            2,
            101,
            ResizeDomain::Local,
        ));
        let _ = scheduler.submit_intent(intent_with_domain(
            3,
            1,
            ResizeWorkClass::Interactive,
            2,
            102,
            ResizeDomain::Local,
        ));
        let _ = scheduler.submit_intent(intent_with_domain(
            4,
            1,
            ResizeWorkClass::Interactive,
            2,
            103,
            ResizeDomain::Ssh {
                host: "remote".into(),
            },
        ));
        let _ = scheduler.submit_intent(intent_with_domain(
            5,
            1,
            ResizeWorkClass::Interactive,
            2,
            104,
            ResizeDomain::Ssh {
                host: "remote".into(),
            },
        ));
        let _ = scheduler.submit_intent(intent_with_domain(
            6,
            1,
            ResizeWorkClass::Interactive,
            2,
            105,
            ResizeDomain::Ssh {
                host: "remote".into(),
            },
        ));

        let frame = scheduler.schedule_frame();
        let local_picks = frame.scheduled.iter().filter(|s| s.pane_id <= 3).count();
        let ssh_picks = frame.scheduled.iter().filter(|s| s.pane_id > 3).count();
        assert!(
            local_picks <= 2,
            "local should be capped at ~4 units (2 picks of 2): got {local_picks}"
        );
        assert!(
            ssh_picks <= 1,
            "ssh should be capped at ~2 units (1 pick of 2): got {ssh_picks}"
        );
        assert!(scheduler.metrics().domain_budget_throttled > 0);
    }

    #[test]
    fn domain_budget_disabled_by_default() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 6,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        // Mixed domains, but domain_budget_enabled is false (default).
        let _ = scheduler.submit_intent(intent_with_domain(
            1,
            1,
            ResizeWorkClass::Interactive,
            2,
            100,
            ResizeDomain::Local,
        ));
        let _ = scheduler.submit_intent(intent_with_domain(
            2,
            1,
            ResizeWorkClass::Interactive,
            2,
            101,
            ResizeDomain::Local,
        ));
        let _ = scheduler.submit_intent(intent_with_domain(
            3,
            1,
            ResizeWorkClass::Interactive,
            2,
            102,
            ResizeDomain::Local,
        ));

        let frame = scheduler.schedule_frame();
        assert_eq!(
            frame.scheduled.len(),
            3,
            "no domain budget: all local panes scheduled"
        );
        assert_eq!(scheduler.metrics().domain_budget_throttled, 0);
    }

    #[test]
    fn forced_starvation_bypasses_domain_throttle() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 4,
            domain_budget_enabled: true,
            max_deferrals_before_force: 0,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        // Local interactive pane (weight 4) and SSH background pane (weight 2).
        // With budget=4: local share = 4*4/6 = 2, ssh share = 4*2/6 = 1.
        // SSH pane needs 2 units but SSH budget is only 1.
        // However, starvation forcing should bypass domain budget check.
        let _ = scheduler.submit_intent(intent_with_domain(
            1,
            1,
            ResizeWorkClass::Interactive,
            2,
            100,
            ResizeDomain::Local,
        ));
        let _ = scheduler.submit_intent(intent_with_domain(
            2,
            1,
            ResizeWorkClass::Background,
            2,
            101,
            ResizeDomain::Ssh {
                host: "slow".into(),
            },
        ));

        let frame = scheduler.schedule_frame();
        let bg_pick = frame.scheduled.iter().find(|s| s.pane_id == 2);
        assert!(
            bg_pick.is_some(),
            "starvation-forced background should be scheduled"
        );
        assert!(bg_pick.unwrap().forced_by_starvation);
    }

    // -----------------------------------------------------------------------
    // Config, type helpers, and defaults
    // -----------------------------------------------------------------------

    #[test]
    fn config_default_values() {
        let cfg = ResizeSchedulerConfig::default();
        assert!(cfg.control_plane_enabled);
        assert!(!cfg.emergency_disable);
        assert!(cfg.legacy_fallback_enabled);
        assert_eq!(cfg.frame_budget_units, 8);
        assert!(cfg.input_guardrail_enabled);
        assert_eq!(cfg.input_backlog_threshold, 1);
        assert_eq!(cfg.input_reserve_units, 2);
        assert_eq!(cfg.max_deferrals_before_force, 3);
        assert_eq!(cfg.aging_credit_per_frame, 5);
        assert_eq!(cfg.max_aging_credit, 80);
        assert!(cfg.allow_single_oversubscription);
        assert_eq!(cfg.max_pending_panes, 128);
        assert_eq!(cfg.max_deferrals_before_drop, 12);
        assert_eq!(cfg.max_lifecycle_events, 256);
        assert_eq!(cfg.storm_window_ms, 50);
        assert_eq!(cfg.storm_threshold_intents, 4);
        assert_eq!(cfg.max_storm_picks_per_tab, 2);
        assert!(!cfg.domain_budget_enabled);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = ResizeSchedulerConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: ResizeSchedulerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn work_class_base_priority_interactive_higher_than_background() {
        assert!(
            ResizeWorkClass::Interactive.base_priority()
                > ResizeWorkClass::Background.base_priority()
        );
    }

    #[test]
    fn work_class_serde_roundtrip() {
        for class in [ResizeWorkClass::Interactive, ResizeWorkClass::Background] {
            let json = serde_json::to_string(&class).unwrap();
            let parsed: ResizeWorkClass = serde_json::from_str(&json).unwrap();
            assert_eq!(class, parsed);
        }
    }

    #[test]
    fn domain_key_formats() {
        assert_eq!(ResizeDomain::Local.key(), "local");
        assert_eq!(
            ResizeDomain::Ssh {
                host: "box1".into()
            }
            .key(),
            "ssh:box1"
        );
        assert_eq!(
            ResizeDomain::Mux {
                endpoint: "ep1".into()
            }
            .key(),
            "mux:ep1"
        );
    }

    #[test]
    fn domain_default_is_local() {
        assert_eq!(ResizeDomain::default(), ResizeDomain::Local);
    }

    #[test]
    fn domain_serde_roundtrip() {
        for domain in [
            ResizeDomain::Local,
            ResizeDomain::Ssh { host: "h1".into() },
            ResizeDomain::Mux {
                endpoint: "e1".into(),
            },
        ] {
            let json = serde_json::to_string(&domain).unwrap();
            let parsed: ResizeDomain = serde_json::from_str(&json).unwrap();
            assert_eq!(domain, parsed);
        }
    }

    #[test]
    fn intent_zero_work_units_normalized_to_one() {
        let i = intent(1, 1, ResizeWorkClass::Interactive, 0, 100);
        assert_eq!(i.normalized_work_units(), 1);
    }

    #[test]
    fn intent_nonzero_work_units_unchanged() {
        let i = intent(1, 1, ResizeWorkClass::Interactive, 5, 100);
        assert_eq!(i.normalized_work_units(), 5);
    }

    #[test]
    fn metrics_default_is_all_zero() {
        let m = super::ResizeSchedulerMetrics::default();
        assert_eq!(m.frames, 0);
        assert_eq!(m.superseded_intents, 0);
        assert_eq!(m.rejected_non_monotonic, 0);
        assert_eq!(m.forced_background_runs, 0);
        assert_eq!(m.over_budget_runs, 0);
        assert_eq!(m.overload_rejected, 0);
        assert_eq!(m.storm_events_detected, 0);
        assert_eq!(m.domain_budget_throttled, 0);
    }

    #[test]
    fn schedule_frame_result_default_is_empty() {
        let r = super::ScheduleFrameResult::default();
        assert!(r.scheduled.is_empty());
        assert_eq!(r.budget_spent_units, 0);
        assert_eq!(r.pending_after, 0);
    }

    // -----------------------------------------------------------------------
    // Scheduling edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn schedule_frame_with_no_pending_work() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let frame = scheduler.schedule_frame();
        assert!(frame.scheduled.is_empty());
        assert_eq!(frame.budget_spent_units, 0);
        assert_eq!(frame.pending_after, 0);
        assert_eq!(scheduler.metrics().frames, 1);
    }

    #[test]
    fn schedule_multiple_panes_all_fit_in_budget() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 10,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 2, 100));
        let _ = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 3, 101));
        let _ = scheduler.submit_intent(intent(3, 1, ResizeWorkClass::Interactive, 4, 102));

        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 3);
        assert_eq!(frame.budget_spent_units, 9); // 2+3+4
        assert_eq!(frame.pending_after, 0);
    }

    #[test]
    fn oversubscription_allowed_for_first_pick_only() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 1,
            allow_single_oversubscription: true,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 5, 100));
        let frame = scheduler.schedule_frame();
        assert_eq!(frame.scheduled.len(), 1);
        assert!(frame.scheduled[0].over_budget);
        assert_eq!(frame.budget_spent_units, 5);
    }

    #[test]
    fn oversubscription_disabled_prevents_over_budget_first_pick() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 1,
            allow_single_oversubscription: false,
            max_deferrals_before_force: 100, // prevent forced service
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 5, 100));
        let frame = scheduler.schedule_frame();
        assert!(frame.scheduled.is_empty());
        assert_eq!(frame.pending_after, 1);
    }

    // -----------------------------------------------------------------------
    // Lifecycle/phase edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn mark_active_phase_wrong_intent_seq_returns_false() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
        let _ = scheduler.schedule_frame();

        // Active seq is 1, try marking phase with seq 99
        assert!(!scheduler.mark_active_phase(1, 99, ResizeExecutionPhase::Reflowing, 200));
    }

    #[test]
    fn mark_active_phase_nonexistent_pane_returns_false() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        assert!(!scheduler.mark_active_phase(999, 1, ResizeExecutionPhase::Reflowing, 100));
    }

    #[test]
    fn complete_active_wrong_seq_returns_false() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
        let _ = scheduler.schedule_frame();
        assert!(!scheduler.complete_active(1, 99));
        assert_eq!(scheduler.metrics().completion_rejected, 1);
    }

    #[test]
    fn complete_active_nonexistent_pane_returns_false() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        assert!(!scheduler.complete_active(999, 1));
    }

    #[test]
    fn active_is_superseded_nonexistent_pane_returns_false() {
        let scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        assert!(!scheduler.active_is_superseded(999));
    }

    #[test]
    fn cancel_active_if_superseded_nonexistent_pane_returns_false() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        assert!(!scheduler.cancel_active_if_superseded(999));
    }

    // -----------------------------------------------------------------------
    // Aging credit accumulation
    // -----------------------------------------------------------------------

    #[test]
    fn aging_credit_accumulates_on_deferred_panes() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 1,
            aging_credit_per_frame: 10,
            max_aging_credit: 80,
            allow_single_oversubscription: false,
            max_deferrals_before_force: 100, // prevent forced service
            ..ResizeSchedulerConfig::default()
        });

        // Submit background pane (can't fit budget=1 with work_units=5)
        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 5, 100));
        // Submit interactive pane that fits budget
        let _ = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 1, 101));

        let _ = scheduler.schedule_frame(); // pane 2 scheduled, pane 1 deferred
        assert!(scheduler.complete_active(2, 1));

        let snap = scheduler.snapshot();
        let pane1 = snap.panes.iter().find(|r| r.pane_id == 1).unwrap();
        assert_eq!(pane1.deferrals, 1);
        assert_eq!(pane1.aging_credit, 10); // Background gets full aging_credit_per_frame
    }

    #[test]
    fn aging_credit_capped_at_max() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 1,
            aging_credit_per_frame: 50,
            max_aging_credit: 80,
            allow_single_oversubscription: false,
            max_deferrals_before_force: 100,
            ..ResizeSchedulerConfig::default()
        });

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 5, 100));

        // Schedule 3 empty frames (no other work, but pane 1 won't fit)
        // Actually need another pane to take the slot each time
        for seq in 1..=3 {
            let _ =
                scheduler.submit_intent(intent(2, seq, ResizeWorkClass::Interactive, 1, 100 + seq));
            let _ = scheduler.schedule_frame();
            assert!(scheduler.complete_active(2, seq));
        }

        let snap = scheduler.snapshot();
        let pane1 = snap.panes.iter().find(|r| r.pane_id == 1).unwrap();
        assert!(pane1.aging_credit <= 80, "should cap at max_aging_credit");
    }

    // -----------------------------------------------------------------------
    // Lifecycle events and snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn lifecycle_events_with_limit_returns_most_recent() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

        // Submit several intents to generate lifecycle events
        for seq in 1..=5 {
            let _ =
                scheduler.submit_intent(intent(1, seq, ResizeWorkClass::Interactive, 1, seq * 100));
        }

        let limited = scheduler.lifecycle_events(2);
        assert_eq!(limited.len(), 2);
        // Should be the most recent events
        let all = scheduler.lifecycle_events(0);
        assert_eq!(limited[0], all[all.len() - 2]);
        assert_eq!(limited[1], all[all.len() - 1]);
    }

    #[test]
    fn snapshot_panes_sorted_by_id() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

        // Submit in reverse order
        let _ = scheduler.submit_intent(intent(100, 1, ResizeWorkClass::Interactive, 1, 100));
        let _ = scheduler.submit_intent(intent(50, 1, ResizeWorkClass::Interactive, 1, 101));
        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 102));

        let snap = scheduler.snapshot();
        let ids: Vec<u64> = snap.panes.iter().map(|p| p.pane_id).collect();
        assert_eq!(ids, vec![1, 50, 100]);
    }

    #[test]
    fn pending_and_active_totals_accurate() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 2,
            ..ResizeSchedulerConfig::default()
        });

        assert_eq!(scheduler.pending_total(), 0);
        assert_eq!(scheduler.active_total(), 0);

        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
        let _ = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 1, 101));
        assert_eq!(scheduler.pending_total(), 2);
        assert_eq!(scheduler.active_total(), 0);

        let _ = scheduler.schedule_frame();
        assert_eq!(scheduler.pending_total(), 0);
        assert_eq!(scheduler.active_total(), 2);

        assert!(scheduler.complete_active(1, 1));
        assert_eq!(scheduler.active_total(), 1);
        assert!(scheduler.complete_active(2, 1));
        assert_eq!(scheduler.active_total(), 0);
    }

    #[test]
    fn gate_state_reflects_config() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let gate = scheduler.gate_state();
        assert!(gate.control_plane_enabled);
        assert!(!gate.emergency_disable);
        assert!(gate.active);

        scheduler.set_emergency_disable(true);
        let gate = scheduler.gate_state();
        assert!(!gate.active);
        assert!(gate.emergency_disable);

        scheduler.set_emergency_disable(false);
        scheduler.set_control_plane_enabled(false);
        let gate = scheduler.gate_state();
        assert!(!gate.active);
        assert!(!gate.control_plane_enabled);
    }

    #[test]
    fn stalled_transactions_below_threshold_returns_empty() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
        let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 5_000));
        let _ = scheduler.schedule_frame();
        assert!(scheduler.mark_active_phase(1, 1, ResizeExecutionPhase::Reflowing, 5_100));

        let debug = scheduler.debug_snapshot(64);
        // Transaction age is 5200-5100=100ms, threshold is 2000ms
        let stalled = debug.stalled_transactions(5_200, 2_000);
        assert!(stalled.is_empty());
    }

    #[test]
    fn execution_phase_lifecycle_stage_mapping() {
        assert_eq!(
            ResizeExecutionPhase::Preparing.lifecycle_stage(),
            ResizeLifecycleStage::Preparing
        );
        assert_eq!(
            ResizeExecutionPhase::Reflowing.lifecycle_stage(),
            ResizeLifecycleStage::Reflowing
        );
        assert_eq!(
            ResizeExecutionPhase::Presenting.lifecycle_stage(),
            ResizeLifecycleStage::Presenting
        );
    }

    #[test]
    fn storm_window_prunes_old_submissions() {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 20,
            storm_window_ms: 50,
            storm_threshold_intents: 3,
            max_storm_picks_per_tab: 1,
            allow_single_oversubscription: false,
            ..ResizeSchedulerConfig::default()
        });

        // Submit 2 intents early, then 2 late (outside storm window from the first).
        let _ = scheduler.submit_intent(intent_with_tab(
            1,
            1,
            ResizeWorkClass::Interactive,
            1,
            100,
            Some(1),
        ));
        let _ = scheduler.submit_intent(intent_with_tab(
            2,
            1,
            ResizeWorkClass::Interactive,
            1,
            110,
            Some(1),
        ));
        // These arrive 60ms later; the first two should be pruned from the window.
        let _ = scheduler.submit_intent(intent_with_tab(
            3,
            1,
            ResizeWorkClass::Interactive,
            1,
            160,
            Some(1),
        ));
        let _ = scheduler.submit_intent(intent_with_tab(
            4,
            1,
            ResizeWorkClass::Interactive,
            1,
            170,
            Some(1),
        ));

        let frame = scheduler.schedule_frame();
        // Only 2 entries remain in the 50ms window (160, 170), below threshold of 3.
        assert_eq!(
            frame.scheduled.len(),
            4,
            "no storm: old submissions pruned from window"
        );
        assert_eq!(scheduler.metrics().storm_picks_throttled, 0);
    }
}
