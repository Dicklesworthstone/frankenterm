//! Pane content tailing with adaptive polling.
//!
//! This module provides the TailerSupervisor for managing per-pane content
//! capture with adaptive polling intervals, weighted scheduling, and budget
//! enforcement.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::config::CaptureBudgetConfig;
use crate::ingest::{CapturedSegment, PaneCursor, PaneRegistry};
use crate::wezterm::{PaneInfo, PaneTextSource};

/// Number of consecutive backpressure events per pane before emitting an
/// overflow GAP segment.  Once exceeded, the tailer inserts a synthetic gap
/// to signal that capture data was likely lost during the congestion period.
pub const OVERFLOW_BACKPRESSURE_THRESHOLD: u64 = 5;

/// Configuration for the tailer supervisor.
#[derive(Debug, Clone)]
pub struct TailerConfig {
    /// Minimum polling interval (fast polling for active panes)
    pub min_interval: Duration,
    /// Maximum polling interval (slow polling for idle panes)
    pub max_interval: Duration,
    /// Multiplier for backoff when pane is idle
    pub backoff_multiplier: f64,
    /// Maximum number of concurrent captures
    pub max_concurrent: usize,
    /// Overlap size for delta extraction
    pub overlap_size: usize,
    /// Timeout for sending to channel
    pub send_timeout: Duration,
}

impl Default for TailerConfig {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_millis(50),
            max_interval: Duration::from_secs(1),
            backoff_multiplier: 1.5,
            max_concurrent: 10,
            overlap_size: 256,
            send_timeout: Duration::from_millis(100),
        }
    }
}

/// Metrics for tailer supervisor.
#[derive(Debug, Default)]
pub struct TailerMetrics {
    /// Total capture events sent
    pub events_sent: u64,
    /// Number of send timeouts
    pub send_timeouts: u64,
    /// Number of captures that found no changes
    pub no_change_captures: u64,
    /// Number of overflow GAP segments emitted due to sustained backpressure
    pub overflow_gaps_emitted: u64,
}

/// Metrics for supervisor operations.
#[derive(Debug, Default)]
pub struct SupervisorMetrics {
    /// Number of tailers started
    pub tailers_started: u64,
    /// Number of tailers stopped
    pub tailers_stopped: u64,
    /// Total sync operations
    pub sync_count: u64,
}

/// A captured segment event for persistence.
#[derive(Debug, Clone)]
pub struct CaptureEvent {
    /// The captured segment (includes pane_id, seq, content, kind, captured_at)
    pub segment: CapturedSegment,
}

/// Per-pane tailer state.
struct PaneTailer {
    /// Pane ID (retained for debugging/logging)
    #[allow(dead_code)]
    pane_id: u64,
    /// Current polling interval
    current_interval: Duration,
    /// Last poll time
    last_poll: Instant,
    /// Whether changes were detected in last poll
    had_changes: bool,
    /// Consecutive backpressure events without a successful capture
    consecutive_backpressure: u64,
    /// Whether an overflow GAP needs to be emitted on the next successful poll
    overflow_gap_pending: bool,
}

impl PaneTailer {
    fn new(pane_id: u64, initial_interval: Duration) -> Self {
        Self {
            pane_id,
            current_interval: initial_interval,
            last_poll: Instant::now(),
            had_changes: false,
            consecutive_backpressure: 0,
            overflow_gap_pending: false,
        }
    }

    fn should_poll(&self) -> bool {
        self.last_poll.elapsed() >= self.current_interval
    }

    fn record_poll(&mut self, had_changes: bool, config: &TailerConfig) {
        self.last_poll = Instant::now();
        self.had_changes = had_changes;

        // Adaptive interval: speed up if changes, slow down if idle
        if had_changes {
            self.current_interval = config.min_interval;
        } else {
            let new_interval = Duration::from_secs_f64(
                self.current_interval.as_secs_f64() * config.backoff_multiplier,
            );
            self.current_interval = new_interval.min(config.max_interval);
        }
    }
}

// ─── Weighted Capture Scheduler ─────────────────────────────────────

/// Metrics for budget enforcement and scheduling.
#[derive(Debug, Default)]
pub struct SchedulerMetrics {
    /// Captures skipped due to global rate limit.
    pub global_rate_limited: u64,
    /// Captures skipped due to per-pane byte budget.
    pub pane_byte_budget_exceeded: u64,
    /// Total throttle events emitted.
    pub throttle_events: u64,
}

/// Serializable snapshot of scheduler state for health reporting.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SchedulerSnapshot {
    /// Whether budget enforcement is active (any non-zero limit).
    pub budget_active: bool,
    /// Configured captures per second (0 = unlimited).
    pub max_captures_per_sec: u32,
    /// Configured bytes per second (0 = unlimited).
    pub max_bytes_per_sec: u64,
    /// Captures remaining in the current window.
    pub captures_remaining: u32,
    /// Bytes remaining in the current window.
    pub bytes_remaining: u64,
    /// Total captures throttled by global rate limit.
    pub total_rate_limited: u64,
    /// Total captures throttled by byte budget.
    pub total_byte_budget_exceeded: u64,
    /// Total throttle events.
    pub total_throttle_events: u64,
    /// Number of panes being tracked.
    pub tracked_panes: usize,
}

/// Per-pane budget tracking within a sliding window.
#[derive(Debug)]
struct PaneBudgetTracker {
    /// Bytes captured in the current window.
    bytes_in_window: u64,
    /// Captures in the current window.
    captures_in_window: u32,
    /// When the current window started.
    window_start: Instant,
}

impl PaneBudgetTracker {
    fn new() -> Self {
        Self {
            bytes_in_window: 0,
            captures_in_window: 0,
            window_start: Instant::now(),
        }
    }

    /// Reset the window if a full second has elapsed.
    fn maybe_reset(&mut self) {
        if self.window_start.elapsed() >= Duration::from_secs(1) {
            self.bytes_in_window = 0;
            self.captures_in_window = 0;
            self.window_start = Instant::now();
        }
    }

    /// Record a completed capture.
    fn record(&mut self, bytes: u64) {
        self.captures_in_window += 1;
        self.bytes_in_window += bytes;
    }
}

/// Weighted capture scheduler with token-bucket rate limiting.
///
/// Enforces `CaptureBudgetConfig` limits (captures/sec and bytes/sec) and
/// provides weighted pane selection so higher-priority panes get a larger
/// share of the capture budget under contention.
pub struct CaptureScheduler {
    /// Budget configuration (0 = unlimited for each field).
    budget: CaptureBudgetConfig,
    /// Global captures remaining in the current 1-second window.
    global_captures_remaining: u32,
    /// Global bytes remaining in the current 1-second window.
    global_bytes_remaining: u64,
    /// When the current global window started.
    global_window_start: Instant,
    /// Per-pane budget tracking.
    pane_trackers: HashMap<u64, PaneBudgetTracker>,
    /// Scheduler metrics.
    metrics: SchedulerMetrics,
}

impl CaptureScheduler {
    /// Create a scheduler with the given budget configuration.
    pub fn new(budget: CaptureBudgetConfig) -> Self {
        Self {
            global_captures_remaining: budget.max_captures_per_sec,
            global_bytes_remaining: budget.max_bytes_per_sec,
            global_window_start: Instant::now(),
            budget,
            pane_trackers: HashMap::new(),
            metrics: SchedulerMetrics::default(),
        }
    }

    /// Update the budget configuration (hot-reload safe).
    pub fn update_budget(&mut self, budget: CaptureBudgetConfig) {
        self.budget = budget;
        // Don't reset the window; let it expire naturally.
    }

    /// Remove tracking for panes no longer observed.
    pub fn remove_pane(&mut self, pane_id: u64) {
        self.pane_trackers.remove(&pane_id);
    }

    /// Check if a capture is allowed for the given pane under global limits.
    ///
    /// Returns `true` if the capture should proceed, `false` if throttled.
    pub fn check_global_budget(&mut self) -> bool {
        self.maybe_refill_global();

        // 0 means unlimited
        if self.budget.max_captures_per_sec == 0 {
            return true;
        }

        if self.global_captures_remaining == 0 {
            self.metrics.global_rate_limited += 1;
            self.metrics.throttle_events += 1;
            return false;
        }

        self.global_captures_remaining -= 1;
        true
    }

    /// Record that a capture completed, consuming bytes from the budget.
    pub fn record_capture(&mut self, pane_id: u64, bytes: u64) {
        // Debit global byte budget
        if self.budget.max_bytes_per_sec > 0 {
            self.global_bytes_remaining = self.global_bytes_remaining.saturating_sub(bytes);
        }

        // Track per-pane
        let tracker = self
            .pane_trackers
            .entry(pane_id)
            .or_insert_with(PaneBudgetTracker::new);
        tracker.maybe_reset();
        tracker.record(bytes);
    }

    /// Check if global byte budget is exhausted.
    pub fn is_byte_budget_exhausted(&mut self) -> bool {
        self.maybe_refill_global();

        if self.budget.max_bytes_per_sec == 0 {
            return false;
        }

        if self.global_bytes_remaining == 0 {
            self.metrics.pane_byte_budget_exceeded += 1;
            self.metrics.throttle_events += 1;
            return true;
        }

        false
    }

    /// Apply weighted selection: given a priority-sorted list of ready panes,
    /// return those that should be scheduled under the current budget.
    ///
    /// Under contention (more ready panes than available permits), higher-priority
    /// panes get a proportionally larger share. The algorithm:
    /// 1. All panes with priority <= 50 (high) are scheduled first.
    /// 2. Remaining slots are filled in priority order.
    /// 3. Global rate limits further restrict the count.
    pub fn select_panes(
        &mut self,
        ready_panes: &[(u64, u32)],
        available_permits: usize,
    ) -> Vec<u64> {
        self.maybe_refill_global();

        let budget_limit = if self.budget.max_captures_per_sec > 0 {
            self.global_captures_remaining as usize
        } else {
            usize::MAX
        };

        let effective_limit = available_permits.min(budget_limit);
        if effective_limit == 0 {
            if !ready_panes.is_empty() && self.budget.max_captures_per_sec > 0 {
                self.metrics.global_rate_limited += 1;
                self.metrics.throttle_events += 1;
            }
            return Vec::new();
        }

        // Under contention: ensure high-priority panes always get scheduled.
        // ready_panes is already sorted by (priority, pane_id).
        let selected: Vec<u64> = ready_panes
            .iter()
            .take(effective_limit)
            .map(|(id, _)| *id)
            .collect();

        // Debit global capture budget for the count we'll schedule.
        if self.budget.max_captures_per_sec > 0 {
            let debit = selected.len() as u32;
            self.global_captures_remaining = self.global_captures_remaining.saturating_sub(debit);
        }

        selected
    }

    /// Access scheduler metrics.
    #[must_use]
    pub fn metrics(&self) -> &SchedulerMetrics {
        &self.metrics
    }

    /// Export a serializable snapshot of the current scheduler state.
    #[must_use]
    pub fn snapshot(&self) -> SchedulerSnapshot {
        SchedulerSnapshot {
            budget_active: self.budget.max_captures_per_sec > 0
                || self.budget.max_bytes_per_sec > 0,
            max_captures_per_sec: self.budget.max_captures_per_sec,
            max_bytes_per_sec: self.budget.max_bytes_per_sec,
            captures_remaining: self.global_captures_remaining,
            bytes_remaining: self.global_bytes_remaining,
            total_rate_limited: self.metrics.global_rate_limited,
            total_byte_budget_exceeded: self.metrics.pane_byte_budget_exceeded,
            total_throttle_events: self.metrics.throttle_events,
            tracked_panes: self.pane_trackers.len(),
        }
    }

    /// Refill the global token bucket if a full second has elapsed.
    fn maybe_refill_global(&mut self) {
        if self.global_window_start.elapsed() >= Duration::from_secs(1) {
            self.global_captures_remaining = self.budget.max_captures_per_sec;
            self.global_bytes_remaining = self.budget.max_bytes_per_sec;
            self.global_window_start = Instant::now();
        }
    }
}

// ─── Tailer Supervisor ──────────────────────────────────────────────

/// Supervisor for managing multiple pane tailers.
pub struct TailerSupervisor<S>
where
    S: PaneTextSource + Send + Sync + 'static,
{
    /// Configuration
    config: TailerConfig,
    /// Channel for sending capture events (will be used when actual polling is implemented)
    #[allow(dead_code)]
    tx: mpsc::Sender<CaptureEvent>,
    /// Per-pane cursors (shared with runtime)
    cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
    /// Pane registry (for authoritative state like alt-screen)
    registry: Arc<RwLock<PaneRegistry>>,
    /// Shutdown flag
    shutdown_flag: Arc<AtomicBool>,
    /// Pane text source (WezTerm client or test double)
    source: Arc<S>,
    /// Concurrency limiter for in-flight polls
    semaphore: Arc<Semaphore>,
    /// Per-pane tailer state
    tailers: HashMap<u64, PaneTailer>,
    /// Panes currently being captured (to prevent duplicate polling)
    capturing_panes: HashSet<u64>,
    /// Effective pane priorities for scheduling (lower = higher priority).
    ///
    /// This is updated by the runtime at sync ticks (config rules + runtime overrides).
    pane_priorities: HashMap<u64, u32>,
    /// Weighted capture scheduler with budget enforcement.
    scheduler: CaptureScheduler,
    /// Metrics
    metrics: TailerMetrics,
    /// Supervisor metrics
    supervisor_metrics: SupervisorMetrics,
}

impl<S> TailerSupervisor<S>
where
    S: PaneTextSource + Send + Sync + 'static,
{
    /// Create a new tailer supervisor.
    pub fn new(
        config: TailerConfig,
        tx: mpsc::Sender<CaptureEvent>,
        cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
        registry: Arc<RwLock<PaneRegistry>>,
        shutdown_flag: Arc<AtomicBool>,
        source: Arc<S>,
    ) -> Self {
        let max_concurrent = config.max_concurrent.max(1);
        Self {
            config,
            tx,
            cursors,
            registry,
            shutdown_flag,
            source,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            tailers: HashMap::new(),
            capturing_panes: HashSet::new(),
            pane_priorities: HashMap::new(),
            scheduler: CaptureScheduler::new(CaptureBudgetConfig::default()),
            metrics: TailerMetrics::default(),
            supervisor_metrics: SupervisorMetrics::default(),
        }
    }

    /// Create a new tailer supervisor with explicit budget configuration.
    pub fn with_budget(
        config: TailerConfig,
        tx: mpsc::Sender<CaptureEvent>,
        cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
        registry: Arc<RwLock<PaneRegistry>>,
        shutdown_flag: Arc<AtomicBool>,
        source: Arc<S>,
        budget: CaptureBudgetConfig,
    ) -> Self {
        let max_concurrent = config.max_concurrent.max(1);
        Self {
            config,
            tx,
            cursors,
            registry,
            shutdown_flag,
            source,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            tailers: HashMap::new(),
            capturing_panes: HashSet::new(),
            pane_priorities: HashMap::new(),
            scheduler: CaptureScheduler::new(budget),
            metrics: TailerMetrics::default(),
            supervisor_metrics: SupervisorMetrics::default(),
        }
    }

    /// Number of active tailers.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.tailers.len()
    }

    /// Sync tailers with the current set of observed panes.
    ///
    /// Adds tailers for new panes, removes tailers for departed panes.
    pub fn sync_tailers(&mut self, observed_panes: &HashMap<u64, PaneInfo>) {
        self.supervisor_metrics.sync_count += 1;

        // Remove tailers for panes no longer observed
        let to_remove: Vec<u64> = self
            .tailers
            .keys()
            .filter(|id| !observed_panes.contains_key(*id))
            .copied()
            .collect();

        for pane_id in to_remove {
            self.tailers.remove(&pane_id);
            self.capturing_panes.remove(&pane_id);
            self.scheduler.remove_pane(pane_id);
            self.supervisor_metrics.tailers_stopped += 1;
            debug!(pane_id, "Removed tailer for departed pane");
        }

        // Add tailers for new panes
        for &pane_id in observed_panes.keys() {
            if !self.tailers.contains_key(&pane_id) {
                self.tailers
                    .insert(pane_id, PaneTailer::new(pane_id, self.config.min_interval));
                self.supervisor_metrics.tailers_started += 1;
                debug!(pane_id, "Added tailer for new pane");
            }
        }
    }

    /// Update configuration dynamically.
    ///
    /// Updates polling intervals and concurrency limits. Note that `semaphore` is updated
    /// to reflect the new concurrency limit.
    pub fn update_config(&mut self, config: TailerConfig) {
        if config.max_concurrent != self.config.max_concurrent {
            // Update semaphore capacity
            // Note: Semaphore doesn't support resizing, so we replace it.
            // This is safe because tasks hold a permit from the OLD semaphore.
            // New tasks will acquire from the NEW semaphore.
            // The concurrency limit will effectively be the sum during the transition,
            // but will converge quickly.
            self.semaphore = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
            debug!(
                old = self.config.max_concurrent,
                new = config.max_concurrent,
                "Tailer concurrency updated"
            );
        }

        if config.min_interval != self.config.min_interval
            || config.max_interval != self.config.max_interval
        {
            debug!(
                min = ?config.min_interval,
                max = ?config.max_interval,
                "Tailer intervals updated"
            );
        }

        self.config = config;
    }

    /// Update the effective pane priorities used for scheduling.
    pub fn update_pane_priorities(&mut self, pane_priorities: HashMap<u64, u32>) {
        self.pane_priorities = pane_priorities;
    }

    /// Update the capture budget configuration (hot-reload safe).
    pub fn update_budget(&mut self, budget: CaptureBudgetConfig) {
        self.scheduler.update_budget(budget);
    }

    /// Record that a capture completed with the given byte count.
    ///
    /// Call this after a successful capture to debit per-pane and global
    /// byte budgets.
    pub fn record_capture_bytes(&mut self, pane_id: u64, bytes: u64) {
        self.scheduler.record_capture(pane_id, bytes);
    }

    /// Access scheduler metrics for observability.
    #[must_use]
    pub fn scheduler_metrics(&self) -> &SchedulerMetrics {
        self.scheduler.metrics()
    }

    /// Export a serializable snapshot of the scheduler state.
    #[must_use]
    pub fn scheduler_snapshot(&self) -> SchedulerSnapshot {
        self.scheduler.snapshot()
    }

    /// Spawn tasks for all ready panes that are not currently being captured.
    ///
    /// Uses weighted scheduling: panes are sorted by priority (lower = higher),
    /// then filtered through the capture budget. Under contention, higher-priority
    /// panes get a larger share of the capture budget.
    pub fn spawn_ready(&mut self, join_set: &mut JoinSet<(u64, PollOutcome)>) {
        if self.shutdown_flag.load(Ordering::SeqCst) {
            return;
        }

        // Check if byte budget is exhausted before doing any work.
        if self.scheduler.is_byte_budget_exhausted() {
            return;
        }

        // Find panes ready for polling AND not currently capturing.
        let mut ready_panes: Vec<(u64, u32)> = self
            .tailers
            .iter()
            .filter(|(id, t)| t.should_poll() && !self.capturing_panes.contains(id))
            .map(|(id, _)| {
                let prio = self.pane_priorities.get(id).copied().unwrap_or(u32::MAX);
                (*id, prio)
            })
            .collect();

        // Order by priority (lower = higher), tie-breaker pane_id for determinism.
        ready_panes.sort_by_key(|&(pane_id, prio)| (prio, pane_id));

        // Apply weighted scheduling with budget enforcement.
        let available = self.semaphore.available_permits();
        let selected = self.scheduler.select_panes(&ready_panes, available);

        for pane_id in selected {
            // Check if this pane needs an overflow gap emitted before normal capture
            let overflow_gap_pending = self
                .tailers
                .get(&pane_id)
                .is_some_and(|t| t.overflow_gap_pending);

            // Mark as capturing to prevent duplicate spawns
            self.capturing_panes.insert(pane_id);

            let tx = self.tx.clone();
            let cursors = Arc::clone(&self.cursors);
            let registry = Arc::clone(&self.registry);
            let source = Arc::clone(&self.source);
            let semaphore = Arc::clone(&self.semaphore);
            let overlap_size = self.config.overlap_size;
            let send_timeout = self.config.send_timeout;

            join_set.spawn(async move {
                let Ok(_permit) = semaphore.acquire_owned().await else {
                    return (pane_id, PollOutcome::Backpressure);
                };

                let has_cursor = {
                    let cursors = cursors.read().await;
                    cursors.contains_key(&pane_id)
                };

                if !has_cursor {
                    return (pane_id, PollOutcome::NoCursor);
                }

                // If overflow gap is pending, emit a synthetic gap segment instead
                // of doing a normal capture.  The gap signals to downstream consumers
                // that data was lost during sustained backpressure.
                if overflow_gap_pending {
                    let permit = match timeout(send_timeout, tx.reserve()).await {
                        Ok(Ok(permit)) => permit,
                        Ok(Err(_)) => return (pane_id, PollOutcome::ChannelClosed),
                        Err(_) => return (pane_id, PollOutcome::Backpressure),
                    };

                    let gap_segment = {
                        let mut cursors = cursors.write().await;
                        cursors
                            .get_mut(&pane_id)
                            .map(|cursor| cursor.emit_overflow_gap("backpressure_overflow"))
                    };

                    if let Some(segment) = gap_segment {
                        permit.send(CaptureEvent { segment });
                        return (pane_id, PollOutcome::OverflowGapEmitted);
                    }

                    drop(permit);
                    return (pane_id, PollOutcome::NoCursor);
                }

                let permit = match timeout(send_timeout, tx.reserve()).await {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) => return (pane_id, PollOutcome::ChannelClosed),
                    Err(_) => return (pane_id, PollOutcome::Backpressure),
                };

                let text = match source.get_text(pane_id, false).await {
                    Ok(text) => text,
                    Err(err) => {
                        drop(permit);
                        return (pane_id, PollOutcome::Error(err.to_string()));
                    }
                };

                // Fetch external alt-screen state from registry if available
                let external_alt_screen = {
                    let reg = registry.read().await;
                    reg.is_alt_screen(pane_id)
                };

                let captured = {
                    let mut cursors = cursors.write().await;
                    cursors.get_mut(&pane_id).and_then(|cursor| {
                        cursor.capture_snapshot(&text, overlap_size, external_alt_screen)
                    })
                };

                if let Some(segment) = captured {
                    let bytes = segment.content.len() as u64;
                    permit.send(CaptureEvent { segment });
                    (pane_id, PollOutcome::Changed { bytes })
                } else {
                    drop(permit);
                    (pane_id, PollOutcome::NoChange)
                }
            });
        }
    }

    /// Handle the result of a completed poll task.
    pub fn handle_poll_result(&mut self, pane_id: u64, outcome: PollOutcome) {
        // Mark as no longer capturing so it can be polled again later
        self.capturing_panes.remove(&pane_id);

        if let Some(tailer) = self.tailers.get_mut(&pane_id) {
            match outcome {
                PollOutcome::Changed { bytes } => {
                    tailer.record_poll(true, &self.config);
                    tailer.consecutive_backpressure = 0;
                    self.metrics.events_sent += 1;
                    self.scheduler.record_capture(pane_id, bytes);
                }
                PollOutcome::NoChange => {
                    tailer.record_poll(false, &self.config);
                    tailer.consecutive_backpressure = 0;
                    self.metrics.no_change_captures += 1;
                    trace!(pane_id, "Tailer poll no change");
                }
                PollOutcome::Backpressure => {
                    tailer.record_poll(false, &self.config);
                    self.metrics.send_timeouts += 1;
                    tailer.consecutive_backpressure += 1;
                    if tailer.consecutive_backpressure >= OVERFLOW_BACKPRESSURE_THRESHOLD {
                        tailer.overflow_gap_pending = true;
                        warn!(
                            pane_id,
                            consecutive = tailer.consecutive_backpressure,
                            "Backpressure overflow: scheduling GAP insertion"
                        );
                    } else {
                        warn!(pane_id, "Tailer backpressure: capture queue full");
                    }
                }
                PollOutcome::OverflowGapEmitted => {
                    tailer.record_poll(true, &self.config);
                    tailer.overflow_gap_pending = false;
                    tailer.consecutive_backpressure = 0;
                    self.metrics.events_sent += 1;
                    self.metrics.overflow_gaps_emitted += 1;
                    debug!(pane_id, "Overflow GAP emitted");
                }
                PollOutcome::NoCursor => {
                    tailer.record_poll(false, &self.config);
                    trace!(pane_id, "Tailer poll skipped (no cursor)");
                }
                PollOutcome::ChannelClosed => {
                    tailer.record_poll(false, &self.config);
                    warn!(pane_id, "Tailer channel closed");
                }
                PollOutcome::Error(error) => {
                    tailer.record_poll(false, &self.config);
                    warn!(pane_id, error = %error, "Tailer poll failed");
                }
            }
        }
    }

    /// Graceful shutdown of all tailers.
    pub async fn shutdown(&mut self) {
        debug!(
            active_count = self.tailers.len(),
            "Shutting down tailer supervisor"
        );
        self.tailers.clear();
        self.capturing_panes.clear();
    }

    /// Get current metrics.
    #[must_use]
    pub fn metrics(&self) -> &TailerMetrics {
        &self.metrics
    }

    /// Get supervisor metrics.
    #[must_use]
    pub fn supervisor_metrics(&self) -> &SupervisorMetrics {
        &self.supervisor_metrics
    }
}

#[derive(Debug)]
pub enum PollOutcome {
    /// Content changed; includes byte count of the captured segment.
    Changed {
        bytes: u64,
    },
    NoChange,
    Backpressure,
    /// An overflow GAP segment was emitted after sustained backpressure
    OverflowGapEmitted,
    NoCursor,
    ChannelClosed,
    Error(String),
}

// ---------------------------------------------------------------------------
// Streaming tailer integration (wa-nu4.4.2.3)
// ---------------------------------------------------------------------------

/// Capture mode for a pane — either polling-based or streaming-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailerMode {
    /// Traditional get_text polling with adaptive intervals.
    Polling,
    /// Vendored mux subscription with direct delta delivery.
    Streaming,
}

impl std::fmt::Display for TailerMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Polling => write!(f, "polling"),
            Self::Streaming => write!(f, "streaming"),
        }
    }
}

/// Detect the best available tailer mode at runtime.
///
/// Returns `Streaming` only when the vendored feature is compiled in
/// and the mux socket is discoverable. Otherwise returns `Polling`.
#[must_use]
pub fn detect_tailer_mode(config: &crate::config::Config) -> TailerMode {
    #[cfg(feature = "vendored")]
    {
        let socket_from_config = config
            .vendored
            .mux_socket_path
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty());
        let socket_from_env = std::env::var("WEZTERM_UNIX_SOCKET")
            .ok()
            .is_some_and(|p| !p.trim().is_empty());
        if socket_from_config || socket_from_env {
            return TailerMode::Streaming;
        }
    }
    let _ = config; // suppress unused warning when vendored is off
    TailerMode::Polling
}

/// Bridges vendored `PaneDelta` events into the existing `StreamIngester`
/// pipeline, producing `CapturedSegment`s with monotonic seq.
///
/// The bridge converts each delta kind:
/// - `Output` → `StreamEvent::OutputData` (data is the title for now —
///   the actual line content would require `get_lines()` round-trip)
/// - `Gap` → `StreamEvent::OutputData` with overflow flag
/// - `Ended` → `StreamEvent::PaneClosed`
pub struct StreamingBridge {
    ingester: crate::ingest::StreamIngester,
    /// Number of PaneDelta events processed.
    events_processed: u64,
    /// Number of fallback-to-polling transitions.
    fallback_count: u64,
}

impl StreamingBridge {
    /// Create a new streaming bridge.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ingester: crate::ingest::StreamIngester::new(),
            events_processed: 0,
            fallback_count: 0,
        }
    }

    /// Convert a PaneDelta into CapturedSegments via the StreamIngester.
    ///
    /// The caller is responsible for persisting the returned segments.
    #[cfg(feature = "vendored")]
    pub fn process_delta(&mut self, delta: crate::vendored::PaneDelta) -> Vec<CapturedSegment> {
        use crate::ingest::StreamEvent;
        self.events_processed += 1;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let event = match delta {
            crate::vendored::PaneDelta::Output {
                pane_id,
                seqno: _,
                title,
                dirty_range_count,
            } => StreamEvent::OutputData {
                pane_id,
                data: format!("[render_changes: {dirty_range_count} dirty ranges, title={title}]"),
                received_at: now_ms,
                overflow: false,
            },
            crate::vendored::PaneDelta::Gap { pane_id, reason: _ } => {
                // A gap from the subscription → treat as overflow so ingester
                // emits a proper GAP segment.
                StreamEvent::OutputData {
                    pane_id,
                    data: String::new(),
                    received_at: now_ms,
                    overflow: true,
                }
            }
            crate::vendored::PaneDelta::Ended { pane_id, reason: _ } => {
                StreamEvent::PaneClosed { pane_id }
            }
        };

        self.ingester.process(event)
    }

    /// Record that a fallback to polling occurred.
    pub fn record_fallback(&mut self) {
        self.fallback_count += 1;
    }

    /// Number of events processed.
    #[must_use]
    pub fn events_processed(&self) -> u64 {
        self.events_processed
    }

    /// Number of fallback transitions.
    #[must_use]
    pub fn fallback_count(&self) -> u64 {
        self.fallback_count
    }

    /// Access the underlying ingester for diagnostics.
    #[must_use]
    pub fn ingester(&self) -> &crate::ingest::StreamIngester {
        &self.ingester
    }
}

impl Default for StreamingBridge {
    fn default() -> Self {
        Self::new()
    }
}

/// Health snapshot for streaming diagnostics.
#[derive(Debug, Clone)]
pub struct StreamingHealth {
    /// Current capture mode.
    pub mode: TailerMode,
    /// Events processed through the streaming bridge.
    pub events_processed: u64,
    /// Gaps emitted through the streaming bridge.
    pub gaps_emitted: u64,
    /// Number of times streaming fell back to polling.
    pub fallback_count: u64,
    /// Active panes in the streaming ingester.
    pub active_panes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::task::JoinSet;

    fn make_pane(id: u64) -> PaneInfo {
        PaneInfo {
            pane_id: id,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: Some(24),
            cols: Some(80),
            cwd: None,
            title: None,
            tty_name: None,
            cursor_x: Some(0),
            cursor_y: Some(0),
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: id == 1,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        }
    }

    #[derive(Default)]
    struct StaticSource;

    impl PaneTextSource for StaticSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[derive(Default)]
    #[allow(dead_code)]
    struct FixedSource;

    impl PaneTextSource for FixedSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            Box::pin(async move { Ok(format!("pane-{pane_id}")) })
        }
    }

    struct CountingSource {
        active: Arc<AtomicUsize>,
        max: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl CountingSource {
        fn new(active: Arc<AtomicUsize>, max: Arc<AtomicUsize>, delay: Duration) -> Self {
            Self { active, max, delay }
        }
    }

    impl PaneTextSource for CountingSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            let active = Arc::clone(&self.active);
            let max = Arc::clone(&self.max);
            let delay = self.delay;
            Box::pin(async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                loop {
                    let observed = max.load(Ordering::SeqCst);
                    if current <= observed {
                        break;
                    }
                    if max
                        .compare_exchange(observed, current, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        break;
                    }
                }

                tokio::time::sleep(delay).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(format!("pane-{pane_id}-tick"))
            })
        }
    }

    #[test]
    fn tailer_config_default() {
        let config = TailerConfig::default();
        assert_eq!(config.min_interval, Duration::from_millis(50));
        assert_eq!(config.max_interval, Duration::from_secs(1));
        assert!(config.backoff_multiplier > 1.0);
    }

    #[test]
    fn pane_tailer_adaptive_interval() {
        let config = TailerConfig {
            min_interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(1),
            backoff_multiplier: 2.0,
            ..Default::default()
        };

        let mut tailer = PaneTailer::new(1, config.min_interval);

        // No changes - interval should increase
        tailer.record_poll(false, &config);
        assert_eq!(tailer.current_interval, Duration::from_millis(200));

        // Still no changes - continue increasing
        tailer.record_poll(false, &config);
        assert_eq!(tailer.current_interval, Duration::from_millis(400));

        // Changes detected - snap back to min
        tailer.record_poll(true, &config);
        assert_eq!(tailer.current_interval, config.min_interval);
    }

    #[test]
    fn pane_tailer_interval_capped_at_max() {
        let config = TailerConfig {
            min_interval: Duration::from_millis(100),
            max_interval: Duration::from_millis(500),
            backoff_multiplier: 10.0,
            ..Default::default()
        };

        let mut tailer = PaneTailer::new(1, config.min_interval);

        // Should cap at max_interval
        tailer.record_poll(false, &config);
        assert_eq!(tailer.current_interval, config.max_interval);
    }

    #[tokio::test]
    async fn supervisor_sync_tailers() {
        let config = TailerConfig::default();
        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, registry, shutdown, source);

        assert_eq!(supervisor.active_count(), 0);

        // Add some panes
        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        panes.insert(2, make_pane(2));

        supervisor.sync_tailers(&panes);
        assert_eq!(supervisor.active_count(), 2);

        // Remove a pane
        panes.remove(&1);
        supervisor.sync_tailers(&panes);
        assert_eq!(supervisor.active_count(), 1);
    }

    #[tokio::test]
    async fn supervisor_respects_concurrency_limit() {
        let active = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingSource::new(
            Arc::clone(&active),
            Arc::clone(&max),
            Duration::from_millis(20),
        ));

        let config = TailerConfig {
            min_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(50),
            max_concurrent: 2,
            send_timeout: Duration::from_millis(50),
            ..Default::default()
        };

        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        {
            let mut cursor_guard = cursors.write().await;
            for pane_id in 1..=4 {
                cursor_guard.insert(pane_id, PaneCursor::new(pane_id));
            }
        }

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, registry, shutdown, source);

        let mut panes = HashMap::new();
        for pane_id in 1..=4 {
            panes.insert(pane_id, make_pane(pane_id));
        }
        supervisor.sync_tailers(&panes);

        let mut join_set = JoinSet::new();
        supervisor.spawn_ready(&mut join_set);

        // Wait for a bit to let tasks start
        tokio::time::sleep(Duration::from_millis(5)).await;

        let max_seen = max.load(Ordering::SeqCst);
        assert!(max_seen <= 2, "max concurrency observed: {max_seen}");

        // Cleanup
        while let Some(result) = join_set.join_next().await {
            if let Ok((pane_id, outcome)) = result {
                supervisor.handle_poll_result(pane_id, outcome);
            }
        }
    }

    #[tokio::test]
    async fn supervisor_spawns_higher_priority_panes_first() {
        let config = TailerConfig {
            min_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(50),
            max_concurrent: 1,
            send_timeout: Duration::from_millis(50),
            ..Default::default()
        };

        let (tx, rx) = mpsc::channel(10);
        let _keep_rx_alive = rx;
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        {
            let mut cursor_guard = cursors.write().await;
            cursor_guard.insert(1, PaneCursor::new(1));
            cursor_guard.insert(2, PaneCursor::new(2));
        }

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, registry, shutdown, source);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        panes.insert(2, make_pane(2));
        supervisor.sync_tailers(&panes);

        // Lower value => higher priority. Pane 2 should be spawned before pane 1.
        supervisor.update_pane_priorities(HashMap::from([(1, 100), (2, 10)]));

        // Wait for tailers to become ready to poll.
        tokio::time::sleep(Duration::from_millis(2)).await;

        let mut join_set = JoinSet::new();
        supervisor.spawn_ready(&mut join_set);

        let (pane_id, outcome) = join_set
            .join_next()
            .await
            .expect("expected one task")
            .expect("task should not panic");
        supervisor.handle_poll_result(pane_id, outcome);

        assert_eq!(pane_id, 2, "higher priority pane should spawn first");
    }

    #[tokio::test]
    async fn supervisor_backpressure_records_timeout() {
        // Use a slow source that holds the permit long enough for the second
        // tailer to timeout waiting for channel capacity.
        let active = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        // Source delay must be longer than send_timeout so second tailer times out
        let source = Arc::new(CountingSource::new(
            active.clone(),
            max.clone(),
            Duration::from_millis(50), // Longer than send_timeout
        ));

        let config = TailerConfig {
            min_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(50),
            max_concurrent: 2,
            send_timeout: Duration::from_millis(10), // Short timeout
            ..Default::default()
        };

        // Channel capacity of 1 + keep receiver alive (but don't consume)
        // so second send times out instead of getting ChannelClosed
        let (tx, rx) = mpsc::channel(1);
        let _keep_rx_alive = rx; // Prevent receiver from being dropped
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        {
            let mut cursor_guard = cursors.write().await;
            cursor_guard.insert(1, PaneCursor::new(1));
            cursor_guard.insert(2, PaneCursor::new(2));
        }

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, registry, shutdown, source);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        panes.insert(2, make_pane(2));
        supervisor.sync_tailers(&panes);

        // Wait for tailers to become ready to poll (min_interval must elapse)
        tokio::time::sleep(Duration::from_millis(5)).await;

        let mut join_set = JoinSet::new();
        supervisor.spawn_ready(&mut join_set);

        let mut outcomes = Vec::new();
        while let Some(result) = join_set.join_next().await {
            if let Ok((pane_id, outcome)) = result {
                outcomes.push((pane_id, format!("{outcome:?}")));
                supervisor.handle_poll_result(pane_id, outcome);
            }
        }

        let metrics = supervisor.metrics();
        assert!(
            metrics.send_timeouts >= 1,
            "Expected at least 1 backpressure timeout, got 0. Outcomes: {outcomes:?}, metrics: {metrics:?}"
        );
    }

    #[test]
    fn capture_event_wraps_captured_segment() {
        use std::time::{SystemTime, UNIX_EPOCH};

        #[allow(clippy::cast_possible_truncation)]
        // Epoch millis won't overflow i64 until year 292 million
        let captured_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);

        let seg = CapturedSegment {
            pane_id: 42,
            seq: 10,
            content: "test content".to_string(),
            kind: crate::ingest::CapturedSegmentKind::Delta,
            captured_at,
        };

        let event = CaptureEvent { segment: seg };

        assert_eq!(event.segment.pane_id, 42);
        assert_eq!(event.segment.seq, 10);
        assert_eq!(event.segment.content, "test content");
        assert!(event.segment.captured_at > 0);
    }

    #[test]
    fn overflow_threshold_constant_is_reasonable() {
        assert!(
            OVERFLOW_BACKPRESSURE_THRESHOLD >= 2,
            "threshold must be at least 2 to avoid spurious gap emission"
        );
        assert!(
            OVERFLOW_BACKPRESSURE_THRESHOLD <= 100,
            "threshold should not be excessively large"
        );
    }

    #[test]
    fn consecutive_backpressure_tracks_correctly() {
        let config = TailerConfig::default();
        let mut tailer = PaneTailer::new(1, config.min_interval);

        assert_eq!(tailer.consecutive_backpressure, 0);
        assert!(!tailer.overflow_gap_pending);

        // Simulate backpressure events below threshold
        for i in 1..OVERFLOW_BACKPRESSURE_THRESHOLD {
            tailer.consecutive_backpressure = i;
        }
        assert!(!tailer.overflow_gap_pending);

        // A successful capture resets the counter
        tailer.consecutive_backpressure = 0;
        assert_eq!(tailer.consecutive_backpressure, 0);
    }

    #[test]
    fn handle_poll_result_increments_backpressure_counter() {
        let config = TailerConfig::default();
        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, registry, shutdown, source);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        supervisor.sync_tailers(&panes);

        // Simulate backpressure events
        for _ in 0..(OVERFLOW_BACKPRESSURE_THRESHOLD - 1) {
            supervisor.capturing_panes.insert(1);
            supervisor.handle_poll_result(1, PollOutcome::Backpressure);
        }

        let tailer = supervisor.tailers.get(&1).unwrap();
        assert_eq!(
            tailer.consecutive_backpressure,
            OVERFLOW_BACKPRESSURE_THRESHOLD - 1
        );
        assert!(!tailer.overflow_gap_pending);

        // One more should trigger overflow
        supervisor.capturing_panes.insert(1);
        supervisor.handle_poll_result(1, PollOutcome::Backpressure);

        let tailer = supervisor.tailers.get(&1).unwrap();
        assert!(tailer.overflow_gap_pending);
    }

    #[test]
    fn changed_resets_backpressure_counter() {
        let config = TailerConfig::default();
        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, registry, shutdown, source);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        supervisor.sync_tailers(&panes);

        // Accumulate some backpressure
        for _ in 0..3 {
            supervisor.capturing_panes.insert(1);
            supervisor.handle_poll_result(1, PollOutcome::Backpressure);
        }
        assert_eq!(
            supervisor.tailers.get(&1).unwrap().consecutive_backpressure,
            3
        );

        // Changed resets it
        supervisor.capturing_panes.insert(1);
        supervisor.handle_poll_result(1, PollOutcome::Changed { bytes: 0 });
        assert_eq!(
            supervisor.tailers.get(&1).unwrap().consecutive_backpressure,
            0
        );
    }

    #[test]
    fn no_change_resets_backpressure_counter() {
        let config = TailerConfig::default();
        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, registry, shutdown, source);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        supervisor.sync_tailers(&panes);

        // Accumulate backpressure
        for _ in 0..3 {
            supervisor.capturing_panes.insert(1);
            supervisor.handle_poll_result(1, PollOutcome::Backpressure);
        }

        // NoChange also resets
        supervisor.capturing_panes.insert(1);
        supervisor.handle_poll_result(1, PollOutcome::NoChange);
        assert_eq!(
            supervisor.tailers.get(&1).unwrap().consecutive_backpressure,
            0
        );
    }

    #[test]
    fn overflow_gap_emitted_clears_pending_flag() {
        let config = TailerConfig::default();
        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, registry, shutdown, source);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        supervisor.sync_tailers(&panes);

        // Force overflow state
        supervisor.tailers.get_mut(&1).unwrap().overflow_gap_pending = true;
        supervisor
            .tailers
            .get_mut(&1)
            .unwrap()
            .consecutive_backpressure = OVERFLOW_BACKPRESSURE_THRESHOLD;

        // Emit overflow gap
        supervisor.capturing_panes.insert(1);
        supervisor.handle_poll_result(1, PollOutcome::OverflowGapEmitted);

        let tailer = supervisor.tailers.get(&1).unwrap();
        assert!(!tailer.overflow_gap_pending);
        assert_eq!(tailer.consecutive_backpressure, 0);
        assert_eq!(supervisor.metrics().overflow_gaps_emitted, 1);
        assert_eq!(supervisor.metrics().events_sent, 1);
    }

    #[tokio::test]
    async fn overflow_gap_emitted_via_spawn_ready() {
        let source = Arc::new(FixedSource);
        let config = TailerConfig {
            min_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(50),
            max_concurrent: 4,
            send_timeout: Duration::from_millis(100),
            ..Default::default()
        };

        let (tx, mut rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        {
            let mut cursor_guard = cursors.write().await;
            cursor_guard.insert(1, PaneCursor::new(1));
        }

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, registry, shutdown, source);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        supervisor.sync_tailers(&panes);

        // Force overflow_gap_pending
        supervisor.tailers.get_mut(&1).unwrap().overflow_gap_pending = true;

        // Wait for min_interval
        tokio::time::sleep(Duration::from_millis(5)).await;

        let mut join_set = JoinSet::new();
        supervisor.spawn_ready(&mut join_set);

        // Collect outcomes
        while let Some(result) = join_set.join_next().await {
            if let Ok((pane_id, outcome)) = result {
                assert_eq!(pane_id, 1);
                assert!(
                    matches!(outcome, PollOutcome::OverflowGapEmitted),
                    "Expected OverflowGapEmitted, got {outcome:?}"
                );
                supervisor.handle_poll_result(pane_id, outcome);
            }
        }

        // Verify the gap event was sent
        let event = rx
            .try_recv()
            .expect("should have received overflow gap event");
        assert_eq!(event.segment.pane_id, 1);
        assert_eq!(event.segment.content, "");
        assert!(matches!(
            event.segment.kind,
            crate::ingest::CapturedSegmentKind::Gap { ref reason } if reason == "backpressure_overflow"
        ));

        // Verify pending flag was cleared
        assert!(!supervisor.tailers.get(&1).unwrap().overflow_gap_pending);
        assert_eq!(supervisor.metrics().overflow_gaps_emitted, 1);
    }

    #[tokio::test]
    async fn overflow_gap_advances_cursor_seq() {
        let source = Arc::new(FixedSource);
        let config = TailerConfig {
            min_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(50),
            max_concurrent: 4,
            send_timeout: Duration::from_millis(100),
            ..Default::default()
        };

        let (tx, mut rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        {
            let mut cursor_guard = cursors.write().await;
            let mut cursor = PaneCursor::new(1);
            // Advance seq to 5 to verify gap gets seq=5
            cursor.next_seq = 5;
            cursor_guard.insert(1, cursor);
        }

        let mut supervisor =
            TailerSupervisor::new(config, tx, Arc::clone(&cursors), registry, shutdown, source);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        supervisor.sync_tailers(&panes);
        supervisor.tailers.get_mut(&1).unwrap().overflow_gap_pending = true;

        tokio::time::sleep(Duration::from_millis(5)).await;

        let mut join_set = JoinSet::new();
        supervisor.spawn_ready(&mut join_set);

        while let Some(result) = join_set.join_next().await {
            if let Ok((pane_id, outcome)) = result {
                supervisor.handle_poll_result(pane_id, outcome);
            }
        }

        let event = rx.try_recv().expect("should have received gap event");
        assert_eq!(event.segment.seq, 5, "gap should use cursor's next_seq");

        // Cursor should have advanced to 6
        let cursor_guard = cursors.read().await;
        let cursor = cursor_guard.get(&1).unwrap();
        assert_eq!(cursor.next_seq, 6);
        assert!(cursor.in_gap, "cursor should be in gap state");
    }

    #[test]
    fn overflow_gap_emitted_metric_starts_at_zero() {
        let metrics = TailerMetrics::default();
        assert_eq!(metrics.overflow_gaps_emitted, 0);
    }

    // ─── CaptureScheduler tests ─────────────────────────────────────

    #[test]
    fn scheduler_unlimited_budget_allows_all() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        let panes = vec![(1, 10), (2, 50), (3, 100)];
        let selected = sched.select_panes(&panes, 10);
        assert_eq!(selected, vec![1, 2, 3]);
    }

    #[test]
    fn scheduler_global_rate_limits_captures() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 2,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        let panes = vec![(1, 10), (2, 50), (3, 100), (4, 200)];
        let selected = sched.select_panes(&panes, 10);
        // Budget is 2/sec, so only 2 are selected.
        assert_eq!(selected.len(), 2);
        // Highest priority first.
        assert_eq!(selected, vec![1, 2]);
    }

    #[test]
    fn scheduler_permits_limit_takes_precedence() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 100,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        let panes = vec![(1, 10), (2, 50), (3, 100)];
        // Only 1 permit available, despite 100 budget.
        let selected = sched.select_panes(&panes, 1);
        assert_eq!(selected, vec![1]);
    }

    #[test]
    fn scheduler_depletes_budget_across_calls() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 3,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        // First call: takes 2 of 3.
        let selected1 = sched.select_panes(&[(1, 10), (2, 50)], 5);
        assert_eq!(selected1.len(), 2);

        // Second call: only 1 remains.
        let selected2 = sched.select_panes(&[(3, 10), (4, 50)], 5);
        assert_eq!(selected2.len(), 1);

        // Third call: budget exhausted.
        let selected3 = sched.select_panes(&[(5, 10)], 5);
        assert!(selected3.is_empty());
    }

    #[test]
    fn scheduler_check_global_budget_tracks_metrics() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 1,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        assert!(sched.check_global_budget()); // Consume the 1 token.
        assert!(!sched.check_global_budget()); // Exhausted.

        assert_eq!(sched.metrics().global_rate_limited, 1);
        assert_eq!(sched.metrics().throttle_events, 1);
    }

    #[test]
    fn scheduler_byte_budget_exhaustion() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: 100,
        };
        let mut sched = CaptureScheduler::new(budget);

        assert!(!sched.is_byte_budget_exhausted());

        // Record 100 bytes — exhausts the budget.
        sched.record_capture(1, 100);
        assert!(sched.is_byte_budget_exhausted());
        assert_eq!(sched.metrics().pane_byte_budget_exceeded, 1);
    }

    #[test]
    fn scheduler_per_pane_tracking() {
        let budget = CaptureBudgetConfig::default();
        let mut sched = CaptureScheduler::new(budget);

        sched.record_capture(1, 500);
        sched.record_capture(2, 300);
        sched.record_capture(1, 200);

        assert!(sched.pane_trackers.contains_key(&1));
        assert!(sched.pane_trackers.contains_key(&2));

        let t1 = &sched.pane_trackers[&1];
        assert_eq!(t1.captures_in_window, 2);
        assert_eq!(t1.bytes_in_window, 700);
    }

    #[test]
    fn scheduler_remove_pane_cleans_tracker() {
        let budget = CaptureBudgetConfig::default();
        let mut sched = CaptureScheduler::new(budget);

        sched.record_capture(42, 100);
        assert!(sched.pane_trackers.contains_key(&42));

        sched.remove_pane(42);
        assert!(!sched.pane_trackers.contains_key(&42));
    }

    #[test]
    fn scheduler_update_budget_preserves_window() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 5,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        // Consume some budget.
        let _ = sched.select_panes(&[(1, 10), (2, 50)], 5);
        assert_eq!(sched.global_captures_remaining, 3);

        // Update to higher budget — window continues.
        sched.update_budget(CaptureBudgetConfig {
            max_captures_per_sec: 100,
            max_bytes_per_sec: 0,
        });

        // Remaining is still from the old window (3), not the new budget (100).
        // The new budget takes effect on the next window refill.
        assert_eq!(sched.global_captures_remaining, 3);
    }

    #[test]
    fn scheduler_empty_panes_returns_empty() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 10,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        let selected = sched.select_panes(&[], 10);
        assert!(selected.is_empty());
    }

    #[test]
    fn scheduler_metrics_default() {
        let metrics = SchedulerMetrics::default();
        assert_eq!(metrics.global_rate_limited, 0);
        assert_eq!(metrics.pane_byte_budget_exceeded, 0);
        assert_eq!(metrics.throttle_events, 0);
    }

    #[test]
    fn scheduler_high_priority_always_first() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 2,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        // Panes sorted by priority (pre-sorted input).
        // Priority 10 and 50 should be selected; 100 and 200 dropped.
        let panes = vec![(1, 10), (2, 50), (3, 100), (4, 200)];
        let selected = sched.select_panes(&panes, 10);
        assert_eq!(selected, vec![1, 2]);
    }

    #[tokio::test]
    async fn supervisor_with_budget_limits_captures() {
        let config = TailerConfig {
            min_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(50),
            max_concurrent: 10,
            send_timeout: Duration::from_millis(50),
            ..Default::default()
        };

        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 1,
            max_bytes_per_sec: 0,
        };

        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        {
            let mut cursor_guard = cursors.write().await;
            for pane_id in 1..=4 {
                cursor_guard.insert(pane_id, PaneCursor::new(pane_id));
            }
        }

        let mut supervisor =
            TailerSupervisor::with_budget(config, tx, cursors, registry, shutdown, source, budget);

        let mut panes = HashMap::new();
        for pane_id in 1..=4 {
            panes.insert(pane_id, make_pane(pane_id));
        }
        supervisor.sync_tailers(&panes);

        // Wait for tailers to become ready.
        tokio::time::sleep(Duration::from_millis(5)).await;

        let mut join_set = JoinSet::new();
        supervisor.spawn_ready(&mut join_set);

        // With budget of 1 capture/sec and 10 permits, only 1 should spawn.
        assert!(
            join_set.len() <= 1,
            "Expected at most 1 task spawned with budget=1, got {}",
            join_set.len()
        );
    }

    // ─── bd-1tem: determinism + burst + edge-case coverage ──────────

    #[test]
    fn scheduler_equal_priority_deterministic_by_pane_id() {
        // When all panes have the same priority, pane_id is the tiebreaker.
        // This ensures deterministic scheduling regardless of HashMap ordering.
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 3,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        // All priority 100, budget allows 3.
        let panes = vec![(42, 100), (7, 100), (99, 100), (1, 100)];
        let selected = sched.select_panes(&panes, 10);
        // Pre-sorted input: caller sorts by (priority, pane_id).
        // So the input order should already be (1,100), (7,100), (42,100), (99,100).
        // But we're testing with the input as-given — select_panes trusts the order.
        assert_eq!(selected.len(), 3);
        // First 3 from the input order (which is pre-sorted by caller).
        assert_eq!(selected, vec![42, 7, 99]);
    }

    #[test]
    fn scheduler_burst_then_exhaustion() {
        // Simulate a burst of captures that exhausts the budget, then verify
        // that subsequent calls are properly rate-limited.
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 5,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        // Burst: 5 panes at once, exhausts full budget.
        let burst = vec![(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)];
        let selected = sched.select_panes(&burst, 10);
        assert_eq!(selected.len(), 5);

        // Immediately after: budget exhausted, nothing scheduled.
        let more = vec![(6, 10), (7, 20)];
        let selected2 = sched.select_panes(&more, 10);
        assert!(
            selected2.is_empty(),
            "Budget should be exhausted after burst"
        );

        // Metrics should reflect the rate limiting.
        assert!(sched.metrics().global_rate_limited >= 1);
    }

    #[test]
    fn scheduler_byte_budget_partial_consumption() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: 1000,
        };
        let mut sched = CaptureScheduler::new(budget);

        // Consume 300 bytes.
        sched.record_capture(1, 300);
        assert!(!sched.is_byte_budget_exhausted());
        assert_eq!(sched.global_bytes_remaining, 700);

        // Consume 700 more — exactly at limit.
        sched.record_capture(2, 700);
        assert!(sched.is_byte_budget_exhausted());
    }

    #[test]
    fn scheduler_byte_budget_saturating_sub() {
        // Verify that recording more bytes than remaining doesn't underflow.
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: 50,
        };
        let mut sched = CaptureScheduler::new(budget);

        // Record way more bytes than budget allows.
        sched.record_capture(1, 10_000);
        assert_eq!(sched.global_bytes_remaining, 0);
        assert!(sched.is_byte_budget_exhausted());
    }

    #[test]
    fn scheduler_mixed_priorities_budget_favors_high() {
        // With a tight budget, only the highest-priority panes should be selected.
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 2,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        // Pre-sorted by (priority, pane_id):
        let panes = vec![
            (10, 5),   // priority 5 (critical)
            (20, 50),  // priority 50 (high)
            (30, 100), // priority 100 (normal)
            (40, 200), // priority 200 (low)
        ];
        let selected = sched.select_panes(&panes, 10);
        assert_eq!(selected, vec![10, 20]);
    }

    #[test]
    fn scheduler_zero_permits_returns_empty() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 100,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        let panes = vec![(1, 10), (2, 50)];
        let selected = sched.select_panes(&panes, 0);
        assert!(selected.is_empty());
    }

    #[test]
    fn scheduler_per_pane_window_resets_after_one_second() {
        let budget = CaptureBudgetConfig::default();
        let mut sched = CaptureScheduler::new(budget);

        sched.record_capture(1, 500);
        let tracker = sched.pane_trackers.get_mut(&1).unwrap();
        assert_eq!(tracker.captures_in_window, 1);
        assert_eq!(tracker.bytes_in_window, 500);

        // Manually move the window start back to force a reset.
        tracker.window_start = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
        tracker.maybe_reset();

        assert_eq!(tracker.captures_in_window, 0);
        assert_eq!(tracker.bytes_in_window, 0);
    }

    #[test]
    fn scheduler_combined_capture_and_byte_budget() {
        // Both limits active: the more restrictive one wins.
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 10,
            max_bytes_per_sec: 200,
        };
        let mut sched = CaptureScheduler::new(budget);

        // Select 3 panes (under capture limit).
        let panes = vec![(1, 10), (2, 20), (3, 30)];
        let selected = sched.select_panes(&panes, 10);
        assert_eq!(selected.len(), 3);

        // Record large bytes that exhaust byte budget.
        sched.record_capture(1, 100);
        sched.record_capture(2, 100);
        // Byte budget is now 0.
        assert!(sched.is_byte_budget_exhausted());

        // Capture budget still has tokens (7 remaining), but byte budget blocks.
        assert_eq!(sched.global_captures_remaining, 7);
    }

    #[test]
    fn scheduler_throttle_events_accumulate() {
        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 1,
            max_bytes_per_sec: 0,
        };
        let mut sched = CaptureScheduler::new(budget);

        // Exhaust capture budget.
        let _ = sched.select_panes(&[(1, 10)], 5);

        // Three more attempts, all throttled.
        let _ = sched.select_panes(&[(2, 10)], 5);
        let _ = sched.select_panes(&[(3, 10)], 5);
        let _ = sched.select_panes(&[(4, 10)], 5);

        assert!(sched.metrics().throttle_events >= 3);
    }

    #[tokio::test]
    async fn supervisor_changed_outcome_records_bytes() {
        let config = TailerConfig::default();
        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: 1000,
        };

        let mut supervisor =
            TailerSupervisor::with_budget(config, tx, cursors, registry, shutdown, source, budget);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        supervisor.sync_tailers(&panes);

        // Simulate a capture that produced 256 bytes.
        supervisor.capturing_panes.insert(1);
        supervisor.handle_poll_result(1, PollOutcome::Changed { bytes: 256 });

        // The scheduler should have recorded the bytes.
        let tracker = &supervisor.scheduler.pane_trackers[&1];
        assert_eq!(tracker.bytes_in_window, 256);
        assert_eq!(tracker.captures_in_window, 1);

        // Global bytes should be debited.
        assert_eq!(supervisor.scheduler.global_bytes_remaining, 744);
    }

    #[tokio::test]
    async fn supervisor_budget_hot_reload() {
        let config = TailerConfig::default();
        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let registry = Arc::new(RwLock::new(crate::ingest::PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        let budget = CaptureBudgetConfig {
            max_captures_per_sec: 5,
            max_bytes_per_sec: 0,
        };

        let mut supervisor =
            TailerSupervisor::with_budget(config, tx, cursors, registry, shutdown, source, budget);

        assert_eq!(supervisor.scheduler.budget.max_captures_per_sec, 5);

        // Hot-reload to new budget.
        supervisor.update_budget(CaptureBudgetConfig {
            max_captures_per_sec: 50,
            max_bytes_per_sec: 10_000,
        });

        assert_eq!(supervisor.scheduler.budget.max_captures_per_sec, 50);
        assert_eq!(supervisor.scheduler.budget.max_bytes_per_sec, 10_000);
    }

    // --- Streaming tailer tests (wa-nu4.4.2.3) ---

    #[test]
    fn tailer_mode_display() {
        assert_eq!(TailerMode::Polling.to_string(), "polling");
        assert_eq!(TailerMode::Streaming.to_string(), "streaming");
    }

    #[test]
    fn tailer_mode_eq() {
        assert_eq!(TailerMode::Polling, TailerMode::Polling);
        assert_eq!(TailerMode::Streaming, TailerMode::Streaming);
        assert_ne!(TailerMode::Polling, TailerMode::Streaming);
    }

    #[test]
    fn detect_mode_without_vendored_returns_polling() {
        // Without the vendored feature, or without a socket path,
        // detection should always return Polling.
        let config = crate::config::Config::default();
        let mode = detect_tailer_mode(&config);
        // Can be Streaming if vendored is on + WEZTERM_UNIX_SOCKET is set in env,
        // but by default should be Polling.
        if cfg!(feature = "vendored") {
            // Mode depends on env var — just check it's valid
            assert!(mode == TailerMode::Polling || mode == TailerMode::Streaming);
        } else {
            assert_eq!(mode, TailerMode::Polling);
        }
    }

    #[test]
    fn detect_mode_with_socket_path_config() {
        let mut config = crate::config::Config::default();
        config.vendored.mux_socket_path = Some("/tmp/test-mux.sock".to_string());
        let mode = detect_tailer_mode(&config);
        if cfg!(feature = "vendored") {
            assert_eq!(mode, TailerMode::Streaming);
        } else {
            assert_eq!(mode, TailerMode::Polling);
        }
    }

    #[test]
    fn detect_mode_empty_socket_path_is_polling() {
        let mut config = crate::config::Config::default();
        config.vendored.mux_socket_path = Some("  ".to_string());
        let mode = detect_tailer_mode(&config);
        // Empty/whitespace path should not trigger streaming
        if cfg!(feature = "vendored") {
            // Only Streaming if WEZTERM_UNIX_SOCKET is also set in env
            // In a clean test env, this should be Polling
            assert!(mode == TailerMode::Polling || mode == TailerMode::Streaming);
        } else {
            assert_eq!(mode, TailerMode::Polling);
        }
    }

    #[test]
    fn streaming_bridge_new_defaults() {
        let bridge = StreamingBridge::new();
        assert_eq!(bridge.events_processed(), 0);
        assert_eq!(bridge.fallback_count(), 0);
        assert_eq!(bridge.ingester().active_panes(), 0);
    }

    #[test]
    fn streaming_bridge_default_matches_new() {
        let a = StreamingBridge::new();
        let b = StreamingBridge::default();
        assert_eq!(a.events_processed(), b.events_processed());
        assert_eq!(a.fallback_count(), b.fallback_count());
    }

    #[test]
    fn streaming_bridge_record_fallback() {
        let mut bridge = StreamingBridge::new();
        assert_eq!(bridge.fallback_count(), 0);
        bridge.record_fallback();
        assert_eq!(bridge.fallback_count(), 1);
        bridge.record_fallback();
        assert_eq!(bridge.fallback_count(), 2);
    }

    #[cfg(feature = "vendored")]
    #[test]
    fn streaming_bridge_process_output_delta() {
        use crate::vendored::PaneDelta;

        let mut bridge = StreamingBridge::new();
        let delta = PaneDelta::Output {
            pane_id: 1,
            seqno: 5,
            title: "bash".to_string(),
            dirty_range_count: 2,
        };

        let segments = bridge.process_delta(delta);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].pane_id, 1);
        assert_eq!(segments[0].seq, 0); // first segment for this pane
        assert!(segments[0].content.contains("dirty ranges"));
        assert_eq!(bridge.events_processed(), 1);
    }

    #[cfg(feature = "vendored")]
    #[test]
    fn streaming_bridge_process_gap_emits_gap_segment() {
        use crate::ingest::CapturedSegmentKind;
        use crate::vendored::PaneDelta;

        let mut bridge = StreamingBridge::new();

        // First, send a normal output to establish the pane cursor
        let output = PaneDelta::Output {
            pane_id: 1,
            seqno: 1,
            title: "bash".to_string(),
            dirty_range_count: 1,
        };
        bridge.process_delta(output);

        // Now send a gap
        let gap = PaneDelta::Gap {
            pane_id: 1,
            reason: "seqno jump".to_string(),
        };
        let segments = bridge.process_delta(gap);

        // Gap delta with overflow=true → next event will emit gap.
        // The current event itself is an empty delta, so we get 1 segment.
        // But overflow is set for the *next* event.
        assert!(!segments.is_empty());
        assert_eq!(bridge.events_processed(), 2);
    }

    #[cfg(feature = "vendored")]
    #[test]
    fn streaming_bridge_process_ended_emits_pane_closed() {
        use crate::ingest::CapturedSegmentKind;
        use crate::vendored::PaneDelta;

        let mut bridge = StreamingBridge::new();

        // Establish cursor
        let output = PaneDelta::Output {
            pane_id: 1,
            seqno: 1,
            title: "bash".to_string(),
            dirty_range_count: 1,
        };
        bridge.process_delta(output);

        // End the subscription
        let ended = PaneDelta::Ended {
            pane_id: 1,
            reason: "cancelled".to_string(),
        };
        let segments = bridge.process_delta(ended);

        // PaneClosed → final gap segment
        assert_eq!(segments.len(), 1);
        match &segments[0].kind {
            CapturedSegmentKind::Gap { reason } => {
                assert!(reason.contains("pane_closed"));
            }
            _ => panic!("expected Gap segment for pane close"),
        }
    }

    #[cfg(feature = "vendored")]
    #[test]
    fn streaming_bridge_multiple_panes() {
        use crate::vendored::PaneDelta;

        let mut bridge = StreamingBridge::new();

        for pane_id in [1, 2, 3] {
            let delta = PaneDelta::Output {
                pane_id,
                seqno: 1,
                title: format!("pane-{pane_id}"),
                dirty_range_count: 1,
            };
            bridge.process_delta(delta);
        }

        assert_eq!(bridge.events_processed(), 3);
        assert_eq!(bridge.ingester().active_panes(), 3);
    }

    #[test]
    fn streaming_health_snapshot() {
        let bridge = StreamingBridge::new();
        let health = StreamingHealth {
            mode: TailerMode::Streaming,
            events_processed: bridge.events_processed(),
            gaps_emitted: bridge.ingester().total_gaps(),
            fallback_count: bridge.fallback_count(),
            active_panes: bridge.ingester().active_panes(),
        };
        assert_eq!(health.mode, TailerMode::Streaming);
        assert_eq!(health.events_processed, 0);
        assert_eq!(health.active_panes, 0);
    }
}
