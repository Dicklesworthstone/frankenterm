//! Observation Runtime for the watcher daemon.
//!
//! This module orchestrates the passive observation loop:
//! - Pane discovery and content tailers
//! - Delta extraction and storage persistence
//! - Pattern detection and event emission
//!
//! # Architecture
//!
//! ```text
//! WezTerm CLI ──┬──► PaneRegistry (discovery)
//!               │
//!               └──► PaneCursor (deltas) ──┬──► StorageHandle (segments)
//!                                          │
//!                                          └──► PatternEngine ──► StorageHandle (events)
//! ```
//!
//! The runtime explicitly enforces that the observation loop never calls any
//! send/act APIs - it is purely passive.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::sharded_counter::{ShardedCounter, ShardedGauge, ShardedMax};

use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, error, info, instrument, warn};

use crate::config::{
    CaptureBudgetConfig, HotReloadableConfig, PaneFilterConfig, PanePriorityConfig, PatternsConfig,
    SnapshotConfig, SnapshotSchedulingMode,
};
use crate::crash::{HealthSnapshot, ShutdownSummary};
use crate::error::Result;
use crate::events::{Event, EventBus, UserVarPayload, event_identity_key};
use crate::gc::{CacheGcSettings, compact_u64_map, should_vacuum};
use crate::ingest::{PaneCursor, PaneRegistry, persist_captured_segment};
#[cfg(feature = "native-wezterm")]
use crate::native_events::{NativeEvent, NativeEventListener};
use crate::patterns::{Detection, DetectionContext, PatternEngine, Severity};
use crate::recording::RecordingManager;
use crate::resize_scheduler::{ResizeSchedulerDebugSnapshot, ResizeStalledTransaction};
use crate::runtime_compat::{RwLock, mpsc, sleep, timeout, watch};
use crate::spsc_ring_buffer::{SpscConsumer, SpscProducer, channel as spsc_channel};
#[cfg(feature = "native-wezterm")]
use crate::storage::PaneRecord;
use crate::storage::{MaintenanceRecord, StorageHandle, StoredEvent};
use crate::tailer::{CaptureEvent, TailerConfig, TailerSupervisor};
use crate::watchdog::HeartbeatRegistry;
use crate::wezterm::{
    PaneInfo, WeztermHandle, WeztermHandleSource, WeztermInterface, wezterm_handle_with_timeout,
};

// ---------------------------------------------------------------------------
// Native event output coalescing (wa-x4rq)
// ---------------------------------------------------------------------------
//
// When using the `native-wezterm` integration, WezTerm can emit extremely
// high-frequency pane output events during bursty terminal activity. Persisting
// every micro-chunk as its own CapturedSegment creates avoidable overhead
// (channel pressure, DB writes, pattern scans).
//
// We batch per-pane output events into a single capture delta within a short
// coalescing window (default 50ms). This is a rate-limit style coalescer:
// - output within the window is merged
// - output is flushed once the window elapses (or sooner on state transitions)
// - a max delay guard exists for safety when misconfigured

#[cfg(feature = "native-wezterm")]
const NATIVE_OUTPUT_COALESCE_WINDOW_MS: u64 = 50;
#[cfg(feature = "native-wezterm")]
const NATIVE_OUTPUT_COALESCE_MAX_DELAY_MS: u64 = 200;
#[cfg(feature = "native-wezterm")]
const NATIVE_OUTPUT_COALESCE_MAX_BYTES: usize = 256 * 1024;

#[cfg(feature = "native-wezterm")]
#[derive(Debug)]
struct PendingNativeOutput {
    first_seen_ms: u64,
    last_timestamp_ms: i64,
    input_events: u32,
    bytes: Vec<u8>,
}

#[cfg(feature = "native-wezterm")]
#[derive(Debug)]
struct CoalescedNativeOutput {
    pane_id: u64,
    bytes: Vec<u8>,
    timestamp_ms: i64,
    input_events: u32,
}

#[cfg(feature = "native-wezterm")]
#[derive(Debug)]
struct NativeOutputCoalescer {
    window_ms: u64,
    max_delay_ms: u64,
    max_bytes: usize,
    pending: HashMap<u64, PendingNativeOutput>,
}

#[cfg(feature = "native-wezterm")]
impl NativeOutputCoalescer {
    fn new(window_ms: u64, max_delay_ms: u64, max_bytes: usize) -> Self {
        Self {
            window_ms,
            max_delay_ms,
            max_bytes,
            pending: HashMap::new(),
        }
    }

    fn push(
        &mut self,
        pane_id: u64,
        bytes: Vec<u8>,
        timestamp_ms: i64,
        now_ms: u64,
    ) -> Option<CoalescedNativeOutput> {
        if bytes.is_empty() {
            return None;
        }

        match self.pending.entry(pane_id) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(PendingNativeOutput {
                    first_seen_ms: now_ms,
                    last_timestamp_ms: timestamp_ms,
                    input_events: 1,
                    bytes,
                });
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let pending = o.get_mut();

                if !pending.bytes.is_empty()
                    && pending.bytes.len().saturating_add(bytes.len()) > self.max_bytes
                {
                    let flushed = CoalescedNativeOutput {
                        pane_id,
                        bytes: std::mem::take(&mut pending.bytes),
                        timestamp_ms: pending.last_timestamp_ms,
                        input_events: pending.input_events,
                    };

                    pending.first_seen_ms = now_ms;
                    pending.last_timestamp_ms = timestamp_ms;
                    pending.input_events = 1;
                    pending.bytes = bytes;

                    return Some(flushed);
                }

                pending.input_events = pending.input_events.saturating_add(1);
                pending.last_timestamp_ms = timestamp_ms;
                pending.bytes.extend(bytes);
                None
            }
        }
    }

    fn drain_due(&mut self, now_ms: u64) -> Vec<CoalescedNativeOutput> {
        let mut due = Vec::new();
        let mut due_ids = Vec::new();

        for (&pane_id, pending) in &self.pending {
            let age_ms = now_ms.saturating_sub(pending.first_seen_ms);
            if age_ms >= self.window_ms || age_ms >= self.max_delay_ms {
                due_ids.push(pane_id);
            }
        }

        for pane_id in due_ids {
            if let Some(pending) = self.pending.remove(&pane_id) {
                due.push(CoalescedNativeOutput {
                    pane_id,
                    bytes: pending.bytes,
                    timestamp_ms: pending.last_timestamp_ms,
                    input_events: pending.input_events,
                });
            }
        }

        due
    }

    fn flush_pane(&mut self, pane_id: u64) -> Option<CoalescedNativeOutput> {
        self.pending
            .remove(&pane_id)
            .map(|pending| CoalescedNativeOutput {
                pane_id,
                bytes: pending.bytes,
                timestamp_ms: pending.last_timestamp_ms,
                input_events: pending.input_events,
            })
    }

    fn drain_all(&mut self) -> Vec<CoalescedNativeOutput> {
        let mut out = Vec::with_capacity(self.pending.len());
        for (pane_id, pending) in self.pending.drain() {
            out.push(CoalescedNativeOutput {
                pane_id,
                bytes: pending.bytes,
                timestamp_ms: pending.last_timestamp_ms,
                input_events: pending.input_events,
            });
        }
        out
    }
}

/// Configuration for the observation runtime.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Polling interval for pane discovery
    pub discovery_interval: Duration,
    /// Maximum polling interval for content capture (idle panes)
    pub capture_interval: Duration,
    /// Minimum polling interval for content capture (active panes)
    pub min_capture_interval: Duration,
    /// Delta extraction overlap window size
    pub overlap_size: usize,
    /// Pane filter configuration
    pub pane_filter: PaneFilterConfig,
    /// Pane priority configuration
    pub pane_priorities: PanePriorityConfig,
    /// Capture budget configuration
    pub capture_budgets: CaptureBudgetConfig,
    /// Pattern detection configuration
    pub patterns: PatternsConfig,
    /// Optional root for resolving file-based pattern packs
    pub patterns_root: Option<PathBuf>,
    /// Channel buffer size for internal queues
    pub channel_buffer: usize,
    /// Maximum concurrent capture operations
    pub max_concurrent_captures: usize,
    /// Data retention period in days
    pub retention_days: u32,
    /// Maximum size of storage in MB (0 = unlimited)
    pub retention_max_mb: u32,
    /// Database checkpoint interval in seconds
    pub checkpoint_interval_secs: u32,
    /// Optional Unix socket path for native WezTerm events
    pub native_event_socket: Option<PathBuf>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            discovery_interval: Duration::from_secs(5),
            capture_interval: Duration::from_millis(200),
            min_capture_interval: Duration::from_millis(50),
            overlap_size: 1_048_576, // 1MB default
            pane_filter: PaneFilterConfig::default(),
            pane_priorities: PanePriorityConfig::default(),
            capture_budgets: CaptureBudgetConfig::default(),
            patterns: PatternsConfig::default(),
            patterns_root: None,
            channel_buffer: 1024,
            max_concurrent_captures: 10,
            retention_days: 30,
            retention_max_mb: 0,
            checkpoint_interval_secs: 60,
            native_event_socket: None,
        }
    }
}

/// Runtime metrics for health snapshots and shutdown summaries.
///
/// Uses sharded atomics (cache-line-padded per-core counters) to eliminate
/// false sharing and SeqCst contention. Writes distribute across shards;
/// reads aggregate infrequently.
static GLOBAL_RUNTIME_LOCK_MEMORY_TELEMETRY: OnceLock<
    StdRwLock<Option<RuntimeLockMemoryTelemetrySnapshot>>,
> = OnceLock::new();
/// Number of recent samples retained for lock/memory percentile telemetry.
const TELEMETRY_PERCENTILE_WINDOW_CAPACITY: usize = 1024;
/// Warning threshold for stalled resize transactions.
const RESIZE_WATCHDOG_WARNING_THRESHOLD_MS: u64 = 2_000;
/// Critical threshold for stalled resize transactions.
const RESIZE_WATCHDOG_CRITICAL_THRESHOLD_MS: u64 = 8_000;
/// Number of critical stalls that triggers safe-mode recommendation.
const RESIZE_WATCHDOG_CRITICAL_STALLED_LIMIT: usize = 2;
/// Maximum stalled-transaction samples retained in watchdog payloads.
const RESIZE_WATCHDOG_SAMPLE_LIMIT: usize = 8;

/// Machine-readable lock contention and cursor-memory telemetry snapshot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RuntimeLockMemoryTelemetrySnapshot {
    /// Snapshot timestamp in epoch milliseconds.
    pub timestamp_ms: u64,
    /// Average storage lock wait in milliseconds.
    pub avg_storage_lock_wait_ms: f64,
    /// p50 storage lock wait in milliseconds (rolling window).
    pub p50_storage_lock_wait_ms: f64,
    /// p95 storage lock wait in milliseconds (rolling window).
    pub p95_storage_lock_wait_ms: f64,
    /// Maximum storage lock wait in milliseconds.
    pub max_storage_lock_wait_ms: f64,
    /// Count of lock acquisitions that crossed contention threshold.
    pub storage_lock_contention_events: u64,
    /// Average storage lock hold time in milliseconds.
    pub avg_storage_lock_hold_ms: f64,
    /// p50 storage lock hold time in milliseconds (rolling window).
    pub p50_storage_lock_hold_ms: f64,
    /// p95 storage lock hold time in milliseconds (rolling window).
    pub p95_storage_lock_hold_ms: f64,
    /// Maximum storage lock hold time in milliseconds.
    pub max_storage_lock_hold_ms: f64,
    /// Last cursor snapshot memory sample in bytes.
    pub cursor_snapshot_bytes_last: u64,
    /// p50 cursor snapshot memory in bytes (rolling window).
    pub p50_cursor_snapshot_bytes: u64,
    /// p95 cursor snapshot memory in bytes (rolling window).
    pub p95_cursor_snapshot_bytes: u64,
    /// Peak cursor snapshot memory sample in bytes.
    pub cursor_snapshot_bytes_max: u64,
    /// Average cursor snapshot memory in bytes.
    pub avg_cursor_snapshot_bytes: f64,
}

impl RuntimeLockMemoryTelemetrySnapshot {
    /// Update the latest global lock/memory telemetry snapshot.
    pub fn update_global(snapshot: Self) {
        let lock = GLOBAL_RUNTIME_LOCK_MEMORY_TELEMETRY.get_or_init(|| StdRwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(snapshot);
        }
    }

    /// Get the latest lock/memory telemetry snapshot.
    #[must_use]
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_RUNTIME_LOCK_MEMORY_TELEMETRY.get_or_init(|| StdRwLock::new(None));
        lock.read().ok().and_then(|guard| guard.clone())
    }
}

/// Resize watchdog health severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeWatchdogSeverity {
    /// No stalled resize transactions above warning threshold.
    Healthy,
    /// One or more stalled transactions above warning threshold.
    Warning,
    /// Pathological stalls detected; safe-mode fallback should be enabled.
    Critical,
    /// Safe-mode is currently active via resize control-plane kill-switch.
    SafeModeActive,
}

/// Machine-readable resize watchdog assessment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResizeWatchdogAssessment {
    /// Current severity classification.
    pub severity: ResizeWatchdogSeverity,
    /// Number of stalled transactions above warning threshold.
    pub stalled_total: usize,
    /// Number of stalled transactions above critical threshold.
    pub stalled_critical: usize,
    /// Warning threshold used for detection.
    pub warning_threshold_ms: u64,
    /// Critical threshold used for detection.
    pub critical_threshold_ms: u64,
    /// Critical stall count needed before safe-mode recommendation.
    pub critical_stalled_limit: usize,
    /// Whether safe-mode fallback should be enabled by operators/runtime policy.
    pub safe_mode_recommended: bool,
    /// Whether safe-mode is already active.
    pub safe_mode_active: bool,
    /// Whether legacy fallback path is available when safe-mode is active.
    pub legacy_fallback_enabled: bool,
    /// Suggested operator/runtime action.
    pub recommended_action: String,
    /// Sample stalled transactions for diagnostics.
    pub sample_stalled: Vec<ResizeStalledTransaction>,
}

impl ResizeWatchdogAssessment {
    /// Render an operator-facing warning line for health snapshots.
    #[must_use]
    pub fn warning_line(&self) -> Option<String> {
        match self.severity {
            ResizeWatchdogSeverity::Healthy => None,
            ResizeWatchdogSeverity::Warning => Some(format!(
                "Resize watchdog warning: {} stalled transaction(s) >= {}ms",
                self.stalled_total, self.warning_threshold_ms
            )),
            ResizeWatchdogSeverity::Critical => Some(format!(
                "Resize watchdog CRITICAL: {} stalled transaction(s) >= {}ms; recommend safe-mode fallback{}",
                self.stalled_critical,
                self.critical_threshold_ms,
                if self.legacy_fallback_enabled {
                    " with legacy path enabled"
                } else {
                    ""
                }
            )),
            ResizeWatchdogSeverity::SafeModeActive => Some(format!(
                "Resize watchdog: safe-mode active ({} stalled >= {}ms)",
                self.stalled_total, self.warning_threshold_ms
            )),
        }
    }
}

/// Evaluate resize control-plane stall health from the latest global debug snapshot.
#[must_use]
pub fn evaluate_resize_watchdog(now_ms: u64) -> Option<ResizeWatchdogAssessment> {
    let snapshot = ResizeSchedulerDebugSnapshot::get_global()?;
    let stalled_warning =
        snapshot.stalled_transactions(now_ms, RESIZE_WATCHDOG_WARNING_THRESHOLD_MS);
    let stalled_critical =
        snapshot.stalled_transactions(now_ms, RESIZE_WATCHDOG_CRITICAL_THRESHOLD_MS);
    let safe_mode_active = snapshot.gate.emergency_disable;
    let safe_mode_recommended =
        !safe_mode_active && stalled_critical.len() >= RESIZE_WATCHDOG_CRITICAL_STALLED_LIMIT;

    let severity = if safe_mode_active {
        ResizeWatchdogSeverity::SafeModeActive
    } else if safe_mode_recommended {
        ResizeWatchdogSeverity::Critical
    } else if !stalled_warning.is_empty() {
        ResizeWatchdogSeverity::Warning
    } else {
        ResizeWatchdogSeverity::Healthy
    };

    let recommended_action = match severity {
        ResizeWatchdogSeverity::Healthy => "none",
        ResizeWatchdogSeverity::Warning => "monitor_stalled_transactions",
        ResizeWatchdogSeverity::Critical => "enable_safe_mode_fallback",
        ResizeWatchdogSeverity::SafeModeActive => "safe_mode_active_monitor_and_recover",
    }
    .to_string();

    let sample_source = if !stalled_critical.is_empty() {
        &stalled_critical
    } else {
        &stalled_warning
    };
    let sample_stalled = sample_source
        .iter()
        .take(RESIZE_WATCHDOG_SAMPLE_LIMIT)
        .cloned()
        .collect();

    Some(ResizeWatchdogAssessment {
        severity,
        stalled_total: stalled_warning.len(),
        stalled_critical: stalled_critical.len(),
        warning_threshold_ms: RESIZE_WATCHDOG_WARNING_THRESHOLD_MS,
        critical_threshold_ms: RESIZE_WATCHDOG_CRITICAL_THRESHOLD_MS,
        critical_stalled_limit: RESIZE_WATCHDOG_CRITICAL_STALLED_LIMIT,
        safe_mode_recommended,
        safe_mode_active,
        legacy_fallback_enabled: snapshot.gate.legacy_fallback_enabled,
        recommended_action,
        sample_stalled,
    })
}

/// Derive ordered resize degradation ladder state from watchdog assessment.
///
/// Escalation order is enforced by `degradation::evaluate_resize_degradation_ladder`:
/// quality reductions first, correctness guards second, emergency compatibility last.
#[must_use]
pub fn derive_resize_degradation_ladder(
    watchdog: &ResizeWatchdogAssessment,
) -> crate::degradation::ResizeDegradationAssessment {
    crate::degradation::evaluate_resize_degradation_ladder(
        crate::degradation::ResizeDegradationSignals {
            stalled_total: watchdog.stalled_total,
            stalled_critical: watchdog.stalled_critical,
            warning_threshold_ms: watchdog.warning_threshold_ms,
            critical_threshold_ms: watchdog.critical_threshold_ms,
            critical_stalled_limit: watchdog.critical_stalled_limit,
            safe_mode_recommended: watchdog.safe_mode_recommended,
            safe_mode_active: watchdog.safe_mode_active,
            legacy_fallback_enabled: watchdog.legacy_fallback_enabled,
        },
    )
}

/// Evaluate resize degradation ladder from the latest global scheduler snapshot.
#[must_use]
pub fn evaluate_resize_degradation_ladder_state(
    now_ms: u64,
) -> Option<crate::degradation::ResizeDegradationAssessment> {
    let watchdog = evaluate_resize_watchdog(now_ms)?;
    Some(derive_resize_degradation_ladder(&watchdog))
}

#[derive(Debug)]
pub struct RuntimeMetrics {
    /// Count of segments persisted
    segments_persisted: ShardedCounter,
    /// Count of events recorded
    events_recorded: ShardedCounter,
    /// Timestamp when runtime started (epoch ms)
    started_at: ShardedGauge,
    /// Last DB write timestamp (epoch ms)
    last_db_write_at: ShardedGauge,
    /// Sum of ingest lag samples (for averaging)
    ingest_lag_sum_ms: ShardedCounter,
    /// Count of ingest lag samples
    ingest_lag_count: ShardedCounter,
    /// Maximum ingest lag observed
    ingest_lag_max_ms: ShardedMax,
    /// Sum of storage mutex wait time samples (microseconds).
    storage_lock_wait_us_sum: ShardedCounter,
    /// Count of storage mutex wait time samples.
    storage_lock_wait_samples: ShardedCounter,
    /// Maximum storage mutex wait time observed (microseconds).
    storage_lock_wait_us_max: ShardedMax,
    /// Recent storage mutex wait samples for percentile telemetry.
    storage_lock_wait_recent_us: StdMutex<VecDeque<u64>>,
    /// Number of storage lock acquisitions with meaningful wait (contention).
    storage_lock_contention_events: ShardedCounter,
    /// Sum of storage mutex hold time samples (microseconds).
    storage_lock_hold_us_sum: ShardedCounter,
    /// Count of storage mutex hold time samples.
    storage_lock_hold_samples: ShardedCounter,
    /// Maximum storage mutex hold time observed (microseconds).
    storage_lock_hold_us_max: ShardedMax,
    /// Recent storage mutex hold samples for percentile telemetry.
    storage_lock_hold_recent_us: StdMutex<VecDeque<u64>>,
    /// Number of cursor snapshot memory samples.
    cursor_snapshot_samples: ShardedCounter,
    /// Sum of cursor snapshot bytes across samples.
    cursor_snapshot_bytes_sum: ShardedCounter,
    /// Peak cursor snapshot bytes observed.
    cursor_snapshot_bytes_max: ShardedMax,
    /// Last cursor snapshot bytes sample.
    cursor_snapshot_bytes_last: ShardedGauge,
    /// Recent cursor snapshot bytes for percentile telemetry.
    cursor_snapshot_recent_bytes: StdMutex<VecDeque<u64>>,
    /// Total native pane output events received (pre-coalesce).
    native_output_input_events: ShardedCounter,
    /// Total native pane output batches emitted (post-coalesce).
    native_output_batches_emitted: ShardedCounter,
    /// Total native output bytes received (pre-coalesce).
    native_output_input_bytes: ShardedCounter,
    /// Total native output bytes emitted (post-coalesce).
    native_output_emitted_bytes: ShardedCounter,
    /// Maximum number of input events merged into one batch.
    native_output_max_batch_events: ShardedMax,
    /// Maximum size (bytes) of one emitted batch.
    native_output_max_batch_bytes: ShardedMax,
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self {
            segments_persisted: ShardedCounter::new(),
            events_recorded: ShardedCounter::new(),
            started_at: ShardedGauge::new(),
            last_db_write_at: ShardedGauge::new(),
            ingest_lag_sum_ms: ShardedCounter::new(),
            ingest_lag_count: ShardedCounter::new(),
            ingest_lag_max_ms: ShardedMax::new(),
            storage_lock_wait_us_sum: ShardedCounter::new(),
            storage_lock_wait_samples: ShardedCounter::new(),
            storage_lock_wait_us_max: ShardedMax::new(),
            storage_lock_wait_recent_us: StdMutex::new(VecDeque::with_capacity(
                TELEMETRY_PERCENTILE_WINDOW_CAPACITY,
            )),
            storage_lock_contention_events: ShardedCounter::new(),
            storage_lock_hold_us_sum: ShardedCounter::new(),
            storage_lock_hold_samples: ShardedCounter::new(),
            storage_lock_hold_us_max: ShardedMax::new(),
            storage_lock_hold_recent_us: StdMutex::new(VecDeque::with_capacity(
                TELEMETRY_PERCENTILE_WINDOW_CAPACITY,
            )),
            cursor_snapshot_samples: ShardedCounter::new(),
            cursor_snapshot_bytes_sum: ShardedCounter::new(),
            cursor_snapshot_bytes_max: ShardedMax::new(),
            cursor_snapshot_bytes_last: ShardedGauge::new(),
            cursor_snapshot_recent_bytes: StdMutex::new(VecDeque::with_capacity(
                TELEMETRY_PERCENTILE_WINDOW_CAPACITY,
            )),
            native_output_input_events: ShardedCounter::new(),
            native_output_batches_emitted: ShardedCounter::new(),
            native_output_input_bytes: ShardedCounter::new(),
            native_output_emitted_bytes: ShardedCounter::new(),
            native_output_max_batch_events: ShardedMax::new(),
            native_output_max_batch_bytes: ShardedMax::new(),
        }
    }
}

impl RuntimeMetrics {
    /// Record an ingest lag sample.
    pub fn record_ingest_lag(&self, lag_ms: u64) {
        self.ingest_lag_sum_ms.add(lag_ms);
        self.ingest_lag_count.increment();
        self.ingest_lag_max_ms.observe(lag_ms);
    }

    /// Record a successful DB write.
    pub fn record_db_write(&self) {
        self.last_db_write_at.set(epoch_ms_u64());
    }

    /// Record storage mutex lock wait duration.
    pub fn record_storage_lock_wait(&self, waited: Duration) {
        let waited_us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
        self.storage_lock_wait_us_sum.add(waited_us);
        self.storage_lock_wait_samples.increment();
        self.storage_lock_wait_us_max.observe(waited_us);
        record_bounded_sample(&self.storage_lock_wait_recent_us, waited_us);
        if waited_us >= STORAGE_LOCK_CONTENTION_MIN_US {
            self.storage_lock_contention_events.increment();
        }
    }

    /// Record storage mutex lock hold duration.
    pub fn record_storage_lock_hold(&self, held: Duration) {
        let held_us = u64::try_from(held.as_micros()).unwrap_or(u64::MAX);
        self.storage_lock_hold_us_sum.add(held_us);
        self.storage_lock_hold_samples.increment();
        self.storage_lock_hold_us_max.observe(held_us);
        record_bounded_sample(&self.storage_lock_hold_recent_us, held_us);
    }

    /// Record a cursor snapshot memory sample.
    pub fn record_cursor_snapshot_memory(&self, total_bytes: u64) {
        self.cursor_snapshot_samples.increment();
        self.cursor_snapshot_bytes_sum.add(total_bytes);
        self.cursor_snapshot_bytes_max.observe(total_bytes);
        self.cursor_snapshot_bytes_last.set(total_bytes);
        record_bounded_sample(&self.cursor_snapshot_recent_bytes, total_bytes);
    }

    pub fn record_native_output_input(&self, bytes: usize) {
        self.native_output_input_events.increment();
        self.native_output_input_bytes.add(bytes as u64);
    }

    pub fn record_native_output_batch(&self, input_events: u32, bytes: usize) {
        self.native_output_batches_emitted.increment();
        self.native_output_emitted_bytes.add(bytes as u64);
        self.native_output_max_batch_events
            .observe(u64::from(input_events));
        self.native_output_max_batch_bytes.observe(bytes as u64);
    }

    /// Get average ingest lag in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn avg_ingest_lag_ms(&self) -> f64 {
        let sum = self.ingest_lag_sum_ms.get();
        let count = self.ingest_lag_count.get();
        if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        }
    }

    /// Get total ingest lag sample count.
    pub fn ingest_lag_count(&self) -> u64 {
        self.ingest_lag_count.get()
    }

    /// Get total ingest lag sum in milliseconds.
    pub fn ingest_lag_sum_ms(&self) -> u64 {
        self.ingest_lag_sum_ms.get()
    }

    /// Get maximum ingest lag in milliseconds.
    pub fn max_ingest_lag_ms(&self) -> u64 {
        self.ingest_lag_max_ms.get()
    }

    /// Average storage mutex wait time in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn avg_storage_lock_wait_ms(&self) -> f64 {
        let sum = self.storage_lock_wait_us_sum.get();
        let count = self.storage_lock_wait_samples.get();
        if count == 0 {
            0.0
        } else {
            (sum as f64 / count as f64) / 1000.0
        }
    }

    /// Maximum storage mutex wait time in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn max_storage_lock_wait_ms(&self) -> f64 {
        self.storage_lock_wait_us_max.get() as f64 / 1000.0
    }

    /// p50 storage mutex wait time in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn p50_storage_lock_wait_ms(&self) -> f64 {
        percentile_from_samples(&self.storage_lock_wait_recent_us, 50) as f64 / 1000.0
    }

    /// p95 storage mutex wait time in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn p95_storage_lock_wait_ms(&self) -> f64 {
        percentile_from_samples(&self.storage_lock_wait_recent_us, 95) as f64 / 1000.0
    }

    /// Total number of storage lock contention events (wait >= threshold).
    pub fn storage_lock_contention_events(&self) -> u64 {
        self.storage_lock_contention_events.get()
    }

    /// Average storage mutex hold time in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn avg_storage_lock_hold_ms(&self) -> f64 {
        let sum = self.storage_lock_hold_us_sum.get();
        let count = self.storage_lock_hold_samples.get();
        if count == 0 {
            0.0
        } else {
            (sum as f64 / count as f64) / 1000.0
        }
    }

    /// Maximum storage mutex hold time in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn max_storage_lock_hold_ms(&self) -> f64 {
        self.storage_lock_hold_us_max.get() as f64 / 1000.0
    }

    /// p50 storage mutex hold time in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn p50_storage_lock_hold_ms(&self) -> f64 {
        percentile_from_samples(&self.storage_lock_hold_recent_us, 50) as f64 / 1000.0
    }

    /// p95 storage mutex hold time in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn p95_storage_lock_hold_ms(&self) -> f64 {
        percentile_from_samples(&self.storage_lock_hold_recent_us, 95) as f64 / 1000.0
    }

    /// Last sampled cursor snapshot bytes.
    pub fn cursor_snapshot_bytes_last(&self) -> u64 {
        self.cursor_snapshot_bytes_last.get_max()
    }

    /// Maximum sampled cursor snapshot bytes.
    pub fn cursor_snapshot_bytes_max(&self) -> u64 {
        self.cursor_snapshot_bytes_max.get()
    }

    /// p50 cursor snapshot memory in bytes.
    pub fn p50_cursor_snapshot_bytes(&self) -> u64 {
        percentile_from_samples(&self.cursor_snapshot_recent_bytes, 50)
    }

    /// p95 cursor snapshot memory in bytes.
    pub fn p95_cursor_snapshot_bytes(&self) -> u64 {
        percentile_from_samples(&self.cursor_snapshot_recent_bytes, 95)
    }

    /// Average sampled cursor snapshot bytes.
    #[allow(clippy::cast_precision_loss)]
    pub fn avg_cursor_snapshot_bytes(&self) -> f64 {
        let sum = self.cursor_snapshot_bytes_sum.get();
        let count = self.cursor_snapshot_samples.get();
        if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        }
    }

    /// Build a machine-readable lock/memory telemetry snapshot.
    #[must_use]
    pub fn lock_memory_snapshot(&self) -> RuntimeLockMemoryTelemetrySnapshot {
        RuntimeLockMemoryTelemetrySnapshot {
            timestamp_ms: epoch_ms_u64(),
            avg_storage_lock_wait_ms: self.avg_storage_lock_wait_ms(),
            p50_storage_lock_wait_ms: self.p50_storage_lock_wait_ms(),
            p95_storage_lock_wait_ms: self.p95_storage_lock_wait_ms(),
            max_storage_lock_wait_ms: self.max_storage_lock_wait_ms(),
            storage_lock_contention_events: self.storage_lock_contention_events(),
            avg_storage_lock_hold_ms: self.avg_storage_lock_hold_ms(),
            p50_storage_lock_hold_ms: self.p50_storage_lock_hold_ms(),
            p95_storage_lock_hold_ms: self.p95_storage_lock_hold_ms(),
            max_storage_lock_hold_ms: self.max_storage_lock_hold_ms(),
            cursor_snapshot_bytes_last: self.cursor_snapshot_bytes_last(),
            p50_cursor_snapshot_bytes: self.p50_cursor_snapshot_bytes(),
            p95_cursor_snapshot_bytes: self.p95_cursor_snapshot_bytes(),
            cursor_snapshot_bytes_max: self.cursor_snapshot_bytes_max(),
            avg_cursor_snapshot_bytes: self.avg_cursor_snapshot_bytes(),
        }
    }

    /// Get last DB write timestamp (epoch ms), or None if never written.
    pub fn last_db_write(&self) -> Option<u64> {
        let ts = self.last_db_write_at.get_max();
        if ts == 0 { None } else { Some(ts) }
    }

    /// Get total segments persisted.
    pub fn segments_persisted(&self) -> u64 {
        self.segments_persisted.get()
    }

    /// Get total events recorded.
    pub fn events_recorded(&self) -> u64 {
        self.events_recorded.get()
    }

    pub fn native_output_input_events(&self) -> u64 {
        self.native_output_input_events.get()
    }

    pub fn native_output_batches_emitted(&self) -> u64 {
        self.native_output_batches_emitted.get()
    }

    pub fn native_output_input_bytes(&self) -> u64 {
        self.native_output_input_bytes.get()
    }

    pub fn native_output_emitted_bytes(&self) -> u64 {
        self.native_output_emitted_bytes.get()
    }

    pub fn native_output_max_batch_events(&self) -> u64 {
        self.native_output_max_batch_events.get()
    }

    pub fn native_output_max_batch_bytes(&self) -> u64 {
        self.native_output_max_batch_bytes.get()
    }
}

/// The observation runtime orchestrates passive monitoring.
///
/// This runtime:
/// 1. Discovers panes via WezTerm CLI
/// 2. Captures content deltas from observed panes
/// 3. Persists segments and gaps to storage
/// 4. Runs pattern detection on new content
/// 5. Persists detection events to storage
///
/// The runtime is explicitly **read-only** - it never sends input to panes.
pub struct ObservationRuntime {
    /// Runtime configuration
    config: RuntimeConfig,
    /// WezTerm interface handle (real or mock)
    wezterm_handle: WeztermHandle,
    /// Storage handle for persistence (wrapped for async sharing)
    storage: Arc<tokio::sync::Mutex<StorageHandle>>,
    /// Pattern detection engine
    pattern_engine: Arc<RwLock<PatternEngine>>,
    /// Pane registry for discovery and tracking
    registry: Arc<RwLock<PaneRegistry>>,
    /// Per-pane cursors for delta extraction
    cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
    /// Per-pane detection contexts for deduplication
    detection_contexts: Arc<RwLock<HashMap<u64, DetectionContext>>>,
    /// Shutdown flag for signaling tasks
    shutdown_flag: Arc<AtomicBool>,
    /// Runtime metrics for health/shutdown
    metrics: Arc<RuntimeMetrics>,
    /// Hot-reloadable config sender (for broadcasting updates to tasks)
    config_tx: Arc<watch::Sender<HotReloadableConfig>>,
    /// Hot-reloadable config receiver (for tasks to receive updates)
    config_rx: watch::Receiver<HotReloadableConfig>,
    /// Optional event bus for publishing detection events to workflow runners
    event_bus: Option<Arc<EventBus>>,
    /// Optional recording manager for capturing session recordings
    recording: Option<Arc<RecordingManager>>,
    /// Optional snapshot engine configuration for session persistence
    snapshot_config: Option<SnapshotConfig>,
    /// Heartbeat registry for watchdog monitoring
    heartbeats: Arc<HeartbeatRegistry>,
    /// Shared scheduler snapshot for health reporting (written by capture task).
    scheduler_snapshot: Arc<RwLock<crate::tailer::SchedulerSnapshot>>,
}

impl ObservationRuntime {
    /// Create a new observation runtime.
    ///
    /// # Arguments
    /// * `config` - Runtime configuration
    /// * `storage` - Storage handle for persistence
    /// * `pattern_engine` - Pattern detection engine (shared)
    #[must_use]
    pub fn new(
        config: RuntimeConfig,
        storage: StorageHandle,
        pattern_engine: Arc<RwLock<PatternEngine>>,
    ) -> Self {
        let registry = PaneRegistry::with_filter(config.pane_filter.clone());
        let metrics = Arc::new(RuntimeMetrics::default());
        metrics.started_at.set(epoch_ms_u64());

        // Initialize hot-reload config channel with current values
        let hot_config = HotReloadableConfig {
            log_level: "info".to_string(), // Default, will be overridden
            poll_interval_ms: duration_ms_u64(config.capture_interval),
            min_poll_interval_ms: duration_ms_u64(config.min_capture_interval),
            max_concurrent_captures: config.max_concurrent_captures as u32,
            pane_priorities: config.pane_priorities.clone(),
            capture_budgets: config.capture_budgets.clone(),
            retention_days: config.retention_days,
            retention_max_mb: config.retention_max_mb,
            checkpoint_interval_secs: config.checkpoint_interval_secs,
            patterns: config.patterns.clone(),
            workflows_enabled: vec![],
            auto_run_allowlist: vec![],
        };
        let (config_tx, config_rx) = watch::channel(hot_config);

        Self {
            config,
            wezterm_handle: wezterm_handle_with_timeout(5),
            storage: Arc::new(tokio::sync::Mutex::new(storage)),
            pattern_engine,
            registry: Arc::new(RwLock::new(registry)),
            cursors: Arc::new(RwLock::new(HashMap::new())),
            detection_contexts: Arc::new(RwLock::new(HashMap::new())),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            metrics,
            config_tx: Arc::new(config_tx),
            config_rx,
            event_bus: None,
            recording: None,
            snapshot_config: None,
            heartbeats: Arc::new(HeartbeatRegistry::new()),
            scheduler_snapshot: Arc::new(RwLock::new(crate::tailer::SchedulerSnapshot::default())),
        }
    }

    /// Set an event bus for publishing detection events.
    ///
    /// When set, the runtime will publish `PatternDetected` events to this bus
    /// after persisting them to storage. This enables workflow runners to
    /// subscribe and handle detections in real-time.
    #[must_use]
    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    /// Set a recording manager for capturing pane output and events.
    #[must_use]
    pub fn with_recording_manager(mut self, recording: Arc<RecordingManager>) -> Self {
        self.recording = Some(recording);
        self
    }

    /// Override the WezTerm interface handle (for mocks or custom clients).
    #[must_use]
    pub fn with_wezterm_handle(mut self, wezterm_handle: WeztermHandle) -> Self {
        self.wezterm_handle = wezterm_handle;
        self
    }

    /// Set snapshot engine configuration for session persistence.
    #[must_use]
    pub fn with_snapshot_config(mut self, config: SnapshotConfig) -> Self {
        self.snapshot_config = Some(config);
        self
    }

    /// Start the observation runtime.
    ///
    /// Returns handles for the spawned tasks. Call `shutdown()` to stop.
    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<RuntimeHandle> {
        info!("Starting observation runtime");

        // Stage 1 ingress: multi-producer capture tasks write into bounded MPSC.
        let (capture_ingress_tx, capture_ingress_rx) =
            mpsc::channel::<CaptureEvent>(self.config.channel_buffer);
        // Stage 2 handoff: single relay task forwards ingress into lock-free SPSC
        // consumed by the persistence task.
        let (capture_ring_tx, capture_ring_rx) =
            spsc_channel::<CaptureEvent>(self.config.channel_buffer);

        // Clone ingress sender for queue depth instrumentation before moving it.
        let capture_tx_probe = capture_ingress_tx.clone();

        // Spawn discovery task
        let discovery_handle = self.spawn_discovery_task();

        let native_socket = self.config.native_event_socket.clone();

        #[cfg(feature = "native-wezterm")]
        let native_enabled = native_socket.is_some();
        #[cfg(not(feature = "native-wezterm"))]
        let native_enabled = false;

        // Spawn capture tasks (polling) unless native events are enabled.
        let capture_handle = if native_enabled {
            self.spawn_idle_capture_task()
        } else {
            self.spawn_capture_task(capture_ingress_tx.clone())
        };

        // Spawn native event listener if configured and supported.
        #[cfg(feature = "native-wezterm")]
        let native_handle = native_socket
            .map(|socket| self.spawn_native_event_task(socket, capture_ingress_tx.clone()));
        #[cfg(not(feature = "native-wezterm"))]
        let native_handle = {
            if native_socket.is_some() {
                warn!(
                    "Native event socket configured but frankenterm-core built without native-wezterm feature"
                );
            }
            None
        };

        // Spawn relay task from multi-producer ingress into SPSC persistence queue.
        let relay_handle = self.spawn_capture_relay_task(capture_ingress_rx, capture_ring_tx);

        // Spawn persistence and detection task
        let persistence_handle = self.spawn_persistence_task(
            capture_ring_rx,
            Arc::clone(&self.cursors),
            Arc::clone(&self.registry),
        );

        // Spawn maintenance task
        let maintenance_handle = self.spawn_maintenance_task(capture_tx_probe.clone());

        // Spawn snapshot engine task (session persistence) if configured
        let (snapshot_handle, snapshot_shutdown_tx, snapshot_triggers) =
            if let Some(ref snap_config) = self.snapshot_config {
                if snap_config.enabled {
                    let db_path = {
                        let (storage_guard, lock_held_since) =
                            lock_storage_with_profile(&self.storage, &self.metrics).await;
                        let db_path = Arc::new(storage_guard.db_path().to_string());
                        drop(storage_guard);
                        self.metrics
                            .record_storage_lock_hold(lock_held_since.elapsed());
                        db_path
                    };
                    let engine = Arc::new(crate::snapshot_engine::SnapshotEngine::new(
                        db_path,
                        snap_config.clone(),
                    ));
                    let (shutdown_tx, shutdown_rx) = watch::channel(false);
                    let wezterm = self.wezterm_handle.clone();
                    let snapshot_triggers = if matches!(
                        snap_config.scheduling.mode,
                        SnapshotSchedulingMode::Intelligent
                    ) {
                        Some(self.spawn_snapshot_trigger_task(
                            Arc::clone(&engine),
                            self.event_bus.clone(),
                        ))
                    } else {
                        None
                    };

                    let handle = tokio::spawn(async move {
                        engine
                            .run_periodic(shutdown_rx, move || {
                                let wez = wezterm.clone();
                                async move {
                                    match wez.list_panes().await {
                                        Ok(panes) => Some(panes),
                                        Err(e) => {
                                            warn!(
                                                error = %e,
                                                "snapshot pane listing failed"
                                            );
                                            None
                                        }
                                    }
                                }
                            })
                            .await;
                    });
                    info!("Snapshot engine started");
                    (Some(handle), Some(shutdown_tx), snapshot_triggers)
                } else {
                    (None, None, None)
                }
            } else {
                (None, None, None)
            };

        info!("Observation runtime started");

        Ok(RuntimeHandle {
            discovery: discovery_handle,
            capture: capture_handle,
            relay: relay_handle,
            persistence: persistence_handle,
            maintenance: Some(maintenance_handle),
            snapshot: snapshot_handle,
            snapshot_triggers,
            snapshot_shutdown: snapshot_shutdown_tx,
            shutdown_flag: Arc::clone(&self.shutdown_flag),
            storage: Arc::clone(&self.storage),
            metrics: Arc::clone(&self.metrics),
            registry: Arc::clone(&self.registry),
            cursors: Arc::clone(&self.cursors),
            start_time: Instant::now(),
            config_tx: Arc::clone(&self.config_tx),
            event_bus: self.event_bus.clone(),
            heartbeats: Arc::clone(&self.heartbeats),
            capture_tx: capture_tx_probe,
            wezterm_handle: Arc::clone(&self.wezterm_handle),
            native_events: native_handle,
            scheduler_snapshot: Arc::clone(&self.scheduler_snapshot),
        })
    }

    /// Spawn a bridge that turns runtime events/health signals into snapshot triggers.
    fn spawn_snapshot_trigger_task(
        &self,
        snapshot_engine: Arc<crate::snapshot_engine::SnapshotEngine>,
        event_bus: Option<Arc<EventBus>>,
    ) -> JoinHandle<()> {
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let registry = Arc::clone(&self.registry);
        let metrics = Arc::clone(&self.metrics);

        tokio::spawn(async move {
            let mut subscriber = event_bus.as_ref().map(|bus| bus.subscribe());
            let idle_enabled = subscriber.is_some();
            let mut last_activity = Instant::now();
            let mut last_idle_trigger = Instant::now();
            let mut last_memory_trigger = Instant::now()
                .checked_sub(Duration::from_secs(SNAPSHOT_MEMORY_TRIGGER_COOLDOWN_SECS))
                .unwrap_or_else(Instant::now);

            loop {
                if shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }

                let mut subscriber_closed = false;
                let mut tick_only = true;

                if let Some(sub) = subscriber.as_mut() {
                    match timeout(
                        Duration::from_secs(SNAPSHOT_TRIGGER_BRIDGE_TICK_SECS),
                        sub.recv(),
                    )
                    .await
                    {
                        Ok(recv) => {
                            tick_only = false;
                            match recv {
                                Ok(event) => {
                                    if event_counts_as_activity(&event) {
                                        last_activity = Instant::now();
                                    }
                                    if let Some(trigger) = snapshot_trigger_from_event(&event)
                                        && !snapshot_engine.emit_trigger(trigger)
                                    {
                                        debug!(
                                            trigger = ?trigger,
                                            event_type = event.type_name(),
                                            "snapshot trigger dropped (queue full or inactive)"
                                        );
                                    }
                                }
                                Err(crate::events::RecvError::Lagged { missed_count }) => {
                                    last_activity = Instant::now();
                                    warn!(
                                        missed = missed_count,
                                        "snapshot trigger bridge lagged on event bus"
                                    );
                                }
                                Err(crate::events::RecvError::Closed) => {
                                    subscriber_closed = true;
                                }
                            }
                        }
                        Err(_elapsed) => {}
                    }
                } else {
                    sleep(Duration::from_secs(SNAPSHOT_TRIGGER_BRIDGE_TICK_SECS)).await;
                }

                if subscriber_closed {
                    subscriber = None;
                }

                if !tick_only {
                    continue;
                }

                let now = Instant::now();

                if idle_enabled
                    && now.duration_since(last_activity)
                        >= Duration::from_secs(SNAPSHOT_IDLE_WINDOW_SECS)
                    && now.duration_since(last_idle_trigger)
                        >= Duration::from_secs(SNAPSHOT_IDLE_WINDOW_SECS)
                {
                    let observed_panes = {
                        let reg = registry.read().await;
                        reg.observed_pane_ids().len()
                    };
                    if observed_panes > 0 {
                        if !snapshot_engine
                            .emit_trigger(crate::snapshot_engine::SnapshotTrigger::IdleWindow)
                        {
                            debug!("snapshot idle-window trigger dropped (queue full or inactive)");
                        }
                        last_idle_trigger = now;
                    }
                }

                let cursor_snapshot_bytes = metrics.cursor_snapshot_bytes_last();
                if cursor_snapshot_bytes >= CURSOR_SNAPSHOT_MEMORY_WARN_BYTES
                    && now.duration_since(last_memory_trigger)
                        >= Duration::from_secs(SNAPSHOT_MEMORY_TRIGGER_COOLDOWN_SECS)
                {
                    if !snapshot_engine
                        .emit_trigger(crate::snapshot_engine::SnapshotTrigger::MemoryPressure)
                    {
                        debug!("snapshot memory-pressure trigger dropped (queue full or inactive)");
                    }
                    last_memory_trigger = now;
                }
            }
        })
    }

    /// Spawn the maintenance task.
    fn spawn_maintenance_task(&self, capture_tx: mpsc::Sender<CaptureEvent>) -> JoinHandle<()> {
        let storage = Arc::clone(&self.storage);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let wezterm_handle = self.wezterm_handle.clone();
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let metrics = Arc::clone(&self.metrics);
        let scheduler_snapshot = Arc::clone(&self.scheduler_snapshot);

        let initial_retention_days = self.config.retention_days;
        let initial_checkpoint_secs = self.config.checkpoint_interval_secs;
        let cache_gc_settings = CacheGcSettings::default();

        tokio::spawn(async move {
            let mut retention_days = initial_retention_days;
            let mut checkpoint_secs = initial_checkpoint_secs;
            let mut last_health_snapshot = Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(Instant::now);
            let health_interval = Duration::from_secs(30);

            // Run maintenance every minute, but only do expensive ops when needed.
            // Keep first tick immediate to preserve prior interval behavior.
            let maintenance_interval = Duration::from_secs(60);
            let mut first_tick = true;
            let mut last_retention_check = Instant::now();
            let mut last_checkpoint = Instant::now();
            let mut last_cache_gc = Instant::now();

            loop {
                if !first_tick {
                    sleep(maintenance_interval).await;
                }
                first_tick = false;
                heartbeats.record_maintenance();

                if shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }

                // Check for config updates
                if watch_has_changed(&config_rx) {
                    let new_config = watch_borrow_and_update_clone(&mut config_rx);
                    if new_config.retention_days != retention_days {
                        info!(
                            old = retention_days,
                            new = new_config.retention_days,
                            "Retention policy updated"
                        );
                        retention_days = new_config.retention_days;
                    }
                    if new_config.checkpoint_interval_secs != checkpoint_secs {
                        info!(
                            old = checkpoint_secs,
                            new = new_config.checkpoint_interval_secs,
                            "Checkpoint interval updated"
                        );
                        checkpoint_secs = new_config.checkpoint_interval_secs;
                    }
                }

                let now = Instant::now();

                // Run retention cleanup every hour (or if just started/updated)
                if now.duration_since(last_retention_check) >= Duration::from_secs(3600) {
                    if retention_days > 0 {
                        let cutoff_days = u64::from(retention_days);
                        let cutoff_window_ms = cutoff_days.saturating_mul(24 * 60 * 60 * 1000);
                        let cutoff_ms = epoch_ms()
                            .saturating_sub(i64::try_from(cutoff_window_ms).unwrap_or(i64::MAX));
                        let (storage_guard, lock_held_since) =
                            lock_storage_with_profile(&storage, &metrics).await;
                        if let Err(e) = storage_guard.retention_cleanup(cutoff_ms).await {
                            error!(error = %e, "Retention cleanup failed");
                        } else {
                            debug!("Retention cleanup completed");
                        }
                        // Also purge old audit actions
                        if let Err(e) = storage_guard.purge_audit_actions_before(cutoff_ms).await {
                            error!(error = %e, "Audit purge failed");
                        }
                        drop(storage_guard);
                        metrics.record_storage_lock_hold(lock_held_since.elapsed());
                    }
                    last_retention_check = now;
                }

                // Run WAL checkpoint + PRAGMA optimize (lightweight)
                if checkpoint_secs > 0
                    && now.duration_since(last_checkpoint)
                        >= Duration::from_secs(u64::from(checkpoint_secs))
                {
                    let (storage_guard, lock_held_since) =
                        lock_storage_with_profile(&storage, &metrics).await;
                    match storage_guard.checkpoint().await {
                        Ok(result) => {
                            debug!(
                                wal_pages = result.wal_pages,
                                optimized = result.optimized,
                                "WAL checkpoint completed"
                            );
                        }
                        Err(e) => {
                            error!(error = %e, "WAL checkpoint failed");
                        }
                    }
                    drop(storage_guard);
                    metrics.record_storage_lock_hold(lock_held_since.elapsed());
                    last_checkpoint = now;
                }

                if cache_gc_settings.enabled
                    && cache_gc_settings.interval_secs > 0
                    && now.duration_since(last_cache_gc)
                        >= Duration::from_secs(cache_gc_settings.interval_secs)
                {
                    let active_panes: HashSet<u64> = {
                        let reg = registry.read().await;
                        reg.observed_pane_ids().into_iter().collect()
                    };

                    let cursor_gc = {
                        let mut cursors_guard = cursors.write().await;
                        compact_u64_map(&mut cursors_guard, &active_panes)
                    };

                    let context_gc = {
                        let mut contexts_guard = detection_contexts.write().await;
                        compact_u64_map(&mut contexts_guard, &active_panes)
                    };

                    let mut page_count = 0_i64;
                    let mut free_pages = 0_i64;
                    let mut free_ratio = 0.0_f64;
                    let mut vacuumed = false;
                    let mut vacuum_error = None::<String>;

                    let (storage_guard, lock_held_since) =
                        lock_storage_with_profile(&storage, &metrics).await;
                    match storage_guard.database_page_stats().await {
                        Ok(stats) => {
                            page_count = stats.page_count;
                            free_pages = stats.free_pages;
                            free_ratio = stats.free_ratio();

                            if should_vacuum(
                                stats.page_count,
                                stats.free_pages,
                                cache_gc_settings.vacuum_threshold,
                            ) {
                                match storage_guard.vacuum().await {
                                    Ok(()) => {
                                        vacuumed = true;
                                    }
                                    Err(err) => {
                                        vacuum_error = Some(err.to_string());
                                        error!(error = %err, "Cache GC vacuum failed");
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            vacuum_error = Some(err.to_string());
                            error!(error = %err, "Cache GC failed to read database page stats");
                        }
                    }

                    let metadata = serde_json::json!({
                        "active_panes": active_panes.len(),
                        "cursor_removed": cursor_gc.removed_entries,
                        "cursor_freed_slots": cursor_gc.freed_slots(),
                        "context_removed": context_gc.removed_entries,
                        "context_freed_slots": context_gc.freed_slots(),
                        "page_count": page_count,
                        "free_pages": free_pages,
                        "free_ratio": free_ratio,
                        "vacuumed": vacuumed,
                        "vacuum_threshold": cache_gc_settings.vacuum_threshold,
                        "vacuum_error": vacuum_error,
                    });
                    let _ = storage_guard
                        .record_maintenance(MaintenanceRecord {
                            id: 0,
                            event_type: "cache_gc".to_string(),
                            message: Some("Periodic cache GC cycle".to_string()),
                            metadata: Some(metadata.to_string()),
                            timestamp: epoch_ms(),
                        })
                        .await;

                    drop(storage_guard);
                    metrics.record_storage_lock_hold(lock_held_since.elapsed());

                    info!(
                        active_panes = active_panes.len(),
                        cursor_removed = cursor_gc.removed_entries,
                        context_removed = context_gc.removed_entries,
                        cursor_freed_slots = cursor_gc.freed_slots(),
                        context_freed_slots = context_gc.freed_slots(),
                        free_ratio,
                        vacuumed,
                        "Cache GC cycle completed"
                    );

                    last_cache_gc = now;
                }

                if now.duration_since(last_health_snapshot) >= health_interval {
                    let (observed_panes, last_activity_by_pane) = {
                        let reg = registry.read().await;
                        let ids = reg.observed_pane_ids();
                        let activity: Vec<(u64, u64)> = reg
                            .entries()
                            .filter(|(_, e)| e.should_observe())
                            .map(|(id, e)| {
                                #[allow(clippy::cast_sign_loss)]
                                (*id, e.last_seen_at as u64)
                            })
                            .collect();
                        (ids.len(), activity)
                    };

                    let (last_seq_by_pane, cursor_snapshot_bytes): (Vec<(u64, i64)>, u64) = {
                        let cursors = cursors.read().await;
                        let mut total_bytes = 0u64;
                        let seqs = cursors
                            .iter()
                            .map(|(pane_id, cursor)| {
                                total_bytes = total_bytes.saturating_add(
                                    u64::try_from(cursor.last_snapshot.len()).unwrap_or(u64::MAX),
                                );
                                (*pane_id, cursor.last_seq())
                            })
                            .collect();
                        (seqs, total_bytes)
                    };
                    metrics.record_cursor_snapshot_memory(cursor_snapshot_bytes);

                    let capture_cap = mpsc_max_capacity(&capture_tx);
                    let capture_depth = capture_cap.saturating_sub(capture_tx.capacity());

                    let (write_depth, write_cap, db_writable) = {
                        let (storage_guard, lock_held_since) =
                            lock_storage_with_profile(&storage, &metrics).await;
                        let wd = storage_guard.write_queue_depth();
                        let wc = storage_guard.write_queue_capacity();
                        let writable = storage_guard.is_writable().await;
                        drop(storage_guard);
                        metrics.record_storage_lock_hold(lock_held_since.elapsed());
                        (wd, wc, writable)
                    };

                    let mut warnings = Vec::new();

                    #[allow(clippy::cast_precision_loss)]
                    if capture_cap > 0 {
                        let ratio = capture_depth as f64 / capture_cap as f64;
                        if ratio >= BACKPRESSURE_WARN_RATIO {
                            warnings.push(format!(
                                        "Capture queue backpressure: {capture_depth}/{capture_cap} ({:.0}%)",
                                        ratio * 100.0
                                    ));
                        }
                    }

                    #[allow(clippy::cast_precision_loss)]
                    if write_cap > 0 {
                        let ratio = write_depth as f64 / write_cap as f64;
                        if ratio >= BACKPRESSURE_WARN_RATIO {
                            warnings.push(format!(
                                "Write queue backpressure: {write_depth}/{write_cap} ({:.0}%)",
                                ratio * 100.0
                            ));
                        }
                    }

                    if !db_writable {
                        warnings.push("Database is not writable".to_string());
                    }
                    if metrics.max_storage_lock_wait_ms() >= STORAGE_LOCK_WAIT_WARN_MS {
                        warnings.push(format!(
                            "Storage lock contention: wait max {:.2} ms, avg {:.2} ms, events {}",
                            metrics.max_storage_lock_wait_ms(),
                            metrics.avg_storage_lock_wait_ms(),
                            metrics.storage_lock_contention_events()
                        ));
                    }
                    if metrics.max_storage_lock_hold_ms() >= STORAGE_LOCK_HOLD_WARN_MS {
                        warnings.push(format!(
                            "Storage lock hold high: max {:.2} ms, avg {:.2} ms",
                            metrics.max_storage_lock_hold_ms(),
                            metrics.avg_storage_lock_hold_ms(),
                        ));
                    }
                    if cursor_snapshot_bytes >= CURSOR_SNAPSHOT_MEMORY_WARN_BYTES {
                        warnings.push(format!(
                            "Cursor snapshot memory high: {:.1} MiB (peak {:.1} MiB)",
                            bytes_to_mib(cursor_snapshot_bytes),
                            bytes_to_mib(metrics.cursor_snapshot_bytes_max()),
                        ));
                    }
                    match wezterm_handle.watchdog_warnings().await {
                        Ok(wezterm_warnings) => warnings.extend(wezterm_warnings),
                        Err(err) => {
                            warnings.push(format!("WezTerm health warning probe failed: {err}"));
                        }
                    }

                    let snapshot = HealthSnapshot {
                        timestamp: epoch_ms_u64(),
                        observed_panes,
                        capture_queue_depth: capture_depth,
                        write_queue_depth: write_depth,
                        last_seq_by_pane,
                        warnings,
                        ingest_lag_avg_ms: metrics.avg_ingest_lag_ms(),
                        ingest_lag_max_ms: metrics.max_ingest_lag_ms(),
                        db_writable,
                        db_last_write_at: metrics.last_db_write(),
                        pane_priority_overrides: {
                            let now = epoch_ms();
                            let reg = registry.read().await;
                            reg.list_active_priority_overrides(now)
                                .into_iter()
                                .map(|(pane_id, ov)| crate::crash::PanePriorityOverrideSnapshot {
                                    pane_id,
                                    priority: ov.priority,
                                    expires_at: ov.expires_at.and_then(|e| u64::try_from(e).ok()),
                                })
                                .collect()
                        },
                        scheduler: {
                            let snap = scheduler_snapshot.read().await;
                            if snap.budget_active {
                                Some(snap.clone())
                            } else {
                                None
                            }
                        },
                        backpressure_tier: None,
                        last_activity_by_pane,
                        restart_count: 0,
                        last_crash_at: None,
                        consecutive_crashes: 0,
                        current_backoff_ms: 0,
                        in_crash_loop: false,
                    };

                    HealthSnapshot::update_global(snapshot);
                    RuntimeLockMemoryTelemetrySnapshot::update_global(
                        metrics.lock_memory_snapshot(),
                    );
                    last_health_snapshot = now;
                }
            }
        })
    }

    /// Spawn the pane discovery task.
    fn spawn_discovery_task(&self) -> JoinHandle<()> {
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let storage = Arc::clone(&self.storage);
        let metrics = Arc::clone(&self.metrics);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let initial_interval = self.config.discovery_interval;
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let wezterm = Arc::clone(&self.wezterm_handle);

        tokio::spawn(async move {
            let mut current_interval = initial_interval;

            loop {
                // Wait for interval, checking shutdown periodically to ensure responsiveness
                let deadline = Instant::now() + current_interval;
                loop {
                    if shutdown_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    // Sleep in short bursts to remain responsive to shutdown signals
                    sleep(Duration::from_millis(100)).await;
                }

                // Check shutdown flag
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Discovery task: shutdown signal received");
                    break;
                }

                // Check for config updates (non-blocking)
                if watch_has_changed(&config_rx) {
                    let new_config = watch_borrow_and_update_clone(&mut config_rx);
                    let new_interval = Duration::from_millis(new_config.poll_interval_ms);
                    if new_interval != current_interval {
                        info!(
                            old_ms = duration_ms_u64(current_interval),
                            new_ms = duration_ms_u64(new_interval),
                            "Discovery interval updated via hot reload"
                        );
                        current_interval = new_interval;
                    }
                }

                match wezterm.list_panes().await {
                    Ok(panes) => {
                        heartbeats.record_discovery();
                        let (diff, new_entries) = {
                            let mut reg = registry.write().await;
                            let diff = reg.discovery_tick(panes);
                            let new_entries: Vec<_> = diff
                                .new_panes
                                .iter()
                                .filter_map(|pane_id| {
                                    reg.get_entry(*pane_id)
                                        .cloned()
                                        .map(|entry| (*pane_id, entry))
                                })
                                .collect();
                            (diff, new_entries)
                        };

                        // Handle new panes
                        for (pane_id, entry) in new_entries {
                            // Upsert pane in storage
                            let record = entry.to_pane_record();
                            let (storage_guard, lock_held_since) =
                                lock_storage_with_profile(&storage, &metrics).await;
                            if let Err(e) = storage_guard.upsert_pane(record).await {
                                error!(pane_id = pane_id, error = %e, "Failed to upsert pane");
                            }
                            drop(storage_guard);
                            metrics.record_storage_lock_hold(lock_held_since.elapsed());

                            // Create cursor if observed
                            if entry.should_observe() {
                                // Initialize cursor from storage to resume capture
                                let (storage_guard, lock_held_since) =
                                    lock_storage_with_profile(&storage, &metrics).await;
                                let max_seq =
                                    storage_guard.get_max_seq(pane_id).await.unwrap_or(None);
                                drop(storage_guard);
                                metrics.record_storage_lock_hold(lock_held_since.elapsed());

                                let next_seq = max_seq.map_or(0, |s| s + 1);

                                {
                                    let mut cursors = cursors.write().await;
                                    cursors
                                        .insert(pane_id, PaneCursor::from_seq(pane_id, next_seq));
                                }

                                {
                                    let mut contexts = detection_contexts.write().await;
                                    let mut ctx = DetectionContext::new();
                                    ctx.pane_id = Some(pane_id);
                                    contexts.insert(pane_id, ctx);
                                }

                                debug!(
                                    pane_id = pane_id,
                                    next_seq = next_seq,
                                    "Started observing pane"
                                );
                            } else if let Some(reason) = entry.observation.ignore_reason() {
                                info!(
                                    pane_id = pane_id,
                                    reason = reason,
                                    "Pane ignored by observation filter"
                                );
                            }
                        }

                        // Handle closed panes
                        for pane_id in &diff.closed_panes {
                            {
                                let mut cursors = cursors.write().await;
                                cursors.remove(pane_id);
                            }

                            {
                                let mut contexts = detection_contexts.write().await;
                                contexts.remove(pane_id);
                            }

                            debug!(pane_id = pane_id, "Stopped observing pane (closed)");
                        }

                        // Handle new generations (pane restarted)
                        for pane_id in &diff.new_generations {
                            // Do NOT reset cursor seq to 0, it causes DB constraint violations.
                            // We keep capturing monotonically on the same pane_id.
                            debug!(
                                pane_id = pane_id,
                                "Restarted observing pane (new generation)"
                            );
                        }

                        if !diff.new_panes.is_empty()
                            || !diff.closed_panes.is_empty()
                            || !diff.new_generations.is_empty()
                        {
                            debug!(
                                new = diff.new_panes.len(),
                                closed = diff.closed_panes.len(),
                                restarted = diff.new_generations.len(),
                                "Pane discovery tick"
                            );
                        }
                    }
                    Err(e) => {
                        heartbeats.record_discovery();
                        warn!(error = %e, "Failed to list panes");
                    }
                }
            }
        })
    }

    /// Spawn the content capture task using TailerSupervisor with adaptive polling.
    ///
    /// This task manages per-pane tailers that:
    /// - Poll fast when output is changing (min_capture_interval)
    /// - Poll slow when idle (capture_interval)
    /// - Respect concurrency limits (max_concurrent_captures)
    /// - Handle backpressure from downstream
    fn spawn_capture_task(&self, capture_tx: mpsc::Sender<CaptureEvent>) -> JoinHandle<()> {
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let discovery_interval = self.config.discovery_interval;
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let wezterm_handle = Arc::clone(&self.wezterm_handle);
        let scheduler_snapshot = Arc::clone(&self.scheduler_snapshot);

        // Create tailer config from runtime config
        // Capture overlap_size for use in the async block (not hot-reloadable)
        let overlap_size = self.config.overlap_size;
        let initial_config = TailerConfig {
            min_interval: self.config.min_capture_interval,
            max_interval: self.config.capture_interval,
            backoff_multiplier: 1.5,
            max_concurrent: self.config.max_concurrent_captures,
            overlap_size,
            send_timeout: Duration::from_millis(100),
        };

        tokio::spawn(async move {
            let source = Arc::new(WeztermHandleSource::new(wezterm_handle));
            // Create tailer supervisor with budget enforcement
            let initial_budget = config_rx.borrow().capture_budgets.clone();
            let mut supervisor = TailerSupervisor::with_budget(
                initial_config,
                capture_tx,
                cursors,
                Arc::clone(&registry), // Pass registry for authoritative state
                Arc::clone(&shutdown_flag),
                source,
                initial_budget,
            );

            // Cache hot-reloadable pane priority config for scheduling.
            let mut pane_priorities = config_rx.borrow().pane_priorities.clone();

            // Sync tailers periodically with discovery interval.
            // Keep the first sync immediate to preserve prior interval behavior.
            let mut next_sync_tick = Instant::now();
            let mut join_set = JoinSet::new();

            loop {
                // Determine poll interval dynamically from supervisor config
                // (Using min_interval for responsiveness)
                // Actually supervisor manages per-tailer intervals. We just need to wake up often enough to spawn ready tasks.
                // A fixed tick is fine, supervisor filters ready tasks.
                let tick_duration = Duration::from_millis(10);

                let sync_wait = next_sync_tick.saturating_duration_since(Instant::now());

                tokio::select! {
                    () = sleep(sync_wait) => {
                        next_sync_tick = Instant::now() + discovery_interval;
                        heartbeats.record_capture();

                        if shutdown_flag.load(Ordering::SeqCst) {
                            debug!("Capture task: shutdown signal received");
                            break;
                        }

                        // Check for config updates
                        if watch_has_changed(&config_rx) {
                            let new_config = watch_borrow_and_update_clone(&mut config_rx);
                            let new_tailer_config = TailerConfig {
                                min_interval: Duration::from_millis(new_config.min_poll_interval_ms),
                                max_interval: Duration::from_millis(new_config.poll_interval_ms),
                                backoff_multiplier: 1.5,
                                max_concurrent: new_config.max_concurrent_captures as usize,
                                overlap_size, // Use captured overlap_size
                                send_timeout: Duration::from_millis(100),
                            };
                            supervisor.update_config(new_tailer_config);
                            supervisor.update_budget(new_config.capture_budgets.clone());
                            pane_priorities = new_config.pane_priorities.clone();
                        }

                        // Get current observed panes from registry
                        let observed_panes: HashMap<u64, PaneInfo> = {
                            let reg = registry.read().await;
                            reg.observed_pane_ids()
                                .into_iter()
                                .filter_map(|id| reg.get_entry(id).map(|e| (id, e.info.clone())))
                                .collect()
                        };

                        supervisor.sync_tailers(&observed_panes);

                        // Update effective priorities (config rules + runtime overrides).
                        //
                        // This is intentionally computed in the runtime (not the tailer) so:
                        // - the tailer stays transport/scheduler focused
                        // - overrides can be set via IPC without restarting
                        let effective_priorities: HashMap<u64, u32> = {
                            let now = epoch_ms();
                            let mut reg = registry.write().await;
                            reg.purge_expired_priority_overrides(now);

                            reg.observed_pane_ids()
                                .into_iter()
                                .filter_map(|id| {
                                    let entry = reg.get_entry(id)?;
                                    let domain = entry.info.inferred_domain();
                                    let title = entry.info.title.as_deref().unwrap_or("");
                                    let cwd = entry.info.cwd.as_deref().unwrap_or("");
                                    let base =
                                        pane_priorities.priority_for_pane(&domain, title, cwd);
                                    let override_priority = entry
                                        .priority_override
                                        .as_ref()
                                        .and_then(|ov| {
                                            if ov.expires_at.is_some_and(|exp| exp <= now) {
                                                None
                                            } else {
                                                Some(ov.priority)
                                            }
                                        });
                                    Some((id, override_priority.unwrap_or(base)))
                                })
                                .collect()
                        };
                        supervisor.update_pane_priorities(effective_priorities);

                        // Publish scheduler snapshot for health reporting.
                        *scheduler_snapshot.write().await = supervisor.scheduler_snapshot();

                        debug!(
                            active_tailers = supervisor.active_count(),
                            observed_panes = observed_panes.len(),
                            "Tailer sync tick"
                        );
                    }
                    // Handle completed captures
                    Some(result) = join_set.join_next(), if !join_set.is_empty() => {
                        match result {
                            Ok((pane_id, outcome)) => supervisor.handle_poll_result(pane_id, outcome),
                            Err(e) => {
                                warn!(error = %e, "Tailer poll task failed");
                            }
                        }
                    }
                    // Spawn new captures if slots available
                    () = sleep(tick_duration) => {
                         if shutdown_flag.load(Ordering::SeqCst) {
                            break;
                        }
                        supervisor.spawn_ready(&mut join_set);
                    }
                }
            }

            // Graceful shutdown of all tailers
            supervisor.shutdown().await;
        })
    }

    /// Spawn a no-op capture task when native events are used for output capture.
    fn spawn_idle_capture_task(&self) -> JoinHandle<()> {
        let shutdown_flag = Arc::clone(&self.shutdown_flag);

        tokio::spawn(async move {
            let idle_capture_interval = Duration::from_millis(500);
            loop {
                if shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }
                sleep(idle_capture_interval).await;
            }
        })
    }

    /// Spawn the native event listener task (vendored WezTerm integration).
    #[cfg(feature = "native-wezterm")]
    fn spawn_native_event_task(
        &self,
        socket_path: PathBuf,
        capture_tx: mpsc::Sender<CaptureEvent>,
    ) -> JoinHandle<()> {
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let cursors = Arc::clone(&self.cursors);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let storage = Arc::clone(&self.storage);
        let metrics = Arc::clone(&self.metrics);
        let event_bus = self.event_bus.clone();
        let pane_filter = self.config.pane_filter.clone();

        tokio::spawn(async move {
            let listener = match NativeEventListener::bind(socket_path.clone()).await {
                Ok(listener) => listener,
                Err(err) => {
                    warn!(error = %err, path = %socket_path.display(), "Failed to bind native event socket");
                    return;
                }
            };

            let (event_tx, mut event_rx) = mpsc::channel::<NativeEvent>(1024);

            let accept_handle = tokio::spawn(listener.run(event_tx, Arc::clone(&shutdown_flag)));

            let mut coalescer = NativeOutputCoalescer::new(
                NATIVE_OUTPUT_COALESCE_WINDOW_MS,
                NATIVE_OUTPUT_COALESCE_MAX_DELAY_MS,
                NATIVE_OUTPUT_COALESCE_MAX_BYTES,
            );
            let start = Instant::now();
            let flush_interval = Duration::from_millis(NATIVE_OUTPUT_COALESCE_WINDOW_MS / 2)
                .max(Duration::from_millis(5));
            let mut next_flush = Instant::now() + flush_interval;

            loop {
                let now = Instant::now();
                if now >= next_flush {
                    next_flush = now + flush_interval;
                    let now_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    for item in coalescer.drain_due(now_ms) {
                        metrics.record_native_output_batch(item.input_events, item.bytes.len());
                        emit_native_output_delta(
                            item.pane_id,
                            item.bytes,
                            item.timestamp_ms,
                            &capture_tx,
                            &cursors,
                        )
                        .await;
                    }

                    if shutdown_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    continue;
                }

                let flush_wait = next_flush.saturating_duration_since(now);
                match timeout(flush_wait, event_rx.recv()).await {
                    Ok(maybe_event) => {
                        let Some(event) = maybe_event else {
                            break;
                        };

                        if shutdown_flag.load(Ordering::SeqCst) {
                            break;
                        }

                        match event {
                            NativeEvent::PaneOutput {
                                pane_id,
                                data,
                                timestamp_ms,
                            } => {
                                metrics.record_native_output_input(data.len());
                                let now_ms =
                                    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                                if let Some(item) =
                                    coalescer.push(pane_id, data, timestamp_ms, now_ms)
                                {
                                    metrics.record_native_output_batch(
                                        item.input_events,
                                        item.bytes.len(),
                                    );
                                    emit_native_output_delta(
                                        item.pane_id,
                                        item.bytes,
                                        item.timestamp_ms,
                                        &capture_tx,
                                        &cursors,
                                    )
                                    .await;
                                }
                            }
                            NativeEvent::StateChange { pane_id, .. }
                            | NativeEvent::PaneDestroyed { pane_id, .. } => {
                                if let Some(item) = coalescer.flush_pane(pane_id) {
                                    metrics.record_native_output_batch(
                                        item.input_events,
                                        item.bytes.len(),
                                    );
                                    emit_native_output_delta(
                                        item.pane_id,
                                        item.bytes,
                                        item.timestamp_ms,
                                        &capture_tx,
                                        &cursors,
                                    )
                                    .await;
                                }

                                handle_native_event(
                                    event,
                                    &capture_tx,
                                    &cursors,
                                    &detection_contexts,
                                    &storage,
                                    event_bus.as_ref(),
                                    &pane_filter,
                                )
                                .await;
                            }
                            _ => {
                                handle_native_event(
                                    event,
                                    &capture_tx,
                                    &cursors,
                                    &detection_contexts,
                                    &storage,
                                    event_bus.as_ref(),
                                    &pane_filter,
                                )
                                .await;
                            }
                        }
                    }
                    Err(_elapsed) => {
                        // Timer elapsed; next loop iteration drains due batches.
                    }
                }
            }

            for item in coalescer.drain_all() {
                metrics.record_native_output_batch(item.input_events, item.bytes.len());
                emit_native_output_delta(
                    item.pane_id,
                    item.bytes,
                    item.timestamp_ms,
                    &capture_tx,
                    &cursors,
                )
                .await;
            }

            let _ = accept_handle.await;
        })
    }

    /// Spawn relay task from capture ingress to lock-free SPSC persistence queue.
    ///
    /// Capture producers (tailers/native handlers) write into a bounded MPSC.
    /// This task is the sole producer for the SPSC ring consumed by persistence.
    fn spawn_capture_relay_task(
        &self,
        mut capture_ingress_rx: mpsc::Receiver<CaptureEvent>,
        capture_ring_tx: SpscProducer<CaptureEvent>,
    ) -> JoinHandle<()> {
        let shutdown_flag = Arc::clone(&self.shutdown_flag);

        tokio::spawn(async move {
            loop {
                match timeout(
                    Duration::from_millis(25),
                    mpsc_recv_option(&mut capture_ingress_rx),
                )
                .await
                {
                    Ok(maybe_event) => match maybe_event {
                        Some(event) => {
                            if shutdown_flag.load(Ordering::SeqCst) {
                                debug!(
                                    "Capture relay: shutdown signal received, draining remaining events"
                                );
                            }

                            if capture_ring_tx.send(event).await.is_err() {
                                debug!("Capture relay: persistence ring closed");
                                return;
                            }
                        }
                        None => break,
                    },
                    Err(_elapsed) => {
                        if shutdown_flag.load(Ordering::SeqCst) && capture_ingress_rx.is_empty() {
                            break;
                        }
                    }
                }
            }

            capture_ring_tx.close();
            debug!("Capture relay exited");
        })
    }

    /// Spawn the persistence and detection task.
    fn spawn_persistence_task(
        &self,
        capture_rx: SpscConsumer<CaptureEvent>,
        cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
        registry: Arc<RwLock<PaneRegistry>>,
    ) -> JoinHandle<()> {
        let storage = Arc::clone(&self.storage);
        let pattern_engine = Arc::clone(&self.pattern_engine);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let metrics = Arc::clone(&self.metrics);
        let event_bus = self.event_bus.clone();
        let recording = self.recording.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let mut config_rx = self.config_rx.clone();
        let mut current_patterns = self.config.patterns.clone();
        let patterns_root = self.config.patterns_root.clone();
        let registry = Arc::clone(&registry);

        tokio::spawn(async move {
            // Process events until producer is closed and the ring is drained.
            while let Some(event) = capture_rx.recv().await {
                heartbeats.record_persistence();
                // Check shutdown flag - if set, drain remaining events quickly
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Persistence task: shutdown signal received, draining remaining events");
                    // Continue to drain but don't block forever
                }

                if watch_has_changed(&config_rx) {
                    let new_config = watch_borrow_and_update_clone(&mut config_rx);
                    if new_config.patterns != current_patterns {
                        match PatternEngine::from_config_with_root(
                            &new_config.patterns,
                            patterns_root.as_deref(),
                        ) {
                            Ok(engine) => {
                                let mut guard = pattern_engine.write().await;
                                *guard = engine;
                                current_patterns = new_config.patterns;
                                info!("Pattern engine reloaded from updated config");
                            }
                            Err(err) => {
                                warn!(
                                    error = %err,
                                    "Failed to reload pattern engine from updated config"
                                );
                            }
                        }
                    }
                }
                let pane_id = event.segment.pane_id;
                let content = event.segment.content.clone();
                let captured_at = event.segment.captured_at;
                let captured_seq = event.segment.seq;

                // Persist the segment
                let (storage_guard, lock_held_since) =
                    lock_storage_with_profile(&storage, &metrics).await;
                match persist_captured_segment(&storage_guard, &event.segment).await {
                    Ok(persisted) => {
                        // Check for sequence discontinuity and resync cursor if needed
                        if persisted.segment.seq != captured_seq {
                            warn!(
                                pane_id,
                                expected_seq = captured_seq,
                                actual_seq = persisted.segment.seq,
                                "Sequence discontinuity detected, resyncing cursor"
                            );
                            let mut cursors_guard = cursors.write().await;
                            if let Some(cursor) = cursors_guard.get_mut(&pane_id) {
                                cursor.resync_seq(persisted.segment.seq);
                            }
                        }

                        // Track metrics
                        metrics.segments_persisted.increment();

                        // Record ingest lag (time from capture to persistence)
                        let now = epoch_ms();
                        let lag_ms = u64::try_from((now - captured_at).max(0)).unwrap_or(0);
                        metrics.record_ingest_lag(lag_ms);
                        metrics.record_db_write();

                        debug!(
                            pane_id = pane_id,
                            seq = persisted.segment.seq,
                            has_gap = persisted.gap.is_some(),
                            "Persisted segment"
                        );

                        if let Some(ref manager) = recording {
                            if let Err(err) = manager.record_segment(&event.segment).await {
                                warn!(
                                    pane_id = pane_id,
                                    error = %err,
                                    "Failed to record segment"
                                );
                            }
                        }

                        // Publish delta/gap events for live stream subscribers.
                        if let Some(ref bus) = event_bus {
                            let delivered = bus.publish(crate::events::Event::SegmentCaptured {
                                pane_id,
                                seq: persisted.segment.seq,
                                content_len: persisted.segment.content_len,
                            });
                            if delivered == 0 {
                                debug!(pane_id, "No subscribers for segment event bus");
                            }

                            if let Some(gap) = &persisted.gap {
                                let delivered_gap =
                                    bus.publish(crate::events::Event::GapDetected {
                                        pane_id: gap.pane_id,
                                        reason: gap.reason.clone(),
                                    });
                                if delivered_gap == 0 {
                                    debug!(pane_id, "No subscribers for gap event bus");
                                }
                            }
                        }

                        // Run pattern detection on the content
                        let detections = {
                            let mut ctx = {
                                let mut contexts = detection_contexts.write().await;
                                contexts.remove(&pane_id).unwrap_or_else(|| {
                                    let mut c = DetectionContext::new();
                                    c.pane_id = Some(pane_id);
                                    c
                                })
                            };

                            // If this was a gap/discontinuity, clear the tail buffer because
                            // previous context is no longer valid or contiguous.
                            if persisted.gap.is_some() {
                                ctx.tail_buffer.clear();
                            }

                            let detections = {
                                let engine = pattern_engine.read().await;
                                engine.detect_with_context(&content, &mut ctx)
                            };

                            {
                                let mut contexts = detection_contexts.write().await;
                                contexts.insert(pane_id, ctx);
                            }
                            detections
                        };

                        if !detections.is_empty() {
                            debug!(
                                pane_id = pane_id,
                                count = detections.len(),
                                "Pattern detections"
                            );

                            let pane_uuid = {
                                let registry_guard = registry.read().await;
                                registry_guard
                                    .get_entry(pane_id)
                                    .map(|entry| entry.pane_uuid.clone())
                            };

                            // Persist each detection as an event
                            for detection in detections {
                                if let Some(ref manager) = recording {
                                    if let Err(err) =
                                        manager.record_event(pane_id, &detection, captured_at).await
                                    {
                                        warn!(
                                            pane_id = pane_id,
                                            rule_id = %detection.rule_id,
                                            error = %err,
                                            "Failed to record detection"
                                        );
                                    }
                                }
                                let stored_event = detection_to_stored_event(
                                    pane_id,
                                    pane_uuid.as_deref(),
                                    &detection,
                                    Some(persisted.segment.id),
                                );

                                match storage_guard.record_event(stored_event).await {
                                    Ok(event_id) => {
                                        metrics.events_recorded.increment();

                                        // Publish to event bus for workflow runners (if configured)
                                        if let Some(ref bus) = event_bus {
                                            let event = crate::events::Event::PatternDetected {
                                                pane_id,
                                                pane_uuid: pane_uuid.clone(),
                                                detection: detection.clone(),
                                                event_id: Some(event_id),
                                            };
                                            let delivered = bus.publish(event);
                                            if delivered == 0 {
                                                debug!(
                                                    pane_id = pane_id,
                                                    rule_id = %detection.rule_id,
                                                    "No subscribers for detection event bus"
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(
                                            pane_id = pane_id,
                                            rule_id = detection.rule_id,
                                            error = %e,
                                            "Failed to record event"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(pane_id = pane_id, error = %e, "Failed to persist segment");
                    }
                }
                drop(storage_guard);
                metrics.record_storage_lock_hold(lock_held_since.elapsed());
            }
        })
    }

    /// Signal tasks to begin shutdown.
    pub fn signal_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
    }

    /// Take ownership of the storage handle for external shutdown.
    ///
    /// Returns the storage handle wrapped in Arc<Mutex>. The caller is
    /// responsible for shutdown. This invalidates the runtime.
    #[must_use]
    pub fn take_storage(self) -> Arc<tokio::sync::Mutex<StorageHandle>> {
        self.storage
    }
}

#[cfg(feature = "native-wezterm")]
async fn handle_native_event(
    event: NativeEvent,
    capture_tx: &mpsc::Sender<CaptureEvent>,
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
    detection_contexts: &Arc<RwLock<HashMap<u64, DetectionContext>>>,
    storage: &Arc<tokio::sync::Mutex<StorageHandle>>,
    event_bus: Option<&Arc<EventBus>>,
    pane_filter: &PaneFilterConfig,
) {
    match event {
        NativeEvent::PaneOutput {
            pane_id,
            data,
            timestamp_ms,
        } => {
            emit_native_output_delta(pane_id, data, timestamp_ms, capture_tx, cursors).await;
        }
        NativeEvent::StateChange { pane_id, state, .. } => {
            let mut gap_segment = None;
            {
                let mut cursors_guard = cursors.write().await;
                if let Some(cursor) = cursors_guard.get_mut(&pane_id) {
                    if cursor.in_alt_screen != state.is_alt_screen {
                        let reason = if state.is_alt_screen {
                            "alt_screen_entered"
                        } else {
                            "alt_screen_exited"
                        };
                        cursor.in_alt_screen = state.is_alt_screen;
                        gap_segment = Some(cursor.emit_gap(reason));
                    } else {
                        cursor.in_alt_screen = state.is_alt_screen;
                    }
                }
            }

            if let Some(segment) = gap_segment {
                if capture_tx.try_send(CaptureEvent { segment }).is_err() {
                    debug!(pane_id, "Native event queue full; dropping gap");
                }
            }
        }
        NativeEvent::UserVarChanged {
            pane_id,
            name,
            value,
            ..
        } => {
            if let Some(bus) = event_bus {
                match UserVarPayload::decode(&value, true) {
                    Ok(payload) => {
                        let event = Event::UserVarReceived {
                            pane_id,
                            name,
                            payload,
                        };
                        let _ = bus.publish(event);
                    }
                    Err(err) => {
                        debug!(pane_id, error = %err, "Failed to decode native user-var payload");
                    }
                }
            }
        }
        NativeEvent::PaneCreated {
            pane_id,
            domain,
            cwd,
            timestamp_ms,
        } => {
            let ignore_reason = pane_filter.check_pane(&domain, "", cwd.as_deref().unwrap_or(""));
            let observed = ignore_reason.is_none();

            let record = PaneRecord {
                pane_id,
                pane_uuid: None,
                domain,
                window_id: None,
                tab_id: None,
                title: None,
                cwd,
                tty_name: None,
                first_seen_at: timestamp_ms,
                last_seen_at: timestamp_ms,
                observed,
                ignore_reason,
                last_decision_at: Some(timestamp_ms),
            };

            let storage_guard = storage.lock().await;
            if let Err(err) = storage_guard.upsert_pane(record).await {
                warn!(pane_id, error = %err, "Failed to upsert pane from native event");
            }
            let max_seq = storage_guard.get_max_seq(pane_id).await.unwrap_or(None);
            drop(storage_guard);

            if observed {
                let next_seq = max_seq.map_or(0, |seq| seq + 1);

                {
                    let mut cursors_guard = cursors.write().await;
                    cursors_guard
                        .entry(pane_id)
                        .or_insert_with(|| PaneCursor::from_seq(pane_id, next_seq));
                }

                {
                    let mut contexts = detection_contexts.write().await;
                    contexts.entry(pane_id).or_insert_with(|| {
                        let mut ctx = DetectionContext::new();
                        ctx.pane_id = Some(pane_id);
                        ctx
                    });
                }
            }
        }
        NativeEvent::PaneDestroyed { pane_id, .. } => {
            let mut cursors_guard = cursors.write().await;
            cursors_guard.remove(&pane_id);
            drop(cursors_guard);

            let mut contexts = detection_contexts.write().await;
            contexts.remove(&pane_id);
        }
    }
}

#[cfg(feature = "native-wezterm")]
async fn emit_native_output_delta(
    pane_id: u64,
    data: Vec<u8>,
    timestamp_ms: i64,
    capture_tx: &mpsc::Sender<CaptureEvent>,
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
) {
    if data.is_empty() {
        return;
    }

    let content = String::from_utf8_lossy(&data).to_string();
    let segment = {
        let mut cursors_guard = cursors.write().await;
        cursors_guard
            .get_mut(&pane_id)
            .map(|cursor| cursor.capture_delta(content, timestamp_ms))
    };

    if let Some(segment) = segment {
        if capture_tx.try_send(CaptureEvent { segment }).is_err() {
            debug!(pane_id, "Native event queue full; dropping output");
        }
    } else {
        debug!(
            pane_id,
            "Native output received before cursor initialized; dropping"
        );
    }
}

/// Handle to the running observation runtime.
pub struct RuntimeHandle {
    /// Discovery task handle
    pub discovery: JoinHandle<()>,
    /// Capture task handle
    pub capture: JoinHandle<()>,
    /// Relay task handle (capture ingress -> SPSC persistence queue)
    pub relay: JoinHandle<()>,
    /// Native events listener task handle (optional)
    pub native_events: Option<JoinHandle<()>>,
    /// Persistence task handle
    pub persistence: JoinHandle<()>,
    /// Maintenance task handle (retention, checkpointing)
    pub maintenance: Option<JoinHandle<()>>,
    /// Snapshot engine task handle (session persistence)
    pub snapshot: Option<JoinHandle<()>>,
    /// Snapshot trigger bridge task handle (event/health → snapshot trigger)
    pub snapshot_triggers: Option<JoinHandle<()>>,
    /// Snapshot engine shutdown sender (bridges AtomicBool → watch channel)
    snapshot_shutdown: Option<watch::Sender<bool>>,
    /// Shutdown flag for signaling tasks
    pub shutdown_flag: Arc<AtomicBool>,
    /// Storage handle for external access
    pub storage: Arc<tokio::sync::Mutex<StorageHandle>>,
    /// Runtime metrics
    pub metrics: Arc<RuntimeMetrics>,
    /// Pane registry
    pub registry: Arc<RwLock<PaneRegistry>>,
    /// Per-pane cursors
    pub cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
    /// Runtime start time
    pub start_time: Instant,
    /// Hot-reload config sender for broadcasting updates
    config_tx: Arc<watch::Sender<HotReloadableConfig>>,
    /// Optional event bus for workflow integration
    pub event_bus: Option<Arc<EventBus>>,
    /// Heartbeat registry for watchdog monitoring
    pub heartbeats: Arc<HeartbeatRegistry>,
    /// Capture channel sender (cloned for queue depth instrumentation)
    capture_tx: mpsc::Sender<CaptureEvent>,
    /// WezTerm interface handle for health/warning probes.
    wezterm_handle: WeztermHandle,
    /// Shared scheduler snapshot for health reporting (written by capture task).
    scheduler_snapshot: Arc<RwLock<crate::tailer::SchedulerSnapshot>>,
}

/// Backpressure warning threshold as a fraction of channel capacity.
///
/// When queue depth exceeds this fraction of max capacity, a warning is
/// included in the health snapshot.  0.75 = warn at 75% full.
const BACKPRESSURE_WARN_RATIO: f64 = 0.75;
/// Storage lock contention threshold (microseconds) used for contention counts.
const STORAGE_LOCK_CONTENTION_MIN_US: u64 = 1_000;
/// Maximum acceptable storage lock wait for healthy operation.
const STORAGE_LOCK_WAIT_WARN_MS: f64 = 15.0;
/// Maximum acceptable storage lock hold for healthy operation.
const STORAGE_LOCK_HOLD_WARN_MS: f64 = 75.0;
/// Memory warning threshold for retained pane snapshots.
const CURSOR_SNAPSHOT_MEMORY_WARN_BYTES: u64 = 64 * 1024 * 1024;
/// Poll interval for snapshot trigger bridge maintenance checks.
const SNAPSHOT_TRIGGER_BRIDGE_TICK_SECS: u64 = 30;
/// Idle duration before emitting `IdleWindow` trigger.
const SNAPSHOT_IDLE_WINDOW_SECS: u64 = 5 * 60;
/// Minimum interval between `MemoryPressure` trigger emissions.
const SNAPSHOT_MEMORY_TRIGGER_COOLDOWN_SECS: u64 = 2 * 60;

fn watch_has_changed<T>(rx: &watch::Receiver<T>) -> bool {
    #[cfg(feature = "asupersync-runtime")]
    {
        rx.has_changed()
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        rx.has_changed().unwrap_or(false)
    }
}

fn watch_borrow_and_update_clone<T: Clone>(rx: &mut watch::Receiver<T>) -> T {
    #[cfg(feature = "asupersync-runtime")]
    {
        rx.borrow_and_clone()
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        rx.borrow_and_update().clone()
    }
}

fn mpsc_max_capacity<T>(tx: &mpsc::Sender<T>) -> usize {
    #[cfg(feature = "asupersync-runtime")]
    {
        tx.capacity()
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        tx.max_capacity()
    }
}

async fn mpsc_recv_option<T>(rx: &mut mpsc::Receiver<T>) -> Option<T> {
    #[cfg(feature = "asupersync-runtime")]
    {
        let cx = crate::cx::for_testing();
        rx.recv(&cx).await.ok()
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        rx.recv().await
    }
}

impl RuntimeHandle {
    /// Current capture channel queue depth (pending items waiting for persistence).
    #[must_use]
    pub fn capture_queue_depth(&self) -> usize {
        mpsc_max_capacity(&self.capture_tx).saturating_sub(self.capture_tx.capacity())
    }

    /// Maximum capture channel capacity.
    #[must_use]
    pub fn capture_queue_capacity(&self) -> usize {
        mpsc_max_capacity(&self.capture_tx)
    }

    /// Current write queue depth (pending commands for the storage writer thread).
    pub async fn write_queue_depth(&self) -> usize {
        let (storage_guard, lock_held_since) =
            lock_storage_with_profile(&self.storage, &self.metrics).await;
        let depth = storage_guard.write_queue_depth();
        drop(storage_guard);
        self.metrics
            .record_storage_lock_hold(lock_held_since.elapsed());
        depth
    }

    /// Wait for all tasks to complete.
    pub async fn join(self) {
        let _ = self.discovery.await;
        let _ = self.capture.await;
        let _ = self.relay.await;
        if let Some(native) = self.native_events {
            let _ = native.await;
        }
        let _ = self.persistence.await;
        if let Some(maintenance) = self.maintenance {
            let _ = maintenance.await;
        }
        if let Some(snapshot) = self.snapshot {
            let _ = snapshot.await;
        }
        if let Some(snapshot_triggers) = self.snapshot_triggers {
            let _ = snapshot_triggers.await;
        }
    }

    /// Request graceful shutdown and collect a summary.
    ///
    /// This method:
    /// 1. Sets the shutdown flag to signal all tasks
    /// 2. Waits for tasks to complete (with timeout)
    /// 3. Flushes storage
    /// 4. Collects and returns a shutdown summary
    pub async fn shutdown_with_summary(self) -> ShutdownSummary {
        let elapsed_secs = self.start_time.elapsed().as_secs();
        let mut warnings = Vec::new();

        // Signal shutdown
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Some(ref tx) = self.snapshot_shutdown {
            let _ = tx.send(true);
        }
        info!("Shutdown signal sent");

        // Wait for tasks with timeout
        let shutdown_timeout = Duration::from_secs(5);
        let join_result = timeout(shutdown_timeout, async {
            let _ = self.discovery.await;
            let _ = self.capture.await;
            let _ = self.relay.await;
            if let Some(native) = self.native_events {
                let _ = native.await;
            }
            let _ = self.persistence.await;
            if let Some(snapshot) = self.snapshot {
                let _ = snapshot.await;
            }
            if let Some(snapshot_triggers) = self.snapshot_triggers {
                let _ = snapshot_triggers.await;
            }
        })
        .await;

        let clean = if join_result.is_err() {
            warnings.push("Tasks did not complete within timeout".to_string());
            false
        } else {
            true
        };

        // Get final metrics
        let segments_persisted = self.metrics.segments_persisted.get();
        let events_recorded = self.metrics.events_recorded.get();

        // Get last seq per pane
        let last_seq_by_pane: Vec<(u64, i64)> = {
            let cursors = self.cursors.read().await;
            cursors
                .iter()
                .map(|(pane_id, cursor)| (*pane_id, cursor.last_seq()))
                .collect()
        };

        // Flush storage
        {
            let (storage_guard, lock_held_since) =
                lock_storage_with_profile(&self.storage, &self.metrics).await;
            if let Err(e) = storage_guard.shutdown().await {
                warnings.push(format!("Storage shutdown error: {e}"));
            }
            drop(storage_guard);
            self.metrics
                .record_storage_lock_hold(lock_held_since.elapsed());
        }

        ShutdownSummary {
            elapsed_secs,
            final_capture_queue: 0, // Channel is consumed
            final_write_queue: 0,
            segments_persisted,
            events_recorded,
            last_seq_by_pane,
            clean,
            warnings,
        }
    }

    /// Request graceful shutdown.
    ///
    /// Sets the shutdown flag and waits for tasks to complete.
    pub async fn shutdown(self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Some(ref tx) = self.snapshot_shutdown {
            let _ = tx.send(true);
        }
        self.join().await;
    }

    /// Signal shutdown without waiting.
    pub fn signal_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Some(ref tx) = self.snapshot_shutdown {
            let _ = tx.send(true);
        }
    }

    /// Update the global health snapshot from current runtime state.
    ///
    /// Call this periodically (e.g., every 30s) to keep crash reports useful.
    pub async fn update_health_snapshot(&self) {
        let (observed_panes, last_activity_by_pane) = {
            let reg = self.registry.read().await;
            let ids = reg.observed_pane_ids();
            let activity: Vec<(u64, u64)> = reg
                .entries()
                .filter(|(_, e)| e.should_observe())
                .map(|(id, e)| {
                    #[allow(clippy::cast_sign_loss)]
                    (*id, e.last_seen_at as u64)
                })
                .collect();
            (ids.len(), activity)
        };

        let (last_seq_by_pane, cursor_snapshot_bytes): (Vec<(u64, i64)>, u64) = {
            let cursors = self.cursors.read().await;
            let mut total_bytes = 0u64;
            let seqs = cursors
                .iter()
                .map(|(pane_id, cursor)| {
                    total_bytes = total_bytes.saturating_add(
                        u64::try_from(cursor.last_snapshot.len()).unwrap_or(u64::MAX),
                    );
                    (*pane_id, cursor.last_seq())
                })
                .collect();
            (seqs, total_bytes)
        };
        self.metrics
            .record_cursor_snapshot_memory(cursor_snapshot_bytes);

        // Measure queue depths for backpressure visibility
        let capture_depth = self.capture_queue_depth();
        let capture_cap = self.capture_queue_capacity();

        let (write_depth, write_cap, db_writable) = {
            let (storage_guard, lock_held_since) =
                lock_storage_with_profile(&self.storage, &self.metrics).await;
            let wd = storage_guard.write_queue_depth();
            let wc = storage_guard.write_queue_capacity();
            let writable = storage_guard.is_writable().await;
            drop(storage_guard);
            self.metrics
                .record_storage_lock_hold(lock_held_since.elapsed());
            (wd, wc, writable)
        };

        // Generate backpressure warnings
        let mut warnings = Vec::new();

        #[allow(clippy::cast_precision_loss)]
        if capture_cap > 0 {
            let ratio = capture_depth as f64 / capture_cap as f64;
            if ratio >= BACKPRESSURE_WARN_RATIO {
                warnings.push(format!(
                    "Capture queue backpressure: {capture_depth}/{capture_cap} ({:.0}%)",
                    ratio * 100.0
                ));
            }
        }

        #[allow(clippy::cast_precision_loss)]
        if write_cap > 0 {
            let ratio = write_depth as f64 / write_cap as f64;
            if ratio >= BACKPRESSURE_WARN_RATIO {
                warnings.push(format!(
                    "Write queue backpressure: {write_depth}/{write_cap} ({:.0}%)",
                    ratio * 100.0
                ));
            }
        }

        if !db_writable {
            warnings.push("Database is not writable".to_string());
        }
        if self.metrics.max_storage_lock_wait_ms() >= STORAGE_LOCK_WAIT_WARN_MS {
            warnings.push(format!(
                "Storage lock contention: wait max {:.2} ms, avg {:.2} ms, events {}",
                self.metrics.max_storage_lock_wait_ms(),
                self.metrics.avg_storage_lock_wait_ms(),
                self.metrics.storage_lock_contention_events()
            ));
        }
        if self.metrics.max_storage_lock_hold_ms() >= STORAGE_LOCK_HOLD_WARN_MS {
            warnings.push(format!(
                "Storage lock hold high: max {:.2} ms, avg {:.2} ms",
                self.metrics.max_storage_lock_hold_ms(),
                self.metrics.avg_storage_lock_hold_ms(),
            ));
        }
        if cursor_snapshot_bytes >= CURSOR_SNAPSHOT_MEMORY_WARN_BYTES {
            warnings.push(format!(
                "Cursor snapshot memory high: {:.1} MiB (peak {:.1} MiB)",
                bytes_to_mib(cursor_snapshot_bytes),
                bytes_to_mib(self.metrics.cursor_snapshot_bytes_max()),
            ));
        }
        match self.wezterm_handle.watchdog_warnings().await {
            Ok(wezterm_warnings) => warnings.extend(wezterm_warnings),
            Err(err) => warnings.push(format!("WezTerm health warning probe failed: {err}")),
        }
        if let Some(resize_watchdog) = evaluate_resize_watchdog(epoch_ms_u64()) {
            if let Some(line) = resize_watchdog.warning_line() {
                warnings.push(line);
            }
            let ladder = derive_resize_degradation_ladder(&resize_watchdog);
            if let Some(line) = ladder.warning_line() {
                warnings.push(line);
            }
        }

        let snapshot = HealthSnapshot {
            timestamp: epoch_ms_u64(),
            observed_panes,
            capture_queue_depth: capture_depth,
            write_queue_depth: write_depth,
            last_seq_by_pane,
            warnings,
            ingest_lag_avg_ms: self.metrics.avg_ingest_lag_ms(),
            ingest_lag_max_ms: self.metrics.max_ingest_lag_ms(),
            db_writable,
            db_last_write_at: self.metrics.last_db_write(),
            pane_priority_overrides: {
                let now = epoch_ms();
                let reg = self.registry.read().await;
                reg.list_active_priority_overrides(now)
                    .into_iter()
                    .map(|(pane_id, ov)| crate::crash::PanePriorityOverrideSnapshot {
                        pane_id,
                        priority: ov.priority,
                        expires_at: ov.expires_at.and_then(|e| u64::try_from(e).ok()),
                    })
                    .collect()
            },
            scheduler: {
                let snap = self.scheduler_snapshot.read().await;
                if snap.budget_active {
                    Some(snap.clone())
                } else {
                    None
                }
            },
            backpressure_tier: None,
            last_activity_by_pane,
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
        };

        HealthSnapshot::update_global(snapshot);
        RuntimeLockMemoryTelemetrySnapshot::update_global(self.metrics.lock_memory_snapshot());
    }

    /// Take ownership of the storage handle for external shutdown.
    ///
    /// The caller is responsible for shutdown. This invalidates the runtime.
    #[must_use]
    pub fn take_storage(self) -> Arc<tokio::sync::Mutex<StorageHandle>> {
        self.storage
    }

    /// Apply a hot-reloadable config update.
    ///
    /// Broadcasts the new config to all running tasks. Returns `Ok(())` if the
    /// update was sent successfully, or an error if the channel is closed.
    ///
    /// # Arguments
    /// * `new_config` - The new hot-reloadable configuration values
    ///
    /// # Errors
    /// Returns an error if the config channel is closed (runtime shutting down).
    pub fn apply_config_update(&self, new_config: HotReloadableConfig) -> Result<()> {
        self.config_tx
            .send(new_config)
            .map_err(|e| crate::Error::Runtime(format!("Failed to send config update: {e}")))
    }

    /// Get the current hot-reloadable config.
    #[must_use]
    pub fn current_config(&self) -> HotReloadableConfig {
        self.config_tx.borrow().clone()
    }
}

async fn lock_storage_with_profile<'a>(
    storage: &'a Arc<tokio::sync::Mutex<StorageHandle>>,
    metrics: &RuntimeMetrics,
) -> (tokio::sync::MutexGuard<'a, StorageHandle>, Instant) {
    let wait_started = Instant::now();
    let guard = storage.lock().await;
    metrics.record_storage_lock_wait(wait_started.elapsed());
    (guard, Instant::now())
}

#[allow(clippy::cast_precision_loss)]
fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn record_bounded_sample(samples: &StdMutex<VecDeque<u64>>, value: u64) {
    let mut guard = samples
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.len() == TELEMETRY_PERCENTILE_WINDOW_CAPACITY {
        let _ = guard.pop_front();
    }
    guard.push_back(value);
}

fn percentile_from_samples(samples: &StdMutex<VecDeque<u64>>, percentile: usize) -> u64 {
    debug_assert!((1..=100).contains(&percentile));
    let mut values: Vec<u64> = {
        let guard = samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.iter().copied().collect()
    };
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let idx = (values.len() - 1)
        .saturating_mul(percentile)
        .saturating_add(99)
        / 100;
    values[idx]
}

/// Get current time as epoch milliseconds.
fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn epoch_ms_u64() -> u64 {
    u64::try_from(epoch_ms()).unwrap_or(0)
}

fn duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn event_counts_as_activity(event: &Event) -> bool {
    matches!(
        event,
        Event::SegmentCaptured { .. }
            | Event::GapDetected { .. }
            | Event::PatternDetected { .. }
            | Event::PaneDiscovered { .. }
            | Event::PaneDisappeared { .. }
            | Event::WorkflowStarted { .. }
            | Event::WorkflowStep { .. }
            | Event::WorkflowCompleted { .. }
            | Event::UserVarReceived { .. }
    )
}

fn snapshot_trigger_from_event(event: &Event) -> Option<crate::snapshot_engine::SnapshotTrigger> {
    use crate::snapshot_engine::SnapshotTrigger;

    match event {
        Event::PatternDetected { detection, .. } => snapshot_trigger_from_detection(detection),
        Event::WorkflowCompleted { success, .. } => {
            if *success {
                Some(SnapshotTrigger::WorkCompleted)
            } else {
                Some(SnapshotTrigger::HazardThreshold)
            }
        }
        Event::UserVarReceived { payload, .. } => snapshot_trigger_from_user_var(payload),
        Event::PaneDiscovered { .. } | Event::PaneDisappeared { .. } => {
            Some(SnapshotTrigger::StateTransition)
        }
        Event::SegmentCaptured { .. }
        | Event::GapDetected { .. }
        | Event::WorkflowStarted { .. }
        | Event::WorkflowStep { .. } => None,
    }
}

fn snapshot_trigger_from_detection(
    detection: &Detection,
) -> Option<crate::snapshot_engine::SnapshotTrigger> {
    use crate::snapshot_engine::SnapshotTrigger;

    let event_type = detection.event_type.as_str();

    if detection.severity == Severity::Critical
        || matches!(
            event_type,
            "usage.reached"
                | "error.network"
                | "error.timeout"
                | "error.overloaded"
                | "mux.error"
                | "auth.error"
                | "auth.login_required"
                | "auth.oauth_required"
        )
    {
        return Some(SnapshotTrigger::HazardThreshold);
    }

    if matches!(
        event_type,
        "session.tool_use"
            | "session.compaction_complete"
            | "session.summary"
            | "session.end"
            | "saved_search.alert"
    ) {
        return Some(SnapshotTrigger::WorkCompleted);
    }

    if matches!(
        event_type,
        "session.start"
            | "session.resume_hint"
            | "session.model"
            | "session.thinking"
            | "session.approval_needed"
    ) {
        return Some(SnapshotTrigger::StateTransition);
    }

    None
}

fn snapshot_trigger_from_user_var(
    payload: &UserVarPayload,
) -> Option<crate::snapshot_engine::SnapshotTrigger> {
    use crate::snapshot_engine::SnapshotTrigger;

    match payload.event_type.as_deref() {
        Some("command_start" | "cmd_start" | "preexec") => Some(SnapshotTrigger::StateTransition),
        Some("command_end" | "cmd_end" | "postexec") => Some(SnapshotTrigger::WorkCompleted),
        _ => None,
    }
}

/// Convert a Detection to a StoredEvent for persistence.
fn detection_to_stored_event(
    pane_id: u64,
    pane_uuid: Option<&str>,
    detection: &Detection,
    segment_id: Option<i64>,
) -> StoredEvent {
    const EVENT_DEDUPE_BUCKET_MS: i64 = 5 * 60 * 1000;
    let detected_at = epoch_ms();
    let identity_key = event_identity_key(detection, pane_id, pane_uuid);
    let bucket = if EVENT_DEDUPE_BUCKET_MS > 0 {
        detected_at / EVENT_DEDUPE_BUCKET_MS
    } else {
        0
    };
    let dedupe_key = format!("{identity_key}:{bucket}");
    StoredEvent {
        id: 0, // Will be assigned by storage
        pane_id,
        rule_id: detection.rule_id.clone(),
        agent_type: detection.agent_type.to_string(),
        event_type: detection.event_type.clone(),
        severity: match detection.severity {
            crate::patterns::Severity::Info => "info".to_string(),
            crate::patterns::Severity::Warning => "warning".to_string(),
            crate::patterns::Severity::Critical => "critical".to_string(),
        },
        confidence: detection.confidence,
        extracted: Some(detection.extracted.clone()),
        matched_text: Some(detection.matched_text.clone()),
        segment_id,
        detected_at,
        dedupe_key: Some(dedupe_key),
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::PaneRecord;
    use tempfile::TempDir;

    async fn send_mpsc<T>(tx: &mpsc::Sender<T>, value: T) {
        #[cfg(feature = "asupersync-runtime")]
        {
            let cx = crate::cx::for_testing();
            let sent = tx.send(&cx, value).await;
            assert!(sent.is_ok(), "test mpsc send should succeed");
        }
        #[cfg(not(feature = "asupersync-runtime"))]
        {
            let sent = tx.send(value).await;
            assert!(sent.is_ok(), "test mpsc send should succeed");
        }
    }

    async fn recv_mpsc<T>(rx: &mut mpsc::Receiver<T>) -> T {
        #[cfg(feature = "asupersync-runtime")]
        {
            let cx = crate::cx::for_testing();
            rx.recv(&cx)
                .await
                .expect("test mpsc recv should succeed")
        }
        #[cfg(not(feature = "asupersync-runtime"))]
        {
            rx.recv().await.expect("test mpsc recv should succeed")
        }
    }

    fn temp_db_path() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db").to_string_lossy().to_string();
        (dir, path)
    }

    #[allow(dead_code)]
    fn test_pane_record(pane_id: u64) -> PaneRecord {
        PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("test".to_string()),
            cwd: Some("/tmp".to_string()),
            tty_name: None,
            first_seen_at: epoch_ms(),
            last_seen_at: epoch_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    #[test]
    fn detection_to_stored_event_converts_correctly() {
        use crate::patterns::{AgentType, Severity};

        let detection = Detection {
            rule_id: "test.rule".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "test_event".to_string(),
            severity: Severity::Info,
            confidence: 0.95,
            extracted: serde_json::json!({"key": "value"}),
            matched_text: "matched text".to_string(),
            span: (0, 0),
        };

        let event = detection_to_stored_event(42, Some("pane-uuid"), &detection, Some(123));

        assert_eq!(event.pane_id, 42);
        assert_eq!(event.rule_id, "test.rule");
        assert_eq!(event.event_type, "test_event");
        assert!((event.confidence - 0.95).abs() < f64::EPSILON);
        assert!(event.dedupe_key.is_some());
        assert_eq!(event.segment_id, Some(123));
        assert!(event.handled_at.is_none());
    }

    fn test_detection(event_type: &str, severity: Severity) -> Detection {
        Detection {
            rule_id: "test.rule".to_string(),
            agent_type: crate::patterns::AgentType::ClaudeCode,
            event_type: event_type.to_string(),
            severity,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: String::new(),
            span: (0, 0),
        }
    }

    #[test]
    fn snapshot_trigger_from_detection_maps_work_completed() {
        let detection = test_detection("session.tool_use", Severity::Info);
        let trigger = snapshot_trigger_from_detection(&detection);
        assert_eq!(
            trigger,
            Some(crate::snapshot_engine::SnapshotTrigger::WorkCompleted)
        );
    }

    #[test]
    fn snapshot_trigger_from_detection_maps_state_transition() {
        let detection = test_detection("session.start", Severity::Info);
        let trigger = snapshot_trigger_from_detection(&detection);
        assert_eq!(
            trigger,
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_from_detection_maps_hazard() {
        let detection = test_detection("error.timeout", Severity::Warning);
        let trigger = snapshot_trigger_from_detection(&detection);
        assert_eq!(
            trigger,
            Some(crate::snapshot_engine::SnapshotTrigger::HazardThreshold)
        );
    }

    #[test]
    fn snapshot_trigger_from_user_var_maps_command_events() {
        let start = UserVarPayload {
            value: "raw".to_string(),
            event_type: Some("command_start".to_string()),
            event_data: None,
        };
        let end = UserVarPayload {
            value: "raw".to_string(),
            event_type: Some("command_end".to_string()),
            event_data: None,
        };

        assert_eq!(
            snapshot_trigger_from_user_var(&start),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
        assert_eq!(
            snapshot_trigger_from_user_var(&end),
            Some(crate::snapshot_engine::SnapshotTrigger::WorkCompleted)
        );
    }

    #[test]
    fn snapshot_trigger_from_event_maps_workflow_outcome() {
        let ok_event = Event::WorkflowCompleted {
            workflow_id: "wf-1".to_string(),
            success: true,
            reason: None,
        };
        let fail_event = Event::WorkflowCompleted {
            workflow_id: "wf-2".to_string(),
            success: false,
            reason: Some("failed".to_string()),
        };

        assert_eq!(
            snapshot_trigger_from_event(&ok_event),
            Some(crate::snapshot_engine::SnapshotTrigger::WorkCompleted)
        );
        assert_eq!(
            snapshot_trigger_from_event(&fail_event),
            Some(crate::snapshot_engine::SnapshotTrigger::HazardThreshold)
        );
    }

    #[tokio::test]
    async fn runtime_config_defaults_are_reasonable() {
        let config = RuntimeConfig::default();

        assert_eq!(config.discovery_interval, Duration::from_secs(5));
        assert_eq!(config.capture_interval, Duration::from_millis(200));
        assert_eq!(config.overlap_size, 1_048_576); // 1MB default
        assert_eq!(config.channel_buffer, 1024);
    }

    #[tokio::test]
    async fn runtime_can_be_created() {
        let (_dir, db_path) = temp_db_path();
        let storage = StorageHandle::new(&db_path).await.unwrap();
        let engine = PatternEngine::new();

        let config = RuntimeConfig::default();
        let _runtime = ObservationRuntime::new(config, storage, Arc::new(RwLock::new(engine)));
    }

    #[test]
    fn runtime_metrics_records_ingest_lag() {
        let metrics = RuntimeMetrics::default();

        // Initially no samples
        assert!((metrics.avg_ingest_lag_ms() - 0.0).abs() < f64::EPSILON);
        assert_eq!(metrics.max_ingest_lag_ms(), 0);

        // Record some samples
        metrics.record_ingest_lag(10);
        metrics.record_ingest_lag(20);
        metrics.record_ingest_lag(30);

        // Verify average
        assert!((metrics.avg_ingest_lag_ms() - 20.0).abs() < f64::EPSILON);

        // Verify max
        assert_eq!(metrics.max_ingest_lag_ms(), 30);
    }

    #[test]
    fn runtime_metrics_tracks_max_correctly_with_decreasing_values() {
        let metrics = RuntimeMetrics::default();

        // Record high value first
        metrics.record_ingest_lag(100);
        assert_eq!(metrics.max_ingest_lag_ms(), 100);

        // Lower values shouldn't change max
        metrics.record_ingest_lag(50);
        metrics.record_ingest_lag(25);
        assert_eq!(metrics.max_ingest_lag_ms(), 100);

        // Higher value should update max
        metrics.record_ingest_lag(150);
        assert_eq!(metrics.max_ingest_lag_ms(), 150);
    }

    #[test]
    fn runtime_metrics_last_db_write() {
        let metrics = RuntimeMetrics::default();

        // Initially no writes
        assert!(metrics.last_db_write().is_none());

        // Record a write
        metrics.record_db_write();

        // Should now have a timestamp
        assert!(metrics.last_db_write().is_some());
        assert!(metrics.last_db_write().unwrap() > 0);
    }

    #[test]
    fn runtime_metrics_record_storage_lock_profiles() {
        let metrics = RuntimeMetrics::default();

        assert!((metrics.avg_storage_lock_wait_ms() - 0.0).abs() < f64::EPSILON);
        assert!((metrics.max_storage_lock_wait_ms() - 0.0).abs() < f64::EPSILON);
        assert_eq!(metrics.storage_lock_contention_events(), 0);
        assert!((metrics.avg_storage_lock_hold_ms() - 0.0).abs() < f64::EPSILON);
        assert!((metrics.max_storage_lock_hold_ms() - 0.0).abs() < f64::EPSILON);

        metrics.record_storage_lock_wait(Duration::from_micros(500));
        metrics.record_storage_lock_wait(Duration::from_micros(2_000));
        metrics.record_storage_lock_hold(Duration::from_millis(2));
        metrics.record_storage_lock_hold(Duration::from_millis(10));

        assert!(metrics.avg_storage_lock_wait_ms() > 0.0);
        assert!(metrics.max_storage_lock_wait_ms() >= 2.0);
        assert!(metrics.p50_storage_lock_wait_ms() >= 0.5);
        assert!(metrics.p95_storage_lock_wait_ms() >= metrics.p50_storage_lock_wait_ms());
        assert_eq!(metrics.storage_lock_contention_events(), 1);
        assert!(metrics.avg_storage_lock_hold_ms() >= 2.0);
        assert!(metrics.max_storage_lock_hold_ms() >= 10.0);
        assert!(metrics.p50_storage_lock_hold_ms() >= 2.0);
        assert!(metrics.p95_storage_lock_hold_ms() >= metrics.p50_storage_lock_hold_ms());
    }

    #[test]
    fn runtime_metrics_record_cursor_snapshot_memory() {
        let metrics = RuntimeMetrics::default();

        assert_eq!(metrics.cursor_snapshot_bytes_last(), 0);
        assert_eq!(metrics.cursor_snapshot_bytes_max(), 0);
        assert!((metrics.avg_cursor_snapshot_bytes() - 0.0).abs() < f64::EPSILON);

        metrics.record_cursor_snapshot_memory(1024);
        metrics.record_cursor_snapshot_memory(4096);

        assert_eq!(metrics.cursor_snapshot_bytes_last(), 4096);
        assert_eq!(metrics.cursor_snapshot_bytes_max(), 4096);
        assert!((metrics.avg_cursor_snapshot_bytes() - 2560.0).abs() < f64::EPSILON);
        assert_eq!(metrics.p50_cursor_snapshot_bytes(), 4096);
        assert_eq!(metrics.p95_cursor_snapshot_bytes(), 4096);
    }

    #[test]
    fn runtime_metrics_lock_memory_snapshot_reflects_metrics() {
        let metrics = RuntimeMetrics::default();
        metrics.record_storage_lock_wait(Duration::from_micros(750));
        metrics.record_storage_lock_wait(Duration::from_millis(2));
        metrics.record_storage_lock_hold(Duration::from_millis(4));
        metrics.record_storage_lock_hold(Duration::from_millis(12));
        metrics.record_cursor_snapshot_memory(1024);
        metrics.record_cursor_snapshot_memory(8192);

        let snapshot = metrics.lock_memory_snapshot();
        assert!(snapshot.timestamp_ms > 0);
        assert!(snapshot.avg_storage_lock_wait_ms > 0.0);
        assert!(snapshot.p50_storage_lock_wait_ms >= 0.75);
        assert!(snapshot.p95_storage_lock_wait_ms >= snapshot.p50_storage_lock_wait_ms);
        assert!(snapshot.max_storage_lock_wait_ms >= 2.0);
        assert_eq!(snapshot.storage_lock_contention_events, 1);
        assert!(snapshot.avg_storage_lock_hold_ms >= 4.0);
        assert!(snapshot.p50_storage_lock_hold_ms >= 4.0);
        assert!(snapshot.p95_storage_lock_hold_ms >= snapshot.p50_storage_lock_hold_ms);
        assert!(snapshot.max_storage_lock_hold_ms >= 12.0);
        assert_eq!(snapshot.cursor_snapshot_bytes_last, 8192);
        assert_eq!(snapshot.p50_cursor_snapshot_bytes, 8192);
        assert_eq!(snapshot.p95_cursor_snapshot_bytes, 8192);
        assert_eq!(snapshot.cursor_snapshot_bytes_max, 8192);
        assert!((snapshot.avg_cursor_snapshot_bytes - 4608.0).abs() < f64::EPSILON);
    }

    #[test]
    fn runtime_lock_memory_snapshot_global_roundtrip() {
        let snapshot = RuntimeLockMemoryTelemetrySnapshot {
            timestamp_ms: 42,
            avg_storage_lock_wait_ms: 1.25,
            p50_storage_lock_wait_ms: 1.0,
            p95_storage_lock_wait_ms: 4.5,
            max_storage_lock_wait_ms: 5.0,
            storage_lock_contention_events: 7,
            avg_storage_lock_hold_ms: 2.5,
            p50_storage_lock_hold_ms: 2.0,
            p95_storage_lock_hold_ms: 7.0,
            max_storage_lock_hold_ms: 8.0,
            cursor_snapshot_bytes_last: 128,
            p50_cursor_snapshot_bytes: 256,
            p95_cursor_snapshot_bytes: 480,
            cursor_snapshot_bytes_max: 512,
            avg_cursor_snapshot_bytes: 320.0,
        };
        RuntimeLockMemoryTelemetrySnapshot::update_global(snapshot.clone());
        assert_eq!(
            RuntimeLockMemoryTelemetrySnapshot::get_global(),
            Some(snapshot)
        );
    }

    #[test]
    fn health_snapshot_reflects_runtime_metrics() {
        use crate::crash::HealthSnapshot;

        let metrics = RuntimeMetrics::default();
        metrics.record_ingest_lag(10);
        metrics.record_ingest_lag(50);
        metrics.record_db_write();

        let snapshot = HealthSnapshot {
            timestamp: 0,
            observed_panes: 2,
            capture_queue_depth: 0,
            write_queue_depth: 0,
            last_seq_by_pane: vec![],
            warnings: vec![],
            ingest_lag_avg_ms: metrics.avg_ingest_lag_ms(),
            ingest_lag_max_ms: metrics.max_ingest_lag_ms(),
            db_writable: true,
            db_last_write_at: metrics.last_db_write(),
            pane_priority_overrides: vec![],
            scheduler: None,
            backpressure_tier: None,
            last_activity_by_pane: vec![],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
        };

        // Verify metrics are correctly reflected in snapshot
        assert!((snapshot.ingest_lag_avg_ms - 30.0).abs() < f64::EPSILON);
        assert_eq!(snapshot.ingest_lag_max_ms, 50);
        assert!(snapshot.db_writable);
        assert!(snapshot.db_last_write_at.is_some());
    }

    // =========================================================================
    // Backpressure Instrumentation Tests (wa-upg.12.2)
    // =========================================================================

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn native_output_coalescer_batches_within_window() {
        let mut c = NativeOutputCoalescer::new(50, 200, 1024 * 1024);

        assert!(c.push(1, b"a".to_vec(), 1000, 0).is_none());
        assert!(c.push(1, b"b".to_vec(), 1001, 10).is_none());
        assert!(c.push(1, b"c".to_vec(), 1002, 20).is_none());

        // Not due until >= window.
        assert!(c.drain_due(49).is_empty());

        let drained = c.drain_due(50);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].pane_id, 1);
        assert_eq!(drained[0].bytes, b"abc");
        assert_eq!(drained[0].timestamp_ms, 1002);
        assert_eq!(drained[0].input_events, 3);
    }

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn native_output_coalescer_enforces_max_delay_when_window_is_large() {
        let mut c = NativeOutputCoalescer::new(1_000, 200, 1024 * 1024);
        c.push(7, b"x".to_vec(), 555, 0);

        // Not due by window, but due by max_delay.
        assert!(c.drain_due(199).is_empty());
        let drained = c.drain_due(200);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].pane_id, 7);
    }

    #[test]
    fn backpressure_warn_ratio_is_valid() {
        assert!(BACKPRESSURE_WARN_RATIO > 0.0);
        assert!(BACKPRESSURE_WARN_RATIO < 1.0);
    }

    #[test]
    fn mpsc_queue_depth_computation_is_correct() {
        // Validates queue depth accounting for a fixed-capacity channel.
        let (tx, _rx) = mpsc::channel::<u8>(16);
        let max_cap = 16usize;
        assert_eq!(max_cap, 16);

        // Empty queue: depth should be 0
        let depth = max_cap - tx.capacity();
        assert_eq!(depth, 0);
    }

    #[tokio::test]
    async fn mpsc_queue_depth_increases_with_sends() {
        let (tx, mut rx) = mpsc::channel::<u8>(16);
        let max_cap = 16usize;

        // Send some items
        send_mpsc(&tx, 1).await;
        send_mpsc(&tx, 2).await;
        send_mpsc(&tx, 3).await;

        let depth = max_cap - tx.capacity();
        assert_eq!(depth, 3);

        // Drain one item, depth should decrease
        let _ = recv_mpsc(&mut rx).await;
        let depth = max_cap - tx.capacity();
        assert_eq!(depth, 2);
    }

    #[test]
    fn backpressure_warning_fires_above_threshold() {
        // Test the same logic used in update_health_snapshot
        let capacity = 100usize;
        let depth_below = 74usize; // 74% — below 75%
        let depth_at = 75usize; // 75% — at threshold
        let depth_above = 80usize; // 80% — above threshold

        #[allow(clippy::cast_precision_loss)]
        let ratio_below = depth_below as f64 / capacity as f64;
        #[allow(clippy::cast_precision_loss)]
        let ratio_at = depth_at as f64 / capacity as f64;
        #[allow(clippy::cast_precision_loss)]
        let ratio_above = depth_above as f64 / capacity as f64;

        assert!(
            ratio_below < BACKPRESSURE_WARN_RATIO,
            "74% should not trigger warning"
        );
        assert!(
            ratio_at >= BACKPRESSURE_WARN_RATIO,
            "75% should trigger warning"
        );
        assert!(
            ratio_above >= BACKPRESSURE_WARN_RATIO,
            "80% should trigger warning"
        );
    }

    #[test]
    fn backpressure_warning_message_format() {
        // Verify the warning format matches what update_health_snapshot produces
        let depth = 80usize;
        let cap = 100usize;
        #[allow(clippy::cast_precision_loss)]
        let ratio = depth as f64 / cap as f64;

        let warning = format!(
            "Capture queue backpressure: {depth}/{cap} ({:.0}%)",
            ratio * 100.0
        );

        assert!(warning.contains("Capture queue backpressure"));
        assert!(warning.contains("80/100"));
        assert!(warning.contains("80%"));
    }

    #[test]
    fn storage_lock_contention_warning_threshold_fires() {
        let metrics = RuntimeMetrics::default();
        metrics.record_storage_lock_wait(Duration::from_millis(20));

        assert!(metrics.max_storage_lock_wait_ms() >= STORAGE_LOCK_WAIT_WARN_MS);

        let warning = format!(
            "Storage lock contention: wait max {:.2} ms, avg {:.2} ms, events {}",
            metrics.max_storage_lock_wait_ms(),
            metrics.avg_storage_lock_wait_ms(),
            metrics.storage_lock_contention_events()
        );
        assert!(warning.contains("Storage lock contention"));
        assert!(warning.contains("events"));
    }

    #[test]
    fn storage_lock_hold_warning_threshold_fires() {
        let metrics = RuntimeMetrics::default();
        metrics.record_storage_lock_hold(Duration::from_millis(80));

        assert!(metrics.max_storage_lock_hold_ms() >= STORAGE_LOCK_HOLD_WARN_MS);

        let warning = format!(
            "Storage lock hold high: max {:.2} ms, avg {:.2} ms",
            metrics.max_storage_lock_hold_ms(),
            metrics.avg_storage_lock_hold_ms(),
        );
        assert!(warning.contains("Storage lock hold high"));
    }

    #[test]
    fn cursor_snapshot_memory_warning_threshold_fires() {
        let metrics = RuntimeMetrics::default();
        let sample = CURSOR_SNAPSHOT_MEMORY_WARN_BYTES.saturating_add(1024);
        metrics.record_cursor_snapshot_memory(sample);

        let warning = format!(
            "Cursor snapshot memory high: {:.1} MiB (peak {:.1} MiB)",
            bytes_to_mib(sample),
            bytes_to_mib(metrics.cursor_snapshot_bytes_max()),
        );
        assert!(warning.contains("Cursor snapshot memory high"));
        assert!(warning.contains("MiB"));
    }

    #[test]
    fn health_snapshot_with_queue_depths() {
        use crate::crash::HealthSnapshot;

        let snapshot = HealthSnapshot {
            timestamp: 0,
            observed_panes: 1,
            capture_queue_depth: 500,
            write_queue_depth: 200,
            last_seq_by_pane: vec![],
            warnings: vec!["Capture queue backpressure: 500/1024 (49%)".to_string()],
            ingest_lag_avg_ms: 0.0,
            ingest_lag_max_ms: 0,
            db_writable: true,
            db_last_write_at: None,
            pane_priority_overrides: vec![],
            scheduler: None,
            backpressure_tier: None,
            last_activity_by_pane: vec![],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
        };

        assert_eq!(snapshot.capture_queue_depth, 500);
        assert_eq!(snapshot.write_queue_depth, 200);
        assert_eq!(snapshot.warnings.len(), 1);
        assert!(snapshot.warnings[0].contains("backpressure"));
    }

    #[test]
    fn health_snapshot_includes_scheduler_when_active() {
        use crate::crash::HealthSnapshot;
        use crate::tailer::SchedulerSnapshot;

        let sched = SchedulerSnapshot {
            budget_active: true,
            max_captures_per_sec: 50,
            max_bytes_per_sec: 1_000_000,
            captures_remaining: 42,
            bytes_remaining: 500_000,
            total_rate_limited: 3,
            total_byte_budget_exceeded: 1,
            total_throttle_events: 4,
            tracked_panes: 5,
        };

        let snapshot = HealthSnapshot {
            timestamp: 0,
            observed_panes: 5,
            capture_queue_depth: 0,
            write_queue_depth: 0,
            last_seq_by_pane: vec![],
            warnings: vec![],
            ingest_lag_avg_ms: 0.0,
            ingest_lag_max_ms: 0,
            db_writable: true,
            db_last_write_at: None,
            pane_priority_overrides: vec![],
            scheduler: Some(sched),
            backpressure_tier: Some("Green".to_string()),
            last_activity_by_pane: vec![],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
        };

        let sched = snapshot.scheduler.as_ref().unwrap();
        assert!(sched.budget_active);
        assert_eq!(sched.max_captures_per_sec, 50);
        assert_eq!(sched.total_rate_limited, 3);
        assert_eq!(sched.tracked_panes, 5);
        assert_eq!(snapshot.backpressure_tier.as_deref(), Some("Green"));
    }

    #[test]
    fn health_snapshot_scheduler_serializes_roundtrip() {
        use crate::crash::HealthSnapshot;
        use crate::tailer::SchedulerSnapshot;

        let snapshot = HealthSnapshot {
            timestamp: 100,
            observed_panes: 1,
            capture_queue_depth: 0,
            write_queue_depth: 0,
            last_seq_by_pane: vec![],
            warnings: vec![],
            ingest_lag_avg_ms: 0.0,
            ingest_lag_max_ms: 0,
            db_writable: true,
            db_last_write_at: None,
            pane_priority_overrides: vec![],
            scheduler: Some(SchedulerSnapshot {
                budget_active: true,
                max_captures_per_sec: 10,
                max_bytes_per_sec: 500,
                captures_remaining: 8,
                bytes_remaining: 400,
                total_rate_limited: 0,
                total_byte_budget_exceeded: 0,
                total_throttle_events: 0,
                tracked_panes: 2,
            }),
            backpressure_tier: None,
            last_activity_by_pane: vec![],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
        };

        let json = serde_json::to_string(&snapshot).unwrap();
        let deser: HealthSnapshot = serde_json::from_str(&json).unwrap();
        let sched = deser.scheduler.unwrap();
        assert_eq!(sched.max_captures_per_sec, 10);
        assert_eq!(sched.tracked_panes, 2);
        assert!(deser.backpressure_tier.is_none());
    }

    // =========================================================================
    // Resize Watchdog Tests (wa-1u90p.7.1)
    // =========================================================================

    #[test]
    fn watchdog_severity_serde_roundtrip() {
        for severity in [
            ResizeWatchdogSeverity::Healthy,
            ResizeWatchdogSeverity::Warning,
            ResizeWatchdogSeverity::Critical,
            ResizeWatchdogSeverity::SafeModeActive,
        ] {
            let json = serde_json::to_string(&severity).unwrap();
            let parsed: ResizeWatchdogSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(severity, parsed);
        }
    }

    #[test]
    fn watchdog_severity_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&ResizeWatchdogSeverity::Healthy).unwrap(),
            "\"healthy\""
        );
        assert_eq!(
            serde_json::to_string(&ResizeWatchdogSeverity::SafeModeActive).unwrap(),
            "\"safe_mode_active\""
        );
    }

    #[test]
    fn watchdog_warning_line_healthy_returns_none() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Healthy,
            stalled_total: 0,
            stalled_critical: 0,
            warning_threshold_ms: 2000,
            critical_threshold_ms: 5000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: true,
            recommended_action: "none".into(),
            sample_stalled: vec![],
        };
        assert!(assessment.warning_line().is_none());
    }

    #[test]
    fn watchdog_warning_line_warning_contains_stalled_count() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Warning,
            stalled_total: 2,
            stalled_critical: 0,
            warning_threshold_ms: 2000,
            critical_threshold_ms: 5000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: true,
            recommended_action: "monitor_stalled_transactions".into(),
            sample_stalled: vec![],
        };
        let line = assessment.warning_line().unwrap();
        assert!(line.contains("warning"));
        assert!(line.contains("2 stalled"));
        assert!(line.contains("2000ms"));
    }

    #[test]
    fn watchdog_warning_line_critical_recommends_safe_mode() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Critical,
            stalled_total: 5,
            stalled_critical: 4,
            warning_threshold_ms: 2000,
            critical_threshold_ms: 5000,
            critical_stalled_limit: 3,
            safe_mode_recommended: true,
            safe_mode_active: false,
            legacy_fallback_enabled: true,
            recommended_action: "enable_safe_mode_fallback".into(),
            sample_stalled: vec![],
        };
        let line = assessment.warning_line().unwrap();
        assert!(line.contains("CRITICAL"));
        assert!(line.contains("4 stalled"));
        assert!(line.contains("5000ms"));
        assert!(line.contains("safe-mode fallback"));
        assert!(line.contains("legacy path enabled"));
    }

    #[test]
    fn watchdog_warning_line_critical_without_legacy() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Critical,
            stalled_total: 3,
            stalled_critical: 3,
            warning_threshold_ms: 2000,
            critical_threshold_ms: 5000,
            critical_stalled_limit: 3,
            safe_mode_recommended: true,
            safe_mode_active: false,
            legacy_fallback_enabled: false,
            recommended_action: "enable_safe_mode_fallback".into(),
            sample_stalled: vec![],
        };
        let line = assessment.warning_line().unwrap();
        assert!(line.contains("CRITICAL"));
        assert!(!line.contains("legacy path enabled"));
    }

    #[test]
    fn watchdog_warning_line_safe_mode_active() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::SafeModeActive,
            stalled_total: 1,
            stalled_critical: 0,
            warning_threshold_ms: 2000,
            critical_threshold_ms: 5000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: true,
            legacy_fallback_enabled: true,
            recommended_action: "safe_mode_active_monitor_and_recover".into(),
            sample_stalled: vec![],
        };
        let line = assessment.warning_line().unwrap();
        assert!(line.contains("safe-mode active"));
        assert!(line.contains("1 stalled"));
    }

    #[test]
    fn watchdog_assessment_serde_roundtrip() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Warning,
            stalled_total: 2,
            stalled_critical: 0,
            warning_threshold_ms: 2000,
            critical_threshold_ms: 5000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: true,
            recommended_action: "monitor_stalled_transactions".into(),
            sample_stalled: vec![],
        };
        let json = serde_json::to_string(&assessment).unwrap();
        let parsed: ResizeWatchdogAssessment = serde_json::from_str(&json).unwrap();
        assert_eq!(assessment, parsed);
    }

    #[test]
    fn derive_resize_degradation_ladder_uses_quality_tier_for_warning() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Warning,
            stalled_total: 2,
            stalled_critical: 0,
            warning_threshold_ms: 2_000,
            critical_threshold_ms: 8_000,
            critical_stalled_limit: 2,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: true,
            recommended_action: "monitor_stalled_transactions".into(),
            sample_stalled: vec![],
        };

        let ladder = derive_resize_degradation_ladder(&assessment);
        assert_eq!(
            ladder.tier,
            crate::degradation::ResizeDegradationTier::QualityReduced
        );
    }

    #[test]
    fn derive_resize_degradation_ladder_uses_emergency_tier_when_safe_mode_active() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::SafeModeActive,
            stalled_total: 3,
            stalled_critical: 2,
            warning_threshold_ms: 2_000,
            critical_threshold_ms: 8_000,
            critical_stalled_limit: 2,
            safe_mode_recommended: false,
            safe_mode_active: true,
            legacy_fallback_enabled: true,
            recommended_action: "safe_mode_active_monitor_and_recover".into(),
            sample_stalled: vec![],
        };

        let ladder = derive_resize_degradation_ladder(&assessment);
        assert_eq!(
            ladder.tier,
            crate::degradation::ResizeDegradationTier::EmergencyCompatibility
        );
    }

    // =========================================================================
    // RuntimeMetrics edge cases
    // =========================================================================

    #[test]
    fn runtime_metrics_default_zero_values() {
        let metrics = RuntimeMetrics::default();
        assert!((metrics.avg_ingest_lag_ms() - 0.0).abs() < f64::EPSILON);
        assert_eq!(metrics.max_ingest_lag_ms(), 0);
        assert!(metrics.last_db_write().is_none());
        assert!((metrics.avg_storage_lock_wait_ms() - 0.0).abs() < f64::EPSILON);
        assert!((metrics.max_storage_lock_wait_ms() - 0.0).abs() < f64::EPSILON);
        assert_eq!(metrics.storage_lock_contention_events(), 0);
        assert!((metrics.avg_storage_lock_hold_ms() - 0.0).abs() < f64::EPSILON);
        assert!((metrics.max_storage_lock_hold_ms() - 0.0).abs() < f64::EPSILON);
        assert_eq!(metrics.cursor_snapshot_bytes_last(), 0);
        assert_eq!(metrics.cursor_snapshot_bytes_max(), 0);
        assert!((metrics.avg_cursor_snapshot_bytes() - 0.0).abs() < f64::EPSILON);
        assert_eq!(metrics.p50_cursor_snapshot_bytes(), 0);
        assert_eq!(metrics.p95_cursor_snapshot_bytes(), 0);
    }

    #[test]
    fn runtime_metrics_single_ingest_lag_sample() {
        let metrics = RuntimeMetrics::default();
        metrics.record_ingest_lag(42);
        assert!((metrics.avg_ingest_lag_ms() - 42.0).abs() < f64::EPSILON);
        assert_eq!(metrics.max_ingest_lag_ms(), 42);
    }

    #[test]
    fn runtime_metrics_single_lock_wait_sample() {
        let metrics = RuntimeMetrics::default();
        metrics.record_storage_lock_wait(Duration::from_millis(5));
        assert!(metrics.avg_storage_lock_wait_ms() >= 5.0);
        assert!(metrics.max_storage_lock_wait_ms() >= 5.0);
        // Single sample: p50 and p95 should both equal the sample
        assert!(metrics.p50_storage_lock_wait_ms() >= 5.0);
        assert!(metrics.p95_storage_lock_wait_ms() >= 5.0);
    }

    #[test]
    fn runtime_metrics_single_cursor_snapshot_sample() {
        let metrics = RuntimeMetrics::default();
        metrics.record_cursor_snapshot_memory(2048);
        assert_eq!(metrics.cursor_snapshot_bytes_last(), 2048);
        assert_eq!(metrics.cursor_snapshot_bytes_max(), 2048);
        assert!((metrics.avg_cursor_snapshot_bytes() - 2048.0).abs() < f64::EPSILON);
        assert_eq!(metrics.p50_cursor_snapshot_bytes(), 2048);
        assert_eq!(metrics.p95_cursor_snapshot_bytes(), 2048);
    }

    #[test]
    fn runtime_metrics_many_ingest_lag_samples() {
        let metrics = RuntimeMetrics::default();
        for i in 1..=100 {
            metrics.record_ingest_lag(i);
        }
        // Average should be 50.5
        assert!((metrics.avg_ingest_lag_ms() - 50.5).abs() < f64::EPSILON);
        assert_eq!(metrics.max_ingest_lag_ms(), 100);
    }

    #[test]
    fn runtime_metrics_lock_contention_counts_above_threshold() {
        let metrics = RuntimeMetrics::default();
        // Sub-threshold: 500us is below the 1ms contention threshold
        metrics.record_storage_lock_wait(Duration::from_micros(500));
        assert_eq!(metrics.storage_lock_contention_events(), 0);

        // Above threshold: 2ms
        metrics.record_storage_lock_wait(Duration::from_millis(2));
        assert_eq!(metrics.storage_lock_contention_events(), 1);

        // Another above threshold
        metrics.record_storage_lock_wait(Duration::from_millis(5));
        assert_eq!(metrics.storage_lock_contention_events(), 2);
    }

    #[test]
    fn lock_memory_snapshot_zeroed_round_trips() {
        let snap = RuntimeLockMemoryTelemetrySnapshot {
            timestamp_ms: 0,
            avg_storage_lock_wait_ms: 0.0,
            p50_storage_lock_wait_ms: 0.0,
            p95_storage_lock_wait_ms: 0.0,
            max_storage_lock_wait_ms: 0.0,
            storage_lock_contention_events: 0,
            avg_storage_lock_hold_ms: 0.0,
            p50_storage_lock_hold_ms: 0.0,
            p95_storage_lock_hold_ms: 0.0,
            max_storage_lock_hold_ms: 0.0,
            cursor_snapshot_bytes_last: 0,
            p50_cursor_snapshot_bytes: 0,
            p95_cursor_snapshot_bytes: 0,
            cursor_snapshot_bytes_max: 0,
            avg_cursor_snapshot_bytes: 0.0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: RuntimeLockMemoryTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
        assert_eq!(back.timestamp_ms, 0);
        assert_eq!(back.storage_lock_contention_events, 0);
        assert_eq!(back.cursor_snapshot_bytes_last, 0);
    }

    #[test]
    fn health_snapshot_without_scheduler_deserializes() {
        // Old snapshots without scheduler/backpressure fields should deserialize fine
        let json = r#"{
            "timestamp": 1,
            "observed_panes": 0,
            "capture_queue_depth": 0,
            "write_queue_depth": 0,
            "last_seq_by_pane": [],
            "warnings": [],
            "ingest_lag_avg_ms": 0.0,
            "ingest_lag_max_ms": 0,
            "db_writable": true,
            "db_last_write_at": null,
            "pane_priority_overrides": []
        }"#;

        let snapshot: crate::crash::HealthSnapshot = serde_json::from_str(json).unwrap();
        assert!(snapshot.scheduler.is_none());
        assert!(snapshot.backpressure_tier.is_none());
    }
}
