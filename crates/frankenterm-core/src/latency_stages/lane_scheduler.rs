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
    fn new(lane: SchedulerLane, capacity: usize) -> Self {
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
    pub config: LaneSchedulerConfig,
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
        let lane = stage_to_lane(stage);
        let item_id = self.next_item_id;
        self.next_item_id += 1;

        let item = WorkItem {
            id: item_id,
            lane,
            stage,
            estimated_cost_us,
            correlation_id: correlation_id.to_string(),
            deadline_us,
        };

        let decision = self.apply_admission(&item, now_us);

        let lane_state = &self.lanes[lane as usize];
        self.push_event(SchedulingEvent {
            item_id,
            lane,
            stage,
            decision: decision.clone(),
            queue_depth_before: if matches!(decision, AdmissionDecision::Admitted) {
                lane_state.depth.saturating_sub(1)
            } else {
                lane_state.depth
            },
            queue_depth_after: lane_state.depth,
            correlation_id: correlation_id.to_string(),
            reason_code: match &decision {
                AdmissionDecision::Deferred => Some("BULK_QUEUE_FULL".into()),
                AdmissionDecision::Shed => Some("QUEUE_OVERFLOW".into()),
                AdmissionDecision::Promoted { .. } => Some("DEADLINE_PROMOTION".into()),
                AdmissionDecision::Admitted => None,
            },
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

    fn apply_admission(&mut self, item: &WorkItem, now_us: u64) -> AdmissionDecision {
        let lane_idx = item.lane as usize;

        // Check if input lane is under pressure — shed bulk items.
        if item.lane == SchedulerLane::Bulk && self.input_under_pressure() {
            self.lanes[lane_idx].total_shed += 1;
            return AdmissionDecision::Shed;
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
                    return AdmissionDecision::Promoted {
                        from: SchedulerLane::Bulk,
                        to: SchedulerLane::Control,
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
                    AdmissionDecision::Deferred
                }
                SchedulerLane::Bulk => {
                    state.total_shed += 1;
                    AdmissionDecision::Shed
                }
            }
        } else {
            state.depth += 1;
            state.total_admitted += 1;
            AdmissionDecision::Admitted
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
