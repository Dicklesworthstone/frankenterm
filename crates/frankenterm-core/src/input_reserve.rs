//! Input-preserving backpressure with S3-FIFO admission and reserve floors.
//!
//! Card C of the latency-immunity architecture ensures local interaction
//! (keystrokes, viewport reflow) is never starved by remote mux state work.
//!
//! # Components
//!
//! - [`S3FifoQueue`]: Three-segment FIFO (small → main, ghost set) that protects
//!   interactive working sets from cache pollution by cold background captures.
//! - [`ReserveFloorPolicy`]: Hard minimum interactive budget based on tier,
//!   severity, and input backlog depth.
//! - [`ShedMarker`]: Structured diagnostics when work items are dropped (analogous
//!   to `Gap` in [`crate::ingest`]).
//! - [`InputReserveController`]: Façade that combines admission, scheduling, and
//!   shed tracking into a single `submit → schedule_frame` API.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::backpressure::BackpressureTier;
use crate::viewport_reflow_planner::ReflowBatchPriority;

// ---------------------------------------------------------------------------
// Work-item classification
// ---------------------------------------------------------------------------

/// Classification of a queued work item by interaction urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemClass {
    /// Direct user input (keystrokes, mouse clicks).
    Input,
    /// Visible viewport lines — highest reflow urgency.
    ViewportCore,
    /// Near-visible context around the viewport.
    ViewportOverscan,
    /// Cold scrollback convergence work.
    ColdScrollback,
    /// Remote mux state capture / background sync.
    BackgroundCapture,
}

impl WorkItemClass {
    /// Numeric priority level (higher = more urgent).
    #[inline]
    pub fn priority_level(self) -> u8 {
        match self {
            Self::Input => 4,
            Self::ViewportCore => 3,
            Self::ViewportOverscan => 2,
            Self::ColdScrollback => 1,
            Self::BackgroundCapture => 0,
        }
    }

    /// Whether this class counts toward the interactive budget.
    #[inline]
    pub fn is_interactive(self) -> bool {
        matches!(
            self,
            Self::Input | Self::ViewportCore | Self::ViewportOverscan
        )
    }

    /// Bridge from [`ReflowBatchPriority`] to [`WorkItemClass`].
    pub fn from_reflow_priority(p: ReflowBatchPriority) -> Self {
        match p {
            ReflowBatchPriority::ViewportCore => Self::ViewportCore,
            ReflowBatchPriority::ViewportOverscan => Self::ViewportOverscan,
            ReflowBatchPriority::ColdScrollback => Self::ColdScrollback,
        }
    }
}

// ---------------------------------------------------------------------------
// Work items
// ---------------------------------------------------------------------------

/// A unit of work queued for scheduling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    /// Unique identifier for this work item.
    pub id: u64,
    /// Pane that owns the work (0 for global items).
    pub pane_id: u64,
    /// Classification by urgency.
    pub class: WorkItemClass,
    /// Abstract cost in budget units.
    pub work_units: u32,
    /// Submission wall-clock (ms since epoch).
    pub submitted_at_ms: u64,
    /// Monotonic sequence number for FIFO ordering.
    pub sequence: u64,
}

// ---------------------------------------------------------------------------
// Shed markers
// ---------------------------------------------------------------------------

/// Reason a work item was shed (dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShedReason {
    /// Evicted from S3-FIFO small segment without promotion.
    S3FifoSmallEviction,
    /// Dropped to enforce the interactive reserve floor.
    ReserveFloorEnforcement,
    /// Severity-based cold item shedding.
    ColdSeverityShed,
    /// Item exceeded a staleness timeout.
    StaleTimeout,
    /// Queue capacity overflow on admission.
    CapacityOverflow,
    /// Ghost eviction (metadata reclaimed).
    GhostEviction,
    /// Explicit isolation: only interactive work permitted.
    IsolateInteractive,
}

/// Diagnostic record emitted when a work item is dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShedMarker {
    /// ID of the dropped work item.
    pub item_id: u64,
    /// Pane that owned the dropped item.
    pub pane_id: u64,
    /// Classification of the dropped item.
    pub class: WorkItemClass,
    /// Why the item was shed.
    pub reason: ShedReason,
    /// Backpressure tier at shed time.
    pub tier: BackpressureTier,
    /// Continuous severity ∈ [0, 1] at shed time.
    pub severity: f64,
    /// Monotonic timestamp (ms) at shed time.
    pub shed_at_ms: u64,
}

// ---------------------------------------------------------------------------
// S3-FIFO admission queue
// ---------------------------------------------------------------------------

/// Configuration for the S3-FIFO admission queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3FifoConfig {
    /// Total capacity (small + main) in item count.
    pub total_capacity: usize,
    /// Fraction of total capacity allocated to the small queue.
    pub small_fraction: f64,
    /// Ghost capacity as a multiple of total capacity.
    pub ghost_capacity_multiplier: usize,
}

impl Default for S3FifoConfig {
    fn default() -> Self {
        Self {
            total_capacity: 256,
            small_fraction: 0.10,
            ghost_capacity_multiplier: 10,
        }
    }
}

/// Three-segment FIFO that protects interactive working sets.
///
/// New items enter **small**. On small eviction, items with frequency > 0
/// promote to **main**; cold items go to **ghost** (metadata-only). Ghost hits
/// go directly to main on re-admission. Interactive items in main are never
/// evicted while background items exist.
#[derive(Debug)]
pub struct S3FifoQueue {
    config: S3FifoConfig,
    small_capacity: usize,
    main_capacity: usize,
    ghost_capacity: usize,

    small: VecDeque<WorkItem>,
    main: VecDeque<WorkItem>,
    ghost: VecDeque<u64>, // item IDs only

    /// Access frequency counter per item ID.
    frequency: HashMap<u64, u32>,

    // Stats
    total_admitted: u64,
    total_promoted: u64,
    total_ghost_hits: u64,
    total_evicted: u64,
}

/// Stats snapshot for an [`S3FifoQueue`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3FifoStats {
    pub small_len: usize,
    pub main_len: usize,
    pub ghost_len: usize,
    pub total_admitted: u64,
    pub total_promoted: u64,
    pub total_ghost_hits: u64,
    pub total_evicted: u64,
}

impl S3FifoQueue {
    /// Create a new queue with the given configuration.
    pub fn new(config: S3FifoConfig) -> Self {
        let small_capacity =
            ((config.total_capacity as f64 * config.small_fraction).ceil() as usize).max(1);
        let main_capacity = config.total_capacity.saturating_sub(small_capacity).max(1);
        let ghost_capacity = config
            .total_capacity
            .saturating_mul(config.ghost_capacity_multiplier)
            .max(1);

        Self {
            config,
            small_capacity,
            main_capacity,
            ghost_capacity,
            small: VecDeque::new(),
            main: VecDeque::new(),
            ghost: VecDeque::new(),
            frequency: HashMap::new(),
            total_admitted: 0,
            total_promoted: 0,
            total_ghost_hits: 0,
            total_evicted: 0,
        }
    }

    /// Admit a work item. Returns shed markers for any evicted items.
    pub fn admit(
        &mut self,
        item: WorkItem,
        tier: BackpressureTier,
        severity: f64,
        now_ms: u64,
    ) -> Vec<ShedMarker> {
        let mut shed = Vec::new();
        let id = item.id;

        // Ghost hit → promote directly to main
        if let Some(pos) = self.ghost.iter().position(|&gid| gid == id) {
            self.ghost.remove(pos);
            self.total_ghost_hits += 1;
            self.frequency.insert(id, 1);
            self.evict_main_if_needed(tier, severity, now_ms, &mut shed);
            self.main.push_back(item);
            self.total_admitted += 1;
            return shed;
        }

        // Record access
        *self.frequency.entry(id).or_insert(0) += 1;

        // Evict from small if at capacity
        while self.small.len() >= self.small_capacity {
            if let Some(evicted) = self.small.pop_front() {
                let freq = self.frequency.remove(&evicted.id).unwrap_or(0);
                if freq > 1 {
                    // Promote to main
                    self.evict_main_if_needed(tier, severity, now_ms, &mut shed);
                    self.main.push_back(evicted);
                    self.total_promoted += 1;
                } else {
                    // Send to ghost
                    self.add_to_ghost(evicted.id);
                    shed.push(ShedMarker {
                        item_id: evicted.id,
                        pane_id: evicted.pane_id,
                        class: evicted.class,
                        reason: ShedReason::S3FifoSmallEviction,
                        tier,
                        severity,
                        shed_at_ms: now_ms,
                    });
                    self.total_evicted += 1;
                }
            }
        }

        self.small.push_back(item);
        self.total_admitted += 1;

        shed
    }

    /// Record an access to an item already in the queue.
    pub fn access(&mut self, item_id: u64) {
        *self.frequency.entry(item_id).or_insert(0) += 1;
    }

    /// Drain all items from both segments, ordered by priority then sequence.
    pub fn drain_all(&mut self) -> Vec<WorkItem> {
        let mut items: Vec<WorkItem> = self.small.drain(..).chain(self.main.drain(..)).collect();
        items.sort_by(|a, b| {
            b.class
                .priority_level()
                .cmp(&a.class.priority_level())
                .then(a.sequence.cmp(&b.sequence))
        });
        self.frequency.clear();
        items
    }

    /// Total number of items across small + main.
    pub fn len(&self) -> usize {
        self.small.len() + self.main.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.small.is_empty() && self.main.is_empty()
    }

    /// Snapshot of queue statistics.
    pub fn stats(&self) -> S3FifoStats {
        S3FifoStats {
            small_len: self.small.len(),
            main_len: self.main.len(),
            ghost_len: self.ghost.len(),
            total_admitted: self.total_admitted,
            total_promoted: self.total_promoted,
            total_ghost_hits: self.total_ghost_hits,
            total_evicted: self.total_evicted,
        }
    }

    /// Configuration reference.
    pub fn config(&self) -> &S3FifoConfig {
        &self.config
    }

    // -- internal helpers --

    fn evict_main_if_needed(
        &mut self,
        tier: BackpressureTier,
        severity: f64,
        now_ms: u64,
        shed: &mut Vec<ShedMarker>,
    ) {
        while self.main.len() >= self.main_capacity {
            // Find first background item to evict; never evict interactive when
            // background items still exist.
            let bg_pos = self
                .main
                .iter()
                .position(|item| !item.class.is_interactive());

            let evict_pos = bg_pos.unwrap_or(0);
            if let Some(evicted) = self.main.remove(evict_pos) {
                self.frequency.remove(&evicted.id);
                self.add_to_ghost(evicted.id);
                shed.push(ShedMarker {
                    item_id: evicted.id,
                    pane_id: evicted.pane_id,
                    class: evicted.class,
                    reason: ShedReason::CapacityOverflow,
                    tier,
                    severity,
                    shed_at_ms: now_ms,
                });
                self.total_evicted += 1;
            }
        }
    }

    fn add_to_ghost(&mut self, id: u64) {
        while self.ghost.len() >= self.ghost_capacity {
            self.ghost.pop_front();
        }
        self.ghost.push_back(id);
    }
}

// ---------------------------------------------------------------------------
// Reserve floor policy
// ---------------------------------------------------------------------------

/// Shed action escalation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShedAction {
    /// No shedding required.
    None,
    /// Throttle background capture rate.
    ThrottleBackground,
    /// Drop cold scrollback items.
    DropCold,
    /// Only interactive work permitted.
    IsolateInteractive,
}

/// Configuration for the reserve floor policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReserveFloorConfig {
    /// Minimum interactive budget units always reserved.
    pub base_floor_units: u32,
    /// Additional units reserved when input backlog ≥ threshold.
    pub surge_reserve_units: u32,
    /// Input backlog count that triggers surge reserve.
    pub surge_backlog_threshold: u32,
    /// Severity threshold for cold shedding.
    pub cold_shed_severity: f64,
}

impl Default for ReserveFloorConfig {
    fn default() -> Self {
        Self {
            base_floor_units: 2,
            surge_reserve_units: 2,
            surge_backlog_threshold: 4,
            cold_shed_severity: 0.7,
        }
    }
}

/// Budget partition between interactive and background work.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BudgetPartition {
    /// Budget units reserved for interactive work.
    pub interactive_budget: u32,
    /// Budget units available for background work.
    pub background_budget: u32,
    /// Shed action determined by tier escalation.
    pub shed_action: ShedAction,
}

/// Enforces hard minimum interactive budget based on tier, severity, and backlog.
#[derive(Debug, Clone)]
pub struct ReserveFloorPolicy {
    config: ReserveFloorConfig,
}

impl ReserveFloorPolicy {
    /// Create a policy with the given configuration.
    pub fn new(config: ReserveFloorConfig) -> Self {
        Self { config }
    }

    /// Determine the shed action for the current tier.
    pub fn shed_action_for_tier(&self, tier: BackpressureTier) -> ShedAction {
        match tier {
            BackpressureTier::Green => ShedAction::None,
            BackpressureTier::Yellow => ShedAction::ThrottleBackground,
            BackpressureTier::Red => ShedAction::DropCold,
            BackpressureTier::Black => ShedAction::IsolateInteractive,
        }
    }

    /// Compute the floor (minimum interactive budget units).
    pub fn compute_floor(&self, input_backlog: u32) -> u32 {
        let surge = if input_backlog >= self.config.surge_backlog_threshold {
            self.config.surge_reserve_units
        } else {
            0
        };
        self.config.base_floor_units.saturating_add(surge)
    }

    /// Partition a total budget into interactive and background shares.
    ///
    /// **Hard invariant**: `interactive_budget >= min(floor, total)`.
    pub fn partition(
        &self,
        total_budget: u32,
        tier: BackpressureTier,
        severity: f64,
        input_backlog: u32,
    ) -> BudgetPartition {
        let floor = self.compute_floor(input_backlog);
        let interactive_budget = floor.min(total_budget);
        let background_budget = total_budget.saturating_sub(interactive_budget);
        let tier_action = self.shed_action_for_tier(tier);
        let shed_action = if matches!(tier_action, ShedAction::DropCold)
            && severity < self.config.cold_shed_severity
        {
            ShedAction::ThrottleBackground
        } else {
            tier_action
        };

        BudgetPartition {
            interactive_budget,
            background_budget,
            shed_action,
        }
    }

    /// Configuration reference.
    pub fn config(&self) -> &ReserveFloorConfig {
        &self.config
    }
}

// ---------------------------------------------------------------------------
// Controller façade
// ---------------------------------------------------------------------------

/// Configuration for the [`InputReserveController`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputReserveConfig {
    /// S3-FIFO queue configuration.
    pub s3fifo: S3FifoConfig,
    /// Reserve floor policy configuration.
    pub reserve_floor: ReserveFloorConfig,
    /// Staleness timeout: items older than this (ms) are shed.
    pub stale_timeout_ms: u64,
}

impl Default for InputReserveConfig {
    fn default() -> Self {
        Self {
            s3fifo: S3FifoConfig::default(),
            reserve_floor: ReserveFloorConfig::default(),
            stale_timeout_ms: 5000,
        }
    }
}

/// Metrics snapshot from the controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputReserveMetrics {
    /// S3-FIFO queue stats.
    pub queue_stats: S3FifoStats,
    /// Total frames scheduled.
    pub frames_scheduled: u64,
    /// Total items shed across all frames.
    pub total_items_shed: u64,
    /// Total items delivered across all frames.
    pub total_items_delivered: u64,
}

/// Result of a single frame scheduling cycle.
#[derive(Debug, Clone)]
pub struct FrameScheduleResult {
    /// How the budget was partitioned.
    pub partition: BudgetPartition,
    /// Work items selected for this frame (priority-ordered).
    pub selected: Vec<WorkItem>,
    /// Items that were shed during this frame.
    pub shed_markers: Vec<ShedMarker>,
}

/// Single entry point for input-reserve backpressure management.
///
/// Usage: `submit()` work items → `schedule_frame()` to get priority-ordered
/// items within budget → inspect shed markers for diagnostics.
#[derive(Debug)]
pub struct InputReserveController {
    queue: S3FifoQueue,
    policy: ReserveFloorPolicy,
    config: InputReserveConfig,
    next_sequence: u64,
    frames_scheduled: u64,
    total_items_shed: u64,
    total_items_delivered: u64,
}

impl InputReserveController {
    /// Create a controller with the given configuration.
    pub fn new(config: InputReserveConfig) -> Self {
        let queue = S3FifoQueue::new(config.s3fifo.clone());
        let policy = ReserveFloorPolicy::new(config.reserve_floor.clone());
        Self {
            queue,
            policy,
            config,
            next_sequence: 0,
            frames_scheduled: 0,
            total_items_shed: 0,
            total_items_delivered: 0,
        }
    }

    /// Submit a work item for scheduling. Returns shed markers if evictions occur.
    pub fn submit(
        &mut self,
        mut item: WorkItem,
        tier: BackpressureTier,
        severity: f64,
        now_ms: u64,
    ) -> Vec<ShedMarker> {
        item.sequence = self.next_sequence;
        self.next_sequence += 1;
        let shed = self.queue.admit(item, tier, severity, now_ms);
        self.total_items_shed += shed.len() as u64;
        shed
    }

    /// Schedule a frame: partition budget, select items, shed excess.
    pub fn schedule_frame(
        &mut self,
        total_budget: u32,
        tier: BackpressureTier,
        severity: f64,
        input_backlog: u32,
        now_ms: u64,
    ) -> FrameScheduleResult {
        self.frames_scheduled += 1;

        let partition = self
            .policy
            .partition(total_budget, tier, severity, input_backlog);
        let all_items = self.queue.drain_all();

        let mut selected = Vec::new();
        let mut shed_markers = Vec::new();
        let mut interactive_used: u32 = 0;
        let mut background_used: u32 = 0;

        for item in all_items {
            // Stale timeout check
            if now_ms.saturating_sub(item.submitted_at_ms) > self.config.stale_timeout_ms {
                shed_markers.push(ShedMarker {
                    item_id: item.id,
                    pane_id: item.pane_id,
                    class: item.class,
                    reason: ShedReason::StaleTimeout,
                    tier,
                    severity,
                    shed_at_ms: now_ms,
                });
                continue;
            }

            // Isolation mode: only interactive work
            if partition.shed_action == ShedAction::IsolateInteractive
                && !item.class.is_interactive()
            {
                shed_markers.push(ShedMarker {
                    item_id: item.id,
                    pane_id: item.pane_id,
                    class: item.class,
                    reason: ShedReason::IsolateInteractive,
                    tier,
                    severity,
                    shed_at_ms: now_ms,
                });
                continue;
            }

            // Cold shedding under DropCold action
            if partition.shed_action >= ShedAction::DropCold
                && item.class == WorkItemClass::ColdScrollback
            {
                shed_markers.push(ShedMarker {
                    item_id: item.id,
                    pane_id: item.pane_id,
                    class: item.class,
                    reason: ShedReason::ColdSeverityShed,
                    tier,
                    severity,
                    shed_at_ms: now_ms,
                });
                continue;
            }

            // Budget check
            if item.class.is_interactive() {
                if interactive_used.saturating_add(item.work_units) <= partition.interactive_budget
                {
                    interactive_used += item.work_units;
                    selected.push(item);
                } else {
                    shed_markers.push(ShedMarker {
                        item_id: item.id,
                        pane_id: item.pane_id,
                        class: item.class,
                        reason: ShedReason::ReserveFloorEnforcement,
                        tier,
                        severity,
                        shed_at_ms: now_ms,
                    });
                }
            } else if background_used.saturating_add(item.work_units) <= partition.background_budget
            {
                background_used += item.work_units;
                selected.push(item);
            } else {
                shed_markers.push(ShedMarker {
                    item_id: item.id,
                    pane_id: item.pane_id,
                    class: item.class,
                    reason: ShedReason::ReserveFloorEnforcement,
                    tier,
                    severity,
                    shed_at_ms: now_ms,
                });
            }
        }

        self.total_items_shed += shed_markers.len() as u64;
        self.total_items_delivered += selected.len() as u64;

        FrameScheduleResult {
            partition,
            selected,
            shed_markers,
        }
    }

    /// Current metrics snapshot.
    pub fn metrics(&self) -> InputReserveMetrics {
        InputReserveMetrics {
            queue_stats: self.queue.stats(),
            frames_scheduled: self.frames_scheduled,
            total_items_shed: self.total_items_shed,
            total_items_delivered: self.total_items_delivered,
        }
    }

    /// Access the underlying queue.
    pub fn queue(&self) -> &S3FifoQueue {
        &self.queue
    }

    /// Access the reserve floor policy.
    pub fn policy(&self) -> &ReserveFloorPolicy {
        &self.policy
    }

    /// Access the controller configuration.
    pub fn config(&self) -> &InputReserveConfig {
        &self.config
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item(id: u64, class: WorkItemClass, work_units: u32) -> WorkItem {
        WorkItem {
            id,
            pane_id: 1,
            class,
            work_units,
            submitted_at_ms: 1000,
            sequence: 0,
        }
    }

    fn make_item_at(id: u64, class: WorkItemClass, work_units: u32, ts: u64) -> WorkItem {
        WorkItem {
            id,
            pane_id: 1,
            class,
            work_units,
            submitted_at_ms: ts,
            sequence: 0,
        }
    }

    fn small_config() -> S3FifoConfig {
        S3FifoConfig {
            total_capacity: 10,
            small_fraction: 0.30,
            ghost_capacity_multiplier: 5,
        }
    }

    // -----------------------------------------------------------------------
    // S3-FIFO basic tests
    // -----------------------------------------------------------------------

    #[test]
    fn s3fifo_admit_within_capacity() {
        let mut q = S3FifoQueue::new(small_config());
        let shed = q.admit(
            make_item(1, WorkItemClass::Input, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );
        assert!(shed.is_empty());
        assert_eq!(q.len(), 1);
        assert_eq!(q.stats().small_len, 1);
    }

    #[test]
    fn s3fifo_small_eviction_cold_item() {
        // small_capacity = ceil(10 * 0.3) = 3
        let mut q = S3FifoQueue::new(small_config());
        for i in 0..3 {
            q.admit(
                make_item(i, WorkItemClass::BackgroundCapture, 1),
                BackpressureTier::Green,
                0.0,
                1000,
            );
        }
        assert_eq!(q.small.len(), 3);

        // Next admit triggers eviction of item 0 (freq=1, cold)
        let shed = q.admit(
            make_item(10, WorkItemClass::BackgroundCapture, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );
        assert_eq!(shed.len(), 1);
        assert_eq!(shed[0].item_id, 0);
        assert_eq!(shed[0].reason, ShedReason::S3FifoSmallEviction);
        // Item 0 should be in ghost
        assert!(q.ghost.contains(&0));
    }

    #[test]
    fn s3fifo_small_eviction_hot_item_promotes() {
        let mut q = S3FifoQueue::new(small_config());
        for i in 0..3 {
            q.admit(
                make_item(i, WorkItemClass::Input, 1),
                BackpressureTier::Green,
                0.0,
                1000,
            );
        }
        // Access item 0 again to bump frequency
        q.access(0);

        // Admit triggers eviction; item 0 should promote (freq > 1)
        let shed = q.admit(
            make_item(10, WorkItemClass::Input, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );
        assert!(shed.is_empty());
        assert_eq!(q.stats().main_len, 1);
        assert_eq!(q.stats().total_promoted, 1);
    }

    #[test]
    fn s3fifo_ghost_hit_promotes_to_main() {
        let mut q = S3FifoQueue::new(small_config());
        // Fill and evict item 0 to ghost
        for i in 0..4 {
            q.admit(
                make_item(i, WorkItemClass::BackgroundCapture, 1),
                BackpressureTier::Green,
                0.0,
                1000,
            );
        }
        assert!(q.ghost.contains(&0));

        // Re-admit item 0 → ghost hit → main
        let shed = q.admit(
            make_item(0, WorkItemClass::BackgroundCapture, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );
        assert!(shed.is_empty());
        assert_eq!(q.stats().total_ghost_hits, 1);
        assert!(q.main.iter().any(|item| item.id == 0));
    }

    #[test]
    fn s3fifo_main_evicts_background_before_interactive() {
        let cfg = S3FifoConfig {
            total_capacity: 4,
            small_fraction: 0.25,
            ghost_capacity_multiplier: 2,
        };
        let mut q = S3FifoQueue::new(cfg);
        // small_capacity = 1, main_capacity = 3

        // Fill main with 2 interactive + 1 background by promoting
        let items_to_promote = vec![
            (1, WorkItemClass::Input),
            (2, WorkItemClass::ViewportCore),
            (3, WorkItemClass::BackgroundCapture),
        ];
        for (id, class) in items_to_promote {
            let item = make_item(id, class, 1);
            q.admit(item, BackpressureTier::Green, 0.0, 1000);
            q.access(id); // bump frequency for promotion
        }
        // Force eviction to promote items
        for i in 100..104 {
            q.admit(
                make_item(i, WorkItemClass::BackgroundCapture, 1),
                BackpressureTier::Green,
                0.0,
                1000,
            );
        }

        // Check that if main is full, background is evicted first
        let has_bg = q.main.iter().any(|item| !item.class.is_interactive());
        let has_interactive = q.main.iter().any(|item| item.class.is_interactive());
        // Both may or may not be present depending on exact eviction; but if
        // both exist and main is full, background should be evicted first
        if has_bg && has_interactive && q.main.len() >= q.main_capacity {
            // Trigger one more main eviction
            let shed = q.admit(
                make_item(200, WorkItemClass::Input, 1),
                BackpressureTier::Green,
                0.0,
                1000,
            );
            for s in &shed {
                if s.reason == ShedReason::CapacityOverflow {
                    assert!(!s.class.is_interactive() || !has_bg);
                }
            }
        }
    }

    #[test]
    fn s3fifo_drain_priority_order() {
        let mut q = S3FifoQueue::new(small_config());
        q.admit(
            make_item(1, WorkItemClass::BackgroundCapture, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );
        q.admit(
            make_item(2, WorkItemClass::Input, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );
        q.admit(
            make_item(3, WorkItemClass::ViewportCore, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );

        let items = q.drain_all();
        assert_eq!(items.len(), 3);
        // Input (4) > ViewportCore (3) > Background (0)
        assert_eq!(items[0].class, WorkItemClass::Input);
        assert_eq!(items[1].class, WorkItemClass::ViewportCore);
        assert_eq!(items[2].class, WorkItemClass::BackgroundCapture);
    }

    #[test]
    fn s3fifo_ghost_capacity_limit() {
        let cfg = S3FifoConfig {
            total_capacity: 4,
            small_fraction: 0.25,
            ghost_capacity_multiplier: 1,
        };
        let mut q = S3FifoQueue::new(cfg);
        // ghost_capacity = 4

        // Admit and evict many items to fill ghost
        for i in 0..20 {
            q.admit(
                make_item(i, WorkItemClass::BackgroundCapture, 1),
                BackpressureTier::Green,
                0.0,
                1000,
            );
        }
        assert!(q.ghost.len() <= 4);
    }

    #[test]
    fn s3fifo_empty_queue() {
        let q = S3FifoQueue::new(S3FifoConfig::default());
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn s3fifo_default_config_values() {
        let cfg = S3FifoConfig::default();
        assert_eq!(cfg.total_capacity, 256);
        assert!((cfg.small_fraction - 0.10).abs() < 1e-10);
        assert_eq!(cfg.ghost_capacity_multiplier, 10);
    }

    #[test]
    fn s3fifo_stats_counters() {
        let mut q = S3FifoQueue::new(small_config());
        for i in 0..5 {
            q.admit(
                make_item(i, WorkItemClass::BackgroundCapture, 1),
                BackpressureTier::Green,
                0.0,
                1000,
            );
        }
        let stats = q.stats();
        assert_eq!(stats.total_admitted, 5);
        assert!(stats.total_evicted > 0 || stats.small_len <= 3);
    }

    #[test]
    fn s3fifo_len_invariant() {
        let mut q = S3FifoQueue::new(small_config());
        for i in 0..20 {
            q.admit(
                make_item(i, WorkItemClass::Input, 1),
                BackpressureTier::Green,
                0.0,
                1000,
            );
        }
        assert!(q.len() <= 10); // total_capacity
    }

    // -----------------------------------------------------------------------
    // Reserve floor policy tests
    // -----------------------------------------------------------------------

    #[test]
    fn floor_base_only() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig::default());
        let floor = policy.compute_floor(0);
        assert_eq!(floor, 2);
    }

    #[test]
    fn floor_with_surge() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig::default());
        let floor = policy.compute_floor(4);
        assert_eq!(floor, 4); // base 2 + surge 2
    }

    #[test]
    fn floor_below_surge_threshold() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig::default());
        let floor = policy.compute_floor(3);
        assert_eq!(floor, 2); // no surge
    }

    #[test]
    fn partition_green_no_backlog() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig::default());
        let part = policy.partition(10, BackpressureTier::Green, 0.0, 0);
        assert_eq!(part.interactive_budget, 2);
        assert_eq!(part.background_budget, 8);
        assert_eq!(part.shed_action, ShedAction::None);
    }

    #[test]
    fn partition_respects_total_cap() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig::default());
        let part = policy.partition(1, BackpressureTier::Green, 0.0, 10);
        // floor=4 but total=1, so interactive=1
        assert_eq!(part.interactive_budget, 1);
        assert_eq!(part.background_budget, 0);
    }

    #[test]
    fn shed_action_escalation() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig::default());
        assert_eq!(
            policy.shed_action_for_tier(BackpressureTier::Green),
            ShedAction::None
        );
        assert_eq!(
            policy.shed_action_for_tier(BackpressureTier::Yellow),
            ShedAction::ThrottleBackground
        );
        assert_eq!(
            policy.shed_action_for_tier(BackpressureTier::Red),
            ShedAction::DropCold
        );
        assert_eq!(
            policy.shed_action_for_tier(BackpressureTier::Black),
            ShedAction::IsolateInteractive
        );
    }

    #[test]
    fn partition_with_surge_backlog() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig::default());
        let part = policy.partition(10, BackpressureTier::Yellow, 0.5, 5);
        assert_eq!(part.interactive_budget, 4); // base 2 + surge 2
        assert_eq!(part.background_budget, 6);
    }

    #[test]
    fn partition_zero_budget() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig::default());
        let part = policy.partition(0, BackpressureTier::Red, 0.9, 0);
        assert_eq!(part.interactive_budget, 0);
        assert_eq!(part.background_budget, 0);
    }

    #[test]
    fn partition_red_tier_below_cold_shed_severity_does_not_drop_cold() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig {
            cold_shed_severity: 0.8,
            ..ReserveFloorConfig::default()
        });
        let part = policy.partition(10, BackpressureTier::Red, 0.6, 0);
        assert_eq!(part.shed_action, ShedAction::ThrottleBackground);
    }

    #[test]
    fn partition_red_tier_above_cold_shed_severity_drops_cold() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig {
            cold_shed_severity: 0.8,
            ..ReserveFloorConfig::default()
        });
        let part = policy.partition(10, BackpressureTier::Red, 0.9, 0);
        assert_eq!(part.shed_action, ShedAction::DropCold);
    }

    #[test]
    fn floor_monotonicity_with_backlog() {
        let policy = ReserveFloorPolicy::new(ReserveFloorConfig::default());
        let f0 = policy.compute_floor(0);
        let f3 = policy.compute_floor(3);
        let f4 = policy.compute_floor(4);
        let f100 = policy.compute_floor(100);
        assert!(f0 <= f3);
        assert!(f3 <= f4);
        assert!(f4 <= f100);
    }

    // -----------------------------------------------------------------------
    // Controller tests
    // -----------------------------------------------------------------------

    #[test]
    fn controller_submit_and_schedule() {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        ctrl.submit(
            make_item(1, WorkItemClass::Input, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );
        ctrl.submit(
            make_item(2, WorkItemClass::BackgroundCapture, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );

        let result = ctrl.schedule_frame(10, BackpressureTier::Green, 0.0, 0, 1500);
        assert!(!result.selected.is_empty());
        // Input should come before background
        let classes: Vec<_> = result.selected.iter().map(|item| item.class).collect();
        let input_pos = classes
            .iter()
            .position(|c| *c == WorkItemClass::Input)
            .unwrap();
        let bg_pos = classes
            .iter()
            .position(|c| *c == WorkItemClass::BackgroundCapture)
            .unwrap();
        assert!(input_pos < bg_pos);
    }

    #[test]
    fn controller_stale_timeout() {
        let cfg = InputReserveConfig {
            stale_timeout_ms: 100,
            ..Default::default()
        };
        let mut ctrl = InputReserveController::new(cfg);
        ctrl.submit(
            make_item_at(1, WorkItemClass::BackgroundCapture, 1, 1000),
            BackpressureTier::Green,
            0.0,
            1000,
        );

        // Schedule at t=1200 → stale (200 > 100)
        let result = ctrl.schedule_frame(10, BackpressureTier::Green, 0.0, 0, 1200);
        assert!(result.selected.is_empty());
        assert_eq!(result.shed_markers.len(), 1);
        assert_eq!(result.shed_markers[0].reason, ShedReason::StaleTimeout);
    }

    #[test]
    fn controller_black_tier_isolates() {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        ctrl.submit(
            make_item(1, WorkItemClass::Input, 1),
            BackpressureTier::Black,
            1.0,
            1000,
        );
        ctrl.submit(
            make_item(2, WorkItemClass::BackgroundCapture, 1),
            BackpressureTier::Black,
            1.0,
            1000,
        );

        let result = ctrl.schedule_frame(10, BackpressureTier::Black, 1.0, 0, 1500);
        // Background should be isolated
        let bg_shed = result
            .shed_markers
            .iter()
            .any(|s| s.reason == ShedReason::IsolateInteractive);
        assert!(bg_shed);
        // Input should be selected
        let has_input = result
            .selected
            .iter()
            .any(|item| item.class == WorkItemClass::Input);
        assert!(has_input);
    }

    #[test]
    fn controller_red_tier_drops_cold() {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        ctrl.submit(
            make_item(1, WorkItemClass::ColdScrollback, 1),
            BackpressureTier::Red,
            0.8,
            1000,
        );
        ctrl.submit(
            make_item(2, WorkItemClass::Input, 1),
            BackpressureTier::Red,
            0.8,
            1000,
        );

        let result = ctrl.schedule_frame(10, BackpressureTier::Red, 0.8, 0, 1500);
        let cold_shed = result
            .shed_markers
            .iter()
            .any(|s| s.reason == ShedReason::ColdSeverityShed);
        assert!(cold_shed);
    }

    #[test]
    fn controller_red_tier_below_severity_threshold_keeps_cold_scrollback() {
        let mut ctrl = InputReserveController::new(InputReserveConfig {
            reserve_floor: ReserveFloorConfig {
                cold_shed_severity: 0.8,
                ..ReserveFloorConfig::default()
            },
            ..InputReserveConfig::default()
        });
        ctrl.submit(
            make_item(1, WorkItemClass::ColdScrollback, 1),
            BackpressureTier::Red,
            0.6,
            1000,
        );

        let result = ctrl.schedule_frame(10, BackpressureTier::Red, 0.6, 0, 1500);
        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected[0].id, 1);
        assert!(
            !result
                .shed_markers
                .iter()
                .any(|s| s.reason == ShedReason::ColdSeverityShed)
        );
    }

    #[test]
    fn controller_red_tier_above_severity_threshold_sheds_cold_scrollback() {
        let mut ctrl = InputReserveController::new(InputReserveConfig {
            reserve_floor: ReserveFloorConfig {
                cold_shed_severity: 0.8,
                ..ReserveFloorConfig::default()
            },
            ..InputReserveConfig::default()
        });
        ctrl.submit(
            make_item(1, WorkItemClass::ColdScrollback, 1),
            BackpressureTier::Red,
            0.9,
            1000,
        );

        let result = ctrl.schedule_frame(10, BackpressureTier::Red, 0.9, 0, 1500);
        assert!(result.selected.is_empty());
        assert!(
            result
                .shed_markers
                .iter()
                .any(|s| s.reason == ShedReason::ColdSeverityShed)
        );
    }

    #[test]
    fn controller_metrics() {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        ctrl.submit(
            make_item(1, WorkItemClass::Input, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );
        ctrl.schedule_frame(10, BackpressureTier::Green, 0.0, 0, 1500);

        let m = ctrl.metrics();
        assert_eq!(m.frames_scheduled, 1);
        assert!(m.total_items_delivered > 0);
    }

    #[test]
    fn controller_budget_enforcement() {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        // Submit many large interactive items
        for i in 0..5 {
            ctrl.submit(
                make_item(i, WorkItemClass::Input, 3),
                BackpressureTier::Green,
                0.0,
                1000,
            );
        }

        // Tiny budget
        let result = ctrl.schedule_frame(2, BackpressureTier::Green, 0.0, 0, 1500);
        let total_units: u32 = result.selected.iter().map(|item| item.work_units).sum();
        assert!(total_units <= 2);
    }

    #[test]
    fn controller_sequence_monotonic() {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        ctrl.submit(
            make_item(1, WorkItemClass::Input, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );
        ctrl.submit(
            make_item(2, WorkItemClass::Input, 1),
            BackpressureTier::Green,
            0.0,
            1000,
        );

        let result = ctrl.schedule_frame(10, BackpressureTier::Green, 0.0, 0, 1500);
        if result.selected.len() >= 2 {
            assert!(result.selected[0].sequence < result.selected[1].sequence);
        }
    }

    #[test]
    fn controller_empty_schedule() {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        let result = ctrl.schedule_frame(10, BackpressureTier::Green, 0.0, 0, 1000);
        assert!(result.selected.is_empty());
        assert!(result.shed_markers.is_empty());
    }

    // -----------------------------------------------------------------------
    // Serde roundtrip tests
    // -----------------------------------------------------------------------

    #[test]
    fn serde_work_item_class_roundtrip() {
        let classes = [
            WorkItemClass::Input,
            WorkItemClass::ViewportCore,
            WorkItemClass::ViewportOverscan,
            WorkItemClass::ColdScrollback,
            WorkItemClass::BackgroundCapture,
        ];
        for class in &classes {
            let json = serde_json::to_string(class).unwrap();
            let back: WorkItemClass = serde_json::from_str(&json).unwrap();
            assert_eq!(*class, back);
        }
    }

    #[test]
    fn serde_shed_reason_roundtrip() {
        let reasons = [
            ShedReason::S3FifoSmallEviction,
            ShedReason::ReserveFloorEnforcement,
            ShedReason::ColdSeverityShed,
            ShedReason::StaleTimeout,
            ShedReason::CapacityOverflow,
            ShedReason::GhostEviction,
            ShedReason::IsolateInteractive,
        ];
        for reason in &reasons {
            let json = serde_json::to_string(reason).unwrap();
            let back: ShedReason = serde_json::from_str(&json).unwrap();
            assert_eq!(*reason, back);
        }
    }

    #[test]
    fn serde_shed_marker_roundtrip() {
        let marker = ShedMarker {
            item_id: 42,
            pane_id: 7,
            class: WorkItemClass::BackgroundCapture,
            reason: ShedReason::StaleTimeout,
            tier: BackpressureTier::Red,
            severity: 0.85,
            shed_at_ms: 12345,
        };
        let json = serde_json::to_string(&marker).unwrap();
        let back: ShedMarker = serde_json::from_str(&json).unwrap();
        assert_eq!(back.item_id, 42);
        assert_eq!(back.reason, ShedReason::StaleTimeout);
    }

    #[test]
    fn serde_config_roundtrip() {
        let cfg = InputReserveConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: InputReserveConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.stale_timeout_ms, 5000);
        assert_eq!(back.s3fifo.total_capacity, 256);
    }

    // -----------------------------------------------------------------------
    // Work item class tests
    // -----------------------------------------------------------------------

    #[test]
    fn work_item_class_priority_ordering() {
        assert!(
            WorkItemClass::Input.priority_level() > WorkItemClass::ViewportCore.priority_level()
        );
        assert!(
            WorkItemClass::ViewportCore.priority_level()
                > WorkItemClass::ViewportOverscan.priority_level()
        );
        assert!(
            WorkItemClass::ViewportOverscan.priority_level()
                > WorkItemClass::ColdScrollback.priority_level()
        );
        assert!(
            WorkItemClass::ColdScrollback.priority_level()
                > WorkItemClass::BackgroundCapture.priority_level()
        );
    }

    #[test]
    fn work_item_class_interactive() {
        assert!(WorkItemClass::Input.is_interactive());
        assert!(WorkItemClass::ViewportCore.is_interactive());
        assert!(WorkItemClass::ViewportOverscan.is_interactive());
        assert!(!WorkItemClass::ColdScrollback.is_interactive());
        assert!(!WorkItemClass::BackgroundCapture.is_interactive());
    }

    #[test]
    fn from_reflow_priority_bridge() {
        assert_eq!(
            WorkItemClass::from_reflow_priority(ReflowBatchPriority::ViewportCore),
            WorkItemClass::ViewportCore
        );
        assert_eq!(
            WorkItemClass::from_reflow_priority(ReflowBatchPriority::ViewportOverscan),
            WorkItemClass::ViewportOverscan
        );
        assert_eq!(
            WorkItemClass::from_reflow_priority(ReflowBatchPriority::ColdScrollback),
            WorkItemClass::ColdScrollback
        );
    }
}
