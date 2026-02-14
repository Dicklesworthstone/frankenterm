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
//! - observability via scheduler metrics and snapshots

use std::collections::{HashMap, VecDeque};
use std::sync::{OnceLock, RwLock};

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

#[derive(Debug, Clone, Copy)]
struct Candidate {
    pane_id: u64,
    score: u32,
    forced_by_starvation: bool,
    intent_seq: u64,
    submitted_at_ms: u64,
    work_units: u32,
}

/// Global resize scheduler.
#[derive(Debug)]
pub struct ResizeScheduler {
    config: ResizeSchedulerConfig,
    panes: HashMap<u64, PaneState>,
    metrics: ResizeSchedulerMetrics,
    lifecycle_events: VecDeque<ResizeTransactionLifecycleEvent>,
    next_lifecycle_event_seq: u64,
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
        let Some(active_seq) = self.panes.get(&pane_id).and_then(|state| state.active_seq) else {
            return false;
        };

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

        for candidate in candidates {
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
        ResizeSchedulerDebugSnapshot {
            gate: self.gate_state(),
            scheduler: self.snapshot(),
            lifecycle_events: self.lifecycle_events(lifecycle_event_limit),
        }
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

    fn push_lifecycle_event(
        &mut self,
        pane_id: u64,
        intent_seq: u64,
        observed_at_ms: Option<u64>,
        stage: ResizeLifecycleStage,
        detail: ResizeLifecycleDetail,
    ) {
        self.next_lifecycle_event_seq = self.next_lifecycle_event_seq.wrapping_add(1);
        let state = self.panes.get(&pane_id);
        self.lifecycle_events
            .push_back(ResizeTransactionLifecycleEvent {
                event_seq: self.next_lifecycle_event_seq,
                frame_seq: self.metrics.frames,
                pane_id,
                intent_seq,
                observed_at_ms,
                latest_seq: state.and_then(|s| s.latest_seq),
                pending_seq: state.and_then(|s| s.pending.as_ref().map(|p| p.intent_seq)),
                active_seq: state.and_then(|s| s.active_seq),
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
        ResizeExecutionPhase, ResizeIntent, ResizeLifecycleDetail, ResizeLifecycleStage,
        ResizeScheduler, ResizeSchedulerConfig, ResizeSchedulerDebugSnapshot, ResizeWorkClass,
        SubmitOutcome,
    };

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

        assert!(scheduler.complete_active(7, 1));
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
}
