use std::{collections::VecDeque, fmt};

use serde::{Deserialize, Serialize};

use super::LatencyStage;

// ── B1: Three-Lane Scheduler Architecture ─────────────────────────
//
// Defines three scheduling lanes for the pipeline:
// - Input: User keystrokes, terminal I/O — highest priority, bounded queue.
// - Control: System signals, health checks — medium priority.
// - Bulk: Background tasks, batch indexing — lowest priority, elastic.
//
// Admission policy ensures input lane immunity during bulk pressure.
// AARSP Bead: ft-2p9cb.2.1.1

/// Scheduling lane classification.
///
/// Tasks are assigned to lanes based on their latency-sensitivity.
/// The scheduler services lanes in strict priority order: Input > Control > Bulk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SchedulerLane {
    /// User-facing I/O: keystrokes, display updates, PTY reads.
    /// Latency target: < 5ms p99. Never starved.
    Input = 0,
    /// System control: health checks, pane lifecycle, config reloads.
    /// Latency target: < 50ms p99. May be deferred under extreme input pressure.
    Control = 1,
    /// Background work: batch indexing, pattern scanning, log rotation.
    /// Latency target: best-effort. Throttled to protect input/control lanes.
    Bulk = 2,
}

impl SchedulerLane {
    /// All lanes in priority order (highest first).
    pub const ALL: &'static [Self] = &[Self::Input, Self::Control, Self::Bulk];

    /// Priority value (lower = higher priority).
    pub fn priority(self) -> u8 {
        self as u8
    }

    /// Which pipeline stages belong to this lane by default.
    pub fn default_stages(self) -> &'static [LatencyStage] {
        match self {
            Self::Input => &[
                LatencyStage::PtyCapture,
                LatencyStage::DeltaExtraction,
                LatencyStage::ApiResponse,
            ],
            Self::Control => &[
                LatencyStage::EventEmission,
                LatencyStage::WorkflowDispatch,
                LatencyStage::ActionExecution,
            ],
            Self::Bulk => &[LatencyStage::StorageWrite, LatencyStage::PatternDetection],
        }
    }

    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Control => "control",
            Self::Bulk => "bulk",
        }
    }
}

impl fmt::Display for SchedulerLane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Operator-facing QoS class for pane and mission work.
///
/// The enum is intentionally separate from [`SchedulerLane`]: lanes are the
/// concrete queues, while QoS classes describe caller intent before admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QosClass {
    /// Safety-critical operator or control-plane work.
    Critical = 0,
    /// Directly user-visible pane interaction.
    Interactive = 1,
    /// Replay/restore work with bounded catch-up latency.
    Replay = 2,
    /// Ordinary background maintenance.
    Background = 3,
    /// Bulk lexical/vector search and indexing work.
    BulkSearch = 4,
}

impl QosClass {
    /// All QoS classes in priority order (highest first).
    pub const ALL: &'static [Self] = &[
        Self::Critical,
        Self::Interactive,
        Self::Replay,
        Self::Background,
        Self::BulkSearch,
    ];

    /// Priority value (lower = higher priority).
    pub fn priority(self) -> u8 {
        self as u8
    }

    /// Default QoS for a pipeline stage when callers do not provide one.
    pub fn from_stage(stage: LatencyStage) -> Self {
        match stage {
            LatencyStage::PtyCapture
            | LatencyStage::DeltaExtraction
            | LatencyStage::ApiResponse => Self::Interactive,
            LatencyStage::EventEmission
            | LatencyStage::WorkflowDispatch
            | LatencyStage::ActionExecution => Self::Critical,
            LatencyStage::StorageWrite | LatencyStage::EndToEndCapture => Self::Background,
            LatencyStage::PatternDetection | LatencyStage::EndToEndAction => Self::BulkSearch,
        }
    }

    /// Queue lane selected by this QoS class.
    pub fn lane(self) -> SchedulerLane {
        match self {
            Self::Interactive => SchedulerLane::Input,
            Self::Critical | Self::Replay => SchedulerLane::Control,
            Self::Background | Self::BulkSearch => SchedulerLane::Bulk,
        }
    }

    /// Default deadline budget for items without an explicit deadline.
    pub fn default_deadline_budget_us(self) -> u64 {
        match self {
            Self::Critical => 2_000,
            Self::Interactive => 5_000,
            Self::Replay => 100_000,
            Self::Background => 500_000,
            Self::BulkSearch => 2_000_000,
        }
    }

    /// Relative CPU weight for diagnostic budget inheritance.
    pub fn cpu_weight(self) -> u8 {
        match self {
            Self::Critical => 100,
            Self::Interactive => 80,
            Self::Replay => 40,
            Self::Background => 20,
            Self::BulkSearch => 10,
        }
    }

    /// Whether sustained pressure should eventually open an admit window.
    pub fn starvation_protected(self) -> bool {
        matches!(self, Self::Replay | Self::Background)
    }

    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Interactive => "interactive",
            Self::Replay => "replay",
            Self::Background => "background",
            Self::BulkSearch => "bulk_search",
        }
    }
}

impl fmt::Display for QosClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Pane/mission scope attached to a schedulable item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QosScope {
    pub class: QosClass,
    pub pane_id: Option<u64>,
    pub mission_id: Option<String>,
}

impl QosScope {
    pub fn new(class: QosClass) -> Self {
        Self {
            class,
            pane_id: None,
            mission_id: None,
        }
    }

    pub fn for_pane(class: QosClass, pane_id: u64) -> Self {
        Self {
            class,
            pane_id: Some(pane_id),
            mission_id: None,
        }
    }

    pub fn for_mission(class: QosClass, mission_id: impl Into<String>) -> Self {
        Self {
            class,
            pane_id: None,
            mission_id: Some(mission_id.into()),
        }
    }

    pub fn for_pane_mission(class: QosClass, pane_id: u64, mission_id: impl Into<String>) -> Self {
        Self {
            class,
            pane_id: Some(pane_id),
            mission_id: Some(mission_id.into()),
        }
    }
}

/// Map a pipeline stage to its scheduling lane.
pub fn stage_to_lane(stage: LatencyStage) -> SchedulerLane {
    match stage {
        LatencyStage::PtyCapture | LatencyStage::DeltaExtraction | LatencyStage::ApiResponse => {
            SchedulerLane::Input
        }
        LatencyStage::EventEmission
        | LatencyStage::WorkflowDispatch
        | LatencyStage::ActionExecution => SchedulerLane::Control,
        LatencyStage::StorageWrite | LatencyStage::PatternDetection => SchedulerLane::Bulk,
        // Aggregates don't schedule directly.
        LatencyStage::EndToEndCapture | LatencyStage::EndToEndAction => SchedulerLane::Bulk,
    }
}

/// A schedulable work item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Unique item ID.
    pub id: u64,
    /// Which lane this item belongs to.
    pub lane: SchedulerLane,
    /// QoS class and optional pane/mission owner.
    pub qos: QosScope,
    /// Which pipeline stage this work is for.
    pub stage: LatencyStage,
    /// Estimated cost in microseconds.
    pub estimated_cost_us: f64,
    /// Correlation ID for tracing.
    pub correlation_id: String,
    /// Deadline in microseconds from epoch (0 = no deadline).
    pub deadline_us: u64,
}

/// Admission decision for a work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdmissionDecision {
    /// Item admitted to its lane queue.
    Admitted,
    /// Item deferred: bulk lane full, will retry.
    Deferred,
    /// Item shed: queue overflow, item dropped.
    Shed,
    /// Item promoted: moved to higher-priority lane due to deadline pressure.
    Promoted {
        from: SchedulerLane,
        to: SchedulerLane,
    },
}

impl fmt::Display for AdmissionDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admitted => write!(f, "ADMITTED"),
            Self::Deferred => write!(f, "DEFERRED"),
            Self::Shed => write!(f, "SHED"),
            Self::Promoted { from, to } => write!(f, "PROMOTED {}→{}", from, to),
        }
    }
}

/// Configuration for the three-lane scheduler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneSchedulerConfig {
    /// Maximum queue depth per lane.
    pub input_queue_capacity: usize,
    pub control_queue_capacity: usize,
    pub bulk_queue_capacity: usize,
    /// Maximum fraction of CPU time each lane can consume per scheduling epoch.
    /// Must sum to ≤ 1.0.
    pub input_cpu_share: f64,
    pub control_cpu_share: f64,
    pub bulk_cpu_share: f64,
    /// If input queue depth exceeds this fraction, shed bulk items.
    pub input_pressure_threshold: f64,
    /// Enable deadline-based promotion from bulk → control.
    pub enable_deadline_promotion: bool,
    /// Deadline promotion threshold: if remaining time < this fraction of deadline, promote.
    pub deadline_promotion_fraction: f64,
    /// Number of consecutive protected low-priority sheds before opening
    /// one QoS starvation-guard admit window.
    pub qos_starvation_admit_after_sheds: u64,
}

impl Default for LaneSchedulerConfig {
    fn default() -> Self {
        Self {
            input_queue_capacity: 256,
            control_queue_capacity: 128,
            bulk_queue_capacity: 1024,
            input_cpu_share: 0.50,
            control_cpu_share: 0.30,
            bulk_cpu_share: 0.20,
            input_pressure_threshold: 0.75,
            enable_deadline_promotion: true,
            deadline_promotion_fraction: 0.25,
            qos_starvation_admit_after_sheds: 64,
        }
    }
}

impl LaneSchedulerConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let total_share = self.input_cpu_share + self.control_cpu_share + self.bulk_cpu_share;
        if total_share > 1.0 + 1e-6 {
            errors.push(format!("CPU shares sum to {} (must be ≤ 1.0)", total_share));
        }
        if self.input_cpu_share < 0.0 || self.control_cpu_share < 0.0 || self.bulk_cpu_share < 0.0 {
            errors.push("CPU shares must be non-negative".into());
        }
        if self.input_pressure_threshold <= 0.0 || self.input_pressure_threshold > 1.0 {
            errors.push(format!(
                "input_pressure_threshold must be in (0.0, 1.0], got {}",
                self.input_pressure_threshold
            ));
        }
        if self.deadline_promotion_fraction <= 0.0 || self.deadline_promotion_fraction >= 1.0 {
            errors.push(format!(
                "deadline_promotion_fraction must be in (0.0, 1.0), got {}",
                self.deadline_promotion_fraction
            ));
        }
        if self.qos_starvation_admit_after_sheds == 0 {
            errors.push("qos_starvation_admit_after_sheds must be > 0".into());
        }
        errors
    }

    /// Get queue capacity for a lane.
    pub fn capacity(&self, lane: SchedulerLane) -> usize {
        match lane {
            SchedulerLane::Input => self.input_queue_capacity,
            SchedulerLane::Control => self.control_queue_capacity,
            SchedulerLane::Bulk => self.bulk_queue_capacity,
        }
    }

    /// Get CPU share for a lane.
    pub fn cpu_share(&self, lane: SchedulerLane) -> f64 {
        match lane {
            SchedulerLane::Input => self.input_cpu_share,
            SchedulerLane::Control => self.control_cpu_share,
            SchedulerLane::Bulk => self.bulk_cpu_share,
        }
    }
}

/// Per-lane queue state tracked by the scheduler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneState {
    pub lane: SchedulerLane,
    pub depth: usize,
    pub capacity: usize,
    pub total_admitted: u64,
    pub total_deferred: u64,
    pub total_shed: u64,
    pub total_completed: u64,
    pub cpu_used_us: f64,
    pub cpu_budget_us: f64,
}

impl LaneState {
    pub(super) fn new(lane: SchedulerLane, capacity: usize) -> Self {
        Self {
            lane,
            depth: 0,
            capacity,
            total_admitted: 0,
            total_deferred: 0,
            total_shed: 0,
            total_completed: 0,
            cpu_used_us: 0.0,
            cpu_budget_us: 0.0,
        }
    }

    /// Queue utilization fraction (0.0 to 1.0).
    pub fn utilization(&self) -> f64 {
        if self.capacity > 0 {
            self.depth as f64 / self.capacity as f64
        } else {
            0.0
        }
    }

    /// Is the queue at or above capacity?
    pub fn is_full(&self) -> bool {
        self.depth >= self.capacity
    }
}

/// Scheduling event for structured logging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulingEvent {
    pub item_id: u64,
    pub lane: SchedulerLane,
    pub qos_class: QosClass,
    pub pane_id: Option<u64>,
    pub mission_id: Option<String>,
    pub stage: LatencyStage,
    pub decision: AdmissionDecision,
    pub queue_depth_before: usize,
    pub queue_depth_after: usize,
    pub correlation_id: String,
    pub reason_code: Option<String>,
}

/// Diagnostic snapshot of the three-lane scheduler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerSnapshot {
    pub epoch: u64,
    pub lanes: Vec<LaneState>,
    pub total_items_processed: u64,
    pub input_pressure: bool,
    pub qos_starvation_shed_streak: u64,
    pub config: LaneSchedulerConfig,
}

#[derive(Debug, Clone)]
struct AdmissionTrace {
    decision: AdmissionDecision,
    reason_code: Option<String>,
}

/// The three-lane scheduler.
///
/// Manages admission, ordering, and completion tracking for work items
/// across three priority lanes: Input, Control, Bulk.
///
/// # Invariants
///
/// 1. **Input immunity**: Input lane items are never shed while input queue < capacity.
/// 2. **Strict ordering**: Input > Control > Bulk in scheduling priority.
/// 3. **Bounded queues**: Each lane has a fixed capacity; overflow triggers shed/defer.
/// 4. **Determinism**: Same item sequence → same scheduling decisions.
#[derive(Debug, Clone)]
pub struct LaneScheduler {
    config: LaneSchedulerConfig,
    lanes: Vec<LaneState>,
    epoch: u64,
    next_item_id: u64,
    events: VecDeque<SchedulingEvent>,
    max_events: usize,
    qos_starvation_shed_streak: u64,
}

impl LaneScheduler {
    /// Create a new scheduler with the given configuration.
    pub fn new(config: LaneSchedulerConfig) -> Self {
        let lanes = vec![
            LaneState::new(SchedulerLane::Input, config.input_queue_capacity),
            LaneState::new(SchedulerLane::Control, config.control_queue_capacity),
            LaneState::new(SchedulerLane::Bulk, config.bulk_queue_capacity),
        ];
        Self {
            config,
            lanes,
            epoch: 0,
            next_item_id: 1,
            events: VecDeque::new(),
            max_events: 1000,
            qos_starvation_shed_streak: 0,
        }
    }

    /// Create a scheduler with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(LaneSchedulerConfig::default())
    }

    /// Admit a work item to the appropriate lane.
    ///
    /// Returns the admission decision and assigns an item ID.
    pub fn admit(
        &mut self,
        stage: LatencyStage,
        estimated_cost_us: f64,
        correlation_id: &str,
        deadline_us: u64,
        now_us: u64,
    ) -> (WorkItem, AdmissionDecision) {
        self.admit_with_qos(
            stage,
            estimated_cost_us,
            correlation_id,
            deadline_us,
            now_us,
            QosScope::new(QosClass::from_stage(stage)),
        )
    }

    /// Admit a pane/mission-scoped item with an explicit QoS class.
    ///
    /// QoS can override the stage's default lane, e.g. replay storage
    /// recovery work can enter the control lane while bulk search remains in
    /// bulk even with an explicit deadline.
    pub fn admit_with_qos(
        &mut self,
        stage: LatencyStage,
        estimated_cost_us: f64,
        correlation_id: &str,
        deadline_us: u64,
        now_us: u64,
        qos: QosScope,
    ) -> (WorkItem, AdmissionDecision) {
        let default_lane = stage_to_lane(stage);
        let qos_class = qos.class;
        let lane = qos_class.lane();
        let item_id = self.next_item_id;
        self.next_item_id += 1;

        let item = WorkItem {
            id: item_id,
            lane,
            qos,
            stage,
            estimated_cost_us,
            correlation_id: correlation_id.to_string(),
            deadline_us: if deadline_us == 0 && now_us > 0 {
                now_us.saturating_add(qos_class.default_deadline_budget_us())
            } else {
                deadline_us
            },
        };

        let admission = self.apply_admission(&item, now_us);
        let decision = admission.decision;

        let lane_state = &self.lanes[lane as usize];
        self.push_event(SchedulingEvent {
            item_id,
            lane,
            qos_class: item.qos.class,
            pane_id: item.qos.pane_id,
            mission_id: item.qos.mission_id.clone(),
            stage,
            decision: decision.clone(),
            queue_depth_before: if matches!(decision, AdmissionDecision::Admitted) {
                lane_state.depth.saturating_sub(1)
            } else {
                lane_state.depth
            },
            queue_depth_after: lane_state.depth,
            correlation_id: correlation_id.to_string(),
            reason_code: admission.reason_code.or_else(|| {
                if lane != default_lane {
                    Some("QOS_CLASS_LANE_OVERRIDE".into())
                } else {
                    None
                }
            }),
        });

        (item, decision)
    }

    /// Mark an item as completed.
    pub fn complete(&mut self, lane: SchedulerLane, actual_cost_us: f64) {
        let state = &mut self.lanes[lane as usize];
        if state.depth > 0 {
            state.depth -= 1;
            state.total_completed += 1;
            state.cpu_used_us += actual_cost_us;
        }
    }

    /// Start a new scheduling epoch. Resets per-epoch CPU counters.
    pub fn begin_epoch(&mut self, epoch_budget_us: f64) {
        self.epoch += 1;
        for state in &mut self.lanes {
            state.cpu_used_us = 0.0;
            state.cpu_budget_us = epoch_budget_us * self.config.cpu_share(state.lane);
        }
    }

    /// Is the input lane under pressure?
    pub fn input_under_pressure(&self) -> bool {
        let input = &self.lanes[SchedulerLane::Input as usize];
        input.utilization() >= self.config.input_pressure_threshold
    }

    /// Get the lane state for a specific lane.
    pub fn lane_state(&self, lane: SchedulerLane) -> &LaneState {
        &self.lanes[lane as usize]
    }

    /// Get a diagnostic snapshot.
    pub fn snapshot(&self) -> SchedulerSnapshot {
        SchedulerSnapshot {
            epoch: self.epoch,
            lanes: self.lanes.clone(),
            total_items_processed: self.lanes.iter().map(|l| l.total_completed).sum(),
            input_pressure: self.input_under_pressure(),
            qos_starvation_shed_streak: self.qos_starvation_shed_streak,
            config: self.config.clone(),
        }
    }

    /// Get the last N scheduling events.
    pub fn recent_events(&self, n: usize) -> Vec<SchedulingEvent> {
        let start = self.events.len().saturating_sub(n);
        self.events.iter().skip(start).cloned().collect()
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        let depths: Vec<String> = self
            .lanes
            .iter()
            .map(|l| format!("{}={}/{}", l.lane, l.depth, l.capacity))
            .collect();
        format!(
            "scheduler epoch={} [{}] pressure={}",
            self.epoch,
            depths.join(" "),
            self.input_under_pressure()
        )
    }

    fn apply_admission(&mut self, item: &WorkItem, now_us: u64) -> AdmissionTrace {
        let lane_idx = item.lane as usize;

        // Check if input lane is under pressure — shed bulk items.
        if item.lane == SchedulerLane::Bulk && self.input_under_pressure() {
            if item.qos.class.starvation_protected()
                && self.qos_starvation_shed_streak >= self.config.qos_starvation_admit_after_sheds
                && !self.lanes[lane_idx].is_full()
            {
                self.qos_starvation_shed_streak = 0;
                self.lanes[lane_idx].depth += 1;
                self.lanes[lane_idx].total_admitted += 1;
                return AdmissionTrace {
                    decision: AdmissionDecision::Admitted,
                    reason_code: Some("QOS_STARVATION_GUARD".into()),
                };
            }
            self.qos_starvation_shed_streak = self.qos_starvation_shed_streak.saturating_add(1);
            self.lanes[lane_idx].total_shed += 1;
            return AdmissionTrace {
                decision: AdmissionDecision::Shed,
                reason_code: Some("INPUT_PRESSURE_SHED".into()),
            };
        }

        // Check for deadline-based promotion.
        if self.config.enable_deadline_promotion
            && item.lane == SchedulerLane::Bulk
            && item.deadline_us > 0
            && now_us > 0
        {
            let remaining = item.deadline_us.saturating_sub(now_us);
            let threshold =
                (item.deadline_us as f64 * self.config.deadline_promotion_fraction) as u64;
            if remaining < threshold {
                // Promote to control lane.
                let control_idx = SchedulerLane::Control as usize;
                if !self.lanes[control_idx].is_full() {
                    self.lanes[control_idx].depth += 1;
                    self.lanes[control_idx].total_admitted += 1;
                    self.qos_starvation_shed_streak = 0;
                    return AdmissionTrace {
                        decision: AdmissionDecision::Promoted {
                            from: SchedulerLane::Bulk,
                            to: SchedulerLane::Control,
                        },
                        reason_code: Some("DEADLINE_PROMOTION".into()),
                    };
                }
            }
        }

        // Try to admit to the item's lane.
        let state = &mut self.lanes[lane_idx];
        if state.is_full() {
            // Input items are never shed — they wait (defer).
            // Control items defer. Bulk items are shed.
            match item.lane {
                SchedulerLane::Input | SchedulerLane::Control => {
                    state.total_deferred += 1;
                    AdmissionTrace {
                        decision: AdmissionDecision::Deferred,
                        reason_code: Some("QUEUE_FULL_DEFER".into()),
                    }
                }
                SchedulerLane::Bulk => {
                    state.total_shed += 1;
                    self.qos_starvation_shed_streak =
                        self.qos_starvation_shed_streak.saturating_add(1);
                    AdmissionTrace {
                        decision: AdmissionDecision::Shed,
                        reason_code: Some("QUEUE_OVERFLOW".into()),
                    }
                }
            }
        } else {
            state.depth += 1;
            state.total_admitted += 1;
            if item.lane == SchedulerLane::Bulk && item.qos.class.starvation_protected() {
                self.qos_starvation_shed_streak = 0;
            }
            AdmissionTrace {
                decision: AdmissionDecision::Admitted,
                reason_code: None,
            }
        }
    }

    fn push_event(&mut self, event: SchedulingEvent) {
        self.events.push_back(event);
        if self.events.len() > self.max_events {
            let evict_count = self.events.len() / 2;
            self.events.drain(..evict_count);
        }
    }

    /// Check whether a lane has remaining CPU budget in the current epoch.
    pub fn has_cpu_budget(&self, lane: SchedulerLane) -> bool {
        let state = &self.lanes[lane as usize];
        state.cpu_used_us < state.cpu_budget_us
    }

    /// Remaining CPU budget for a lane in the current epoch.
    pub fn remaining_cpu_us(&self, lane: SchedulerLane) -> f64 {
        let state = &self.lanes[lane as usize];
        (state.cpu_budget_us - state.cpu_used_us).max(0.0)
    }

    /// Pick the next lane to service using strict priority.
    ///
    /// Returns the highest-priority lane that has items and CPU budget.
    /// Falls through to lower priority lanes only when higher lanes are empty.
    pub fn next_lane(&self) -> Option<SchedulerLane> {
        for &lane in SchedulerLane::ALL {
            let state = &self.lanes[lane as usize];
            if state.depth > 0 && state.cpu_used_us < state.cpu_budget_us {
                return Some(lane);
            }
        }
        // Fallback: any lane with items (ignore budget for input lane).
        if self.lanes[SchedulerLane::Input as usize].depth > 0 {
            return Some(SchedulerLane::Input);
        }
        None
    }

    /// Compute fairness metric: ratio of actual CPU share to configured share per lane.
    ///
    /// Returns (lane, fairness_ratio) for each lane.
    /// Fairness ratio = 1.0 means exactly fair; < 1.0 means under-served; > 1.0 means over-served.
    pub fn fairness_ratios(&self) -> Vec<(SchedulerLane, f64)> {
        let total_cpu: f64 = self.lanes.iter().map(|l| l.cpu_used_us).sum();
        if total_cpu < 1e-6 {
            return SchedulerLane::ALL.iter().map(|&l| (l, 1.0)).collect();
        }
        SchedulerLane::ALL
            .iter()
            .map(|&lane| {
                let state = &self.lanes[lane as usize];
                let actual_share = state.cpu_used_us / total_cpu;
                let target_share = self.config.cpu_share(lane);
                let ratio = if target_share > 0.0 {
                    actual_share / target_share
                } else {
                    0.0
                };
                (lane, ratio)
            })
            .collect()
    }

    /// Detect scheduler degradation.
    pub fn current_degradation(&self) -> SchedulerDegradation {
        // Check for starvation: any lane with items but 0 completions over many epochs.
        let input = &self.lanes[SchedulerLane::Input as usize];
        let control = &self.lanes[SchedulerLane::Control as usize];
        let bulk = &self.lanes[SchedulerLane::Bulk as usize];

        // Input starvation is critical.
        if input.depth > 0 && input.total_deferred > input.total_admitted / 2 + 1 {
            return SchedulerDegradation::InputStarvation {
                depth: input.depth,
                deferred: input.total_deferred,
            };
        }

        // Bulk starvation: many items shed, few completed.
        if bulk.total_shed > bulk.total_completed + 10 {
            return SchedulerDegradation::BulkStarvation {
                shed_count: bulk.total_shed,
                completed_count: bulk.total_completed,
            };
        }

        // Control backlog: queue growing without drain.
        if control.depth > control.capacity / 2 {
            return SchedulerDegradation::ControlBacklog {
                depth: control.depth,
                capacity: control.capacity,
            };
        }

        SchedulerDegradation::Healthy
    }

    /// Is the scheduler healthy?
    pub fn is_healthy(&self) -> bool {
        matches!(self.current_degradation(), SchedulerDegradation::Healthy)
    }

    /// Generate a structured log entry for the current epoch state.
    pub fn log_entry(&self) -> SchedulerLogEntry {
        SchedulerLogEntry {
            epoch: self.epoch,
            depths: SchedulerLane::ALL
                .iter()
                .map(|&l| (l, self.lanes[l as usize].depth))
                .collect(),
            cpu_used: SchedulerLane::ALL
                .iter()
                .map(|&l| (l, self.lanes[l as usize].cpu_used_us))
                .collect(),
            input_pressure: self.input_under_pressure(),
            degradation: self.current_degradation(),
            fairness: self.fairness_ratios(),
        }
    }
}

/// Scheduler degradation states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchedulerDegradation {
    /// All lanes operating normally.
    Healthy,
    /// Input lane experiencing starvation (critical).
    InputStarvation { depth: usize, deferred: u64 },
    /// Bulk lane heavily shed, few items completing.
    BulkStarvation {
        shed_count: u64,
        completed_count: u64,
    },
    /// Control lane backlog growing.
    ControlBacklog { depth: usize, capacity: usize },
}

impl fmt::Display for SchedulerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::InputStarvation { depth, deferred } => {
                write!(f, "INPUT_STARVATION depth={} deferred={}", depth, deferred)
            }
            Self::BulkStarvation {
                shed_count,
                completed_count,
            } => write!(
                f,
                "BULK_STARVATION shed={} completed={}",
                shed_count, completed_count
            ),
            Self::ControlBacklog { depth, capacity } => {
                write!(f, "CONTROL_BACKLOG depth={}/{}", depth, capacity)
            }
        }
    }
}

/// Structured log entry for a scheduling epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerLogEntry {
    pub epoch: u64,
    pub depths: Vec<(SchedulerLane, usize)>,
    pub cpu_used: Vec<(SchedulerLane, f64)>,
    pub input_pressure: bool,
    pub degradation: SchedulerDegradation,
    pub fairness: Vec<(SchedulerLane, f64)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pressure_config() -> LaneSchedulerConfig {
        LaneSchedulerConfig {
            input_queue_capacity: 4,
            input_pressure_threshold: 0.75,
            qos_starvation_admit_after_sheds: 2,
            ..Default::default()
        }
    }

    fn fill_input_pressure(scheduler: &mut LaneScheduler) {
        for i in 0..3 {
            scheduler.admit(LatencyStage::PtyCapture, 10.0, &format!("input-{i}"), 0, 0);
        }
        assert!(scheduler.input_under_pressure());
    }

    #[test]
    fn qos_class_ordering_and_lane_mapping_are_stable() {
        assert_eq!(QosClass::ALL.len(), 5);
        assert!(QosClass::Critical < QosClass::Interactive);
        assert!(QosClass::Interactive < QosClass::Replay);
        assert!(QosClass::Replay < QosClass::Background);
        assert!(QosClass::Background < QosClass::BulkSearch);

        assert_eq!(QosClass::Interactive.lane(), SchedulerLane::Input);
        assert_eq!(QosClass::Critical.lane(), SchedulerLane::Control);
        assert_eq!(QosClass::Replay.lane(), SchedulerLane::Control);
        assert_eq!(QosClass::Background.lane(), SchedulerLane::Bulk);
        assert_eq!(QosClass::BulkSearch.lane(), SchedulerLane::Bulk);
    }

    #[test]
    fn explicit_qos_records_scope_and_can_override_stage_lane() {
        let mut scheduler = LaneScheduler::with_defaults();
        let scope = QosScope::for_pane_mission(QosClass::Replay, 42, "restore-pane-42");

        let (item, decision) = scheduler.admit_with_qos(
            LatencyStage::StorageWrite,
            100.0,
            "restore",
            0,
            1_000,
            scope,
        );

        assert_eq!(decision, AdmissionDecision::Admitted);
        assert_eq!(item.lane, SchedulerLane::Control);
        assert_eq!(item.qos.class, QosClass::Replay);
        assert_eq!(item.qos.pane_id, Some(42));
        assert_eq!(item.qos.mission_id.as_deref(), Some("restore-pane-42"));
        assert_eq!(item.deadline_us, 101_000);

        let event = scheduler.recent_events(1).pop().unwrap();
        assert_eq!(event.qos_class, QosClass::Replay);
        assert_eq!(event.pane_id, Some(42));
        assert_eq!(event.mission_id.as_deref(), Some("restore-pane-42"));
        assert_eq!(
            event.reason_code.as_deref(),
            Some("QOS_CLASS_LANE_OVERRIDE")
        );
    }

    #[test]
    fn bulk_search_still_sheds_under_input_pressure() {
        let mut scheduler = LaneScheduler::new(pressure_config());
        fill_input_pressure(&mut scheduler);

        let (_item, decision) = scheduler.admit_with_qos(
            LatencyStage::PatternDetection,
            100.0,
            "search",
            0,
            0,
            QosScope::new(QosClass::BulkSearch),
        );

        assert_eq!(decision, AdmissionDecision::Shed);
        let event = scheduler.recent_events(1).pop().unwrap();
        assert_eq!(event.reason_code.as_deref(), Some("INPUT_PRESSURE_SHED"));
    }

    #[test]
    fn background_qos_gets_starvation_guard_admit_window() {
        let mut scheduler = LaneScheduler::new(pressure_config());
        fill_input_pressure(&mut scheduler);

        for i in 0..2 {
            let (_item, decision) = scheduler.admit_with_qos(
                LatencyStage::StorageWrite,
                100.0,
                &format!("background-shed-{i}"),
                0,
                0,
                QosScope::new(QosClass::Background),
            );
            assert_eq!(decision, AdmissionDecision::Shed);
        }

        let (_item, decision) = scheduler.admit_with_qos(
            LatencyStage::StorageWrite,
            100.0,
            "background-guard",
            0,
            0,
            QosScope::new(QosClass::Background),
        );

        assert_eq!(decision, AdmissionDecision::Admitted);
        assert_eq!(scheduler.lane_state(SchedulerLane::Bulk).depth, 1);
        let event = scheduler.recent_events(1).pop().unwrap();
        assert_eq!(event.reason_code.as_deref(), Some("QOS_STARVATION_GUARD"));
    }
}
