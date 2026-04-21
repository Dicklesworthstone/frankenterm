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
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::sharded_counter::{ShardedCounter, ShardedGauge, ShardedMax};

use tracing::{debug, error, info, instrument, warn};

use crate::backpressure::{BackpressureConfig, BackpressureManager, BackpressureMetrics, QueueDepths};
use crate::config::{
    CaptureBudgetConfig, HotReloadableConfig, PaneFilterConfig, PanePriorityConfig, PatternsConfig,
    SnapshotConfig, SnapshotSchedulingMode,
};
use crate::crash::{
    HealthSnapshot, LeakRiskInventorySnapshot, LeakRiskWatchdogComponentSnapshot,
    LeakRiskWatchdogSnapshot, ShutdownSummary,
};
use crate::error::Result;
use crate::events::{Event, EventBus, UserVarPayload, event_identity_key};
use crate::fleet_memory_controller::{FleetMemoryConfig, PaneScrollbackInfo};
use crate::fleet_scrollback_coordinator::{
    CoordinatorConfig, FleetScrollbackCoordinator, SnapshotPaneScrollbackAccess,
};
use crate::gc::{CacheCompactionStats, CacheGcSettings, compact_u64_map, should_vacuum};
use crate::ingest::persist_captured_segment_with_cx;
use crate::ingest::{
    CapturedSegment, PaneCursor, PaneRegistry, PersistedCapture, bounded_segment_for_persistence,
};
use crate::memory_budget::BudgetLevel;
use crate::memory_pressure::{MemoryPressureConfig, MemoryPressureMonitor, MemoryPressureTier};
#[cfg(feature = "native-wezterm")]
use crate::native_events::{NativeEvent, NativeEventListener};
use crate::patterns::{Detection, DetectionContext, PatternEngine, Severity};
use crate::recording::RecordingManager;
use crate::resize_scheduler::{ResizeSchedulerDebugSnapshot, ResizeStalledTransaction};
use crate::runtime_compat::{RwLock, mpsc, task::JoinHandle, watch};
use crate::scrollback_tiers::ScrollbackTierSnapshot;
#[cfg(all(feature = "vendored", unix))]
use crate::sharding::decode_sharded_pane_id;
use crate::spsc_ring_buffer::{SpscConsumer, SpscProducer, channel as spsc_channel};
#[cfg(feature = "native-wezterm")]
use crate::storage::PaneRecord;
use crate::storage::{MaintenanceRecord, StorageHandle, StoredEvent};
#[cfg(all(feature = "vendored", unix))]
use crate::tailer::StreamingBridge;
use crate::tailer::{CaptureEvent, TailerConfig, TailerPollTaskSet, TailerSupervisor};
#[cfg(all(feature = "vendored", unix))]
use crate::vendored::subscribe_pane_output_with_inherited_cx;
#[cfg(all(feature = "vendored", unix))]
use crate::vendored::{DirectMuxClient, DirectMuxClientConfig, PaneDelta, SubscriptionConfig};
use crate::watchdog::HeartbeatRegistry;
use crate::wezterm::{
    PaneInfo, PaneTieredScrollbackSummary, WeztermHandle, WeztermHandleSource, WeztermInterface,
    wezterm_handle_with_timeout,
};

fn config_update_pending(rx: &watch::Receiver<HotReloadableConfig>) -> bool {
    {
        rx.has_changed()
    }

}

type RuntimeLoopCx = crate::cx::Cx;
fn runtime_loop_cx() -> RuntimeLoopCx {
    crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request)
}

async fn persist_captured_segment_for_runtime(
    runtime_cx: &RuntimeLoopCx,
    storage: &StorageHandle,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
) -> Result<PersistedCapture> {
    {
        persist_captured_segment_with_cx(runtime_cx, storage, captured, max_segment_bytes).await
    }

}

#[allow(clippy::needless_pass_by_ref_mut)] // update-taking watch APIs require &mut here
fn config_take_update(rx: &mut watch::Receiver<HotReloadableConfig>) -> HotReloadableConfig {
    {
        rx.borrow_and_clone()
    }

}

async fn runtime_sleep(runtime_cx: &RuntimeLoopCx, duration: Duration) {
    {
        let _ = crate::runtime_compat::sleep_with_cx(runtime_cx, duration).await;
    }

}

const SHUTDOWN_AWARE_SLEEP_POLL_INTERVAL: Duration = Duration::from_millis(100);

async fn runtime_sleep_until_shutdown(
    runtime_cx: &RuntimeLoopCx,
    shutdown_flag: &AtomicBool,
    duration: Duration,
) -> bool {
    let started_at = Instant::now();
    loop {
        if shutdown_flag.load(Ordering::SeqCst) {
            return true;
        }

        let elapsed = started_at.elapsed();
        if elapsed >= duration {
            return false;
        }

        let remaining = duration.saturating_sub(elapsed);
        runtime_sleep(
            runtime_cx,
            remaining.min(SHUTDOWN_AWARE_SLEEP_POLL_INTERVAL),
        )
        .await;
    }
}

async fn runtime_timeout<F>(
    runtime_cx: &RuntimeLoopCx,
    duration: Duration,
    future: F,
) -> std::result::Result<F::Output, String>
where
    F: Future,
{
    {
        crate::runtime_compat::timeout_with_cx(runtime_cx, duration, future).await
    }

}

fn spawn_runtime_task<F, Fut, T>(runtime_cx: &RuntimeLoopCx, task_fn: F) -> JoinHandle<T>
where
    F: FnOnce(RuntimeLoopCx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    {
        crate::runtime_compat::task::spawn_with_cx(runtime_cx, task_fn)
    }

}

/// RAII-guarded holder for the capture task's per-pane vendored streaming
/// subtasks.
///
/// The HashMap stores one `JoinHandle<()>` per observed pane while the
/// capture loop is routing work via the vendored (mux-socket) fast path.
/// On a clean shutdown the capture loop explicitly drains this map and
/// aborts each handle (see the `for (_pane_id, handle) in drain()` block at
/// the bottom of `spawn_capture_task`). However, if the capture future
/// itself is dropped mid-flight — because a parent scope cancels the
/// runtime JoinHandle, because an outer `select!` races to another branch,
/// or because a panic unwinds through the capture loop — that explicit
/// drain never runs and the child streaming subtasks become orphaned
/// (the asupersync runtime detaches, rather than aborts, on bare
/// `JoinHandle` drop). They keep holding the mux socket, capture_tx
/// sender half, and shutdown_flag Arc indefinitely.
///
/// Wrapping the map in this newtype makes cancellation-safe cleanup the
/// default: `Drop::drop` aborts every still-live child regardless of how
/// the capture future is dismantled.
#[cfg(all(feature = "vendored", unix))]
struct StreamingTasks {
    tasks: HashMap<u64, JoinHandle<()>>,
}

#[cfg(all(feature = "vendored", unix))]
impl StreamingTasks {
    fn new() -> Self {
        Self {
            tasks: HashMap::new(),
        }
    }
}

#[cfg(all(feature = "vendored", unix))]
impl std::ops::Deref for StreamingTasks {
    type Target = HashMap<u64, JoinHandle<()>>;

    fn deref(&self) -> &Self::Target {
        &self.tasks
    }
}

#[cfg(all(feature = "vendored", unix))]
impl std::ops::DerefMut for StreamingTasks {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tasks
    }
}

#[cfg(all(feature = "vendored", unix))]
impl Drop for StreamingTasks {
    fn drop(&mut self) {
        for (_pane_id, handle) in self.tasks.drain() {
            handle.abort();
        }
    }
}

async fn recv_event<T>(runtime_cx: &RuntimeLoopCx, rx: &mut mpsc::Receiver<T>) -> Option<T> {
    {
        rx.recv(runtime_cx).await.ok()
    }

}

#[cfg(all(feature = "vendored", unix))]
async fn send_runtime_channel<T>(
    runtime_cx: &RuntimeLoopCx,
    tx: &mpsc::Sender<T>,
    value: T,
) -> bool {
    {
        tx.send(runtime_cx, value).await.is_ok()
    }

}

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

// Output coalesce defaults — canonical values in TuningConfig::RuntimeTuning.
// These consts exist for use in contexts without runtime access.
// To override: set [tuning.runtime] in ft.toml.
#[cfg(feature = "native-wezterm")]
const NATIVE_OUTPUT_COALESCE_WINDOW_MS: u64 =
    crate::tuning_config::RuntimeTuning::DEFAULT_OUTPUT_COALESCE_WINDOW_MS;
#[cfg(feature = "native-wezterm")]
const NATIVE_OUTPUT_COALESCE_MAX_DELAY_MS: u64 =
    crate::tuning_config::RuntimeTuning::DEFAULT_OUTPUT_COALESCE_MAX_DELAY_MS;
#[cfg(feature = "native-wezterm")]
const NATIVE_OUTPUT_COALESCE_MAX_BYTES: usize =
    crate::tuning_config::RuntimeTuning::DEFAULT_OUTPUT_COALESCE_MAX_BYTES;

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
    /// Periodic cache compaction and vacuum policy
    pub gc: CacheGcSettings,
    /// Vendored mux socket paths used for pane-delta streaming subscriptions.
    ///
    /// When sharding is enabled, the index in this vector matches the encoded
    /// shard id embedded in pane ids. When empty, the runtime falls back to
    /// polling-only capture.
    pub vendored_mux_socket_paths: Vec<PathBuf>,
    /// Compression mode for vendored direct-mux subscriptions.
    pub vendored_mux_compression: crate::config::VendoredCompressionMode,
    /// Optional Unix socket path for native WezTerm events
    pub native_event_socket: Option<PathBuf>,
    /// Trauma guard configuration for detecting recurring failure loops
    pub trauma_guard: crate::config::TraumaGuardConfig,
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
            gc: CacheGcSettings::default(),
            vendored_mux_socket_paths: Vec::new(),
            vendored_mux_compression: crate::config::VendoredCompressionMode::Auto,
            native_event_socket: None,
            trauma_guard: crate::config::TraumaGuardConfig::default(),
        }
    }
}

#[cfg(all(feature = "vendored", unix))]
#[derive(Debug)]
struct StreamingTaskExit {
    pane_id: u64,
    reason: String,
}

#[cfg(all(feature = "vendored", unix))]
fn streaming_subscription_config(
    poll_interval: Duration,
    min_poll_interval: Duration,
    channel_capacity: usize,
) -> SubscriptionConfig {
    SubscriptionConfig {
        poll_interval,
        min_poll_interval,
        channel_capacity: channel_capacity.max(1),
    }
}

#[cfg(all(feature = "vendored", unix))]
fn vendored_streaming_socket_for_pane(
    socket_paths: &[PathBuf],
    pane_id: u64,
) -> Option<(u64, PathBuf)> {
    if socket_paths.is_empty() {
        return None;
    }

    if socket_paths.len() == 1 {
        return Some((pane_id, socket_paths[0].clone()));
    }

    let (shard_id, local_pane_id) = decode_sharded_pane_id(pane_id);
    socket_paths
        .get(shard_id.0)
        .cloned()
        .map(|socket_path| (local_pane_id, socket_path))
}

#[cfg(all(feature = "vendored", unix))]
fn should_record_streaming_fallback(reason: &str) -> bool {
    !matches!(reason, "cancelled" | "capture ingress closed" | "shutdown")
}

/// Runtime metrics for health snapshots and shutdown summaries.
///
/// Uses sharded atomics (cache-line-padded per-core counters) to eliminate
/// false sharing and SeqCst contention. Writes distribute across shards;
/// reads aggregate infrequently.
static GLOBAL_RUNTIME_LOCK_MEMORY_TELEMETRY: OnceLock<
    StdRwLock<Option<RuntimeLockMemoryTelemetrySnapshot>>,
> = OnceLock::new();
// Resize watchdog defaults — canonical values in TuningConfig::RuntimeTuning.
// To override: set [tuning.runtime] in ft.toml.
const TELEMETRY_PERCENTILE_WINDOW_CAPACITY: usize =
    crate::tuning_config::RuntimeTuning::DEFAULT_TELEMETRY_PERCENTILE_WINDOW;
const RESIZE_WATCHDOG_WARNING_THRESHOLD_MS: u64 =
    crate::tuning_config::RuntimeTuning::DEFAULT_RESIZE_WATCHDOG_WARNING_MS;
const RESIZE_WATCHDOG_CRITICAL_THRESHOLD_MS: u64 =
    crate::tuning_config::RuntimeTuning::DEFAULT_RESIZE_WATCHDOG_CRITICAL_MS;
const RESIZE_WATCHDOG_CRITICAL_STALLED_LIMIT: usize =
    crate::tuning_config::RuntimeTuning::DEFAULT_RESIZE_WATCHDOG_STALLED_LIMIT;
const RESIZE_WATCHDOG_SAMPLE_LIMIT: usize =
    crate::tuning_config::RuntimeTuning::DEFAULT_RESIZE_WATCHDOG_SAMPLE_LIMIT;

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
    /// [ft-0e179] Shared backpressure metrics exposing per-pane drop
    /// attribution to the native event pipeline. The `BackpressureMetrics`
    /// struct lives on `RuntimeMetrics` (not `BackpressureManager`) because
    /// drop accounting happens in the native-event task, which does not
    /// hold a `BackpressureManager` reference — the manager lives in the
    /// parallel maintenance loop and operates on queue depth sampling.
    backpressure: Arc<BackpressureMetrics>,
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
            backpressure: Arc::new(BackpressureMetrics::default()),
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

    /// [ft-0e179] Borrow the shared backpressure metrics — used by the
    /// native-event pipeline to record per-pane segment drops. Returns a
    /// reference (not a clone) so the hot drop path is free of atomic
    /// refcount traffic.
    #[must_use]
    pub fn backpressure_metrics(&self) -> &Arc<BackpressureMetrics> {
        &self.backpressure
    }

    /// [ft-0e179] Convenience: record a backpressure-driven segment drop
    /// from `pane_id`. Delegates to `BackpressureMetrics::record_segment_dropped`
    /// so drop sites don't have to deref through `backpressure_metrics()`.
    pub fn record_segment_dropped(&self, pane_id: u64) {
        self.backpressure.record_segment_dropped(pane_id);
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
    storage: StorageHandle,
    /// Pattern detection engine
    pattern_engine: Arc<RwLock<PatternEngine>>,
    /// Pane registry for discovery and tracking
    registry: Arc<RwLock<PaneRegistry>>,
    /// Per-pane cursors for delta extraction
    cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
    /// Per-pane detection contexts for deduplication
    detection_contexts: Arc<RwLock<HashMap<u64, DetectionContext>>>,
    /// Best-effort per-pane output activity tracker for health snapshots.
    pane_activity_tracker: Arc<RwLock<HashMap<u64, PaneActivityState>>>,
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
    /// Optional replay capture adapter for `.ftreplay` recorder events.
    replay_capture: Option<crate::replay_capture::SharedCaptureAdapter>,
    /// Optional snapshot engine configuration for session persistence
    snapshot_config: Option<SnapshotConfig>,
    /// Heartbeat registry for watchdog monitoring
    heartbeats: Arc<HeartbeatRegistry>,
    /// Shared scheduler snapshot for health reporting (written by capture task).
    scheduler_snapshot: Arc<RwLock<crate::tailer::SchedulerSnapshot>>,
    /// Operator-tunable constants (loaded from ft.toml `[tuning]` section).
    tuning: Arc<crate::tuning_config::TuningConfig>,
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
            gc: config.gc,
            patterns: config.patterns.clone(),
            workflows_enabled: vec![],
            auto_run_allowlist: vec![],
            trauma_guard: config.trauma_guard.clone(),
        };
        let (config_tx, config_rx) = watch::channel(hot_config);

        Self {
            config,
            wezterm_handle: wezterm_handle_with_timeout(5),
            storage,
            pattern_engine,
            registry: Arc::new(RwLock::new(registry)),
            cursors: Arc::new(RwLock::new(HashMap::new())),
            detection_contexts: Arc::new(RwLock::new(HashMap::new())),
            pane_activity_tracker: Arc::new(RwLock::new(HashMap::new())),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            metrics,
            config_tx: Arc::new(config_tx),
            config_rx,
            event_bus: None,
            recording: None,
            replay_capture: None,
            snapshot_config: None,
            heartbeats: Arc::new(HeartbeatRegistry::new()),
            scheduler_snapshot: Arc::new(RwLock::new(crate::tailer::SchedulerSnapshot::default())),
            tuning: Arc::new(crate::tuning_config::TuningConfig::default()),
        }
    }

    /// Override the default tuning configuration.
    ///
    /// When building from an `ft.toml` that has a `[tuning]` section, pass
    /// the parsed `TuningConfig` here. If not called, all tuning values
    /// use their compiled defaults (identical to pre-migration behavior).
    #[must_use]
    pub fn with_tuning(mut self, tuning: crate::tuning_config::TuningConfig) -> Self {
        self.tuning = Arc::new(tuning);
        self
    }

    /// Access the operator-tunable constants.
    ///
    /// Subsystems that previously used hard-coded `const` values should read
    /// from this instead. The `Arc` makes it cheap to clone into spawned tasks.
    pub fn tuning(&self) -> &crate::tuning_config::TuningConfig {
        &self.tuning
    }

    /// Get a clone-friendly handle to the tuning config (for spawned tasks).
    pub fn tuning_arc(&self) -> Arc<crate::tuning_config::TuningConfig> {
        Arc::clone(&self.tuning)
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

    /// Set a replay capture adapter for extracting deterministic recorder events.
    #[must_use]
    pub fn with_replay_capture_adapter(
        mut self,
        adapter: crate::replay_capture::SharedCaptureAdapter,
    ) -> Self {
        self.replay_capture = Some(adapter);
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

        // Always spawn the capture task. It owns the polling supervisor and,
        // when vendored direct-mux sockets are configured, per-pane streaming
        // subscriptions. Native push events can still feed the same ingress
        // channel in parallel, with polling remaining the safety net.
        let capture_handle = self.spawn_capture_task(capture_ingress_tx.clone());

        // Spawn native event listener if socket path is configured and the
        // native-wezterm feature is compiled in. Auto-detection: always try
        // to bind the socket regardless of config.native.enabled.
        #[cfg(feature = "native-wezterm")]
        let native_handle = native_socket.map(|socket| {
            info!(
                path = %socket.display(),
                "Switching to native push events (auto-detected socket path)"
            );
            self.spawn_native_event_task(socket, capture_ingress_tx.clone())
        });
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
                    let db_path = Arc::new(self.storage.db_path().to_string());
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

                    let loop_cx = runtime_loop_cx();
                    let handle = spawn_runtime_task(&loop_cx, move |_task_cx| async move {
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
            storage: self.storage.clone(),
            metrics: Arc::clone(&self.metrics),
            registry: Arc::clone(&self.registry),
            cursors: Arc::clone(&self.cursors),
            pane_activity_tracker: Arc::clone(&self.pane_activity_tracker),
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

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
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
                    match runtime_timeout(
                        &loop_cx,
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
                    runtime_sleep(
                        &loop_cx,
                        Duration::from_secs(SNAPSHOT_TRIGGER_BRIDGE_TICK_SECS),
                    )
                    .await;
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
        let storage = self.storage.clone();
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let wezterm_handle = self.wezterm_handle.clone();
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let pane_activity_tracker = Arc::clone(&self.pane_activity_tracker);
        let metrics = Arc::clone(&self.metrics);
        let scheduler_snapshot = Arc::clone(&self.scheduler_snapshot);

        let initial_retention_days = self.config.retention_days;
        let initial_checkpoint_secs = self.config.checkpoint_interval_secs;
        let initial_cache_gc_settings = self.config.gc;

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let mut retention_days = initial_retention_days;
            let mut checkpoint_secs = initial_checkpoint_secs;
            let mut cache_gc_settings = initial_cache_gc_settings;
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
            let backpressure_manager = BackpressureManager::new(BackpressureConfig::default());
            let memory_pressure_monitor =
                MemoryPressureMonitor::new(MemoryPressureConfig::default());

            // Fleet scrollback coordinator (ft-dwjtm): evaluates fleet memory
            // pressure from queue depths and triggers warm-page eviction when
            // TieredScrollback instances are available.
            let mut fleet_coordinator = FleetScrollbackCoordinator::new(
                CoordinatorConfig::default(),
                FleetMemoryConfig::default(),
            );

            loop {
                if !first_tick
                    && runtime_sleep_until_shutdown(
                        &loop_cx,
                        shutdown_flag.as_ref(),
                        maintenance_interval,
                    )
                    .await
                {
                    break;
                }
                first_tick = false;
                heartbeats.record_maintenance();

                if shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }

                // Check for config updates
                if config_update_pending(&config_rx) {
                    let new_config = config_take_update(&mut config_rx);
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
                    if new_config.gc != cache_gc_settings {
                        info!(
                            old = ?cache_gc_settings,
                            new = ?new_config.gc,
                            "Cache GC settings updated"
                        );
                        cache_gc_settings = new_config.gc;
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
                        if let Err(e) = storage.retention_cleanup(cutoff_ms).await {
                            error!(error = %e, "Retention cleanup failed");
                        } else {
                            debug!("Retention cleanup completed");
                        }
                        // Also purge old audit actions
                        // ft-xbnl0.2.3 tick 251: cx-first retention purge.
                        if let Err(e) = storage
                            .purge_audit_actions_before_with_cx(&loop_cx, cutoff_ms)
                            .await
                        {
                            error!(error = %e, "Audit purge failed");
                        }
                    }
                    last_retention_check = now;
                }

                // Run WAL checkpoint + PRAGMA optimize (lightweight)
                if checkpoint_secs > 0
                    && now.duration_since(last_checkpoint)
                        >= Duration::from_secs(u64::from(checkpoint_secs))
                {
                    match storage.checkpoint().await {
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

                    last_checkpoint = now;
                }

                if cache_gc_settings.enabled
                    && cache_gc_settings.interval_seconds > 0
                    && now.duration_since(last_cache_gc)
                        >= Duration::from_secs(cache_gc_settings.interval_seconds)
                {
                    let active_panes: HashSet<u64> = {
                        let reg = registry.read().await;
                        reg.observed_pane_ids().into_iter().collect()
                    };

                    let RuntimePaneStateCompaction {
                        cursors: cursor_gc,
                        detection_contexts: context_gc,
                        pane_activity_tracker: activity_gc,
                    } = {
                        let mut cursors_guard = cursors.write().await;
                        let mut contexts_guard = detection_contexts.write().await;
                        let mut tracker_guard = pane_activity_tracker.write().await;
                        compact_runtime_pane_state(
                            &mut cursors_guard,
                            &mut contexts_guard,
                            &mut tracker_guard,
                            &active_panes,
                        )
                    };

                    let mut page_count = 0_i64;
                    let mut free_pages = 0_i64;
                    let mut free_ratio = 0.0_f64;
                    let mut vacuumed = false;
                    let mut vacuum_error = None::<String>;

                    match storage.database_page_stats().await {
                        Ok(stats) => {
                            page_count = stats.page_count;
                            free_pages = stats.free_pages;
                            free_ratio = stats.free_ratio();

                            if should_vacuum(
                                stats.page_count,
                                stats.free_pages,
                                cache_gc_settings.vacuum_threshold,
                            ) {
                                match storage.vacuum().await {
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

                    let cache_reports = [
                        ("cursors", cursor_gc),
                        ("detection_contexts", context_gc),
                        ("pane_activity_tracker", activity_gc),
                    ];
                    let total_estimated_bytes_freed = cache_reports
                        .iter()
                        .map(|(_, stats)| stats.estimated_bytes_freed())
                        .sum::<usize>();
                    let caches = cache_reports
                        .iter()
                        .map(|(name, stats)| {
                            serde_json::json!({
                                "name": name,
                                "before_len": stats.before_len,
                                "before_capacity": stats.before_capacity,
                                "after_len": stats.after_len,
                                "after_capacity": stats.after_capacity,
                                "removed_entries": stats.removed_entries,
                                "freed_slots": stats.freed_slots(),
                                "estimated_bytes_freed": stats.estimated_bytes_freed(),
                            })
                        })
                        .collect::<Vec<_>>();
                    let metadata = serde_json::json!({
                        "active_panes": active_panes.len(),
                        "cursor_removed": cursor_gc.removed_entries,
                        "cursor_freed_slots": cursor_gc.freed_slots(),
                        "cursor_estimated_bytes_freed": cursor_gc.estimated_bytes_freed(),
                        "context_removed": context_gc.removed_entries,
                        "context_freed_slots": context_gc.freed_slots(),
                        "context_estimated_bytes_freed": context_gc.estimated_bytes_freed(),
                        "pane_activity_removed": activity_gc.removed_entries,
                        "pane_activity_freed_slots": activity_gc.freed_slots(),
                        "pane_activity_estimated_bytes_freed": activity_gc.estimated_bytes_freed(),
                        "total_estimated_bytes_freed": total_estimated_bytes_freed,
                        "page_count": page_count,
                        "free_pages": free_pages,
                        "free_ratio": free_ratio,
                        "vacuumed": vacuumed,
                        "vacuum_threshold": cache_gc_settings.vacuum_threshold,
                        "vacuum_error": vacuum_error,
                        "log_report": cache_gc_settings.log_report,
                        "caches": caches,
                    });
                    let _ = storage
                        .record_maintenance(MaintenanceRecord {
                            id: 0,
                            event_type: "cache_gc".to_string(),
                            message: Some("Periodic cache GC cycle".to_string()),
                            metadata: Some(metadata.to_string()),
                            timestamp: epoch_ms(),
                        })
                        .await;

                    if cache_gc_settings.log_report {
                        info!(
                            active_panes = active_panes.len(),
                            cursor_removed = cursor_gc.removed_entries,
                            cursor_freed_slots = cursor_gc.freed_slots(),
                            cursor_estimated_bytes_freed = cursor_gc.estimated_bytes_freed(),
                            context_removed = context_gc.removed_entries,
                            context_freed_slots = context_gc.freed_slots(),
                            context_estimated_bytes_freed = context_gc.estimated_bytes_freed(),
                            pane_activity_removed = activity_gc.removed_entries,
                            pane_activity_freed_slots = activity_gc.freed_slots(),
                            pane_activity_estimated_bytes_freed =
                                activity_gc.estimated_bytes_freed(),
                            total_estimated_bytes_freed,
                            free_ratio,
                            vacuumed,
                            "Cache GC cycle completed"
                        );
                    } else {
                        debug!(
                            active_panes = active_panes.len(),
                            total_estimated_bytes_freed,
                            free_ratio,
                            vacuumed,
                            "Cache GC cycle completed"
                        );
                    }

                    last_cache_gc = now;
                }

                if now.duration_since(last_health_snapshot) >= health_interval {
                    let (health_panes, leak_risk_inventory) = {
                        let reg = registry.read().await;
                        let cursors = cursors.read().await;
                        let mut tracker = pane_activity_tracker.write().await;
                        let health_panes = build_health_pane_snapshot(
                            &reg,
                            &cursors,
                            &mut tracker,
                            epoch_ms_u64(),
                        );
                        let leak_risk_inventory =
                            build_leak_risk_inventory(&reg, &metrics, &heartbeats);
                        (health_panes, leak_risk_inventory)
                    };
                    let observed_panes = health_panes.observed_panes;
                    let last_activity_by_pane = health_panes.last_activity_by_pane;
                    let last_seq_by_pane = health_panes.last_seq_by_pane;
                    let cursor_snapshot_bytes = health_panes.cursor_snapshot_bytes;
                    metrics.record_cursor_snapshot_memory(cursor_snapshot_bytes);

                    let capture_cap = mpsc_max_capacity(&capture_tx);
                    let capture_depth = capture_cap.saturating_sub(capture_tx.capacity());

                    let (write_depth, write_cap, db_writable) = {
                        let wd = storage.write_queue_depth();
                        let wc = storage.write_queue_capacity();
                        let writable = storage.is_writable().await;
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
                    let backpressure_tier = classify_backpressure_tier(
                        capture_depth,
                        capture_cap,
                        write_depth,
                        write_cap,
                    );

                    // ── Fleet scrollback coordinator tick (ft-dwjtm) ────────
                    //
                    // Drive the coordinator from the runtime's actual pressure
                    // surfaces instead of cursor-derived placeholders:
                    // - queue depths via BackpressureManager
                    // - host memory sampling via MemoryPressureMonitor
                    // - per-pane logical accounting via PaneRegistry arenas
                    let fleet_signals = build_fleet_pressure_signals(
                        &backpressure_manager,
                        &QueueDepths {
                            capture_depth,
                            capture_capacity: capture_cap,
                            write_depth,
                            write_capacity: write_cap,
                        },
                        memory_pressure_monitor.sample().tier,
                        BudgetLevel::Normal,
                        observed_panes,
                    );
                    let observed_pane_ids = {
                        let reg = registry.read().await;
                        reg.observed_pane_ids()
                    };
                    let observed_pane_count = observed_pane_ids.len();
                    let tiered_scrollback_fetch = collect_pane_tiered_scrollback_summaries(
                        &loop_cx,
                        &wezterm_handle,
                        &observed_pane_ids,
                    )
                    .await;
                    let (fleet_pane_infos, fleet_pane_snapshots) = {
                        let reg = registry.read().await;
                        let cur = cursors.read().await;
                        (
                            fleet_pane_infos_from_registry(
                                &reg,
                                &cur,
                                &tiered_scrollback_fetch.summaries,
                            ),
                            fleet_pane_scrollback_snapshots_from_registry(
                                &reg,
                                &cur,
                                &tiered_scrollback_fetch.summaries,
                            ),
                        )
                    };
                    let pane_scrollback_snapshot_count = fleet_pane_snapshots.len();
                    let mut pane_snapshots =
                        SnapshotPaneScrollbackAccess::new(fleet_pane_snapshots);
                    let fleet_eval = fleet_coordinator.evaluate(
                        &fleet_signals,
                        &fleet_pane_infos,
                        &mut pane_snapshots,
                    );

                    if fleet_eval.pages_evicted > 0 || fleet_eval.bytes_reclaimed > 0 {
                        info!(
                            tier = ?fleet_eval.compound_tier,
                            pages_evicted = fleet_eval.pages_evicted,
                            bytes_reclaimed = fleet_eval.bytes_reclaimed,
                            "fleet scrollback coordinator eviction"
                        );
                    } else if !matches!(
                        fleet_eval.compound_tier,
                        crate::fleet_memory_controller::FleetPressureTier::Normal
                    ) {
                        debug!(
                            tier = ?fleet_eval.compound_tier,
                            actions = fleet_eval.actions.len(),
                            "fleet scrollback coordinator: elevated pressure"
                        );
                    }

                    // Record fleet coordinator evaluation as a maintenance
                    // event so it shows up in diagnostics and audit trail.
                    {
                        let telem = fleet_coordinator.telemetry_snapshot();
                        let telemetry_blind =
                            tiered_scrollback_fetch.telemetry_blind(observed_pane_count);
                        let telemetry_partial =
                            tiered_scrollback_fetch.telemetry_partial(observed_pane_count);
                        let blind_reason = telemetry_blind.then_some(
                            "vendored tiered scrollback telemetry unavailable; runtime is operating without mux-side pane telemetry",
                        );

                        if telemetry_blind {
                            warn!(
                                observed_panes = observed_pane_count,
                                pane_status_errors = tiered_scrollback_fetch.errors,
                                error_samples = ?tiered_scrollback_fetch.error_samples,
                                "Tiered scrollback telemetry is blind; vendored mux compatibility or health has degraded"
                            );
                        } else if telemetry_partial {
                            warn!(
                                observed_panes = observed_pane_count,
                                pane_status_snapshots = tiered_scrollback_fetch.summaries.len(),
                                pane_status_errors = tiered_scrollback_fetch.errors,
                                error_samples = ?tiered_scrollback_fetch.error_samples,
                                "Tiered scrollback telemetry is partially unavailable"
                            );
                        }

                        let metadata = serde_json::json!({
                            "compound_tier": format!("{:?}", fleet_eval.compound_tier),
                            "pages_evicted": fleet_eval.pages_evicted,
                            "bytes_reclaimed": fleet_eval.bytes_reclaimed,
                            "targets_applied": fleet_eval.targets_applied,
                            "actions": fleet_eval.actions.len(),
                            "observed_panes": observed_panes,
                            "pane_status_snapshots": tiered_scrollback_fetch.summaries.len(),
                            "pane_scrollback_snapshots": pane_scrollback_snapshot_count,
                            "pane_status_errors": tiered_scrollback_fetch.errors,
                            "pane_status_error_samples": tiered_scrollback_fetch.error_samples,
                            "tiered_scrollback_telemetry_blind": telemetry_blind,
                            "tiered_scrollback_telemetry_partial": telemetry_partial,
                            "tiered_scrollback_blind_reason": blind_reason,
                            "cumulative_ticks": telem.ticks,
                            "cumulative_elevated_ticks": telem.elevated_ticks,
                            "cumulative_pages_evicted": telem.pages_evicted,
                            "cumulative_bytes_reclaimed": telem.bytes_reclaimed,
                        });
                        let _ = storage
                            .record_maintenance(MaintenanceRecord {
                                id: 0,
                                event_type: "fleet_scrollback_coordinator".to_string(),
                                message: Some(if telemetry_blind {
                                    format!(
                                        "Fleet coordinator tick: tier={:?}, evicted={} pages; tiered scrollback telemetry blind",
                                        fleet_eval.compound_tier, fleet_eval.pages_evicted,
                                    )
                                } else {
                                    format!(
                                        "Fleet coordinator tick: tier={:?}, evicted={} pages",
                                        fleet_eval.compound_tier, fleet_eval.pages_evicted,
                                    )
                                }),
                                metadata: Some(metadata.to_string()),
                                timestamp: epoch_ms(),
                            })
                            .await;
                    }
                    // ── end fleet coordinator tick ──────────────────────────

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
                        backpressure_tier,
                        last_activity_by_pane,
                        restart_count: 0,
                        last_crash_at: None,
                        consecutive_crashes: 0,
                        current_backoff_ms: 0,
                        in_crash_loop: false,
                        fleet_pressure_tier: Some(format!("{:?}", fleet_eval.compound_tier)),
                        leak_risk_inventory,
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
        let pane_activity_tracker = Arc::clone(&self.pane_activity_tracker);
        let storage = self.storage.clone();
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let initial_interval = self.config.discovery_interval;
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let wezterm = Arc::clone(&self.wezterm_handle);
        let replay_capture = self.replay_capture.clone();

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
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
                    runtime_sleep(&loop_cx, Duration::from_millis(100)).await;
                }

                // Check shutdown flag
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Discovery task: shutdown signal received");
                    break;
                }

                // Check for config updates (non-blocking)
                if config_update_pending(&config_rx) {
                    let new_config = config_take_update(&mut config_rx);
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
                        for (pane_id, mut entry) in new_entries {
                            if let Some(ref adapter) = replay_capture {
                                adapter.capture_lifecycle(
                                    pane_id,
                                    crate::recording::RecorderLifecyclePhase::PaneOpened,
                                    None,
                                    serde_json::json!({
                                        "domain": entry.info.inferred_domain().to_string(),
                                        "title": entry.info.title.clone(),
                                        "cwd": entry.info.cwd.clone(),
                                    }),
                                );
                            }

                            // Check if pane exists in storage to recover stable UUID
                            // ft-xbnl0.2.3 tick 251: cx-first storage in pane-setup block.
                            let stable_uuid = {
                                let result = storage
                                    .get_pane_with_cx(&loop_cx, pane_id)
                                    .await
                                    .unwrap_or_else(|e| {
                                        warn!(pane_id, error = %e, "Failed to check storage for existing pane");
                                        None
                                    });

                                result.and_then(|r| r.pane_uuid)
                            };

                            if let Some(uuid) = stable_uuid {
                                if uuid != entry.pane_uuid {
                                    debug!(
                                        pane_id,
                                        old_uuid = %entry.pane_uuid,
                                        new_uuid = %uuid,
                                        "Adopting stable UUID from storage"
                                    );
                                    let adopted = {
                                        let mut reg = registry.write().await;
                                        reg.adopt_uuid(pane_id, uuid.clone())
                                    };

                                    if adopted {
                                        entry.pane_uuid = uuid;
                                    } else {
                                        warn!(
                                            pane_id,
                                            rejected_uuid = %uuid,
                                            "Rejected stable UUID adoption due to registry collision"
                                        );
                                    }
                                }
                            }

                            // Upsert pane in storage
                            // ft-xbnl0.2.3 tick 251: cx-first upsert (reuse pane_setup_cx).
                            let record = entry.to_pane_record();
                            if let Err(e) = storage.upsert_pane_with_cx(&loop_cx, record).await {
                                error!(pane_id = pane_id, error = %e, "Failed to upsert pane");
                            }

                            // Create cursor if observed
                            if entry.should_observe() {
                                // Initialize cursor from storage to resume capture
                                // ft-xbnl0.2.3 tick 251: cx-first max_seq (reuse pane_setup_cx).
                                let max_seq = storage
                                    .get_max_seq_with_cx(&loop_cx, pane_id)
                                    .await
                                    .unwrap_or(None);

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
                                if let Some(ref adapter) = replay_capture {
                                    adapter.capture_lifecycle(
                                        pane_id,
                                        crate::recording::RecorderLifecyclePhase::CaptureStarted,
                                        None,
                                        serde_json::json!({
                                            "reason": "observation_started",
                                        }),
                                    );
                                }
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

                            if let Some(ref adapter) = replay_capture {
                                adapter.capture_lifecycle(
                                    *pane_id,
                                    crate::recording::RecorderLifecyclePhase::CaptureStopped,
                                    Some("pane_closed".to_string()),
                                    serde_json::json!({}),
                                );
                                adapter.capture_lifecycle(
                                    *pane_id,
                                    crate::recording::RecorderLifecyclePhase::PaneClosed,
                                    Some("pane_closed".to_string()),
                                    serde_json::json!({}),
                                );
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

                        let active_panes: HashSet<u64> = {
                            let reg = registry.read().await;
                            reg.observed_pane_ids().into_iter().collect()
                        };
                        let RuntimePaneStateCompaction {
                            cursors: cursor_gc,
                            detection_contexts: context_gc,
                            pane_activity_tracker: activity_gc,
                        } = {
                            let mut cursors_guard = cursors.write().await;
                            let mut contexts_guard = detection_contexts.write().await;
                            let mut tracker_guard = pane_activity_tracker.write().await;
                            compact_runtime_pane_state(
                                &mut cursors_guard,
                                &mut contexts_guard,
                                &mut tracker_guard,
                                &active_panes,
                            )
                        };
                        if cursor_gc.removed_entries > 0
                            || context_gc.removed_entries > 0
                            || activity_gc.removed_entries > 0
                        {
                            debug!(
                                cursor_removed = cursor_gc.removed_entries,
                                context_removed = context_gc.removed_entries,
                                pane_activity_removed = activity_gc.removed_entries,
                                "Discovery tick compacted unobserved runtime pane state"
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

    /// Spawn the content capture task using adaptive polling plus vendored
    /// pane-delta streaming when direct mux sockets are configured.
    ///
    /// This task manages per-pane capture that:
    /// - Subscribes to vendored pane deltas when direct mux is available
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
        let replay_capture = self.replay_capture.clone();
        #[cfg(all(feature = "vendored", unix))]
        let vendored_streaming_enabled = !self.config.vendored_mux_socket_paths.is_empty();
        #[cfg(all(feature = "vendored", unix))]
        let vendored_mux_socket_paths = self.config.vendored_mux_socket_paths.clone();
        #[cfg(all(feature = "vendored", unix))]
        let vendored_mux_compression = self.config.vendored_mux_compression;
        #[cfg(all(feature = "vendored", unix))]
        let vendored_channel_capacity = self.config.channel_buffer.max(1);
        #[cfg(all(feature = "vendored", unix))]
        let initial_vendored_subscription_config = vendored_streaming_enabled.then(|| {
            streaming_subscription_config(
                self.config.capture_interval,
                self.config.min_capture_interval,
                vendored_channel_capacity,
            )
        });

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
            capture_timeout: Duration::from_secs(2),
        };

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let source = Arc::new(WeztermHandleSource::new(wezterm_handle));
            let capture_tx_for_supervisor = capture_tx.clone();
            // Create tailer supervisor with budget enforcement
            let initial_budget = config_rx.borrow().capture_budgets.clone();
            let mut supervisor = TailerSupervisor::with_budget(
                initial_config,
                capture_tx_for_supervisor,
                cursors,
                Arc::clone(&registry), // Pass registry for authoritative state
                Arc::clone(&shutdown_flag),
                source,
                initial_budget,
            );
            if let Some(adapter) = replay_capture {
                let egress_tap: crate::recording::SharedEgressTap = adapter.clone();
                supervisor.set_egress_tap(egress_tap);
                supervisor.set_global_sequence(adapter.global_sequence_handle());
            }

            // Cache hot-reloadable pane priority config for scheduling.
            let mut pane_priorities = config_rx.borrow().pane_priorities.clone();
            #[cfg(all(feature = "vendored", unix))]
            let mut vendored_subscription_config = initial_vendored_subscription_config;
            #[cfg(all(feature = "vendored", unix))]
            let (stream_exit_tx, mut stream_exit_rx) =
                mpsc::channel::<StreamingTaskExit>(vendored_channel_capacity);
            // Per-pane vendored streaming subtasks. Dropping the capture future
            // (runtime shutdown, panic, outer select loss) aborts every still-live
            // child via StreamingTasks::drop — the explicit drain below is kept
            // as the fast path on clean shutdown but is no longer load-bearing.
            #[cfg(all(feature = "vendored", unix))]
            let mut streaming_tasks: StreamingTasks = StreamingTasks::new();
            #[cfg(all(feature = "vendored", unix))]
            let mut observed_panes_cache: HashMap<u64, PaneInfo> = HashMap::new();

            // Sync tailers periodically with discovery interval.
            // Keep the first sync immediate to preserve prior interval behavior.
            let mut next_sync_tick = Instant::now();
            let tick_duration = Duration::from_millis(10);
            let mut next_spawn_tick = Instant::now() + tick_duration;
            let mut poll_tasks = TailerPollTaskSet::new();

            loop {
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Capture task: shutdown signal received");
                    break;
                }

                #[cfg(all(feature = "vendored", unix))]
                while let Ok(exit) = stream_exit_rx.try_recv() {
                    streaming_tasks.remove(&exit.pane_id);
                    if observed_panes_cache.contains_key(&exit.pane_id)
                        && should_record_streaming_fallback(&exit.reason)
                    {
                        warn!(
                            pane_id = exit.pane_id,
                            reason = %exit.reason,
                            "Vendored pane streaming ended; falling back to polling"
                        );
                    } else {
                        debug!(
                            pane_id = exit.pane_id,
                            reason = %exit.reason,
                            "Vendored pane streaming ended"
                        );
                    }

                    let polling_observed_panes: HashMap<u64, PaneInfo> = observed_panes_cache
                        .iter()
                        .filter(|(pane_id, _)| !streaming_tasks.contains_key(*pane_id))
                        .map(|(pane_id, pane)| (*pane_id, pane.clone()))
                        .collect();
                    supervisor.sync_tailers(&polling_observed_panes);
                }

                let now = Instant::now();
                if now >= next_sync_tick {
                    next_sync_tick = now + discovery_interval;
                    heartbeats.record_capture();

                    // Check for config updates
                    if config_update_pending(&config_rx) {
                        let new_config = config_take_update(&mut config_rx);
                        let new_tailer_config = TailerConfig {
                            min_interval: Duration::from_millis(new_config.min_poll_interval_ms),
                            max_interval: Duration::from_millis(new_config.poll_interval_ms),
                            backoff_multiplier: 1.5,
                            max_concurrent: new_config.max_concurrent_captures as usize,
                            overlap_size, // Use captured overlap_size
                            send_timeout: Duration::from_millis(100),
                            capture_timeout: Duration::from_secs(2),
                        };
                        #[cfg(all(feature = "vendored", unix))]
                        let tailer_max_interval = new_tailer_config.max_interval;
                        #[cfg(all(feature = "vendored", unix))]
                        let tailer_min_interval = new_tailer_config.min_interval;
                        supervisor.update_config(new_tailer_config);
                        supervisor.update_budget(new_config.capture_budgets.clone());
                        pane_priorities = new_config.pane_priorities.clone();
                        #[cfg(all(feature = "vendored", unix))]
                        {
                            vendored_subscription_config = vendored_streaming_enabled.then(|| {
                                streaming_subscription_config(
                                    tailer_max_interval,
                                    tailer_min_interval,
                                    vendored_channel_capacity,
                                )
                            });
                        }
                    }

                    // Get current observed panes from registry
                    let observed_panes: HashMap<u64, PaneInfo> = {
                        let reg = registry.read().await;
                        reg.observed_pane_ids()
                            .into_iter()
                            .filter_map(|id| reg.get_entry(id).map(|e| (id, e.info.clone())))
                            .collect()
                    };

                    #[cfg(all(feature = "vendored", unix))]
                    {
                        observed_panes_cache = observed_panes.clone();

                        let departed_streams: Vec<u64> = streaming_tasks
                            .keys()
                            .filter(|pane_id| !observed_panes_cache.contains_key(*pane_id))
                            .copied()
                            .collect();
                        for pane_id in departed_streams {
                            if let Some(handle) = streaming_tasks.remove(&pane_id) {
                                handle.abort();
                                debug!(pane_id, "Cancelled vendored streaming for departed pane");
                            }
                        }

                        if let Some(subscription_config) = vendored_subscription_config.clone() {
                            for &pane_id in observed_panes_cache.keys() {
                                if streaming_tasks.contains_key(&pane_id) {
                                    continue;
                                }

                                let Some((subscription_pane_id, socket_path)) =
                                    vendored_streaming_socket_for_pane(
                                        &vendored_mux_socket_paths,
                                        pane_id,
                                    )
                                else {
                                    continue;
                                };

                                if !socket_path.exists() {
                                    debug!(
                                        pane_id,
                                        path = %socket_path.display(),
                                        "Skipping vendored pane streaming because mux socket is missing"
                                    );
                                    continue;
                                }

                                let capture_tx = capture_tx.clone();
                                let stream_exit_tx = stream_exit_tx.clone();
                                let shutdown_flag = Arc::clone(&shutdown_flag);
                                let subscription_config = subscription_config.clone();
                                let socket_path_for_task = socket_path.clone();
                                let stream_task_cx = loop_cx.clone();
                                let handle = spawn_runtime_task(
                                    &stream_task_cx,
                                    move |stream_task_cx| async move {
                                        let exit_reason = run_vendored_streaming_capture(
                                            pane_id,
                                            subscription_pane_id,
                                            socket_path_for_task,
                                            vendored_mux_compression,
                                            subscription_config,
                                            capture_tx,
                                        )
                                        .await;
                                        let final_reason = if shutdown_flag.load(Ordering::SeqCst)
                                            && exit_reason == "capture ingress closed"
                                        {
                                            "shutdown".to_string()
                                        } else {
                                            exit_reason
                                        };
                                        let _ = send_runtime_channel(
                                            &stream_task_cx,
                                            &stream_exit_tx,
                                            StreamingTaskExit {
                                                pane_id,
                                                reason: final_reason,
                                            },
                                        )
                                        .await;
                                    },
                                );
                                streaming_tasks.insert(pane_id, handle);
                            }
                        }

                        let polling_observed_panes: HashMap<u64, PaneInfo> = observed_panes_cache
                            .iter()
                            .filter(|(pane_id, _)| !streaming_tasks.contains_key(*pane_id))
                            .map(|(pane_id, pane)| (*pane_id, pane.clone()))
                            .collect();
                        supervisor.sync_tailers(&polling_observed_panes);
                    }
                    #[cfg(not(all(feature = "vendored", unix)))]
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
                                let base = pane_priorities.priority_for_pane(&domain, title, cwd);
                                let override_priority =
                                    entry.priority_override.as_ref().and_then(|ov| {
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
                    continue;
                }

                if now >= next_spawn_tick {
                    next_spawn_tick = now + tick_duration;
                    supervisor.spawn_ready(&mut poll_tasks);
                    continue;
                }

                let next_deadline = if next_sync_tick <= next_spawn_tick {
                    next_sync_tick
                } else {
                    next_spawn_tick
                };
                let wait_duration = next_deadline.saturating_duration_since(Instant::now());
                if wait_duration.is_zero() {
                    continue;
                }

                if !poll_tasks.is_empty() {
                    if let Ok(Some((pane_id, outcome))) =
                        runtime_timeout(&loop_cx, wait_duration, poll_tasks.join_next()).await
                    {
                        supervisor.handle_poll_result(pane_id, outcome);
                    }
                } else {
                    runtime_sleep(&loop_cx, wait_duration).await;
                }
            }

            #[cfg(all(feature = "vendored", unix))]
            for (_pane_id, handle) in streaming_tasks.drain() {
                handle.abort();
            }

            // Graceful shutdown of all tailers
            supervisor.shutdown().await;
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
        let storage = self.storage.clone();
        let metrics = Arc::clone(&self.metrics);
        let event_bus = self.event_bus.clone();
        let pane_filter = self.config.pane_filter.clone();

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let listener = match NativeEventListener::bind(socket_path.clone()).await {
                Ok(listener) => {
                    info!(
                        path = %socket_path.display(),
                        "Native event listener bound — waiting for GUI connections"
                    );
                    listener
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        path = %socket_path.display(),
                        "Failed to bind native event socket, falling back to polling only"
                    );
                    return;
                }
            };

            let (event_tx, mut event_rx) = mpsc::channel::<NativeEvent>(1024);

            let accept_shutdown_flag = Arc::clone(&shutdown_flag);
            let accept_handle = spawn_runtime_task(&loop_cx, move |_accept_cx| {
                listener.run(event_tx, accept_shutdown_flag)
            });

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
                            metrics.backpressure_metrics(),
                        )
                        .await;
                    }

                    if shutdown_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    continue;
                }

                let flush_wait = next_flush.saturating_duration_since(now);
                match runtime_timeout(&loop_cx, flush_wait, recv_event(&loop_cx, &mut event_rx))
                    .await
                {
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
                                        metrics.backpressure_metrics(),
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
                                        metrics.backpressure_metrics(),
                                    )
                                    .await;
                                }

                                handle_native_event(
                                    &loop_cx,
                                    event,
                                    &capture_tx,
                                    &cursors,
                                    &detection_contexts,
                                    &storage,
                                    event_bus.as_ref(),
                                    &pane_filter,
                                    metrics.backpressure_metrics(),
                                )
                                .await;
                            }
                            _ => {
                                handle_native_event(
                                    &loop_cx,
                                    event,
                                    &capture_tx,
                                    &cursors,
                                    &detection_contexts,
                                    &storage,
                                    event_bus.as_ref(),
                                    &pane_filter,
                                    metrics.backpressure_metrics(),
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
                    metrics.backpressure_metrics(),
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

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            loop {
                match runtime_timeout(
                    &loop_cx,
                    Duration::from_millis(25),
                    recv_event(&loop_cx, &mut capture_ingress_rx),
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

                            // ft-xbnl0.2.3 tick 267: cx-first capture relay enqueue.
                            if capture_ring_tx.send_with_cx(&loop_cx, event).await.is_err() {
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
        let storage = self.storage.clone();
        let pattern_engine = Arc::clone(&self.pattern_engine);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let metrics = Arc::clone(&self.metrics);
        let event_bus = self.event_bus.clone();
        let recording = self.recording.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let tuning = Arc::clone(&self.tuning);
        let mut config_rx = self.config_rx.clone();
        let mut current_patterns = self.config.patterns.clone();
        let patterns_root = self.config.patterns_root.clone();
        let registry = Arc::clone(&registry);

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let max_persist_segment_bytes = tuning.ingest.max_persist_segment_bytes;

            // Process events until producer is closed and the ring is drained.
            while let Some(event) = capture_rx.recv().await {
                heartbeats.record_persistence();
                // Check shutdown flag - if set, drain remaining events quickly
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Persistence task: shutdown signal received, draining remaining events");
                    // Continue to drain but don't block forever
                }

                if config_update_pending(&config_rx) {
                    let new_config = config_take_update(&mut config_rx);
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
                let bounded_segment =
                    bounded_segment_for_persistence(&event.segment, max_persist_segment_bytes);
                let captured_at = bounded_segment.captured_at;
                let captured_seq = bounded_segment.seq;

                // Persist the segment
                // ft-xbnl0.2.3 tick 254: cx-first segment persist.
                match persist_captured_segment_for_runtime(
                    &loop_cx,
                    &storage,
                    &bounded_segment,
                    max_persist_segment_bytes,
                )
                .await
                {
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
                            // ft-xbnl0.2.3 tick 265: cx-first recording segment write.
                            if let Err(err) = manager
                                .record_segment_with_cx(&loop_cx, &event.segment)
                                .await
                            {
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
                                        seq_before: gap.seq_before,
                                        seq_after: gap.seq_after,
                                        reason: gap.reason.clone(),
                                        detected_at_ms: gap.detected_at,
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
                                engine
                                    .detect_with_context(bounded_segment.content.as_str(), &mut ctx)
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
                            // ft-xbnl0.2.3 tick 265: cx-first recording detection loop (shared cx).
                            for detection in detections {
                                if let Some(ref manager) = recording {
                                    if let Err(err) = manager
                                        .record_event_with_cx(
                                            &loop_cx,
                                            pane_id,
                                            &detection,
                                            captured_at,
                                        )
                                        .await
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

                                // ft-xbnl0.2.3 tick 251: cx-first event record.
                                match storage.record_event_with_cx(&loop_cx, stored_event).await {
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
            }
        })
    }

    /// Signal tasks to begin shutdown.
    pub fn signal_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
    }

    /// Take ownership of the storage handle for external shutdown.
    ///
    /// Returns the storage handle. The caller is responsible for shutdown.
    /// This invalidates the runtime.
    #[must_use]
    pub fn take_storage(self) -> StorageHandle {
        self.storage
    }
}

#[cfg(feature = "native-wezterm")]
async fn handle_native_event(
    runtime_cx: &RuntimeLoopCx,
    event: NativeEvent,
    capture_tx: &mpsc::Sender<CaptureEvent>,
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
    detection_contexts: &Arc<RwLock<HashMap<u64, DetectionContext>>>,
    storage: &StorageHandle,
    event_bus: Option<&Arc<EventBus>>,
    pane_filter: &PaneFilterConfig,
    backpressure: &Arc<BackpressureMetrics>,
) {
    match event {
        NativeEvent::PaneOutput {
            pane_id,
            data,
            timestamp_ms,
        } => {
            emit_native_output_delta(pane_id, data, timestamp_ms, capture_tx, cursors, backpressure).await;
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
                    // [ft-0e179] Per-pane drop attribution. The aggregate
                    // `segments_dropped` counter was unwired prior to this
                    // change; both the aggregate and the per-pane map are
                    // updated here so dashboards and 3am pivots see the
                    // same drop at the same time.
                    backpressure.record_segment_dropped(pane_id);
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

            // ft-xbnl0.2.3 tick 251: cx-first native-event pane upsert.
            if let Err(err) = storage.upsert_pane_with_cx(runtime_cx, record).await {
                warn!(pane_id, error = %err, "Failed to upsert pane from native event");
            }
            let max_seq = storage
                .get_max_seq_with_cx(runtime_cx, pane_id)
                .await
                .unwrap_or(None);

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
    backpressure: &Arc<BackpressureMetrics>,
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
            // [ft-0e179] Output drop is the high-volume case — a pane
            // producing faster than storage can drain saturates the
            // capture queue first. Per-pane attribution lets operators
            // answer "which pane is flooding?" from the drops histogram
            // without having to cross-reference with rate counters.
            backpressure.record_segment_dropped(pane_id);
            debug!(pane_id, "Native event queue full; dropping output");
        }
    } else {
        debug!(
            pane_id,
            "Native output received before cursor initialized; dropping"
        );
    }
}

#[cfg(all(feature = "vendored", unix))]
async fn run_vendored_streaming_capture(
    pane_id: u64,
    subscription_pane_id: u64,
    socket_path: PathBuf,
    compression_mode: crate::config::VendoredCompressionMode,
    subscription_config: SubscriptionConfig,
    capture_tx: mpsc::Sender<CaptureEvent>,
) -> String {
    let runtime_cx = runtime_loop_cx();

    let mut bridge = StreamingBridge::new();
    let mut client_config = DirectMuxClientConfig::default().with_socket_path(socket_path.clone());
    client_config.compression_mode = compression_mode;

    let client = match DirectMuxClient::connect_with_cx(&runtime_cx, client_config).await {
        Ok(client) => client,
        Err(err) => {
            bridge.record_fallback();
            return format!("connect error: {err}");
        }
    };

    info!(
        pane_id,
        local_pane_id = subscription_pane_id,
        socket = %socket_path.display(),
        "Started vendored pane streaming subscription"
    );

    let mut subscription = subscribe_pane_output_with_inherited_cx(
        &runtime_cx,
        client,
        subscription_pane_id,
        subscription_config.clone(),
    );

    if should_record_streaming_fallback(&exit_reason) {
        bridge.record_fallback();
    }
    subscription.shutdown().await;
    exit_reason
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
    pub storage: StorageHandle,
    /// Runtime metrics
    pub metrics: Arc<RuntimeMetrics>,
    /// Pane registry
    pub registry: Arc<RwLock<PaneRegistry>>,
    /// Per-pane cursors
    pub cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
    /// Best-effort per-pane output activity tracker for health snapshots.
    pane_activity_tracker: Arc<RwLock<HashMap<u64, PaneActivityState>>>,
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

// Backpressure, storage lock, and snapshot defaults — canonical values in TuningConfig.
// To override: set [tuning.backpressure], [tuning.runtime], or [tuning.snapshot] in ft.toml.
const BACKPRESSURE_WARN_RATIO: f64 = crate::tuning_config::BackpressureTuning::DEFAULT_WARN_RATIO;
const STORAGE_LOCK_CONTENTION_MIN_US: u64 = 1_000;
const STORAGE_LOCK_WAIT_WARN_MS: f64 =
    crate::tuning_config::RuntimeTuning::DEFAULT_STORAGE_LOCK_WAIT_WARN_MS;
const STORAGE_LOCK_HOLD_WARN_MS: f64 =
    crate::tuning_config::RuntimeTuning::DEFAULT_STORAGE_LOCK_HOLD_WARN_MS;
const CURSOR_SNAPSHOT_MEMORY_WARN_BYTES: u64 =
    crate::tuning_config::RuntimeTuning::DEFAULT_CURSOR_SNAPSHOT_MEMORY_WARN_BYTES;
const SNAPSHOT_TRIGGER_BRIDGE_TICK_SECS: u64 =
    crate::tuning_config::SnapshotTuning::DEFAULT_TRIGGER_BRIDGE_TICK_SECS;
const SNAPSHOT_IDLE_WINDOW_SECS: u64 =
    crate::tuning_config::SnapshotTuning::DEFAULT_IDLE_WINDOW_SECS;
const SNAPSHOT_MEMORY_TRIGGER_COOLDOWN_SECS: u64 =
    crate::tuning_config::SnapshotTuning::DEFAULT_MEMORY_TRIGGER_COOLDOWN_SECS;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneActivityState {
    last_seq: i64,
    last_output_at_ms: u64,
    generation: u32,
    first_seen_at_ms: u64,
}

#[derive(Debug, Clone)]
struct RuntimePaneStateCompaction {
    cursors: CacheCompactionStats,
    detection_contexts: CacheCompactionStats,
    pane_activity_tracker: CacheCompactionStats,
}

fn compact_runtime_pane_state(
    cursors: &mut HashMap<u64, PaneCursor>,
    detection_contexts: &mut HashMap<u64, DetectionContext>,
    pane_activity_tracker: &mut HashMap<u64, PaneActivityState>,
    active_panes: &HashSet<u64>,
) -> RuntimePaneStateCompaction {
    RuntimePaneStateCompaction {
        cursors: compact_u64_map(cursors, active_panes),
        detection_contexts: compact_u64_map(detection_contexts, active_panes),
        pane_activity_tracker: compact_u64_map(pane_activity_tracker, active_panes),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct HealthPaneSnapshot {
    observed_panes: usize,
    last_activity_by_pane: Vec<(u64, u64)>,
    last_seq_by_pane: Vec<(u64, i64)>,
    cursor_snapshot_bytes: u64,
}

fn build_health_pane_snapshot(
    registry: &PaneRegistry,
    cursors: &HashMap<u64, PaneCursor>,
    activity_tracker: &mut HashMap<u64, PaneActivityState>,
    now_ms: u64,
) -> HealthPaneSnapshot {
    let mut observed_ids = registry.observed_pane_ids();
    observed_ids.sort_unstable();
    let observed_set: HashSet<u64> = observed_ids.iter().copied().collect();
    activity_tracker.retain(|pane_id, _| observed_set.contains(pane_id));

    let mut last_activity_by_pane = Vec::with_capacity(observed_ids.len());
    let mut last_seq_by_pane = Vec::with_capacity(observed_ids.len());
    let mut cursor_snapshot_bytes = 0u64;

    for pane_id in observed_ids {
        let current_seq = cursors.get(&pane_id).map_or(-1, PaneCursor::last_seq);
        let (generation, first_seen_at_ms) =
            registry.get_entry(pane_id).map_or((0, now_ms), |entry| {
                (
                    entry.generation,
                    u64::try_from(entry.first_seen_at).unwrap_or(0),
                )
            });
        if let Some(cursor) = cursors.get(&pane_id) {
            cursor_snapshot_bytes = cursor_snapshot_bytes
                .saturating_add(u64::try_from(cursor.last_snapshot.len()).unwrap_or(u64::MAX));
        }

        let state = activity_tracker
            .entry(pane_id)
            .or_insert(PaneActivityState {
                last_seq: current_seq,
                last_output_at_ms: now_ms,
                generation,
                first_seen_at_ms,
            });
        if state.generation != generation || state.first_seen_at_ms != first_seen_at_ms {
            *state = PaneActivityState {
                last_seq: current_seq,
                last_output_at_ms: now_ms,
                generation,
                first_seen_at_ms,
            };
        } else if state.last_seq != current_seq {
            state.last_seq = current_seq;
            state.last_output_at_ms = now_ms;
        }

        last_seq_by_pane.push((pane_id, current_seq));
        last_activity_by_pane.push((pane_id, state.last_output_at_ms));
    }

    HealthPaneSnapshot {
        observed_panes: last_seq_by_pane.len(),
        last_activity_by_pane,
        last_seq_by_pane,
        cursor_snapshot_bytes,
    }
}

fn build_leak_risk_inventory(
    registry: &PaneRegistry,
    metrics: &RuntimeMetrics,
    heartbeats: &HeartbeatRegistry,
) -> LeakRiskInventorySnapshot {
    let mut window_ids = HashSet::new();
    let mut tab_ids = HashSet::new();
    let mut workspaces = HashSet::new();
    let mut observed_pane_count = 0usize;

    for (_, entry) in registry.entries() {
        window_ids.insert(entry.info.window_id);
        tab_ids.insert(entry.info.tab_id);
        if let Some(workspace) = entry.info.workspace.as_deref()
            && !workspace.trim().is_empty()
        {
            workspaces.insert(workspace.to_string());
        }
        if entry.should_observe() {
            observed_pane_count += 1;
        }
    }

    let pane_arenas = registry.pane_arena_stats_snapshot();
    let pane_arena_tracked_bytes = pane_arenas.iter().fold(0u64, |total, snapshot| {
        total.saturating_add(u64::try_from(snapshot.stats.tracked_bytes).unwrap_or(u64::MAX))
    });
    let pane_arena_peak_tracked_bytes = pane_arenas.iter().fold(0u64, |total, snapshot| {
        total.saturating_add(u64::try_from(snapshot.stats.peak_tracked_bytes).unwrap_or(u64::MAX))
    });

    let lock_memory = metrics.lock_memory_snapshot();
    let watchdog_report = heartbeats.check_health(&crate::watchdog::WatchdogConfig::default());
    let unhealthy_components = watchdog_report
        .unhealthy_components()
        .into_iter()
        .map(|component| LeakRiskWatchdogComponentSnapshot {
            component: component.component,
            status: component.status,
            age_ms: component.age_ms,
            threshold_ms: component.threshold_ms,
        })
        .collect();

    LeakRiskInventorySnapshot {
        tracked_pane_entries: registry.len(),
        observed_pane_count,
        window_count: window_ids.len(),
        tab_count: tab_ids.len(),
        workspace_count: workspaces.len(),
        pane_arena_count: pane_arenas.len(),
        pane_arena_tracked_bytes,
        pane_arena_peak_tracked_bytes,
        cursor_snapshot_bytes: lock_memory.cursor_snapshot_bytes_last,
        cursor_snapshot_peak_bytes: lock_memory.cursor_snapshot_bytes_max,
        storage_lock_contention_events: lock_memory.storage_lock_contention_events,
        storage_lock_wait_max_ms: lock_memory.max_storage_lock_wait_ms,
        storage_lock_hold_max_ms: lock_memory.max_storage_lock_hold_ms,
        watchdog: LeakRiskWatchdogSnapshot {
            overall: Some(watchdog_report.overall),
            unhealthy_components,
            telemetry: Some(heartbeats.telemetry().snapshot()),
        },
    }
}

fn build_fleet_pressure_signals(
    backpressure_manager: &BackpressureManager,
    queue_depths: &QueueDepths,
    memory_pressure: MemoryPressureTier,
    worst_budget: BudgetLevel,
    pane_count: usize,
) -> crate::fleet_memory_controller::PressureSignals {
    let _ = backpressure_manager.evaluate(queue_depths);

    crate::fleet_memory_controller::PressureSignals {
        backpressure: backpressure_manager.current_tier(),
        memory_pressure,
        worst_budget,
        pane_count,
        paused_pane_count: backpressure_manager.paused_pane_ids().len(),
    }
}

#[derive(Default)]
struct PaneTieredScrollbackFetch {
    summaries: HashMap<u64, PaneTieredScrollbackSummary>,
    errors: usize,
    error_samples: Vec<String>,
}

impl PaneTieredScrollbackFetch {
    fn note_error(&mut self, pane_id: u64, err: &impl std::fmt::Display) {
        self.errors = self.errors.saturating_add(1);
        if self.error_samples.len() < 4 {
            self.error_samples.push(format!("pane {pane_id}: {err}"));
        }
    }

    fn telemetry_blind(&self, pane_count: usize) -> bool {
        pane_count > 0 && self.summaries.is_empty() && self.errors > 0
    }

    fn telemetry_partial(&self, pane_count: usize) -> bool {
        pane_count > self.summaries.len() && self.errors > 0 && !self.telemetry_blind(pane_count)
    }
}

fn record_pane_tiered_scrollback_summary_result(
    fetch: &mut PaneTieredScrollbackFetch,
    pane_id: u64,
    result: std::result::Result<PaneTieredScrollbackSummary, crate::Error>,
) {
    match result {
        Ok(summary) => {
            fetch.summaries.insert(pane_id, summary);
        }
        Err(err) => {
            fetch.note_error(pane_id, &err);
            debug!(
                pane_id,
                error = %err,
                "failed to collect tiered scrollback summary"
            );
        }
    }
}

async fn collect_pane_tiered_scrollback_summaries(
    runtime_cx: &RuntimeLoopCx,
    wezterm_handle: &WeztermHandle,
    pane_ids: &[u64],
) -> PaneTieredScrollbackFetch {
    let mut fetch = PaneTieredScrollbackFetch::default();

    for &pane_id in pane_ids {
        let result = wezterm_handle
            .pane_tiered_scrollback_summary_with_cx(runtime_cx, pane_id)
            .await;
        record_pane_tiered_scrollback_summary_result(&mut fetch, pane_id, result);
    }

    fetch
}

fn approximate_warm_page_count(summary: &PaneTieredScrollbackSummary) -> usize {
    // The mux telemetry exposes resident warm lines/bytes but not the backing
    // page granularity. Approximate with the current TieredScrollback default
    // page size so the fleet controller can reason about warm capacity until
    // page-size telemetry is surfaced explicitly.
    const DEFAULT_WARM_PAGE_LINES: usize = 256;

    if summary.warm_resident_bytes == 0 {
        return 0;
    }

    if summary.warm_resident_lines == 0 {
        return 1;
    }

    summary
        .warm_resident_lines
        .div_ceil(DEFAULT_WARM_PAGE_LINES)
}

fn estimated_memory_bytes_from_tiered_scrollback(summary: &PaneTieredScrollbackSummary) -> usize {
    // Runtime arena accounting already tracks captured segments; augment it
    // with mux-side warm tier bytes and the in-memory scrollback rows that are
    // not represented in the compressed warm tier.
    const APPROX_IN_MEMORY_SCROLLBACK_LINE_BYTES: usize = 200;

    summary.warm_resident_bytes.saturating_add(
        summary
            .in_memory_scrollback_rows
            .saturating_mul(APPROX_IN_MEMORY_SCROLLBACK_LINE_BYTES),
    )
}

fn scrollback_snapshot_from_tiered_scrollback_summary(
    activity_counter: u64,
    summary: &PaneTieredScrollbackSummary,
) -> ScrollbackTierSnapshot {
    // The mux telemetry surfaces the resident hot/warm state the coordinator
    // needs for bounded fleet reads, but not the full historical spill totals
    // tracked by the in-process TieredScrollback implementation.
    let hot_lines = summary.in_memory_scrollback_rows;
    let warm_lines = summary.warm_resident_lines;
    let resident_lines = hot_lines.saturating_add(warm_lines);

    ScrollbackTierSnapshot {
        hot_lines,
        warm_pages: approximate_warm_page_count(summary),
        warm_bytes: summary.warm_resident_bytes,
        warm_lines,
        cold_lines: 0,
        cold_pages: 0,
        total_lines_added: u64::try_from(resident_lines).unwrap_or(u64::MAX),
        activity_counter,
        cold_uncompressed_bytes: 0,
    }
}

fn fleet_pane_scrollback_snapshots_from_registry(
    registry: &PaneRegistry,
    cursors: &HashMap<u64, PaneCursor>,
    tiered_scrollback_by_pane: &HashMap<u64, PaneTieredScrollbackSummary>,
) -> HashMap<u64, ScrollbackTierSnapshot> {
    registry
        .entries()
        .filter(|(_, entry)| entry.should_observe())
        .filter_map(|(pane_id, _)| {
            tiered_scrollback_by_pane.get(pane_id).map(|summary| {
                let activity_counter = cursors.get(pane_id).map_or(0, |cursor| cursor.next_seq);
                (
                    *pane_id,
                    scrollback_snapshot_from_tiered_scrollback_summary(activity_counter, summary),
                )
            })
        })
        .collect()
}

fn fleet_pane_infos_from_registry(
    registry: &PaneRegistry,
    cursors: &HashMap<u64, PaneCursor>,
    tiered_scrollback_by_pane: &HashMap<u64, PaneTieredScrollbackSummary>,
) -> Vec<PaneScrollbackInfo> {
    registry
        .entries()
        .filter(|(_, entry)| entry.should_observe())
        .map(|(pane_id, entry)| {
            let base_estimated_memory_bytes = registry
                .pane_arena_stats(*pane_id)
                .map(|stats| stats.tracked_bytes)
                .unwrap_or_else(|| entry.estimated_bytes());
            let tiered_scrollback = tiered_scrollback_by_pane.get(pane_id);
            let estimated_memory_bytes = tiered_scrollback
                .map(estimated_memory_bytes_from_tiered_scrollback)
                .map_or(base_estimated_memory_bytes, |bytes| {
                    base_estimated_memory_bytes.max(bytes)
                });
            let activity_counter = cursors.get(pane_id).map_or(0, |cursor| cursor.next_seq);

            PaneScrollbackInfo {
                pane_id: *pane_id,
                activity_counter,
                warm_bytes: tiered_scrollback.map_or(0, |summary| summary.warm_resident_bytes),
                warm_pages: tiered_scrollback.map_or(0, approximate_warm_page_count),
                estimated_memory_bytes,
            }
        })
        .collect()
}

fn classify_backpressure_tier(
    capture_depth: usize,
    capture_capacity: usize,
    write_depth: usize,
    write_capacity: usize,
) -> Option<String> {
    if capture_capacity == 0 && write_capacity == 0 {
        return None;
    }

    let config = BackpressureConfig::default();
    let capture_ratio = if capture_capacity == 0 {
        0.0
    } else {
        capture_depth as f64 / capture_capacity as f64
    };
    let write_ratio = if write_capacity == 0 {
        0.0
    } else {
        write_depth as f64 / write_capacity as f64
    };

    // Match BackpressureManager::classify saturation semantics.
    let capture_saturated =
        capture_capacity > 0 && capture_depth >= capture_capacity.saturating_sub(5);
    let write_saturated = write_capacity > 0 && write_depth >= write_capacity.saturating_sub(100);

    let tier = if capture_saturated || write_saturated {
        "BLACK"
    } else if capture_ratio >= config.red_capture || write_ratio >= config.red_write {
        "RED"
    } else if capture_ratio >= config.yellow_capture || write_ratio >= config.yellow_write {
        "YELLOW"
    } else {
        "GREEN"
    };

    Some(tier.to_string())
}

fn mpsc_max_capacity<T>(tx: &mpsc::Sender<T>) -> usize {
    {
        tx.capacity()
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
        self.storage.write_queue_depth()
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
        let shutdown_cx = runtime_loop_cx();
        let join_result = runtime_timeout(&shutdown_cx, shutdown_timeout, async {
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
            if let Err(e) = self.storage.shutdown().await {
                warnings.push(format!("Storage shutdown error: {e}"));
            }
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
        let (health_panes, leak_risk_inventory) = {
            let reg = self.registry.read().await;
            let cursors = self.cursors.read().await;
            let mut tracker = self.pane_activity_tracker.write().await;
            let health_panes =
                build_health_pane_snapshot(&reg, &cursors, &mut tracker, epoch_ms_u64());
            let leak_risk_inventory =
                build_leak_risk_inventory(&reg, &self.metrics, &self.heartbeats);
            (health_panes, leak_risk_inventory)
        };
        let observed_panes = health_panes.observed_panes;
        let last_activity_by_pane = health_panes.last_activity_by_pane;
        let last_seq_by_pane = health_panes.last_seq_by_pane;
        let cursor_snapshot_bytes = health_panes.cursor_snapshot_bytes;
        self.metrics
            .record_cursor_snapshot_memory(cursor_snapshot_bytes);

        // Measure queue depths for backpressure visibility
        let capture_depth = self.capture_queue_depth();
        let capture_cap = self.capture_queue_capacity();

        let (write_depth, write_cap, db_writable) = {
            let wd = self.storage.write_queue_depth();
            let wc = self.storage.write_queue_capacity();
            let writable = self.storage.is_writable().await;

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
        let backpressure_tier =
            classify_backpressure_tier(capture_depth, capture_cap, write_depth, write_cap);

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
            backpressure_tier,
            last_activity_by_pane,
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
            fleet_pressure_tier: None,
            leak_risk_inventory,
        };

        HealthSnapshot::update_global(snapshot);
        RuntimeLockMemoryTelemetrySnapshot::update_global(self.metrics.lock_memory_snapshot());
    }

    /// Take ownership of the storage handle for external shutdown.
    ///
    /// The caller is responsible for shutdown. This invalidates the runtime.
    #[must_use]
    pub fn take_storage(self) -> StorageHandle {
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
            | "session.compaction"
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
    use crate::runtime_compat::{CompatRuntime, sleep};
    use crate::storage::PaneRecord;
    use tempfile::TempDir;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for runtime tests");
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        crate::runtime_compat::clear_runtime_handle();
        if let Err(payload) = test_result {
            std::panic::resume_unwind(payload);
        }
    }

    /// Like `run_async_test`, but runs the runtime on a dedicated thread so that
    /// TLS destructors don't collide with other tests running in parallel.
    /// Use for tests that spawn background tasks via `task::spawn`.
    ///
    /// The runtime is explicitly dropped inside `catch_unwind` to absorb
    /// asupersync's RuntimeHandle TLS destructor panics (which occur when
    /// spawned tasks' `Sleep` futures are dropped after TLS is destroyed).
    /// Test assertion panics are captured separately and re-raised.
    fn run_async_test_isolated<F>(f: impl FnOnce() -> F + Send + 'static)
    where
        F: std::future::Future<Output = ()>,
    {
        let result = std::thread::Builder::new()
            .name("runtime-test-isolated".into())
            .spawn(move || {
                let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
                    .build()
                    .expect("failed to build runtime for runtime tests");

                // Run the test body, catching any assertion panics.
                let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(f());
                }));

                // Drop runtime inside catch_unwind to absorb TLS destructor
                // panics from asupersync Sleep/Cx cleanup.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(runtime);
                }));
                crate::runtime_compat::clear_runtime_handle();

                // Re-raise test assertion panics after cleanup.
                if let Err(payload) = test_result {
                    std::panic::resume_unwind(payload);
                }
            })
            .expect("failed to spawn isolated test thread")
            .join();
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    async fn send_mpsc<T>(tx: &mpsc::Sender<T>, value: T) {
        #[cfg(all(feature = "vendored", unix))]
        {
            let loop_cx = runtime_loop_cx();
            let sent = send_runtime_channel(&loop_cx, tx, value).await;
            assert!(sent, "test mpsc send should succeed");
        }
        #[cfg(not(all(feature = "vendored", unix)))]
        {
            let sent = {
                let cx = crate::cx::for_testing();
                tx.send(&cx, value).await
            };

            assert!(sent.is_ok(), "test mpsc send should succeed");
        }
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn send_runtime_channel_reports_open_receiver() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel(1);
            let loop_cx = runtime_loop_cx();
            assert!(send_runtime_channel(&loop_cx, &tx, 11).await);
            assert_eq!(recv_mpsc(&mut rx).await, 11);
        });
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn send_runtime_channel_reports_closed_receiver() {
        run_async_test(async {
            let (tx, rx) = mpsc::channel::<u8>(1);
            drop(rx);
            let loop_cx = runtime_loop_cx();
            assert!(!send_runtime_channel(&loop_cx, &tx, 7).await);
        });
    }

    async fn recv_mpsc<T>(rx: &mut mpsc::Receiver<T>) -> T {
        let cx = crate::cx::for_testing();
        rx.recv(&cx).await.expect("test mpsc recv should succeed")
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

    #[test]
    fn snapshot_trigger_from_event_maps_pattern_detection() {
        let event = Event::PatternDetected {
            pane_id: 7,
            pane_uuid: Some("pane-uuid-7".to_string()),
            detection: test_detection("session.start", Severity::Info),
            event_id: Some(42),
        };

        assert_eq!(
            snapshot_trigger_from_event(&event),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_from_event_maps_pane_lifecycle_events() {
        let discovered = Event::PaneDiscovered {
            pane_id: 9,
            domain: "local".to_string(),
            title: "codex".to_string(),
        };
        let disappeared = Event::PaneDisappeared { pane_id: 9 };

        assert_eq!(
            snapshot_trigger_from_event(&discovered),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
        assert_eq!(
            snapshot_trigger_from_event(&disappeared),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_from_event_ignores_non_trigger_events() {
        let events = [
            Event::SegmentCaptured {
                pane_id: 1,
                seq: 10,
                content_len: 25,
            },
            Event::GapDetected {
                pane_id: 1,
                seq_before: 9,
                seq_after: 10,
                reason: "overlap_not_found".to_string(),
                detected_at_ms: 1234,
            },
            Event::WorkflowStarted {
                workflow_id: "wf-3".to_string(),
                workflow_name: "handle_usage_limits".to_string(),
                pane_id: 1,
            },
            Event::WorkflowStep {
                workflow_id: "wf-3".to_string(),
                step_name: "record_and_plan".to_string(),
                result: "continue".to_string(),
            },
        ];

        for event in events {
            assert_eq!(snapshot_trigger_from_event(&event), None);
        }
    }

    #[test]
    fn runtime_config_defaults_are_reasonable() {
        run_async_test(async {
            let config = RuntimeConfig::default();

            assert_eq!(config.discovery_interval, Duration::from_secs(5));
            assert_eq!(config.capture_interval, Duration::from_millis(200));
            assert_eq!(config.overlap_size, 1_048_576); // 1MB default
            assert_eq!(config.channel_buffer, 1024);
        });
    }

    #[test]
    fn runtime_can_be_created() {
        run_async_test(async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let engine = PatternEngine::new();

            let config = RuntimeConfig::default();
            let _runtime = ObservationRuntime::new(config, storage, Arc::new(RwLock::new(engine)));
        });
    }

    #[test]
    fn runtime_startup_shutdown_with_mock_wezterm_is_clean() {
        run_async_test_isolated(|| async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let engine = PatternEngine::new();

            let mut config = RuntimeConfig::default();
            config.discovery_interval = Duration::from_millis(10);
            config.capture_interval = Duration::from_millis(10);
            config.min_capture_interval = Duration::from_millis(5);
            config.channel_buffer = 64;

            let mock = crate::wezterm::MockWezterm::new();
            mock.add_default_pane(0).await;
            mock.inject_output(0, "boot output\n").await.unwrap();
            let wezterm_handle: WeztermHandle = Arc::new(mock);

            let mut runtime =
                ObservationRuntime::new(config, storage, Arc::new(RwLock::new(engine)))
                    .with_wezterm_handle(wezterm_handle);

            let handle = runtime.start().await.expect("runtime should start");
            // Allow generous time for the runtime to complete its initial
            // discovery cycle under heavy parallel-test load.
            sleep(Duration::from_millis(100)).await;

            let summary = handle.shutdown_with_summary().await;
            assert!(
                summary.clean,
                "shutdown should complete cleanly: {:?}",
                summary.warnings
            );
            assert!(
                summary.warnings.is_empty(),
                "shutdown should not emit warnings for mock lifecycle: {:?}",
                summary.warnings
            );
        });
    }

    #[test]
    fn runtime_with_replay_capture_adapter_shuts_down_cleanly() {
        run_async_test_isolated(|| async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let engine = PatternEngine::new();

            let mut config = RuntimeConfig::default();
            config.discovery_interval = Duration::from_millis(10);
            config.capture_interval = Duration::from_millis(10);
            config.min_capture_interval = Duration::from_millis(5);
            config.channel_buffer = 64;

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            let wezterm_handle: WeztermHandle = mock.clone();

            let sink = Arc::new(crate::replay_capture::CollectingCaptureSink::new());
            let adapter = Arc::new(crate::replay_capture::CaptureAdapter::new(
                sink.clone(),
                crate::replay_capture::CaptureConfig {
                    session_id: Some("runtime-replay-test".to_string()),
                    ..Default::default()
                },
            ));
            let adapter_probe = adapter.clone();

            let mut runtime =
                ObservationRuntime::new(config, storage, Arc::new(RwLock::new(engine)))
                    .with_wezterm_handle(wezterm_handle)
                    .with_replay_capture_adapter(adapter);

            let handle = runtime.start().await.expect("runtime should start");

            // Start with no panes, then add one after startup so discovery
            // has a deterministic pane surface for capture. Generous delays
            // for heavy parallel-test load.
            sleep(Duration::from_millis(200)).await;
            mock.add_default_pane(0).await;
            sleep(Duration::from_millis(300)).await;
            mock.inject_output(0, "replay capture smoke\n")
                .await
                .unwrap();
            sleep(Duration::from_millis(300)).await;

            let summary = handle.shutdown_with_summary().await;
            assert!(
                summary.clean,
                "shutdown should complete cleanly: {:?}",
                summary.warnings
            );

            let events = sink.recorder_events();
            assert!(
                events.iter().all(|event| !event.event_id.is_empty()),
                "all captured events should have deterministic event_id values; got {events:#?}"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event.session_id.as_deref() == Some("runtime-replay-test")),
                "captured events should retain configured session_id; got {events:#?}"
            );
            assert!(adapter_probe.is_enabled());
            assert_eq!(adapter_probe.total_captured(), events.len() as u64);
        });
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
        metrics.record_storage_lock_wait(Duration::from_millis(2));
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
            fleet_pressure_tier: None,
            leak_risk_inventory: LeakRiskInventorySnapshot::default(),
        };

        // Verify metrics are correctly reflected in snapshot
        assert!((snapshot.ingest_lag_avg_ms - 30.0).abs() < f64::EPSILON);
        assert_eq!(snapshot.ingest_lag_max_ms, 50);
        assert!(snapshot.db_writable);
        assert!(snapshot.db_last_write_at.is_some());
    }

    #[test]
    fn leak_risk_inventory_counts_registry_and_watchdog_state() {
        let mut registry = PaneRegistry::new();
        let mut pane1 = make_pane(1, "bash");
        pane1.window_id = 10;
        pane1.tab_id = 20;
        pane1.workspace = Some("alpha".to_string());

        let mut pane2 = make_pane(2, "vim");
        pane2.window_id = 10;
        pane2.tab_id = 21;
        pane2.workspace = Some("beta".to_string());

        registry.discovery_tick(vec![pane1, pane2]);

        let metrics = RuntimeMetrics::default();
        metrics.record_storage_lock_wait(Duration::from_millis(2));
        metrics.record_storage_lock_hold(Duration::from_millis(3));
        metrics.record_cursor_snapshot_memory(4096);

        let heartbeats = HeartbeatRegistry::new();
        heartbeats.record_discovery();
        heartbeats.record_capture();
        heartbeats.record_persistence();
        heartbeats.record_maintenance();

        let inventory = build_leak_risk_inventory(&registry, &metrics, &heartbeats);

        assert_eq!(inventory.tracked_pane_entries, 2);
        assert_eq!(inventory.observed_pane_count, 2);
        assert_eq!(inventory.window_count, 1);
        assert_eq!(inventory.tab_count, 2);
        assert_eq!(inventory.workspace_count, 2);
        assert_eq!(inventory.pane_arena_count, 2);
        assert!(inventory.pane_arena_tracked_bytes > 0);
        assert!(inventory.pane_arena_peak_tracked_bytes >= inventory.pane_arena_tracked_bytes);
        assert_eq!(inventory.cursor_snapshot_bytes, 4096);
        assert_eq!(inventory.cursor_snapshot_peak_bytes, 4096);
        assert_eq!(inventory.storage_lock_contention_events, 1);
        assert!(inventory.storage_lock_wait_max_ms >= 2.0);
        assert!(inventory.storage_lock_hold_max_ms >= 3.0);
        assert_eq!(
            inventory.watchdog.overall,
            Some(crate::watchdog::HealthStatus::Healthy)
        );
        assert!(inventory.watchdog.unhealthy_components.is_empty());

        let telemetry = inventory
            .watchdog
            .telemetry
            .expect("watchdog telemetry should be present");
        assert_eq!(telemetry.discovery_heartbeats, 1);
        assert_eq!(telemetry.capture_heartbeats, 1);
        assert_eq!(telemetry.persistence_heartbeats, 1);
        assert_eq!(telemetry.maintenance_heartbeats, 1);
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
    fn classify_backpressure_tier_none_when_capacities_unknown() {
        assert!(classify_backpressure_tier(0, 0, 0, 0).is_none());
    }

    #[test]
    fn classify_backpressure_tier_maps_expected_levels() {
        // Keep write queue far from saturation; otherwise small capacities can
        // classify as BLACK due to the write-side saturation guardrail.
        let write_capacity = 10_000;
        assert_eq!(
            classify_backpressure_tier(0, 100, 0, write_capacity).as_deref(),
            Some("GREEN")
        );
        assert_eq!(
            classify_backpressure_tier(50, 100, 0, write_capacity).as_deref(),
            Some("YELLOW")
        );
        assert_eq!(
            classify_backpressure_tier(75, 100, 0, write_capacity).as_deref(),
            Some("RED")
        );
        assert_eq!(
            classify_backpressure_tier(98, 100, 0, write_capacity).as_deref(),
            Some("BLACK")
        );
    }

    #[test]
    fn classify_backpressure_tier_matches_manager_semantics() {
        use crate::backpressure::{BackpressureManager, QueueDepths};

        let manager = BackpressureManager::new(BackpressureConfig::default());
        let capacities = [0usize, 1, 4, 5, 6, 25, 100, 256];

        for &capture_capacity in &capacities {
            for &write_capacity in &capacities {
                let capture_depths = [
                    0,
                    capture_capacity / 2,
                    capture_capacity.saturating_sub(1),
                    capture_capacity,
                    capture_capacity.saturating_add(3),
                ];
                let write_depths = [
                    0,
                    write_capacity / 2,
                    write_capacity.saturating_sub(1),
                    write_capacity,
                    write_capacity.saturating_add(3),
                ];

                for &capture_depth in &capture_depths {
                    for &write_depth in &write_depths {
                        let actual = classify_backpressure_tier(
                            capture_depth,
                            capture_capacity,
                            write_depth,
                            write_capacity,
                        );
                        let expected = if capture_capacity == 0 && write_capacity == 0 {
                            None
                        } else {
                            Some(
                                manager
                                    .classify(&QueueDepths {
                                        capture_depth,
                                        capture_capacity,
                                        write_depth,
                                        write_capacity,
                                    })
                                    .to_string(),
                            )
                        };

                        assert_eq!(
                            actual, expected,
                            "mismatch for capture={capture_depth}/{capture_capacity}, write={write_depth}/{write_capacity}"
                        );
                    }
                }
            }
        }
    }

    fn make_pane(pane_id: u64, title: &str) -> PaneInfo {
        PaneInfo {
            pane_id,
            tab_id: 1,
            window_id: 1,
            domain_id: None,
            domain_name: None,
            workspace: Some("default".to_string()),
            size: None,
            rows: None,
            cols: None,
            title: Some(title.to_string()),
            cwd: Some("/tmp".to_string()),
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: true,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        }
    }

    fn cursor_map_from_registry(registry: &PaneRegistry) -> HashMap<u64, PaneCursor> {
        registry
            .observed_pane_ids()
            .into_iter()
            .filter_map(|pane_id| {
                registry
                    .get_cursor(pane_id)
                    .cloned()
                    .map(|cursor| (pane_id, cursor))
            })
            .collect()
    }

    #[test]
    fn fleet_pane_infos_from_registry_uses_cursor_activity_and_arena_accounting() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash"), make_pane(2, "vim")]);

        // Build an external cursor map simulating real capture-pipeline state.
        let mut ext_cursors: HashMap<u64, PaneCursor> = HashMap::new();
        let mut c1 = PaneCursor::new(1);
        c1.next_seq = 111;
        ext_cursors.insert(1, c1);
        let mut c2 = PaneCursor::new(2);
        c2.next_seq = 222;
        ext_cursors.insert(2, c2);

        let stats1 = registry.pane_arena_stats(1).expect("pane 1 arena stats");
        let stats2 = registry.pane_arena_stats(2).expect("pane 2 arena stats");

        let mut infos = fleet_pane_infos_from_registry(&registry, &ext_cursors, &HashMap::new());
        infos.sort_by_key(|info| info.pane_id);

        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].pane_id, 1);
        assert_eq!(infos[0].activity_counter, 111);
        assert_eq!(infos[0].warm_bytes, 0);
        assert_eq!(infos[0].warm_pages, 0);
        assert_eq!(infos[0].estimated_memory_bytes, stats1.tracked_bytes);

        assert_eq!(infos[1].pane_id, 2);
        assert_eq!(infos[1].activity_counter, 222);
        assert_eq!(infos[1].warm_bytes, 0);
        assert_eq!(infos[1].warm_pages, 0);
        assert_eq!(infos[1].estimated_memory_bytes, stats2.tracked_bytes);
    }

    #[test]
    fn health_pane_snapshot_ignores_discovery_heartbeats_without_new_output() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash")]);
        registry.get_cursor_mut(1).expect("pane 1 cursor").next_seq = 7;

        let mut tracker = HashMap::new();
        let first = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            1_000,
        );
        assert_eq!(first.last_activity_by_pane, vec![(1, 1_000)]);
        assert_eq!(first.last_seq_by_pane, vec![(1, 6)]);

        registry
            .get_entry_mut(1)
            .expect("pane 1 entry")
            .last_seen_at = 9_000;
        let second = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            9_000,
        );

        assert_eq!(second.last_seq_by_pane, vec![(1, 6)]);
        assert_eq!(
            second.last_activity_by_pane,
            vec![(1, 1_000)],
            "discovery heartbeats must not reset pane activity without new cursor progress"
        );
    }

    #[test]
    fn health_pane_snapshot_updates_activity_when_cursor_progresses() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash")]);
        registry.get_cursor_mut(1).expect("pane 1 cursor").next_seq = 3;

        let mut tracker = HashMap::new();
        let first = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            2_000,
        );
        assert_eq!(first.last_activity_by_pane, vec![(1, 2_000)]);
        assert_eq!(first.last_seq_by_pane, vec![(1, 2)]);

        registry.get_cursor_mut(1).expect("pane 1 cursor").next_seq = 4;
        let second = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            8_000,
        );

        assert_eq!(second.last_seq_by_pane, vec![(1, 3)]);
        assert_eq!(second.last_activity_by_pane, vec![(1, 8_000)]);
    }

    #[test]
    fn health_pane_snapshot_resets_activity_on_generation_change() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash")]);
        registry.get_cursor_mut(1).expect("pane 1 cursor").next_seq = 4;

        let mut tracker = HashMap::new();
        let first = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            2_000,
        );
        assert_eq!(first.last_activity_by_pane, vec![(1, 2_000)]);

        registry.discovery_tick(vec![make_pane(1, "vim")]);
        let second = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            9_000,
        );

        assert_eq!(second.last_seq_by_pane, vec![(1, 3)]);
        assert_eq!(
            second.last_activity_by_pane,
            vec![(1, 9_000)],
            "new pane generations must reset tracked activity even when sequence numbers are unchanged"
        );
    }

    #[test]
    fn health_pane_snapshot_resets_activity_on_first_seen_change() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash")]);
        registry.get_cursor_mut(1).expect("pane 1 cursor").next_seq = 5;

        let mut tracker = HashMap::new();
        let first = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            1_000,
        );
        assert_eq!(first.last_activity_by_pane, vec![(1, 1_000)]);

        registry
            .get_entry_mut(1)
            .expect("pane 1 entry")
            .first_seen_at = 8_000;
        let second = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            8_000,
        );

        assert_eq!(second.last_seq_by_pane, vec![(1, 4)]);
        assert_eq!(second.last_activity_by_pane, vec![(1, 8_000)]);
    }

    #[test]
    fn health_pane_snapshot_drops_closed_panes_from_tracker() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash"), make_pane(2, "vim")]);

        let mut tracker = HashMap::new();
        let first = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            5_000,
        );
        assert_eq!(first.observed_panes, 2);
        assert_eq!(tracker.len(), 2);

        registry.discovery_tick(vec![make_pane(1, "bash")]);
        let second = build_health_pane_snapshot(
            &registry,
            &cursor_map_from_registry(&registry),
            &mut tracker,
            6_000,
        );

        assert_eq!(second.observed_panes, 1);
        assert_eq!(second.last_activity_by_pane.len(), 1);
        assert_eq!(tracker.len(), 1);
        assert!(!tracker.contains_key(&2));
    }

    #[test]
    fn ft_xbnl0_4_3_runtime_state_compaction_drops_unobserved_panes() {
        let mut cursors = HashMap::from([
            (1_u64, PaneCursor::from_seq(1, 4)),
            (2_u64, PaneCursor::from_seq(2, 9)),
        ]);
        let mut detection_contexts = HashMap::from([
            (1_u64, DetectionContext::new()),
            (2_u64, DetectionContext::new()),
        ]);
        let mut pane_activity_tracker = HashMap::from([
            (
                1_u64,
                PaneActivityState {
                    last_seq: 4,
                    last_output_at_ms: 1_000,
                    generation: 1,
                    first_seen_at_ms: 1_000,
                },
            ),
            (
                2_u64,
                PaneActivityState {
                    last_seq: 9,
                    last_output_at_ms: 2_000,
                    generation: 1,
                    first_seen_at_ms: 2_000,
                },
            ),
        ]);
        let active_panes = HashSet::from([1_u64]);

        let stats = compact_runtime_pane_state(
            &mut cursors,
            &mut detection_contexts,
            &mut pane_activity_tracker,
            &active_panes,
        );

        assert_eq!(stats.cursors.removed_entries, 1);
        assert_eq!(stats.detection_contexts.removed_entries, 1);
        assert_eq!(stats.pane_activity_tracker.removed_entries, 1);
        assert!(cursors.contains_key(&1));
        assert!(!cursors.contains_key(&2));
        assert!(detection_contexts.contains_key(&1));
        assert!(!detection_contexts.contains_key(&2));
        assert!(pane_activity_tracker.contains_key(&1));
        assert!(!pane_activity_tracker.contains_key(&2));
    }

    #[test]
    fn ft_xbnl0_4_4_leak_inventory_returns_to_baseline_after_pane_teardown() {
        let mut registry = PaneRegistry::new();

        let mut pane1 = make_pane(1, "bash");
        pane1.window_id = 10;
        pane1.tab_id = 20;
        pane1.workspace = Some("alpha".to_string());

        let mut pane2 = make_pane(2, "vim");
        pane2.window_id = 11;
        pane2.tab_id = 21;
        pane2.workspace = Some("beta".to_string());

        registry.discovery_tick(vec![pane1, pane2]);

        let metrics = RuntimeMetrics::default();
        let heartbeats = HeartbeatRegistry::new();
        heartbeats.record_discovery();
        heartbeats.record_capture();
        heartbeats.record_persistence();
        heartbeats.record_maintenance();

        let active_inventory = build_leak_risk_inventory(&registry, &metrics, &heartbeats);
        assert_eq!(active_inventory.tracked_pane_entries, 2);
        assert_eq!(active_inventory.observed_pane_count, 2);
        assert_eq!(active_inventory.window_count, 2);
        assert_eq!(active_inventory.tab_count, 2);
        assert_eq!(active_inventory.workspace_count, 2);
        assert_eq!(active_inventory.pane_arena_count, 2);

        registry.discovery_tick(vec![]);

        let baseline_inventory = build_leak_risk_inventory(&registry, &metrics, &heartbeats);
        assert_eq!(baseline_inventory.tracked_pane_entries, 0);
        assert_eq!(baseline_inventory.observed_pane_count, 0);
        assert_eq!(baseline_inventory.window_count, 0);
        assert_eq!(baseline_inventory.tab_count, 0);
        assert_eq!(baseline_inventory.workspace_count, 0);
        assert_eq!(baseline_inventory.pane_arena_count, 0);
        assert_eq!(baseline_inventory.pane_arena_tracked_bytes, 0);
        assert_eq!(baseline_inventory.pane_arena_peak_tracked_bytes, 0);
        assert_eq!(
            baseline_inventory.watchdog.overall,
            Some(crate::watchdog::HealthStatus::Healthy)
        );
    }

    #[test]
    fn ft_xbnl0_4_4_leak_inventory_stays_bounded_across_reconnect_cycles() {
        let mut registry = PaneRegistry::new();
        let metrics = RuntimeMetrics::default();
        let heartbeats = HeartbeatRegistry::new();
        heartbeats.record_discovery();
        heartbeats.record_capture();
        heartbeats.record_persistence();
        heartbeats.record_maintenance();

        for cycle in 0_u64..16 {
            let mut pane = make_pane(7, "ssh-reconnect");
            pane.window_id = 100 + cycle;
            pane.tab_id = 200 + cycle;
            pane.workspace = Some(format!("cycle-{cycle}"));

            registry.discovery_tick(vec![pane]);

            let active_inventory = build_leak_risk_inventory(&registry, &metrics, &heartbeats);
            assert_eq!(
                active_inventory.tracked_pane_entries, 1,
                "tracked pane entries grew during reconnect cycle {cycle}"
            );
            assert_eq!(
                active_inventory.observed_pane_count, 1,
                "observed pane count grew during reconnect cycle {cycle}"
            );
            assert_eq!(
                active_inventory.window_count, 1,
                "window count grew during reconnect cycle {cycle}"
            );
            assert_eq!(
                active_inventory.tab_count, 1,
                "tab count grew during reconnect cycle {cycle}"
            );
            assert_eq!(
                active_inventory.workspace_count, 1,
                "workspace count grew during reconnect cycle {cycle}"
            );
            assert_eq!(
                active_inventory.pane_arena_count, 1,
                "pane arena count grew during reconnect cycle {cycle}"
            );

            registry.discovery_tick(vec![]);

            let baseline_inventory = build_leak_risk_inventory(&registry, &metrics, &heartbeats);
            assert_eq!(
                baseline_inventory.tracked_pane_entries, 0,
                "tracked pane entries failed to return to baseline after reconnect cycle {cycle}"
            );
            assert_eq!(baseline_inventory.observed_pane_count, 0);
            assert_eq!(baseline_inventory.window_count, 0);
            assert_eq!(baseline_inventory.tab_count, 0);
            assert_eq!(baseline_inventory.workspace_count, 0);
            assert_eq!(baseline_inventory.pane_arena_count, 0);
            assert_eq!(baseline_inventory.pane_arena_tracked_bytes, 0);
            assert_eq!(baseline_inventory.pane_arena_peak_tracked_bytes, 0);
        }
    }

    #[test]
    fn ft_xbnl0_4_4_runtime_state_compaction_stays_bounded_across_churn_cycles() {
        let mut cursors = HashMap::new();
        let mut detection_contexts = HashMap::new();
        let mut pane_activity_tracker = HashMap::new();

        for cycle in 0_u64..32 {
            let pane_id = cycle + 1;
            cursors.insert(pane_id, PaneCursor::from_seq(pane_id, cycle + 1));
            detection_contexts.insert(pane_id, DetectionContext::new());
            pane_activity_tracker.insert(
                pane_id,
                PaneActivityState {
                    last_seq: cycle as i64 + 1,
                    last_output_at_ms: 10_000 + cycle,
                    generation: 1,
                    first_seen_at_ms: 10_000 + cycle,
                },
            );

            let active_panes = HashSet::from([pane_id]);
            let stats = compact_runtime_pane_state(
                &mut cursors,
                &mut detection_contexts,
                &mut pane_activity_tracker,
                &active_panes,
            );

            assert_eq!(cursors.len(), 1, "cursor map grew during cycle {cycle}");
            assert_eq!(
                detection_contexts.len(),
                1,
                "detection contexts grew during cycle {cycle}"
            );
            assert_eq!(
                pane_activity_tracker.len(),
                1,
                "pane activity tracker grew during cycle {cycle}"
            );
            assert!(cursors.contains_key(&pane_id));
            assert!(detection_contexts.contains_key(&pane_id));
            assert!(pane_activity_tracker.contains_key(&pane_id));
            assert!(
                stats.cursors.removed_entries <= 1
                    && stats.detection_contexts.removed_entries <= 1
                    && stats.pane_activity_tracker.removed_entries <= 1,
                "unexpected multi-entry growth during cycle {cycle}: {stats:?}"
            );
        }

        let empty = HashSet::new();
        let final_stats = compact_runtime_pane_state(
            &mut cursors,
            &mut detection_contexts,
            &mut pane_activity_tracker,
            &empty,
        );

        assert_eq!(final_stats.cursors.removed_entries, 1);
        assert_eq!(final_stats.detection_contexts.removed_entries, 1);
        assert_eq!(final_stats.pane_activity_tracker.removed_entries, 1);
        assert!(cursors.is_empty());
        assert!(detection_contexts.is_empty());
        assert!(pane_activity_tracker.is_empty());
    }

    #[test]
    fn build_fleet_pressure_signals_tracks_manager_state() {
        let manager = BackpressureManager::new(BackpressureConfig::default());
        manager.pause_pane(5);
        manager.pause_pane(9);

        let signals = build_fleet_pressure_signals(
            &manager,
            &QueueDepths {
                capture_depth: 96,
                capture_capacity: 100,
                write_depth: 0,
                write_capacity: 1_000,
            },
            MemoryPressureTier::Orange,
            BudgetLevel::Throttled,
            12,
        );

        assert_eq!(
            signals.backpressure,
            crate::backpressure::BackpressureTier::Black
        );
        assert_eq!(signals.memory_pressure, MemoryPressureTier::Orange);
        assert_eq!(signals.worst_budget, BudgetLevel::Throttled);
        assert_eq!(signals.pane_count, 12);
        assert_eq!(signals.paused_pane_count, 2);
    }

    #[test]
    fn mpsc_queue_depth_computation_is_correct() {
        // Validates queue depth accounting for a fixed-capacity channel.
        let (tx, _rx) = mpsc::channel::<u8>(16);
        #[allow(unused_variables)]
        let max_cap = 16usize;
        assert_eq!(max_cap, 16);

        // Empty queue: depth should be 0
        let depth = max_cap - tx.capacity();
        assert_eq!(depth, 0);
    }

    #[test]
    fn mpsc_queue_depth_increases_with_sends() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel::<u8>(16);
            #[allow(unused_variables)]
            let max_cap = 16usize;

            // Send some items
            send_mpsc(&tx, 1).await;
            send_mpsc(&tx, 2).await;
            send_mpsc(&tx, 3).await;

            let depth = rx.len();
            assert_eq!(depth, 3);

            // Drain one item, depth should decrease
            let _ = recv_mpsc(&mut rx).await;
            let depth = rx.len();
            assert_eq!(depth, 2);
        });
    }

    #[test]
    fn pane_tiered_scrollback_fetch_marks_blind_when_all_samples_fail() {
        let mut fetch = PaneTieredScrollbackFetch::default();
        fetch.note_error(7, &"mux circuit breaker open");
        fetch.note_error(9, &"codec mismatch");

        assert!(fetch.telemetry_blind(2));
        assert!(!fetch.telemetry_partial(2));
        assert_eq!(fetch.error_samples.len(), 2);
    }

    #[test]
    fn pane_tiered_scrollback_fetch_marks_partial_when_some_samples_succeed() {
        let mut fetch = PaneTieredScrollbackFetch::default();
        fetch
            .summaries
            .insert(11, PaneTieredScrollbackSummary::default());
        fetch.note_error(12, &"mux telemetry unavailable");

        assert!(!fetch.telemetry_blind(2));
        assert!(fetch.telemetry_partial(2));
    }

    #[test]
    fn pane_tiered_scrollback_fetch_treats_missing_summary_as_blind_error() {
        let mut fetch = PaneTieredScrollbackFetch::default();
        record_pane_tiered_scrollback_summary_result(
            &mut fetch,
            7,
            Err(crate::Error::Wezterm(
                crate::error::WeztermError::CommandFailed(
                    "backend returned no tiered scrollback summary".to_string(),
                ),
            )),
        );

        assert!(fetch.telemetry_blind(1));
        assert!(!fetch.telemetry_partial(1));
        assert_eq!(fetch.error_samples.len(), 1);
        assert!(fetch.error_samples[0].contains("pane 7:"));
        assert!(fetch.error_samples[0].contains("backend returned no tiered scrollback summary"));
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
            fleet_pressure_tier: None,
            leak_risk_inventory: LeakRiskInventorySnapshot::default(),
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
            fleet_pressure_tier: None,
            leak_risk_inventory: LeakRiskInventorySnapshot::default(),
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
            fleet_pressure_tier: None,
            leak_risk_inventory: LeakRiskInventorySnapshot::default(),
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

    // =========================================================================
    // Pure function tests: bytes_to_mib, epoch_ms, duration_ms
    // =========================================================================

    #[test]
    fn bytes_to_mib_zero() {
        assert!((bytes_to_mib(0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bytes_to_mib_one_mib() {
        assert!((bytes_to_mib(1024 * 1024) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bytes_to_mib_fractional() {
        let result = bytes_to_mib(512 * 1024);
        assert!((result - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn bytes_to_mib_large() {
        let result = bytes_to_mib(10 * 1024 * 1024);
        assert!((result - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn epoch_ms_returns_positive() {
        let ms = epoch_ms();
        assert!(ms > 0, "epoch_ms should return positive value");
    }

    #[test]
    fn epoch_ms_u64_returns_positive() {
        let ms = epoch_ms_u64();
        assert!(ms > 0, "epoch_ms_u64 should return positive value");
    }

    #[test]
    fn epoch_ms_and_u64_are_consistent() {
        let signed = epoch_ms();
        let unsigned = epoch_ms_u64();
        assert_eq!(signed as u64, unsigned);
    }

    #[test]
    fn duration_ms_u64_zero() {
        assert_eq!(duration_ms_u64(Duration::ZERO), 0);
    }

    #[test]
    fn duration_ms_u64_one_second() {
        assert_eq!(duration_ms_u64(Duration::from_secs(1)), 1000);
    }

    #[test]
    fn duration_ms_u64_sub_millisecond() {
        // 500 microseconds = 0 milliseconds (truncated)
        assert_eq!(duration_ms_u64(Duration::from_micros(500)), 0);
    }

    // =========================================================================
    // record_bounded_sample and percentile_from_samples
    // =========================================================================

    #[test]
    fn record_bounded_sample_adds_value() {
        let samples = StdMutex::new(VecDeque::new());
        record_bounded_sample(&samples, 42);
        let guard = samples.lock().unwrap();
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0], 42);
    }

    #[test]
    fn record_bounded_sample_respects_capacity() {
        let samples = StdMutex::new(VecDeque::new());
        for i in 0..TELEMETRY_PERCENTILE_WINDOW_CAPACITY + 10 {
            record_bounded_sample(&samples, i as u64);
        }
        let guard = samples.lock().unwrap();
        assert_eq!(guard.len(), TELEMETRY_PERCENTILE_WINDOW_CAPACITY);
        // First values should have been evicted
        assert_eq!(*guard.front().unwrap(), 10);
    }

    #[test]
    fn percentile_from_samples_empty_returns_zero() {
        let samples = StdMutex::new(VecDeque::new());
        assert_eq!(percentile_from_samples(&samples, 50), 0);
    }

    #[test]
    fn percentile_from_samples_single_value() {
        let samples = StdMutex::new(VecDeque::from([42]));
        assert_eq!(percentile_from_samples(&samples, 50), 42);
        assert_eq!(percentile_from_samples(&samples, 95), 42);
    }

    #[test]
    fn percentile_from_samples_p50_of_two() {
        let samples = StdMutex::new(VecDeque::from([10, 20]));
        let p50 = percentile_from_samples(&samples, 50);
        // With 2 values, p50 should return 20 (index = (2-1)*50+99 / 100 = 1)
        assert_eq!(p50, 20);
    }

    #[test]
    fn percentile_from_samples_sorted_correctly() {
        // Values added out of order should still give correct percentiles
        let samples = StdMutex::new(VecDeque::from([100, 10, 50, 90, 30]));
        let p50 = percentile_from_samples(&samples, 50);
        // Sorted: [10, 30, 50, 90, 100], p50 index = (4*50+99)/100 = 2 => 50
        assert!((30..=90).contains(&p50));
    }

    // =========================================================================
    // event_counts_as_activity
    // =========================================================================

    #[test]
    fn event_counts_as_activity_segment_captured() {
        let event = Event::SegmentCaptured {
            pane_id: 1,
            seq: 1,
            content_len: 100,
        };
        assert!(event_counts_as_activity(&event));
    }

    #[test]
    fn event_counts_as_activity_gap_detected() {
        let event = Event::GapDetected {
            pane_id: 1,
            seq_before: 4,
            seq_after: 5,
            reason: "test gap".to_string(),
            detected_at_ms: 1234,
        };
        assert!(event_counts_as_activity(&event));
    }

    #[test]
    fn event_counts_as_activity_pane_discovered() {
        let event = Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "shell".to_string(),
        };
        assert!(event_counts_as_activity(&event));
    }

    #[test]
    fn event_counts_as_activity_pane_disappeared() {
        let event = Event::PaneDisappeared { pane_id: 1 };
        assert!(event_counts_as_activity(&event));
    }

    #[test]
    fn event_counts_as_activity_workflow_started() {
        let event = Event::WorkflowStarted {
            workflow_id: "wf-1".to_string(),
            workflow_name: "test".to_string(),
            pane_id: 1,
        };
        assert!(event_counts_as_activity(&event));
    }

    #[test]
    fn event_counts_as_activity_workflow_completed() {
        let event = Event::WorkflowCompleted {
            workflow_id: "wf-1".to_string(),
            success: true,
            reason: None,
        };
        assert!(event_counts_as_activity(&event));
    }

    // =========================================================================
    // snapshot_trigger_from_detection — additional event types
    // =========================================================================

    #[test]
    fn snapshot_trigger_critical_severity_always_hazard() {
        let detection = test_detection("any.random.type", Severity::Critical);
        let trigger = snapshot_trigger_from_detection(&detection);
        assert_eq!(
            trigger,
            Some(crate::snapshot_engine::SnapshotTrigger::HazardThreshold)
        );
    }

    #[test]
    fn snapshot_trigger_usage_reached_is_hazard() {
        let detection = test_detection("usage.reached", Severity::Info);
        let trigger = snapshot_trigger_from_detection(&detection);
        assert_eq!(
            trigger,
            Some(crate::snapshot_engine::SnapshotTrigger::HazardThreshold)
        );
    }

    #[test]
    fn snapshot_trigger_error_network_is_hazard() {
        let detection = test_detection("error.network", Severity::Warning);
        let trigger = snapshot_trigger_from_detection(&detection);
        assert_eq!(
            trigger,
            Some(crate::snapshot_engine::SnapshotTrigger::HazardThreshold)
        );
    }

    #[test]
    fn snapshot_trigger_auth_error_is_hazard() {
        let detection = test_detection("auth.error", Severity::Warning);
        assert_eq!(
            snapshot_trigger_from_detection(&detection),
            Some(crate::snapshot_engine::SnapshotTrigger::HazardThreshold)
        );
    }

    #[test]
    fn snapshot_trigger_session_compaction_complete_is_work_completed() {
        let detection = test_detection("session.compaction_complete", Severity::Info);
        assert_eq!(
            snapshot_trigger_from_detection(&detection),
            Some(crate::snapshot_engine::SnapshotTrigger::WorkCompleted)
        );
    }

    #[test]
    fn snapshot_trigger_session_compaction_is_work_completed() {
        let detection = test_detection("session.compaction", Severity::Info);
        assert_eq!(
            snapshot_trigger_from_detection(&detection),
            Some(crate::snapshot_engine::SnapshotTrigger::WorkCompleted)
        );
    }

    #[test]
    fn snapshot_trigger_session_summary_is_work_completed() {
        let detection = test_detection("session.summary", Severity::Info);
        assert_eq!(
            snapshot_trigger_from_detection(&detection),
            Some(crate::snapshot_engine::SnapshotTrigger::WorkCompleted)
        );
    }

    #[test]
    fn snapshot_trigger_session_end_is_work_completed() {
        let detection = test_detection("session.end", Severity::Info);
        assert_eq!(
            snapshot_trigger_from_detection(&detection),
            Some(crate::snapshot_engine::SnapshotTrigger::WorkCompleted)
        );
    }

    #[test]
    fn snapshot_trigger_session_resume_hint_is_state_transition() {
        let detection = test_detection("session.resume_hint", Severity::Info);
        assert_eq!(
            snapshot_trigger_from_detection(&detection),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_session_thinking_is_state_transition() {
        let detection = test_detection("session.thinking", Severity::Info);
        assert_eq!(
            snapshot_trigger_from_detection(&detection),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_session_approval_needed_is_state_transition() {
        let detection = test_detection("session.approval_needed", Severity::Info);
        assert_eq!(
            snapshot_trigger_from_detection(&detection),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_unknown_event_type_returns_none() {
        let detection = test_detection("completely.unknown.event", Severity::Info);
        assert_eq!(snapshot_trigger_from_detection(&detection), None);
    }

    // =========================================================================
    // snapshot_trigger_from_user_var — additional variants
    // =========================================================================

    #[test]
    fn snapshot_trigger_user_var_cmd_start() {
        let payload = UserVarPayload {
            value: "x".to_string(),
            event_type: Some("cmd_start".to_string()),
            event_data: None,
        };
        assert_eq!(
            snapshot_trigger_from_user_var(&payload),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_user_var_preexec() {
        let payload = UserVarPayload {
            value: "x".to_string(),
            event_type: Some("preexec".to_string()),
            event_data: None,
        };
        assert_eq!(
            snapshot_trigger_from_user_var(&payload),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_user_var_cmd_end() {
        let payload = UserVarPayload {
            value: "x".to_string(),
            event_type: Some("cmd_end".to_string()),
            event_data: None,
        };
        assert_eq!(
            snapshot_trigger_from_user_var(&payload),
            Some(crate::snapshot_engine::SnapshotTrigger::WorkCompleted)
        );
    }

    #[test]
    fn snapshot_trigger_user_var_postexec() {
        let payload = UserVarPayload {
            value: "x".to_string(),
            event_type: Some("postexec".to_string()),
            event_data: None,
        };
        assert_eq!(
            snapshot_trigger_from_user_var(&payload),
            Some(crate::snapshot_engine::SnapshotTrigger::WorkCompleted)
        );
    }

    #[test]
    fn snapshot_trigger_user_var_none_event_type() {
        let payload = UserVarPayload {
            value: "x".to_string(),
            event_type: None,
            event_data: None,
        };
        assert_eq!(snapshot_trigger_from_user_var(&payload), None);
    }

    #[test]
    fn snapshot_trigger_user_var_unknown_event_type() {
        let payload = UserVarPayload {
            value: "x".to_string(),
            event_type: Some("random_type".to_string()),
            event_data: None,
        };
        assert_eq!(snapshot_trigger_from_user_var(&payload), None);
    }

    // =========================================================================
    // snapshot_trigger_from_event — additional variants
    // =========================================================================

    #[test]
    fn snapshot_trigger_event_pane_discovered() {
        let event = Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "shell".to_string(),
        };
        assert_eq!(
            snapshot_trigger_from_event(&event),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_event_pane_disappeared() {
        let event = Event::PaneDisappeared { pane_id: 1 };
        assert_eq!(
            snapshot_trigger_from_event(&event),
            Some(crate::snapshot_engine::SnapshotTrigger::StateTransition)
        );
    }

    #[test]
    fn snapshot_trigger_event_segment_captured_returns_none() {
        let event = Event::SegmentCaptured {
            pane_id: 1,
            seq: 1,
            content_len: 100,
        };
        assert_eq!(snapshot_trigger_from_event(&event), None);
    }

    #[test]
    fn snapshot_trigger_event_gap_detected_returns_none() {
        let event = Event::GapDetected {
            pane_id: 1,
            seq_before: 4,
            seq_after: 5,
            reason: "test".to_string(),
            detected_at_ms: 1234,
        };
        assert_eq!(snapshot_trigger_from_event(&event), None);
    }

    #[test]
    fn snapshot_trigger_event_workflow_started_returns_none() {
        let event = Event::WorkflowStarted {
            workflow_id: "wf-1".to_string(),
            workflow_name: "test".to_string(),
            pane_id: 1,
        };
        assert_eq!(snapshot_trigger_from_event(&event), None);
    }

    // =========================================================================
    // detection_to_stored_event — additional edge cases
    // =========================================================================

    #[test]
    fn detection_to_stored_event_warning_severity() {
        let detection = Detection {
            rule_id: "rule.warn".to_string(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "error.timeout".to_string(),
            severity: Severity::Warning,
            confidence: 0.8,
            extracted: serde_json::json!(null),
            matched_text: String::new(),
            span: (10, 20),
        };

        let event = detection_to_stored_event(99, None, &detection, None);
        assert_eq!(event.pane_id, 99);
        assert_eq!(event.severity, "warning");
        assert_eq!(event.segment_id, None);
        assert!(event.detected_at > 0);
    }

    #[test]
    fn detection_to_stored_event_critical_severity() {
        let detection = Detection {
            rule_id: "rule.crit".to_string(),
            agent_type: crate::patterns::AgentType::ClaudeCode,
            event_type: "error.fatal".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::json!({"detail": "oom"}),
            matched_text: "out of memory".to_string(),
            span: (0, 13),
        };

        let event = detection_to_stored_event(1, Some("uuid-abc"), &detection, Some(42));
        assert_eq!(event.severity, "critical");
        assert_eq!(event.segment_id, Some(42));
        assert!(event.matched_text.as_deref() == Some("out of memory"));
        assert!(event.extracted.is_some());
    }

    #[test]
    fn detection_to_stored_event_dedupe_key_contains_bucket() {
        let detection = test_detection("test.event", Severity::Info);
        let event = detection_to_stored_event(1, None, &detection, None);
        let key = event.dedupe_key.as_ref().unwrap();
        // Dedupe key should contain a colon separating identity key from bucket
        assert!(key.contains(':'));
    }

    // =========================================================================
    // RuntimeConfig field tests
    // =========================================================================

    #[test]
    fn runtime_config_default_min_capture_interval() {
        let config = RuntimeConfig::default();
        assert_eq!(config.min_capture_interval, Duration::from_millis(50));
    }

    #[test]
    fn runtime_config_default_max_concurrent_captures() {
        let config = RuntimeConfig::default();
        assert_eq!(config.max_concurrent_captures, 10);
    }

    #[test]
    fn runtime_config_default_retention_days() {
        let config = RuntimeConfig::default();
        assert_eq!(config.retention_days, 30);
    }

    #[test]
    fn runtime_config_default_retention_max_mb_unlimited() {
        let config = RuntimeConfig::default();
        assert_eq!(config.retention_max_mb, 0);
    }

    #[test]
    fn runtime_config_default_checkpoint_interval() {
        let config = RuntimeConfig::default();
        assert_eq!(config.checkpoint_interval_secs, 60);
    }

    #[test]
    fn runtime_config_default_cache_gc_settings() {
        let config = RuntimeConfig::default();
        assert_eq!(config.gc, CacheGcSettings::default());
    }

    #[test]
    fn runtime_config_default_no_vendored_mux_socket_paths() {
        let config = RuntimeConfig::default();
        assert!(config.vendored_mux_socket_paths.is_empty());
    }

    #[test]
    fn runtime_config_default_no_native_event_socket() {
        let config = RuntimeConfig::default();
        assert!(config.native_event_socket.is_none());
    }

    #[test]
    fn runtime_config_default_no_patterns_root() {
        let config = RuntimeConfig::default();
        assert!(config.patterns_root.is_none());
    }

    #[test]
    fn runtime_config_clone() {
        let config = RuntimeConfig::default();
        let cloned = config.clone();
        assert_eq!(cloned.discovery_interval, config.discovery_interval);
        assert_eq!(cloned.channel_buffer, config.channel_buffer);
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn vendored_streaming_socket_for_single_backend_uses_raw_pane_id() {
        let socket_paths = vec![PathBuf::from("/tmp/wa.sock")];
        let (pane_id, socket_path) =
            vendored_streaming_socket_for_pane(&socket_paths, 42).expect("single socket");
        assert_eq!(pane_id, 42);
        assert_eq!(socket_path, PathBuf::from("/tmp/wa.sock"));
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn vendored_streaming_socket_for_sharded_backend_decodes_pane_bits() {
        let socket_paths = vec![
            PathBuf::from("/tmp/wa-0.sock"),
            PathBuf::from("/tmp/wa-1.sock"),
        ];
        let global_pane_id =
            crate::sharding::encode_sharded_pane_id(crate::sharding::ShardId(1), 7);
        let (pane_id, socket_path) =
            vendored_streaming_socket_for_pane(&socket_paths, global_pane_id)
                .expect("sharded socket");
        assert_eq!(pane_id, 7);
        assert_eq!(socket_path, PathBuf::from("/tmp/wa-1.sock"));
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn vendored_streaming_socket_for_unknown_shard_returns_none() {
        let socket_paths = vec![
            PathBuf::from("/tmp/wa-0.sock"),
            PathBuf::from("/tmp/wa-1.sock"),
        ];
        let global_pane_id =
            crate::sharding::encode_sharded_pane_id(crate::sharding::ShardId(3), 9);
        assert!(vendored_streaming_socket_for_pane(&socket_paths, global_pane_id).is_none());
    }

    // =========================================================================
    // ResizeWatchdogSeverity additional tests
    // =========================================================================

    #[test]
    fn watchdog_severity_copy_and_eq() {
        let a = ResizeWatchdogSeverity::Warning;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(a, ResizeWatchdogSeverity::Healthy);
    }

    #[test]
    fn watchdog_severity_debug() {
        let dbg = format!("{:?}", ResizeWatchdogSeverity::Critical);
        assert_eq!(dbg, "Critical");
    }

    // =========================================================================
    // derive_resize_degradation_ladder — Healthy and Critical cases
    // =========================================================================

    #[test]
    fn derive_resize_degradation_ladder_healthy_is_nominal() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Healthy,
            stalled_total: 0,
            stalled_critical: 0,
            warning_threshold_ms: 2_000,
            critical_threshold_ms: 8_000,
            critical_stalled_limit: 2,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: true,
            recommended_action: "none".into(),
            sample_stalled: vec![],
        };

        let ladder = derive_resize_degradation_ladder(&assessment);
        assert_eq!(
            ladder.tier,
            crate::degradation::ResizeDegradationTier::FullQuality
        );
    }

    #[test]
    fn derive_resize_degradation_ladder_critical_is_correctness() {
        let assessment = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Critical,
            stalled_total: 4,
            stalled_critical: 3,
            warning_threshold_ms: 2_000,
            critical_threshold_ms: 8_000,
            critical_stalled_limit: 2,
            safe_mode_recommended: true,
            safe_mode_active: false,
            legacy_fallback_enabled: true,
            recommended_action: "enable_safe_mode_fallback".into(),
            sample_stalled: vec![],
        };

        let ladder = derive_resize_degradation_ladder(&assessment);
        assert_eq!(
            ladder.tier,
            crate::degradation::ResizeDegradationTier::CorrectnessGuarded
        );
    }

    // =========================================================================
    // ResizeWatchdogAssessment field tests
    // =========================================================================

    #[test]
    fn watchdog_assessment_debug_format() {
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
        let dbg = format!("{assessment:?}");
        assert!(dbg.contains("ResizeWatchdogAssessment"));
        assert!(dbg.contains("Healthy"));
    }

    #[test]
    fn watchdog_assessment_eq() {
        let a = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Warning,
            stalled_total: 1,
            stalled_critical: 0,
            warning_threshold_ms: 2000,
            critical_threshold_ms: 5000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: true,
            recommended_action: "monitor".into(),
            sample_stalled: vec![],
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // =========================================================================
    // RuntimeLockMemoryTelemetrySnapshot tests
    // =========================================================================

    #[test]
    fn lock_memory_snapshot_clone_and_debug() {
        let snap = RuntimeLockMemoryTelemetrySnapshot {
            timestamp_ms: 100,
            avg_storage_lock_wait_ms: 1.0,
            p50_storage_lock_wait_ms: 0.5,
            p95_storage_lock_wait_ms: 3.0,
            max_storage_lock_wait_ms: 5.0,
            storage_lock_contention_events: 2,
            avg_storage_lock_hold_ms: 2.0,
            p50_storage_lock_hold_ms: 1.5,
            p95_storage_lock_hold_ms: 6.0,
            max_storage_lock_hold_ms: 8.0,
            cursor_snapshot_bytes_last: 1024,
            p50_cursor_snapshot_bytes: 1024,
            p95_cursor_snapshot_bytes: 2048,
            cursor_snapshot_bytes_max: 4096,
            avg_cursor_snapshot_bytes: 1500.0,
        };
        let cloned = snap.clone();
        assert_eq!(snap, cloned);
        let dbg = format!("{snap:?}");
        assert!(dbg.contains("RuntimeLockMemoryTelemetrySnapshot"));
    }

    // =========================================================================
    // Constant validation tests
    // =========================================================================

    #[test]
    fn telemetry_percentile_window_capacity_is_positive() {
        assert!(TELEMETRY_PERCENTILE_WINDOW_CAPACITY > 0);
    }

    #[test]
    fn resize_watchdog_thresholds_are_ordered() {
        assert!(RESIZE_WATCHDOG_WARNING_THRESHOLD_MS < RESIZE_WATCHDOG_CRITICAL_THRESHOLD_MS);
    }

    #[test]
    fn resize_watchdog_critical_stalled_limit_is_positive() {
        assert!(RESIZE_WATCHDOG_CRITICAL_STALLED_LIMIT > 0);
    }

    #[test]
    fn resize_watchdog_sample_limit_is_positive() {
        assert!(RESIZE_WATCHDOG_SAMPLE_LIMIT > 0);
    }

    #[test]
    fn storage_lock_warn_thresholds_exist() {
        // These constants are used in health snapshot warnings
        assert!(STORAGE_LOCK_WAIT_WARN_MS > 0.0);
        assert!(STORAGE_LOCK_HOLD_WARN_MS > 0.0);
        assert!(CURSOR_SNAPSHOT_MEMORY_WARN_BYTES > 0);
    }

    // =========================================================================
    // RubyBeaver wa-1u90p.7.1 — additional pure unit tests
    // =========================================================================

    #[test]
    fn bytes_to_mib_conversion() {
        assert!((bytes_to_mib(0) - 0.0).abs() < f64::EPSILON);
        assert!((bytes_to_mib(1_048_576) - 1.0).abs() < f64::EPSILON);
        assert!((bytes_to_mib(2_097_152) - 2.0).abs() < f64::EPSILON);
        // 512 KB = 0.5 MiB
        assert!((bytes_to_mib(524_288) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn epoch_ms_returns_plausible_timestamp() {
        let ms = epoch_ms();
        // Should be after 2020-01-01 and positive
        assert!(ms > 1_577_836_800_000);
    }

    #[test]
    fn epoch_ms_u64_returns_plausible_timestamp() {
        let ms = epoch_ms_u64();
        assert!(ms > 1_577_836_800_000);
    }

    #[test]
    fn duration_ms_u64_conversion() {
        assert_eq!(duration_ms_u64(Duration::from_millis(0)), 0);
        assert_eq!(duration_ms_u64(Duration::from_millis(42)), 42);
        assert_eq!(duration_ms_u64(Duration::from_secs(1)), 1000);
        assert_eq!(duration_ms_u64(Duration::from_secs(60)), 60_000);
    }

    #[test]
    fn lock_memory_telemetry_snapshot_serde_roundtrip() {
        let snap = RuntimeLockMemoryTelemetrySnapshot {
            timestamp_ms: 12345,
            avg_storage_lock_wait_ms: 1.5,
            p50_storage_lock_wait_ms: 1.0,
            p95_storage_lock_wait_ms: 3.0,
            max_storage_lock_wait_ms: 5.0,
            storage_lock_contention_events: 10,
            avg_storage_lock_hold_ms: 2.0,
            p50_storage_lock_hold_ms: 1.5,
            p95_storage_lock_hold_ms: 6.0,
            max_storage_lock_hold_ms: 8.0,
            cursor_snapshot_bytes_last: 1024,
            p50_cursor_snapshot_bytes: 1024,
            p95_cursor_snapshot_bytes: 2048,
            cursor_snapshot_bytes_max: 4096,
            avg_cursor_snapshot_bytes: 1500.0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: RuntimeLockMemoryTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn watchdog_assessment_warning_severity_roundtrip() {
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
            recommended_action: "monitor".into(),
            sample_stalled: vec![],
        };
        let json = serde_json::to_string(&assessment).unwrap();
        let back: ResizeWatchdogAssessment = serde_json::from_str(&json).unwrap();
        assert_eq!(assessment, back);
    }

    #[test]
    fn runtime_config_default_channel_buffer() {
        let config = RuntimeConfig::default();
        assert_eq!(config.channel_buffer, 1024);
    }

    #[test]
    fn event_counts_as_activity_pattern_detected() {
        let detection = test_detection("session.tool_use", Severity::Info);
        let event = Event::PatternDetected {
            pane_id: 1,
            pane_uuid: None,
            detection,
            event_id: None,
        };
        assert!(event_counts_as_activity(&event));
    }

    #[test]
    fn event_counts_as_activity_workflow_step() {
        let event = Event::WorkflowStep {
            workflow_id: "wf-1".to_string(),
            step_name: "step1".to_string(),
            result: "ok".to_string(),
        };
        assert!(event_counts_as_activity(&event));
    }

    #[test]
    fn event_counts_as_activity_user_var_received() {
        let event = Event::UserVarReceived {
            pane_id: 1,
            name: "FT_EVENT".to_string(),
            payload: crate::events::UserVarPayload {
                value: String::new(),
                event_type: None,
                event_data: None,
            },
        };
        assert!(event_counts_as_activity(&event));
    }

    #[test]
    fn runtime_metrics_native_output_input_tracking() {
        let m = RuntimeMetrics::default();
        assert_eq!(m.native_output_input_events(), 0);
        assert_eq!(m.native_output_input_bytes(), 0);
        m.record_native_output_input(256);
        assert_eq!(m.native_output_input_events(), 1);
        assert_eq!(m.native_output_input_bytes(), 256);
        m.record_native_output_input(128);
        assert_eq!(m.native_output_input_events(), 2);
        assert_eq!(m.native_output_input_bytes(), 384);
    }

    #[test]
    fn runtime_metrics_native_output_batch_tracking() {
        let m = RuntimeMetrics::default();
        assert_eq!(m.native_output_batches_emitted(), 0);
        assert_eq!(m.native_output_emitted_bytes(), 0);
        m.record_native_output_batch(3, 512);
        assert_eq!(m.native_output_batches_emitted(), 1);
        assert_eq!(m.native_output_emitted_bytes(), 512);
        assert_eq!(m.native_output_max_batch_events(), 3);
        assert_eq!(m.native_output_max_batch_bytes(), 512);
        // second batch smaller — max stays
        m.record_native_output_batch(2, 256);
        assert_eq!(m.native_output_batches_emitted(), 2);
        assert_eq!(m.native_output_max_batch_events(), 3);
        assert_eq!(m.native_output_max_batch_bytes(), 512);
    }

    #[test]
    fn runtime_metrics_avg_ingest_lag_zero_samples() {
        let m = RuntimeMetrics::default();
        assert!((m.avg_ingest_lag_ms() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn runtime_metrics_avg_storage_lock_wait_zero_samples() {
        let m = RuntimeMetrics::default();
        assert!((m.avg_storage_lock_wait_ms() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn runtime_metrics_avg_storage_lock_hold_zero_samples() {
        let m = RuntimeMetrics::default();
        assert!((m.avg_storage_lock_hold_ms() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn runtime_metrics_avg_cursor_snapshot_zero_samples() {
        let m = RuntimeMetrics::default();
        assert!((m.avg_cursor_snapshot_bytes() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn runtime_config_default_discovery_interval() {
        let config = RuntimeConfig::default();
        assert_eq!(config.discovery_interval, Duration::from_secs(5));
    }

    #[test]
    fn runtime_config_default_overlap_size() {
        let config = RuntimeConfig::default();
        assert_eq!(config.overlap_size, 1_048_576);
    }

    #[test]
    fn derive_resize_degradation_ladder_from_warning() {
        let watchdog = ResizeWatchdogAssessment {
            severity: ResizeWatchdogSeverity::Warning,
            stalled_total: 1,
            stalled_critical: 0,
            warning_threshold_ms: 2000,
            critical_threshold_ms: 5000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: false,
            legacy_fallback_enabled: true,
            recommended_action: "monitor".into(),
            sample_stalled: vec![],
        };
        let ladder = derive_resize_degradation_ladder(&watchdog);
        // Warning with no critical stalls should NOT recommend safe-mode
        assert!(!ladder.signals.safe_mode_recommended);
    }

    // ── Fleet coordinator runtime integration tests (ft-dwjtm) ─────────

    #[test]
    fn fleet_pane_infos_from_empty_registry() {
        let registry = PaneRegistry::new();
        let cursors: HashMap<u64, PaneCursor> = HashMap::new();
        let infos = fleet_pane_infos_from_registry(&registry, &cursors, &HashMap::new());
        assert!(infos.is_empty());
    }

    #[test]
    fn fleet_pane_infos_excludes_ignored_panes() {
        let mut registry = PaneRegistry::new();
        // discovery_tick adds all as observed by default
        registry.discovery_tick(vec![make_pane(1, "observed"), make_pane(2, "ignored")]);

        // Mark pane 2 as ignored via the filter path
        if let Some(entry) = registry.get_entry_mut(2) {
            entry.observation = crate::ingest::ObservationDecision::Ignored {
                reason: "excluded by filter".to_string(),
            };
        }

        let cursors: HashMap<u64, PaneCursor> = HashMap::new();
        let infos = fleet_pane_infos_from_registry(&registry, &cursors, &HashMap::new());
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].pane_id, 1);
    }

    #[test]
    fn build_fleet_pressure_signals_green_at_low_utilization() {
        let manager = BackpressureManager::new(BackpressureConfig::default());
        let signals = build_fleet_pressure_signals(
            &manager,
            &QueueDepths {
                capture_depth: 5,
                capture_capacity: 100,
                write_depth: 10,
                write_capacity: 1_000,
            },
            MemoryPressureTier::Green,
            BudgetLevel::Normal,
            5,
        );
        assert_eq!(
            signals.backpressure,
            crate::backpressure::BackpressureTier::Green
        );
        assert_eq!(signals.memory_pressure, MemoryPressureTier::Green);
        assert_eq!(signals.worst_budget, BudgetLevel::Normal);
        assert_eq!(signals.pane_count, 5);
        assert_eq!(signals.paused_pane_count, 0);
    }

    #[test]
    fn build_fleet_pressure_signals_zero_capacity_is_green() {
        let manager = BackpressureManager::new(BackpressureConfig::default());
        let signals = build_fleet_pressure_signals(
            &manager,
            &QueueDepths {
                capture_depth: 0,
                capture_capacity: 0,
                write_depth: 0,
                write_capacity: 0,
            },
            MemoryPressureTier::Green,
            BudgetLevel::Normal,
            0,
        );
        // When capacities are zero the manager should still produce a valid tier
        assert_eq!(signals.pane_count, 0);
    }

    #[test]
    fn build_fleet_pressure_signals_propagates_memory_pressure() {
        let manager = BackpressureManager::new(BackpressureConfig::default());
        let signals = build_fleet_pressure_signals(
            &manager,
            &QueueDepths {
                capture_depth: 0,
                capture_capacity: 100,
                write_depth: 0,
                write_capacity: 100,
            },
            MemoryPressureTier::Red,
            BudgetLevel::Throttled,
            10,
        );
        assert_eq!(signals.memory_pressure, MemoryPressureTier::Red);
        assert_eq!(signals.worst_budget, BudgetLevel::Throttled);
    }

    #[test]
    fn fleet_pane_infos_without_live_tiers_report_zero_warm_pages() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "test")]);

        let cursors: HashMap<u64, PaneCursor> = HashMap::new();
        let infos = fleet_pane_infos_from_registry(&registry, &cursors, &HashMap::new());
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].warm_bytes, 0);
        assert_eq!(infos[0].warm_pages, 0);
    }

    #[test]
    fn fleet_pane_infos_activity_defaults_to_zero_without_cursor_progress() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "idle")]);

        let cursors: HashMap<u64, PaneCursor> = HashMap::new();
        let infos = fleet_pane_infos_from_registry(&registry, &cursors, &HashMap::new());
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].activity_counter, 0);
    }

    #[test]
    fn scrollback_snapshot_from_live_tiered_scrollback_status() {
        let snapshot = scrollback_snapshot_from_tiered_scrollback_summary(
            7,
            &PaneTieredScrollbackSummary {
                tiering_enabled: true,
                configured_scrollback_rows: 10_000,
                configured_hot_lines: 1_024,
                configured_warm_max_bytes: 2_000_000,
                visible_rows: 40,
                in_memory_scrollback_rows: 64,
                warm_resident_lines: 600,
                warm_resident_bytes: 120_000,
            },
        );

        assert_eq!(snapshot.hot_lines, 64);
        assert_eq!(snapshot.warm_lines, 600);
        assert_eq!(snapshot.warm_pages, 3);
        assert_eq!(snapshot.warm_bytes, 120_000);
        assert_eq!(snapshot.total_lines_added, 664);
        assert_eq!(snapshot.activity_counter, 7);
    }

    #[test]
    fn fleet_pane_scrollback_snapshots_skip_panes_without_live_tiers() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "test")]);

        let cursors: HashMap<u64, PaneCursor> = HashMap::new();
        let snapshots =
            fleet_pane_scrollback_snapshots_from_registry(&registry, &cursors, &HashMap::new());
        assert!(snapshots.is_empty());
    }

    #[test]
    fn fleet_pane_scrollback_snapshots_use_live_tiered_scrollback_status() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash"), make_pane(2, "ignored")]);

        if let Some(entry) = registry.get_entry_mut(2) {
            entry.observation = crate::ingest::ObservationDecision::Ignored {
                reason: "excluded by filter".to_string(),
            };
        }

        let mut ext_cursors: HashMap<u64, PaneCursor> = HashMap::new();
        let mut cursor = PaneCursor::new(1);
        cursor.next_seq = 7;
        ext_cursors.insert(1, cursor);

        let mut tiered_scrollback = HashMap::new();
        tiered_scrollback.insert(
            1,
            PaneTieredScrollbackSummary {
                tiering_enabled: true,
                configured_scrollback_rows: 10_000,
                configured_hot_lines: 1_024,
                configured_warm_max_bytes: 2_000_000,
                visible_rows: 40,
                in_memory_scrollback_rows: 64,
                warm_resident_lines: 600,
                warm_resident_bytes: 120_000,
            },
        );
        tiered_scrollback.insert(2, PaneTieredScrollbackSummary::default());

        let snapshots = fleet_pane_scrollback_snapshots_from_registry(
            &registry,
            &ext_cursors,
            &tiered_scrollback,
        );

        assert_eq!(snapshots.len(), 1);
        let snapshot = snapshots.get(&1).expect("snapshot for observed pane");
        assert_eq!(snapshot.hot_lines, 64);
        assert_eq!(snapshot.warm_lines, 600);
        assert_eq!(snapshot.warm_pages, 3);
        assert_eq!(snapshot.warm_bytes, 120_000);
        assert_eq!(snapshot.activity_counter, 7);
    }

    #[test]
    fn fleet_pane_infos_merge_live_tiered_scrollback_status() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash")]);

        let mut ext_cursors: HashMap<u64, PaneCursor> = HashMap::new();
        let mut cursor = PaneCursor::new(1);
        cursor.next_seq = 7;
        ext_cursors.insert(1, cursor);

        let mut tiered_scrollback = HashMap::new();
        tiered_scrollback.insert(
            1,
            PaneTieredScrollbackSummary {
                tiering_enabled: true,
                configured_scrollback_rows: 10_000,
                configured_hot_lines: 1_024,
                configured_warm_max_bytes: 2_000_000,
                visible_rows: 40,
                in_memory_scrollback_rows: 64,
                warm_resident_lines: 600,
                warm_resident_bytes: 120_000,
            },
        );

        let infos = fleet_pane_infos_from_registry(&registry, &ext_cursors, &tiered_scrollback);
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].pane_id, 1);
        assert_eq!(infos[0].activity_counter, 7);
        assert_eq!(infos[0].warm_bytes, 120_000);
        assert_eq!(infos[0].warm_pages, 3);
        assert_eq!(infos[0].estimated_memory_bytes, 132_800);
    }

    // -----------------------------------------------------------------------
    // LabRuntime observation loop tests (wa-1m7nk)
    //
    // These tests exercise runtime channel dispatch, shutdown propagation,
    // RwLock contention, and backpressure under deterministic virtual time
    // provided by asupersync::LabRuntime.
    // -----------------------------------------------------------------------

    mod labruntime_observation {
        use super::*;
        use std::sync::atomic::{AtomicU64, AtomicUsize};

        /// Helper: build a LabRuntime, create a root region and a task, run to
        /// quiescence via auto-advance. Asserts termination is clean.
        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(seed)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    f().await;
                })
                .expect("spawn lab task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(
                !matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ),
                "LabRuntime should not get stuck; termination: {:?}",
                report.termination,
            );
        }

        /// 1. Event loop channel dispatch: send events through mpsc,
        ///    verify the receiver processes them in order.
        #[test]
        fn channel_dispatch_under_labruntime() {
            run_lab(101, || async move {
                let (tx, mut rx) = crate::runtime_compat::mpsc::channel::<u64>(16);
                let cx = asupersync::Cx::current().expect("lab Cx");

                tx.send(&cx, 42).await.expect("send");
                tx.send(&cx, 99).await.expect("send");

                let v1 = rx.recv(&cx).await.expect("recv");
                let v2 = rx.recv(&cx).await.expect("recv");
                assert_eq!(v1, 42);
                assert_eq!(v2, 99);
            });
        }

        /// 2. Multi-channel concurrent dispatch: events on multiple channels
        ///    are all handled.
        #[test]
        fn multi_channel_dispatch_under_labruntime() {
            run_lab(102, || async move {
                let (tx_a, mut rx_a) = crate::runtime_compat::mpsc::channel::<&str>(8);
                let (tx_b, mut rx_b) = crate::runtime_compat::mpsc::channel::<u32>(8);
                let cx = asupersync::Cx::current().expect("lab Cx");

                tx_a.send(&cx, "hello").await.expect("send a");
                tx_b.send(&cx, 7).await.expect("send b");

                let a = rx_a.recv(&cx).await.expect("recv a");
                let b = rx_b.recv(&cx).await.expect("recv b");
                assert_eq!(a, "hello");
                assert_eq!(b, 7);
            });
        }

        /// 3. Adaptive sleep interval: verify virtual time advances correctly
        ///    during the discovery task's short-burst sleep pattern.
        #[test]
        fn adaptive_sleep_advances_virtual_time() {
            let iterations = Arc::new(AtomicU64::new(0));
            let iterations_task = Arc::clone(&iterations);

            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(103)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(100_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    let cx = asupersync::Cx::current().expect("lab Cx");
                    // Simulate the discovery loop's 100ms short-burst sleep pattern
                    for _ in 0..5 {
                        let _ =
                            crate::runtime_compat::sleep_with_cx(&cx, Duration::from_millis(100))
                                .await;
                        iterations_task.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .expect("spawn task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert_eq!(iterations.load(Ordering::SeqCst), 5);
            // Virtual time should have advanced by at least 500ms
            assert!(
                runtime.now() >= asupersync::Time::from_millis(500),
                "virtual time should advance to at least 500ms, got {:?}; termination: {:?}",
                runtime.now(),
                report.termination,
            );
        }

        /// 4. Shutdown propagation: setting AtomicBool flag causes task to exit.
        #[test]
        fn shutdown_flag_propagation_under_labruntime() {
            let exited = Arc::new(AtomicBool::new(false));
            let exited_task = Arc::clone(&exited);

            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(104)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(100_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);

            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let shutdown_flag_loop = Arc::clone(&shutdown_flag);
            let shutdown_flag_trigger = Arc::clone(&shutdown_flag);

            // Task that loops until shutdown, simulating observation loop
            let (t1, _h1) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    let cx = asupersync::Cx::current().expect("lab Cx");
                    let mut ticks = 0u32;
                    loop {
                        if shutdown_flag_loop.load(Ordering::SeqCst) {
                            break;
                        }
                        let _ =
                            crate::runtime_compat::sleep_with_cx(&cx, Duration::from_millis(10))
                                .await;
                        ticks += 1;
                        assert!(ticks <= 1000, "loop did not terminate via shutdown flag");
                    }
                    exited_task.store(true, Ordering::SeqCst);
                })
                .expect("spawn loop task");
            runtime.scheduler.lock().schedule(t1, 0);

            // Task that fires shutdown after 50ms
            let (t2, _h2) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    let cx = asupersync::Cx::current().expect("lab Cx");
                    let _ =
                        crate::runtime_compat::sleep_with_cx(&cx, Duration::from_millis(50)).await;
                    shutdown_flag_trigger.store(true, Ordering::SeqCst);
                })
                .expect("spawn trigger task");
            runtime.scheduler.lock().schedule(t2, 0);

            let _report = runtime.run_with_auto_advance();
            assert!(
                exited.load(Ordering::SeqCst),
                "observation loop should exit after shutdown flag set"
            );
        }

        /// 5. RwLock concurrent readers + writer: verify no deadlocks under
        ///    deterministic scheduling.
        #[test]
        fn rwlock_contention_no_deadlock_under_labruntime() {
            let total_reads = Arc::new(AtomicUsize::new(0));
            let total_writes = Arc::new(AtomicUsize::new(0));
            let reads_clone = Arc::clone(&total_reads);
            let writes_clone = Arc::clone(&total_writes);

            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(105)
                    .with_auto_advance()
                    .worker_count(4)
                    .max_steps(100_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);

            let lock = Arc::new(crate::runtime_compat::RwLock::new(0u64));

            // Spawn 3 reader tasks
            for i in 0..3u32 {
                let lock = Arc::clone(&lock);
                let reads = Arc::clone(&reads_clone);
                let (tid, _h) = runtime
                    .state
                    .create_task(region, asupersync::Budget::INFINITE, async move {
                        let cx = asupersync::Cx::current().expect("lab Cx");
                        for _ in 0..5 {
                            let val = *lock.read().await;
                            let _ = val;
                            reads.fetch_add(1, Ordering::SeqCst);
                            let _ =
                                crate::runtime_compat::sleep_with_cx(&cx, Duration::from_millis(5))
                                    .await;
                        }
                        let _ = i;
                    })
                    .expect("spawn reader");
                runtime.scheduler.lock().schedule(tid, 0);
            }

            // Spawn 1 writer task
            {
                let lock = Arc::clone(&lock);
                let writes = Arc::clone(&writes_clone);
                let (tid, _h) = runtime
                    .state
                    .create_task(region, asupersync::Budget::INFINITE, async move {
                        let cx = asupersync::Cx::current().expect("lab Cx");
                        for _ in 0..5 {
                            let mut guard = lock.write().await;
                            *guard += 1;
                            writes.fetch_add(1, Ordering::SeqCst);
                            drop(guard);
                            let _ = crate::runtime_compat::sleep_with_cx(
                                &cx,
                                Duration::from_millis(10),
                            )
                            .await;
                        }
                    })
                    .expect("spawn writer");
                runtime.scheduler.lock().schedule(tid, 0);
            }

            let _report = runtime.run_with_auto_advance();
            assert_eq!(total_reads.load(Ordering::SeqCst), 15); // 3 readers * 5 reads
            assert_eq!(total_writes.load(Ordering::SeqCst), 5); // 1 writer * 5 writes
        }

        /// 6. Backpressure: bounded channel send/recv ordering is correct.
        #[test]
        fn backpressure_bounded_channel_under_labruntime() {
            run_lab(106, || async move {
                let cx = asupersync::Cx::current().expect("lab Cx");
                let (tx, mut rx) = crate::runtime_compat::mpsc::channel::<u32>(4);

                // Fill the channel to capacity
                for i in 0..4 {
                    tx.send(&cx, i).await.expect("send within capacity");
                }

                // Drain all and verify FIFO ordering
                for expected in 0..4 {
                    let v = rx.recv(&cx).await.expect("recv");
                    assert_eq!(v, expected, "channel should preserve FIFO order");
                }

                // After draining, sending again should work
                tx.send(&cx, 99).await.expect("send after drain");
                let v = rx.recv(&cx).await.expect("recv after re-send");
                assert_eq!(v, 99);
            });
        }

        /// 7. Watch channel under LabRuntime: verify send/recv/borrow work.
        #[test]
        fn watch_channel_under_labruntime() {
            run_lab(107, || async move {
                let (tx, rx) = crate::runtime_compat::watch::channel(0u64);

                // Initial value visible
                assert_eq!(*rx.borrow(), 0);

                // Send updates
                tx.send(42).expect("watch send");
                assert_eq!(*rx.borrow(), 42);

                tx.send(100).expect("watch send");
                assert_eq!(*rx.borrow(), 100);
            });
        }

        /// 8. SPSC ring buffer relay pattern: verify the mpsc→spsc relay
        ///    pattern that the capture relay task uses.
        #[test]
        fn relay_mpsc_to_spsc_under_labruntime() {
            run_lab(108, || async move {
                let cx = asupersync::Cx::current().expect("lab Cx");
                let (mpsc_tx, mut mpsc_rx) = crate::runtime_compat::mpsc::channel::<String>(16);
                let (spsc_tx, spsc_rx) = crate::spsc_ring_buffer::channel::<String>(16);

                // Simulate producer sending events
                mpsc_tx
                    .send(&cx, "event_a".to_string())
                    .await
                    .expect("send a");
                mpsc_tx
                    .send(&cx, "event_b".to_string())
                    .await
                    .expect("send b");

                // Simulate relay: read from mpsc, write to spsc
                let a = mpsc_rx.recv(&cx).await.expect("mpsc recv a");
                spsc_tx.send(a).await.expect("spsc send a");

                let b = mpsc_rx.recv(&cx).await.expect("mpsc recv b");
                spsc_tx.send(b).await.expect("spsc send b");

                // Consumer reads from spsc
                let out_a = spsc_rx.recv().await.expect("spsc recv a");
                let out_b = spsc_rx.recv().await.expect("spsc recv b");
                assert_eq!(out_a, "event_a");
                assert_eq!(out_b, "event_b");
            });
        }

        /// 9. Heartbeat registry recording under virtual time.
        #[test]
        fn heartbeat_recording_under_labruntime() {
            run_lab(109, || async move {
                let heartbeats = HeartbeatRegistry::new();

                // Record some heartbeats
                heartbeats.record_discovery();
                heartbeats.record_persistence();
                heartbeats.record_discovery();

                // Verify health check runs without panic
                let report = heartbeats.check_health(&crate::watchdog::WatchdogConfig::default());
                // The report should exist and not panic — unhealthy is expected
                // since heartbeats just started. We just verify the API works.
                let _unhealthy = report.unhealthy_components();
            });
        }

        /// 10. Timeout wrapping recv: simulates the relay task's
        ///     timeout(25ms, recv_event) pattern.
        #[test]
        fn timeout_recv_pattern_under_labruntime() {
            let timed_out_count = Arc::new(AtomicU64::new(0));
            let received_count = Arc::new(AtomicU64::new(0));
            let timed_out_task = Arc::clone(&timed_out_count);
            let received_task = Arc::clone(&received_count);

            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(110)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(100_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);

            let (tx, mut rx) = asupersync::channel::mpsc::channel::<u32>(16);
            let tx_send = tx.clone();

            // Consumer: timeout-based recv loop using timeout_with_cx
            let (t1, _h1) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    let cx = asupersync::Cx::current().expect("lab Cx");
                    for _ in 0..4 {
                        let recv_fut = rx.recv(&cx);
                        match crate::runtime_compat::timeout_with_cx(
                            &cx,
                            Duration::from_millis(25),
                            recv_fut,
                        )
                        .await
                        {
                            Ok(Ok(val)) => {
                                let _ = val;
                                received_task.fetch_add(1, Ordering::SeqCst);
                            }
                            _ => {
                                timed_out_task.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                })
                .expect("spawn consumer");
            runtime.scheduler.lock().schedule(t1, 0);

            // Producer: send 2 events with delays, causing some timeouts
            let (t2, _h2) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    let cx = asupersync::Cx::current().expect("lab Cx");
                    // Send immediately (consumer should receive)
                    tx_send.send(&cx, 1).await.expect("send 1");
                    // Wait longer than timeout (consumer should timeout once)
                    let _ =
                        crate::runtime_compat::sleep_with_cx(&cx, Duration::from_millis(50)).await;
                    tx_send.send(&cx, 2).await.expect("send 2");
                })
                .expect("spawn producer");
            runtime.scheduler.lock().schedule(t2, 0);

            let _report = runtime.run_with_auto_advance();

            let received = received_count.load(Ordering::SeqCst);
            let timed_out = timed_out_count.load(Ordering::SeqCst);
            assert!(
                received >= 1,
                "should receive at least 1 event, got {received}"
            );
            assert!(
                timed_out >= 1,
                "should timeout at least once, got {timed_out}"
            );
            assert_eq!(received + timed_out, 4, "total iterations should be 4");
        }
    }
}
