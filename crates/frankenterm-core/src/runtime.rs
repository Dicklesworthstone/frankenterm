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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};

use crate::Error;
use crate::sharded_counter::{ShardedCounter, ShardedGauge, ShardedMax};

use tracing::{debug, error, info, instrument, warn};

use crate::backpressure::{
    BackpressureConfig, BackpressureManager, BackpressureMetrics, QueueDepths,
};
use crate::config::{
    CaptureBudgetConfig, CompiledRetentionPolicy, HotReloadableConfig, PaneFilterConfig,
    PanePriorityConfig, PatternsConfig, SnapshotConfig, SnapshotSchedulingMode, StorageConfig,
};
use crate::capture_authority::{
    ActivePaneIdentity, CaptureAuthority, CaptureLease, CapturePersistenceGuard, CaptureRevision,
    CaptureSourceKind, CaptureViewEpoch, PaneIncarnation,
};
#[cfg(feature = "native-wezterm")]
use crate::capture_authority::CaptureProducerGuard;
use crate::connector_inbound_bridge::{
    BridgeRouteResult, ConnectorBridgeError, ConnectorInboundBridge, ConnectorInboundBridgeConfig,
    ConnectorSignal,
};
use crate::connector_outbound_bridge::{
    ConnectorAction, ConnectorOutboundBridge, ConnectorOutboundBridgeConfig, OutboundEvent,
    OutboundRoutingRule,
};
use crate::connector_reliability::ConnectorErrorKind;
use crate::crash::{
    HealthSnapshot, LeakRiskInventorySnapshot, LeakRiskWatchdogComponentSnapshot,
    LeakRiskWatchdogSnapshot, ShutdownSummary,
};
use crate::error::{Result, RuntimeOperationSource};
use crate::events::{Event, EventBus, UserVarPayload, event_identity_key};
use crate::fleet_memory_controller::{FleetMemoryConfig, PaneScrollbackInfo};
use crate::fleet_scrollback_coordinator::{
    CoordinatorConfig, FleetScrollbackCoordinator, SnapshotPaneScrollbackAccess,
};
#[cfg(test)]
use crate::gc::{CacheCompactionStats, compact_u64_map};
use crate::gc::{CacheGcSettings, should_vacuum};
use crate::ingest::{
    CapturedSegment, CapturedSegmentKind, PaneCursor, PaneRegistry, PersistedCapture,
    bounded_segment_for_persistence, persist_authorized_captured_segment_with_zone_with_cx,
};
use crate::lru_cache::LruCache;
use crate::memory_budget::{BudgetLevel, MemoryBudgetConfig};
use crate::memory_pressure::{MemoryPressureConfig, MemoryPressureMonitor, MemoryPressureTier};
#[cfg(feature = "native-wezterm")]
use crate::native_events::{NativeEvent, NativeEventListener};
use crate::patterns::{AgentType, Detection, DetectionContext, PatternEngine, Severity};
use crate::policy::Redactor;
use crate::recording::RecordingManager;
use crate::resize_scheduler::{ResizeSchedulerDebugSnapshot, ResizeStalledTransaction};
use crate::runtime_async::{RuntimeTime, RwLock, mpsc, task::JoinHandle, watch};
use crate::runtime_async::task::{JoinErrorKind, JoinSet, JoinSetSettlement};
use crate::scrollback_tiers::ScrollbackTierSnapshot;
#[cfg(all(feature = "vendored", unix))]
use crate::sharding::{ShardId, try_decode_sharded_pane_id};
use crate::spsc_ring_buffer::{SpscConsumer, SpscProducer, channel as spsc_channel};
#[cfg(feature = "native-wezterm")]
use crate::storage::PaneRecord;
use crate::storage::{MaintenanceRecord, SizeEvictionOutcome, StorageHandle, StoredEvent};
#[cfg(all(feature = "vendored", unix))]
use crate::tailer::StreamingBridge;
use crate::tailer::{
    CaptureEvent, CaptureResyncDecision, CaptureResyncReceipt, TailerConfig, TailerPollTaskSet,
    TailerSupervisor,
};
#[cfg(all(feature = "vendored", unix))]
use crate::vendored::subscribe_pane_output_with_inherited_cx;
#[cfg(all(feature = "vendored", unix))]
use crate::vendored::{DirectMuxClient, DirectMuxClientConfig, PaneDelta, SubscriptionConfig};
use crate::watchdog::HeartbeatRegistry;
use crate::wezterm::{
    MuxSemanticSnapshot, MuxSemanticZoneKind, PaneInfo, PaneTextSource,
    PaneTieredScrollbackSummary, WeztermHandle, WeztermHandleSource, WeztermInterface,
    wezterm_handle_with_timeout,
};

fn runtime_cx_error(
    operation: &'static str,
    cx: &crate::cx::Cx,
    fallback: &'static str,
) -> Error {
    use crate::outcome::CancelKind;

    match cx.root_cancel_cause().map(|reason| reason.kind) {
        Some(CancelKind::Deadline | CancelKind::Timeout) => {
            Error::runtime_backend(operation, "capability deadline exceeded")
        }
        Some(CancelKind::PollQuota) => {
            Error::runtime_backend(operation, "capability poll quota exhausted")
        }
        Some(CancelKind::CostBudget) => {
            Error::runtime_backend(operation, "capability cost budget exhausted")
        }
        Some(
            CancelKind::User
            | CancelKind::FailFast
            | CancelKind::RaceLost
            | CancelKind::ParentCancelled
            | CancelKind::ResourceUnavailable
            | CancelKind::Shutdown
            | CancelKind::LinkedExit,
        ) => Error::runtime_cancelled(operation, "capability context cancelled"),
        None => Error::runtime_backend(operation, fallback),
    }
}

fn runtime_backend_error(operation: &'static str, err: impl std::fmt::Display) -> Error {
    Error::RuntimeOperation {
        operation,
        source: RuntimeOperationSource::Backend(err.to_string()),
    }
}

fn is_runtime_cancellation(error: &Error) -> bool {
    matches!(
        error,
        Error::Cancelled(_)
            | Error::RuntimeOperation {
                source: RuntimeOperationSource::Cancelled(_),
                ..
            }
    )
}

const BOCPD_CHANGE_POINT_RULE_ID: &str = "core.bocpd:change_point";
const BOCPD_CHANGE_POINT_EVENT_TYPE: &str = "bocpd.change_point";

fn config_update_pending(rx: &watch::Receiver<HotReloadableConfig>) -> bool {
    rx.has_changed()
}

type RuntimeLoopCx = crate::cx::Cx;
fn runtime_loop_cx() -> RuntimeLoopCx {
    crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request)
}

fn runtime_deadline_after(now: Instant, duration: Duration, label: &str) -> Instant {
    now.checked_add(duration).unwrap_or_else(|| {
        warn!("{label} duration {duration:?} is too large for Instant; clamping deadline");
        now.checked_add(Duration::from_secs(365 * 24 * 60 * 60))
            .unwrap_or(now)
    })
}

const RETENTION_MAINTENANCE_CADENCE: Duration = Duration::from_secs(60 * 60);
const RETENTION_MAINTENANCE_RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_RUNTIME_WATCHDOG_WARNINGS: usize = 32;
const MAX_RUNTIME_WATCHDOG_WARNING_INPUT_BYTES: usize = 64 * 1024;
const MAX_RUNTIME_WATCHDOG_WARNING_WIDTH: usize = 256;
const MAX_RUNTIME_WATCHDOG_WARNING_BYTES: usize = 1024;

static RUNTIME_HEALTH_REDACTOR: std::sync::LazyLock<Redactor> =
    std::sync::LazyLock::new(Redactor::new);

fn utf8_prefix_at_most(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Admit backend-provided watchdog diagnostics into the crash-health snapshot
/// through finite count, input, display-width, byte, terminal-control, and
/// secret-redaction bounds. The mux/backend boundary is not trusted to return
/// small or content-free strings.
fn append_bounded_watchdog_warnings(target: &mut Vec<String>, source: Vec<String>) {
    let source_len = source.len();
    let retained = if source_len > MAX_RUNTIME_WATCHDOG_WARNINGS {
        MAX_RUNTIME_WATCHDOG_WARNINGS.saturating_sub(1)
    } else {
        MAX_RUNTIME_WATCHDOG_WARNINGS
    };

    target.extend(source.into_iter().take(retained).map(|warning| {
        let bounded_input =
            utf8_prefix_at_most(&warning, MAX_RUNTIME_WATCHDOG_WARNING_INPUT_BYTES);
        crate::output::sanitize_redact_truncate_bounded(
            bounded_input,
            MAX_RUNTIME_WATCHDOG_WARNING_WIDTH,
            MAX_RUNTIME_WATCHDOG_WARNING_BYTES,
            |normalized| RUNTIME_HEALTH_REDACTOR.redact(normalized),
        )
    }));

    if source_len > MAX_RUNTIME_WATCHDOG_WARNINGS {
        target.push(format!(
            "{} additional mux watchdog warnings omitted",
            source_len.saturating_sub(retained)
        ));
    }
}

#[derive(Debug)]
struct RetentionMaintenanceSchedule {
    due: bool,
    last_success: Instant,
    last_attempt: Option<Instant>,
}

impl RetentionMaintenanceSchedule {
    fn new(now: Instant) -> Self {
        Self {
            // Startup configuration must take effect without waiting an hour.
            due: true,
            last_success: now,
            last_attempt: None,
        }
    }

    fn mark_due(&mut self) {
        self.due = true;
        // A new operator policy supersedes any retry delay from an older
        // failed attempt and is eligible on this maintenance turn.
        self.last_attempt = None;
    }

    fn should_attempt(&self, now: Instant) -> bool {
        if self.due {
            return self.last_attempt.is_none_or(|last_attempt| {
                now.saturating_duration_since(last_attempt)
                    >= RETENTION_MAINTENANCE_RETRY_DELAY
            });
        }
        now.saturating_duration_since(self.last_success) >= RETENTION_MAINTENANCE_CADENCE
    }

    fn finish_attempt(&mut self, now: Instant, succeeded: bool) {
        self.last_attempt = Some(now);
        if succeeded {
            self.due = false;
            self.last_success = now;
        } else {
            // Failed or cancelled cleanup is still due, but should not spin:
            // `should_attempt` enforces the bounded retry delay above.
            self.due = true;
        }
    }
}

/// Completion-based cadence shared by periodic maintenance lanes whose
/// success and failure both receive the full configured throttle interval.
#[derive(Debug, Clone, Copy)]
struct CompletionTimedSchedule {
    last_completion: Instant,
}

impl CompletionTimedSchedule {
    const fn new(last_completion: Instant) -> Self {
        Self { last_completion }
    }

    fn should_run(self, now: Instant, interval: Duration) -> bool {
        !interval.is_zero()
            && now.saturating_duration_since(self.last_completion) >= interval
    }

    fn finish(&mut self, completion: Instant) {
        self.last_completion = completion;
    }
}

/// Last fleet-coordinator state durably represented in the maintenance log.
///
/// The live health snapshot still updates on every health cycle.  This compact
/// signature exists solely to keep the audit table off the 30-second hot path:
/// a normal no-op tick is not a new maintenance event, while pressure-state
/// transitions, telemetry-coverage transitions, and actual reclamation work
/// remain durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FleetCoordinatorMaintenanceState {
    pressure: crate::fleet_memory_controller::FleetPressureTier,
    telemetry_blind: bool,
    telemetry_partial: bool,
    recommended_actions: usize,
}

fn fleet_coordinator_maintenance_is_noteworthy(
    previous: Option<FleetCoordinatorMaintenanceState>,
    current: FleetCoordinatorMaintenanceState,
    pages_evicted: u64,
    bytes_reclaimed: u64,
    targets_applied: usize,
) -> bool {
    previous != Some(current)
        || pages_evicted > 0
        || bytes_reclaimed > 0
        || targets_applied > 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeRetentionReceiptStatus {
    Completed,
    InterruptedPartial,
}

impl SizeRetentionReceiptStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::InterruptedPartial => "interrupted_partial",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingSizeRetentionReceipt {
    outcome: SizeEvictionOutcome,
    status: SizeRetentionReceiptStatus,
    retention_max_mb: u32,
    attempt_timestamp: i64,
}

fn size_retention_receipt_record(pending: PendingSizeRetentionReceipt) -> MaintenanceRecord {
    let attempt_status = pending.status.as_str();
    let metadata = serde_json::json!({
        "schema": "size_retention_receipt.v1",
        "attempt_status": attempt_status,
        "retention_max_mb": pending.retention_max_mb,
        "deleted_segments": pending.outcome.deleted_segments,
        "used_bytes_before": pending.outcome.used_bytes_before,
        "used_bytes_after": pending.outcome.used_bytes_after,
        "over_limit_after": pending.outcome.over_limit_after,
    })
    .to_string();
    MaintenanceRecord {
        id: 0,
        event_type: "size_retention".to_string(),
        message: Some(format!(
            "Size retention {attempt_status} after {} durable segment deletions",
            pending.outcome.deleted_segments
        )),
        metadata: Some(metadata),
        timestamp: pending.attempt_timestamp,
    }
}

async fn record_size_retention_receipt(
    storage: &StorageHandle,
    pending: PendingSizeRetentionReceipt,
) -> Result<i64> {
    let receipt_cx = crate::cx::Cx::for_request_with_budget(crate::cx::Budget::MINIMAL);
    storage
        .record_maintenance_with_cx(
            &receipt_cx,
            size_retention_receipt_record(pending),
        )
        .await
}

async fn persist_captured_segment_for_runtime(
    runtime_cx: &RuntimeLoopCx,
    storage: &StorageHandle,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
    zone_type: Option<&str>,
    persistence_guard: &CapturePersistenceGuard,
) -> Result<PersistedCapture> {
    persist_authorized_captured_segment_with_zone_with_cx(
        runtime_cx,
        storage,
        captured,
        max_segment_bytes,
        zone_type,
        persistence_guard,
    )
    .await
}

fn record_authorized_replay_egress(
    adapter: &crate::replay_capture::CaptureAdapter,
    captured: &CapturedSegment,
    durable_sequence: u64,
    persistence_guard: &CapturePersistenceGuard,
) -> std::result::Result<(), crate::replay_capture::ReplayCaptureSequenceError> {
    debug_assert_eq!(
        captured.pane_id,
        persistence_guard.stamp().global_pane_id(),
        "capture authority was validated before replay egress"
    );
    let (segment_kind, is_gap) = crate::recording::captured_kind_to_segment(&captured.kind);
    let gap_reason = match &captured.kind {
        CapturedSegmentKind::Gap { reason } => Some(reason.clone()),
        CapturedSegmentKind::Delta => None,
    };
    let event = crate::recording::EgressEvent {
        pane_id: captured.pane_id,
        text: captured.content.clone(),
        segment_kind,
        is_gap,
        gap_reason,
        encoding: crate::recording::RecorderTextEncoding::Utf8,
        redaction: crate::recording::RecorderRedactionLevel::None,
        occurred_at_ms: u64::try_from(captured.captured_at).unwrap_or(0),
        sequence: durable_sequence,
    };
    adapter.capture_egress_event(&event)
}

#[derive(Debug, Clone)]
struct CachedSemanticZoneSnapshot {
    refreshed_at: Instant,
    snapshot: Option<MuxSemanticSnapshot>,
}

type CapturePaneCacheKey = (u64, PaneIncarnation);

async fn semantic_zone_type_for_captured_segment(
    runtime_cx: &RuntimeLoopCx,
    wezterm_handle: &WeztermHandle,
    cache: &mut HashMap<CapturePaneCacheKey, CachedSemanticZoneSnapshot>,
    cache_ttl: Duration,
    captured: &CapturedSegment,
    pane_incarnation: PaneIncarnation,
    allow_live_lookup: bool,
) -> Option<String> {
    if !matches!(&captured.kind, CapturedSegmentKind::Delta) || captured.content.trim().is_empty() {
        return None;
    }

    let now = Instant::now();
    let cache_key = (captured.pane_id, pane_incarnation);
    if let Some(cached) = cache.get(&cache_key)
        && now.saturating_duration_since(cached.refreshed_at) <= cache_ttl
    {
        return cached
            .snapshot
            .as_ref()
            .and_then(|snapshot| infer_semantic_zone_type_for_segment(captured, snapshot))
            .map(str::to_string);
    }

    // Once discovery has published different metadata for a numeric pane ID,
    // a live mux query can only describe the successor. Existing snapshots are
    // incarnation-keyed and safe to reuse, but a cache miss must fail closed.
    if !allow_live_lookup {
        return None;
    }

    let snapshot = match wezterm_handle
        .get_semantic_zones_with_cx(runtime_cx, captured.pane_id)
        .await
    {
        Ok(snapshot) => Some(snapshot),
        Err(_error) => {
            debug!(
                pane_id = captured.pane_id,
                error_class = "semantic_zone_snapshot_unavailable",
                "semantic zone snapshot unavailable while stamping captured segment"
            );
            None
        }
    };
    cache.insert(
        cache_key,
        CachedSemanticZoneSnapshot {
            refreshed_at: now,
            snapshot: snapshot.clone(),
        },
    );
    snapshot
        .as_ref()
        .and_then(|snapshot| infer_semantic_zone_type_for_segment(captured, snapshot))
        .map(str::to_string)
}

fn infer_semantic_zone_type_for_segment(
    captured: &CapturedSegment,
    snapshot: &MuxSemanticSnapshot,
) -> Option<&'static str> {
    if !matches!(&captured.kind, CapturedSegmentKind::Delta) {
        return None;
    }
    let content = captured.content.trim();
    if content.is_empty() {
        return None;
    }

    snapshot
        .zones
        .iter()
        .filter(|zone| !zone.text.trim().is_empty())
        .filter(|zone| {
            let zone_text = zone.text.trim();
            content.contains(zone_text) || zone_text.contains(content)
        })
        .max_by_key(|zone| (zone.start_y, zone.start_x))
        .map(|zone| semantic_zone_type_label(zone.semantic_type))
        .or_else(|| {
            snapshot
                .zones
                .iter()
                .filter(|zone| zone.semantic_type == MuxSemanticZoneKind::Output)
                .max_by_key(|zone| (zone.start_y, zone.start_x))
                .map(|zone| semantic_zone_type_label(zone.semantic_type))
        })
}

const fn semantic_zone_type_label(kind: MuxSemanticZoneKind) -> &'static str {
    match kind {
        MuxSemanticZoneKind::Prompt => "prompt",
        MuxSemanticZoneKind::Input => "input",
        MuxSemanticZoneKind::Output => "output",
    }
}

#[allow(clippy::needless_pass_by_ref_mut)] // update-taking watch APIs require &mut here
fn config_take_update(rx: &mut watch::Receiver<HotReloadableConfig>) -> HotReloadableConfig {
    rx.borrow_and_clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeWaitFailureKind {
    ContextCancelled,
    DeadlineExceeded,
    PollQuotaExhausted,
    CostBudgetExhausted,
    ContextFailure,
    TimerFailure,
}

impl RuntimeWaitFailureKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ContextCancelled => "context_cancelled",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::PollQuotaExhausted => "poll_quota_exhausted",
            Self::CostBudgetExhausted => "cost_budget_exhausted",
            Self::ContextFailure => "context_failure",
            Self::TimerFailure => "timer_failure",
        }
    }
}

static RUNTIME_WAIT_FAILURES_TOTAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn increment_saturating_atomic(counter: &std::sync::atomic::AtomicU64) -> u64 {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(1);
        match counter.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

fn runtime_cancel_or_budget_failure_kind(
    runtime_cx: &RuntimeLoopCx,
) -> Option<RuntimeWaitFailureKind> {
    use crate::outcome::CancelKind;

    let root_failure = match runtime_cx.root_cancel_cause().map(|reason| reason.kind) {
        Some(CancelKind::Deadline | CancelKind::Timeout) => {
            Some(RuntimeWaitFailureKind::DeadlineExceeded)
        }
        Some(CancelKind::PollQuota) => Some(RuntimeWaitFailureKind::PollQuotaExhausted),
        Some(CancelKind::CostBudget) => Some(RuntimeWaitFailureKind::CostBudgetExhausted),
        Some(
            CancelKind::User
            | CancelKind::FailFast
            | CancelKind::RaceLost
            | CancelKind::ParentCancelled
            | CancelKind::ResourceUnavailable
            | CancelKind::Shutdown
            | CancelKind::LinkedExit,
        ) => Some(RuntimeWaitFailureKind::ContextCancelled),
        None => None,
    };
    if root_failure.is_some() {
        return root_failure;
    }

    // A timer may be the first observer of budget exhaustion, before the Cx
    // materializes a root cancellation cause. Preserve the finite class from
    // the content-free accounting snapshot at every runtime wait boundary.
    let budget = runtime_cx.budget_stats();
    if budget.deadline.at.is_some() && budget.deadline.remaining.is_none() {
        Some(RuntimeWaitFailureKind::DeadlineExceeded)
    } else if budget.polls.remaining == Some(0) {
        Some(RuntimeWaitFailureKind::PollQuotaExhausted)
    } else if budget.cost.remaining == Some(0) {
        Some(RuntimeWaitFailureKind::CostBudgetExhausted)
    } else {
        None
    }
}

fn runtime_context_failure_kind(runtime_cx: &RuntimeLoopCx) -> RuntimeWaitFailureKind {
    runtime_cancel_or_budget_failure_kind(runtime_cx)
        .unwrap_or(RuntimeWaitFailureKind::ContextFailure)
}

fn runtime_context_error_kind(
    runtime_cx: &RuntimeLoopCx,
    error: &crate::runtime_async::ContextError,
) -> RuntimeWaitFailureKind {
    use crate::runtime_async::ContextErrorKind;

    match error.kind() {
        ContextErrorKind::DeadlineExceeded => RuntimeWaitFailureKind::DeadlineExceeded,
        ContextErrorKind::PollQuotaExhausted => RuntimeWaitFailureKind::PollQuotaExhausted,
        ContextErrorKind::CostQuotaExhausted => RuntimeWaitFailureKind::CostBudgetExhausted,
        ContextErrorKind::Cancelled => runtime_cancel_or_budget_failure_kind(runtime_cx)
            .unwrap_or(RuntimeWaitFailureKind::ContextCancelled),
        ContextErrorKind::CancelTimeout => RuntimeWaitFailureKind::ContextFailure,
        _ => RuntimeWaitFailureKind::ContextFailure,
    }
}

fn runtime_checkpoint_failure(
    runtime_cx: &RuntimeLoopCx,
) -> Option<RuntimeWaitFailureKind> {
    match runtime_cx.checkpoint() {
        Err(error) => Some(runtime_context_error_kind(runtime_cx, &error)),
        Ok(()) => runtime_cancel_or_budget_failure_kind(runtime_cx),
    }
}

fn record_runtime_wait_failure(operation: &'static str, failure: RuntimeWaitFailureKind) {
    let failures_total = increment_saturating_atomic(&RUNTIME_WAIT_FAILURES_TOTAL);
    warn!(
        event = "runtime_wait_failed",
        operation,
        failure_class = failure.as_str(),
        failures_total,
        "runtime wait stopped without exposing backend or cancellation text"
    );
}

async fn runtime_sleep(
    runtime_cx: &RuntimeLoopCx,
    duration: Duration,
) -> std::result::Result<(), RuntimeWaitFailureKind> {
    if crate::runtime_async::sleep_with_cx(runtime_cx, duration)
        .await
        .is_err()
    {
        return Err(
            runtime_checkpoint_failure(runtime_cx)
                .unwrap_or(RuntimeWaitFailureKind::TimerFailure),
        );
    }
    runtime_checkpoint_failure(runtime_cx).map_or(Ok(()), Err)
}

const SHUTDOWN_AWARE_SLEEP_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn runtime_time_elapsed_at_least(
    now: RuntimeTime,
    since: RuntimeTime,
    duration: Duration,
) -> bool {
    u128::from(now.duration_since(since)) >= duration.as_nanos()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownAwareSleepOutcome {
    Elapsed,
    ShutdownRequested,
    WaitFailed(RuntimeWaitFailureKind),
}

async fn runtime_sleep_until_shutdown(
    runtime_cx: &RuntimeLoopCx,
    shutdown_flag: &AtomicBool,
    duration: Duration,
) -> ShutdownAwareSleepOutcome {
    let deadline = crate::runtime_async::timer_now_with_cx(runtime_cx) + duration;
    loop {
        if shutdown_flag.load(Ordering::SeqCst) {
            return ShutdownAwareSleepOutcome::ShutdownRequested;
        }

        let now = crate::runtime_async::timer_now_with_cx(runtime_cx);
        if now >= deadline {
            return ShutdownAwareSleepOutcome::Elapsed;
        }

        let remaining = Duration::from_nanos(deadline.duration_since(now));
        if let Err(failure) = runtime_sleep(
            runtime_cx,
            remaining.min(SHUTDOWN_AWARE_SLEEP_POLL_INTERVAL),
        )
        .await
        {
            return ShutdownAwareSleepOutcome::WaitFailed(failure);
        }
    }
}

async fn runtime_timeout<F>(
    runtime_cx: &RuntimeLoopCx,
    duration: Duration,
    future: F,
) -> std::result::Result<F::Output, RuntimeTimeoutFailure>
where
    F: Future,
{
    if let Some(failure) = runtime_checkpoint_failure(runtime_cx) {
        return Err(RuntimeTimeoutFailure::Context(failure));
    }

    match crate::runtime_async::timeout_with_cx_typed(runtime_cx, duration, future).await {
        Ok(output) => match runtime_checkpoint_failure(runtime_cx) {
            Some(failure) => Err(RuntimeTimeoutFailure::Context(failure)),
            None => Ok(output),
        },
        Err(crate::runtime_async::TimeoutError::Elapsed) => {
            match runtime_checkpoint_failure(runtime_cx) {
                Some(failure) => Err(RuntimeTimeoutFailure::Context(failure)),
                None => Err(RuntimeTimeoutFailure::Elapsed),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeTimeoutFailure {
    Elapsed,
    Context(RuntimeWaitFailureKind),
}

fn spawn_runtime_task<F, Fut, T>(runtime_cx: &RuntimeLoopCx, task_fn: F) -> JoinHandle<T>
where
    F: FnOnce(RuntimeLoopCx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    crate::runtime_async::task::spawn_with_cx(runtime_cx, task_fn)
}

const SNAPSHOT_SCHEDULER_RUNNING: u8 = 0;
const SNAPSHOT_SCHEDULER_SHUTDOWN_ACKNOWLEDGED: u8 = 1;
const SNAPSHOT_SCHEDULER_UNEXPECTED_RETURN: u8 = 2;
const SNAPSHOT_SCHEDULER_FAILED: u8 = 3;

const fn snapshot_scheduler_shutdown_acknowledged(status: u8) -> bool {
    status == SNAPSHOT_SCHEDULER_SHUTDOWN_ACKNOWLEDGED
}

/// RAII-guarded holder for the capture task's per-pane vendored streaming
/// subtasks.
///
/// The active map stores one generation- and route-bound `StreamingTask` per
/// observed pane while the capture loop uses the vendored mux-socket fast path.
/// Removed handles transfer into `settling`, so replacement remains nonblocking
/// without discarding terminal authority. The capture loop opportunistically
/// reaps completions and performs one bounded trusted drain on normal shutdown.
///
/// If the capture future itself is dropped mid-flight, `Drop` requests abort
/// for both collections and emits finite orphan-risk telemetry. Synchronous
/// drop cannot prove terminal acknowledgement; it is intentionally a last-resort
/// fallback rather than the clean-shutdown contract.
#[cfg(all(feature = "vendored", unix))]
struct StreamingTasks {
    tasks: HashMap<u64, StreamingTask>,
    settling: JoinSet<()>,
}

#[cfg(all(feature = "vendored", unix))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamingSubscriptionIdentity {
    global_pane_id: u64,
    local_pane_id: u64,
    socket_shard: ShardId,
    socket_path: PathBuf,
    generation: u32,
    capture_stamp: crate::capture_authority::CaptureStamp,
}

/// Identifies one spawned task for exit reconciliation only.
///
/// This token is deliberately separate from the immutable `CaptureStamp` that
/// every `CaptureEvent` carries through both shared queues.  Exit reconciliation
/// uses the task token; persistence fencing uses the pane-incarnation and
/// source-epoch stamp, so neither identity mechanism can stand in for the other.
#[cfg(all(feature = "vendored", unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamingTaskToken(u64);

#[cfg(all(feature = "vendored", unix))]
struct StreamingTask {
    identity: StreamingSubscriptionIdentity,
    lease: CaptureLease,
    token: StreamingTaskToken,
    handle: JoinHandle<()>,
}

#[cfg(all(feature = "vendored", unix))]
struct RetiredStreamingTask {
    identity: StreamingSubscriptionIdentity,
    lease: CaptureLease,
    token: StreamingTaskToken,
}

#[cfg(all(feature = "vendored", unix))]
const STREAMING_TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(all(feature = "vendored", unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingTaskDrainOutcome {
    Settled,
    SettledWithFailure {
        failure: JoinErrorKind,
    },
    TimedOut {
        active_tasks: usize,
        unacknowledged_tasks: usize,
    },
    Incomplete {
        active_tasks: usize,
        unacknowledged_tasks: usize,
        drain_failure: Option<JoinErrorKind>,
    },
}

#[cfg(all(feature = "vendored", unix))]
fn classify_streaming_task_drain(
    timed_out: bool,
    drain_failure: Option<JoinErrorKind>,
    terminal_failure: Option<JoinErrorKind>,
    settlement: JoinSetSettlement,
) -> StreamingTaskDrainOutcome {
    match settlement {
        JoinSetSettlement::Settled => terminal_failure.map_or(
            StreamingTaskDrainOutcome::Settled,
            |failure| StreamingTaskDrainOutcome::SettledWithFailure { failure },
        ),
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } if timed_out => StreamingTaskDrainOutcome::TimedOut {
            active_tasks,
            unacknowledged_tasks,
        },
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } => StreamingTaskDrainOutcome::Incomplete {
            active_tasks,
            unacknowledged_tasks,
            drain_failure,
        },
    }
}

#[cfg(all(feature = "vendored", unix))]
impl StreamingTask {
    fn matches_exit(&self, exit: &StreamingTaskExit) -> bool {
        streaming_exit_matches_active(&self.identity, self.token, exit)
    }
}

#[cfg(all(feature = "vendored", unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingTaskReconcileAction {
    Keep,
    Remove,
    Replace,
}

#[cfg(all(feature = "vendored", unix))]
// The socket route table is immutable for the lifetime of the capture task,
// and global pane id deterministically selects its shard-local route. Comparing
// global id plus PaneEntry generation therefore avoids cloning PathBuf routes
// for every active pane on every discovery tick.
fn streaming_task_reconcile_action(
    active: &StreamingSubscriptionIdentity,
    desired: Option<(u64, u32, crate::capture_authority::CaptureStamp)>,
) -> StreamingTaskReconcileAction {
    match desired {
        Some((global_pane_id, generation, capture_stamp)) => {
            if global_pane_id == active.global_pane_id
                && generation == active.generation
                && capture_stamp == active.capture_stamp
            {
                StreamingTaskReconcileAction::Keep
            } else {
                StreamingTaskReconcileAction::Replace
            }
        }
        None => StreamingTaskReconcileAction::Remove,
    }
}

#[cfg(all(feature = "vendored", unix))]
fn streaming_exit_matches_active(
    active_identity: &StreamingSubscriptionIdentity,
    active_token: StreamingTaskToken,
    exit: &StreamingTaskExit,
) -> bool {
    active_token == exit.token && active_identity == &exit.identity
}

#[cfg(all(feature = "vendored", unix))]
impl StreamingTasks {
    fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            settling: JoinSet::new(),
        }
    }

    fn remove_for_settlement(
        &mut self,
        pane_id: u64,
        abort: bool,
    ) -> Option<RetiredStreamingTask> {
        let task = self.tasks.remove(&pane_id)?;
        let StreamingTask {
            identity,
            lease,
            token,
            handle,
        } = task;
        if abort {
            handle.abort();
        }
        self.settling.insert_handle(handle);
        Some(RetiredStreamingTask {
            identity,
            lease,
            token,
        })
    }

    fn insert_active(&mut self, pane_id: u64, task: StreamingTask) {
        if let Some(replaced) = self.tasks.insert(pane_id, task) {
            let StreamingTask { handle, .. } = replaced;
            handle.abort();
            self.settling.insert_handle(handle);
            error!(
                pane_id,
                event = "streaming_task_active_slot_replaced",
                "Vendored streaming task active slot was replaced without prior reconciliation"
            );
        }
    }

    fn reap_completed(&mut self) {
        while let Some(join_result) = self.settling.try_join_next() {
            if let Err(error) = join_result {
                match error.kind() {
                    JoinErrorKind::Aborted | JoinErrorKind::ContextCancelled => {
                        debug!(
                            failure_class = ?error.kind(),
                            "Vendored streaming task stopped"
                        );
                    }
                    JoinErrorKind::WakerRegistrationFailed => {
                        warn!(
                            event = "streaming_task_join_observation_quarantined",
                            unacknowledged_tasks = self.settling.unacknowledged_len(),
                            "Vendored streaming task join observation failed; terminal authority remains retained"
                        );
                    }
                    _ => {
                        warn!(
                            failure_class = ?error.kind(),
                            "Vendored streaming task failed"
                        );
                    }
                }
            }
        }
    }

    fn abort_all_active(&mut self) {
        let active = std::mem::take(&mut self.tasks);
        for (_pane_id, task) in active {
            let StreamingTask { handle, .. } = task;
            handle.abort();
            self.settling.insert_handle(handle);
        }
    }

    async fn abort_and_settle_all(&mut self) -> StreamingTaskDrainOutcome {
        self.abort_all_active();
        self.settling.abort_all();
        let drain_cx = crate::cx::for_request();
        let mut terminal_failure = None;
        let drain_result = crate::runtime_async::timeout_with_cx(
            &drain_cx,
            STREAMING_TASK_DRAIN_TIMEOUT,
            async {
                loop {
                    match self.settling.drain_next_with_cx(&drain_cx).await {
                        Ok(Some(Ok(()))) => {}
                        Ok(Some(Err(error))) => {
                            match error.kind() {
                                JoinErrorKind::Aborted | JoinErrorKind::ContextCancelled => {}
                                JoinErrorKind::WakerRegistrationFailed => {
                                    terminal_failure
                                        .get_or_insert(JoinErrorKind::WakerRegistrationFailed);
                                }
                                other => terminal_failure = Some(other),
                            }
                        }
                        Ok(None) => return Ok(()),
                        Err(error) => return Err(error.kind()),
                    }
                }
            },
        )
        .await;
        let timed_out = drain_result.is_err();
        let drain_failure = match drain_result {
            Ok(Err(failure)) => Some(failure),
            Ok(Ok(())) | Err(_) => None,
        };
        classify_streaming_task_drain(
            timed_out,
            drain_failure,
            terminal_failure,
            self.settling.settlement(),
        )
    }
}

#[cfg(all(feature = "vendored", unix))]
impl std::ops::Deref for StreamingTasks {
    type Target = HashMap<u64, StreamingTask>;

    fn deref(&self) -> &Self::Target {
        &self.tasks
    }
}

#[cfg(all(feature = "vendored", unix))]
impl Drop for StreamingTasks {
    fn drop(&mut self) {
        let active_tasks = self.tasks.len();
        let already_settling_tasks = self.settling.len();
        self.abort_all_active();
        self.settling.abort_all();
        if !self.settling.is_empty() {
            error!(
                event = "streaming_tasks_dropped_before_settlement",
                active_tasks,
                already_settling_tasks,
                remaining_tasks = self.settling.len(),
                quarantined_unacknowledged_tasks = self.settling.unacknowledged_len(),
                orphan_risk = true,
                "Vendored streaming task owner dropped without terminal acknowledgement"
            );
        }
    }
}

#[cfg(feature = "native-wezterm")]
const NATIVE_ACCEPT_TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(feature = "native-wezterm")]
const NATIVE_ACCEPT_TASK_GRACEFUL_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(feature = "native-wezterm")]
const NATIVE_EVENT_QUEUE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(feature = "native-wezterm")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeEventShutdownDrainState {
    Running,
    Graceful {
        deadline: RuntimeTime,
    },
    Forced {
        deadline: RuntimeTime,
    },
    ProducerClosed,
    Abandoned,
}

#[cfg(feature = "native-wezterm")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeEventShutdownDrainAction {
    None,
    BeginGraceful,
    AbortProducer,
    ProducerClosed,
    Abandon,
}

#[cfg(feature = "native-wezterm")]
impl NativeEventShutdownDrainState {
    fn advance(
        &mut self,
        now: RuntimeTime,
        shutdown_requested: bool,
    ) -> NativeEventShutdownDrainAction {
        match *self {
            Self::Running if shutdown_requested => {
                *self = Self::Graceful {
                    deadline: now + NATIVE_ACCEPT_TASK_GRACEFUL_TIMEOUT,
                };
                NativeEventShutdownDrainAction::BeginGraceful
            }
            Self::Graceful { deadline } if now >= deadline => {
                *self = Self::Forced {
                    deadline: now + NATIVE_EVENT_QUEUE_DRAIN_TIMEOUT,
                };
                NativeEventShutdownDrainAction::AbortProducer
            }
            Self::Forced { deadline } if now >= deadline => {
                *self = Self::Abandoned;
                NativeEventShutdownDrainAction::Abandon
            }
            Self::Running
            | Self::Graceful { .. }
            | Self::Forced { .. }
            | Self::ProducerClosed
            | Self::Abandoned => NativeEventShutdownDrainAction::None,
        }
    }

    fn mark_producer_closed(&mut self) -> NativeEventShutdownDrainAction {
        match self {
            Self::Abandoned => NativeEventShutdownDrainAction::None,
            Self::ProducerClosed => NativeEventShutdownDrainAction::ProducerClosed,
            Self::Running | Self::Graceful { .. } | Self::Forced { .. } => {
                *self = Self::ProducerClosed;
                NativeEventShutdownDrainAction::ProducerClosed
            }
        }
    }

    const fn producer_closed(self) -> bool {
        matches!(self, Self::ProducerClosed)
    }
}

#[cfg(feature = "native-wezterm")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeAcceptTaskSettlement {
    Settled,
    SettledWithFailure {
        failure: JoinErrorKind,
    },
    TimedOut {
        active_tasks: usize,
        unacknowledged_tasks: usize,
    },
    Incomplete {
        active_tasks: usize,
        unacknowledged_tasks: usize,
        drain_failure: Option<JoinErrorKind>,
    },
}

#[cfg(feature = "native-wezterm")]
fn classify_native_accept_task_settlement(
    timed_out: bool,
    drain_failure: Option<JoinErrorKind>,
    terminal_failure: Option<JoinErrorKind>,
    settlement: JoinSetSettlement,
) -> NativeAcceptTaskSettlement {
    match settlement {
        JoinSetSettlement::Settled => terminal_failure.map_or(
            NativeAcceptTaskSettlement::Settled,
            |failure| NativeAcceptTaskSettlement::SettledWithFailure { failure },
        ),
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } if timed_out => NativeAcceptTaskSettlement::TimedOut {
            active_tasks,
            unacknowledged_tasks,
        },
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } => NativeAcceptTaskSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
            drain_failure,
        },
    }
}

/// Owns the native listener's nested accept loop. Normal shutdown first lets
/// the listener observe its shared shutdown flag and settle its own connection
/// tasks; abort is reserved for bounded escalation or direct cancellation.
/// Both paths use the trusted-poll JoinSet drain, so a caller-waker
/// registration failure cannot be mistaken for terminal acknowledgement.
/// If the one-second envelope expires, the returned finite outcome explicitly
/// reports the still-owned active/quarantined authority before guard drop.
#[cfg(feature = "native-wezterm")]
struct AbortOnDropNativeAcceptTask {
    tasks: JoinSet<()>,
}

#[cfg(feature = "native-wezterm")]
impl AbortOnDropNativeAcceptTask {
    fn new(handle: JoinHandle<()>) -> Self {
        let mut tasks = JoinSet::new();
        tasks.insert_handle(handle);
        Self { tasks }
    }

    fn abort(&mut self) {
        self.tasks.abort_all();
    }

    async fn abort_and_settle(&mut self) -> NativeAcceptTaskSettlement {
        self.abort();
        self.settle().await
    }

    async fn settle(&mut self) -> NativeAcceptTaskSettlement {
        let drain_cx = crate::cx::for_request();
        let mut terminal_failure = None;
        let drain_result = crate::runtime_async::timeout_with_cx(
            &drain_cx,
            NATIVE_ACCEPT_TASK_DRAIN_TIMEOUT,
            async {
                loop {
                    match self.tasks.drain_next_with_cx(&drain_cx).await {
                        Ok(Some(Ok(()))) => {}
                        Ok(Some(Err(error))) => {
                            match error.kind() {
                                JoinErrorKind::Aborted | JoinErrorKind::ContextCancelled => {}
                                JoinErrorKind::WakerRegistrationFailed => {
                                    terminal_failure
                                        .get_or_insert(JoinErrorKind::WakerRegistrationFailed);
                                }
                                other => terminal_failure = Some(other),
                            }
                        }
                        Ok(None) => return Ok(()),
                        Err(error) => return Err(error.kind()),
                    }
                }
            },
        )
        .await;
        let timed_out = drain_result.is_err();
        let drain_failure = match drain_result {
            Ok(Err(failure)) => Some(failure),
            Ok(Ok(())) | Err(_) => None,
        };
        classify_native_accept_task_settlement(
            timed_out,
            drain_failure,
            terminal_failure,
            self.tasks.settlement(),
        )
    }

    #[cfg(test)]
    fn force_registration_failure_for_test(&self) {
        if !self.tasks.is_empty() {
            self.tasks.force_join_registration_failure_for_test();
        }
    }
}

#[cfg(feature = "native-wezterm")]
impl Drop for AbortOnDropNativeAcceptTask {
    fn drop(&mut self) {
        self.abort();
    }
}

enum RecvEvent<T> {
    Item(T),
    Closed,
    Cancelled,
}

async fn recv_event<T>(runtime_cx: &RuntimeLoopCx, rx: &mut mpsc::Receiver<T>) -> RecvEvent<T> {
    match rx.recv(runtime_cx).await {
        Ok(value) => RecvEvent::Item(value),
        Err(mpsc::RecvError::Disconnected) => RecvEvent::Closed,
        Err(mpsc::RecvError::Cancelled) => RecvEvent::Cancelled,
        Err(mpsc::RecvError::Empty) => {
            debug_assert!(
                false,
                "runtime recv_event unexpectedly returned RecvError::Empty"
            );
            RecvEvent::Closed
        }
    }
}

#[cfg(all(feature = "vendored", unix))]
async fn send_runtime_channel<T>(
    runtime_cx: &RuntimeLoopCx,
    tx: &mpsc::Sender<T>,
    value: T,
) -> bool {
    tx.send(runtime_cx, value).await.is_ok()
}

#[cfg(all(feature = "vendored", unix))]
fn remap_vendored_streaming_delta(
    identity: &StreamingSubscriptionIdentity,
    delta: PaneDelta,
) -> std::result::Result<PaneDelta, String> {
    let actual_local_pane_id = match &delta {
        PaneDelta::Output { pane_id, .. }
        | PaneDelta::Gap { pane_id, .. }
        | PaneDelta::Ended { pane_id, .. } => *pane_id,
    };
    if actual_local_pane_id != identity.local_pane_id {
        return Err(format!(
            "subscription pane identity mismatch: global pane {} generation {} shard {} expected local pane {} but received local pane {}",
            identity.global_pane_id,
            identity.generation,
            identity.socket_shard.0,
            identity.local_pane_id,
            actual_local_pane_id,
        ));
    }

    Ok(match delta {
        PaneDelta::Output {
            seqno,
            delta_text,
            title,
            dirty_range_count,
            dirty_row_count,
            ..
        } => PaneDelta::Output {
            pane_id: identity.global_pane_id,
            seqno,
            delta_text,
            title,
            dirty_range_count,
            dirty_row_count,
        },
        PaneDelta::Gap { reason, .. } => PaneDelta::Gap {
            pane_id: identity.global_pane_id,
            reason,
        },
        PaneDelta::Ended { reason, .. } => PaneDelta::Ended {
            pane_id: identity.global_pane_id,
            reason,
        },
    })
}

#[cfg(all(feature = "vendored", unix))]
async fn forward_vendored_streaming_delta(
    runtime_cx: &RuntimeLoopCx,
    bridge: &mut StreamingBridge,
    capture_tx: &mpsc::Sender<CaptureEvent>,
    identity: &StreamingSubscriptionIdentity,
    lease: &CaptureLease,
    delta: PaneDelta,
) -> Option<String> {
    let delta = match remap_vendored_streaming_delta(identity, delta) {
        Ok(delta) => delta,
        Err(reason) => return Some(reason),
    };
    let producer_guard = match lease
        .try_acquire_producer(identity.capture_stamp, identity.global_pane_id)
    {
        Ok(guard) => guard,
        Err(error) => return Some(format!("capture authority rejected stream: {error}")),
    };
    let exit_reason = match &delta {
        PaneDelta::Ended { reason, .. } => Some(reason.clone()),
        _ => None,
    };

    for segment in bridge.process_delta(delta) {
        let event = match CaptureEvent::from_producer(segment, &producer_guard) {
            Ok(event) => event,
            Err(error) => return Some(format!("capture authority rejected stream: {error}")),
        };
        if !send_runtime_channel(runtime_cx, capture_tx, event).await {
            return Some("capture ingress closed".to_string());
        }
    }

    exit_reason
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
    producer_guard: CaptureProducerGuard,
}

#[cfg(feature = "native-wezterm")]
#[derive(Debug)]
struct CoalescedNativeOutput {
    pane_id: u64,
    bytes: Vec<u8>,
    timestamp_ms: i64,
    input_events: u32,
    producer_guard: CaptureProducerGuard,
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
        producer_guard: CaptureProducerGuard,
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
                    producer_guard,
                });
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let pending = o.get_mut();
                let source_changed =
                    pending.producer_guard.stamp() != producer_guard.stamp();

                if source_changed
                    || (!pending.bytes.is_empty()
                        && pending.bytes.len().saturating_add(bytes.len()) > self.max_bytes)
                {
                    let flushed = CoalescedNativeOutput {
                        pane_id,
                        bytes: std::mem::take(&mut pending.bytes),
                        timestamp_ms: pending.last_timestamp_ms,
                        input_events: pending.input_events,
                        producer_guard: std::mem::replace(
                            &mut pending.producer_guard,
                            producer_guard,
                        ),
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
                    producer_guard: pending.producer_guard,
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
                producer_guard: pending.producer_guard,
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
                producer_guard: pending.producer_guard,
            });
        }
        out
    }
}

#[cfg(feature = "native-wezterm")]
fn acquire_native_producer(
    authority: &CaptureAuthority,
    metrics: &RuntimeMetrics,
    pane_id: u64,
) -> Option<CaptureProducerGuard> {
    match authority.try_acquire_unstamped_native_producer(pane_id) {
        Ok(guard) => Some(guard),
        Err(error) => {
            metrics.capture_authority_rejections.increment();
            debug!(
                pane_id,
                error = %error,
                "Rejected native event before producer side effects"
            );
            None
        }
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
    pub max_concurrent_captures: u32,
    /// Data retention period in days
    pub retention_days: u32,
    /// Validated, first-match-preserving event-retention policy compiled from
    /// the startup storage configuration.
    pub retention_policy: Arc<CompiledRetentionPolicy>,
    /// Maximum size of storage in MB (0 = unlimited)
    pub retention_max_mb: u32,
    /// Database checkpoint interval in seconds
    pub checkpoint_interval_secs: u32,
    /// Periodic cache/page-stat reporting and manual-VACUUM advisory policy
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
            retention_policy: StorageConfig::default()
                .compile_retention_policy()
                .expect("the built-in retention policy must be valid"),
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

fn initial_hot_reloadable_config(config: &RuntimeConfig) -> HotReloadableConfig {
    HotReloadableConfig {
        log_level: "info".to_string(), // Default, will be overridden
        poll_interval_ms: duration_ms_u64(config.capture_interval),
        min_poll_interval_ms: duration_ms_u64(config.min_capture_interval),
        max_concurrent_captures: config.max_concurrent_captures,
        pane_priorities: config.pane_priorities.clone(),
        capture_budgets: config.capture_budgets.clone(),
        retention_days: config.retention_days,
        retention_max_mb: config.retention_max_mb,
        checkpoint_interval_secs: config.checkpoint_interval_secs,
        retention_policy: Arc::clone(&config.retention_policy),
        gc: config.gc,
        patterns: config.patterns.clone(),
        workflows_enabled: vec![],
        auto_run_allowlist: vec![],
        trauma_guard: config.trauma_guard.clone(),
    }
}

fn capture_concurrency_usize(value: u32) -> usize {
    usize::try_from(value)
        .expect("FrankenTerm's supported targets have a usize wide enough to represent u32")
}

#[cfg(all(feature = "vendored", unix))]
#[derive(Debug)]
struct StreamingTaskExit {
    identity: StreamingSubscriptionIdentity,
    token: StreamingTaskToken,
    reason: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct DiscoveryRevision(u64);

impl DiscoveryRevision {
    const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug)]
struct ObservedCapturePane {
    info: PaneInfo,
    generation: u32,
    pane_uuid: String,
    revision: DiscoveryRevision,
    requires_storage_resync: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaptureTransitionDescriptor {
    desired_revision: DiscoveryRevision,
    predecessor_revision: Option<DiscoveryRevision>,
}

#[derive(Clone, Debug)]
struct DiscoveryCapturePublication {
    epoch: u64,
    observed_panes: Arc<HashMap<u64, ObservedCapturePane>>,
    /// Pane IDs intentionally withheld while discovery prepares a successor.
    /// This distinguishes a pending transition from a terminal close so the
    /// capture coordinator retains exact predecessor resync obligations.
    transitioning_pane_ids: Arc<HashSet<u64>>,
    /// Current successor transition descriptors.  These survive watch-channel
    /// coalescing so capture can recover the predecessor fast path even when it
    /// observes the ready publication without first observing the pending one.
    transitions: Arc<HashMap<u64, CaptureTransitionDescriptor>>,
}

impl Default for DiscoveryCapturePublication {
    fn default() -> Self {
        Self {
            epoch: 0,
            observed_panes: Arc::new(HashMap::new()),
            transitioning_pane_ids: Arc::new(HashSet::new()),
            transitions: Arc::new(HashMap::new()),
        }
    }
}

/// Build the strongest capture view that can be justified from a fresh mux
/// listing without mutating the registry or awaiting storage.
///
/// Existing panes are retained only when the generation-defining metadata
/// still matches exactly. Missing IDs are terminally absent; new or changed
/// IDs are marked as transitioning. Duplicate IDs are ambiguous and therefore
/// excluded. Installing this view before stable-UUID I/O closes predecessor
/// admission across that await.
fn conservative_capture_view_before_storage(
    previous: &HashMap<u64, ObservedCapturePane>,
    panes: &[PaneInfo],
) -> (
    Arc<HashMap<u64, ObservedCapturePane>>,
    Arc<HashSet<u64>>,
) {
    let mut listed = HashMap::<u64, &PaneInfo>::with_capacity(panes.len());
    let mut duplicate_ids = HashSet::new();
    for pane in panes {
        if listed.insert(pane.pane_id, pane).is_some() {
            duplicate_ids.insert(pane.pane_id);
        }
    }

    let mut retained = HashMap::with_capacity(previous.len().min(listed.len()));
    let mut transitioning = HashSet::new();
    for (&pane_id, observed) in previous {
        let Some(listed_pane) = listed.get(&pane_id).copied() else {
            continue;
        };
        if duplicate_ids.contains(&pane_id) {
            transitioning.insert(pane_id);
            continue;
        }
        let previous_fingerprint = crate::ingest::PaneFingerprint::without_content(&observed.info);
        let listed_fingerprint = crate::ingest::PaneFingerprint::without_content(listed_pane);
        if previous_fingerprint.is_same_generation(&listed_fingerprint) {
            retained.insert(pane_id, observed.clone());
        } else {
            transitioning.insert(pane_id);
        }
    }
    for pane in panes {
        if !previous.contains_key(&pane.pane_id) {
            transitioning.insert(pane.pane_id);
        }
    }

    (Arc::new(retained), Arc::new(transitioning))
}

/// Retain every predecessor excluded by the synchronous post-list barrier.
///
/// Discovery may still fail while recovering stable UUIDs, before the registry
/// consumes the listing. The watch publication has already withdrawn those
/// panes at that point, so this side ledger must outlive the failed tick or a
/// same-fingerprint reappearance could otherwise reuse the predecessor's
/// capture revision.
fn remember_withheld_barrier_predecessors(
    unresolved: &mut HashMap<u64, DiscoveryRevision>,
    previous: &HashMap<u64, ObservedCapturePane>,
    conservative: &HashMap<u64, ObservedCapturePane>,
) {
    for (&pane_id, pane) in previous {
        if !conservative.contains_key(&pane_id) {
            unresolved.entry(pane_id).or_insert(pane.revision);
        }
    }
}

/// Classify barrier predecessors only after a successful registry tick.
///
/// Missing or filtered panes are now confirmed terminal. An observed pane that
/// receives a barrier transition even if the ordinary registry diff also
/// contains it; the caller sorts and deduplicates the combined IDs in linear
/// time. This is the failed-I/O ABA case where the registry may never have
/// consumed the intervening absence.
fn classify_unresolved_barrier_predecessors(
    unresolved: &HashMap<u64, DiscoveryRevision>,
    registry: &PaneRegistry,
) -> (Vec<u64>, Vec<u64>) {
    let mut confirmed_terminal = Vec::new();
    let mut forced_transitions = Vec::new();
    for &pane_id in unresolved.keys() {
        match registry.get_entry(pane_id) {
            Some(entry) if entry.should_observe() => {
                forced_transitions.push(pane_id);
            }
            Some(_) | None => confirmed_terminal.push(pane_id),
        }
    }
    confirmed_terminal.sort_unstable();
    forced_transitions.sort_unstable();
    (confirmed_terminal, forced_transitions)
}

fn registry_observes_pane(registry: &PaneRegistry, pane_id: u64) -> bool {
    registry
        .get_entry(pane_id)
        .is_some_and(crate::ingest::PaneEntry::should_observe)
}

fn retain_observed_capture_bookkeeping(
    registry: &PaneRegistry,
    discovery_revisions: &mut HashMap<u64, DiscoveryRevision>,
    storage_resync_revisions: &mut HashMap<u64, DiscoveryRevision>,
    capture_transitions: &mut HashMap<u64, CaptureTransitionDescriptor>,
    capture_setup_pending: &mut HashMap<u64, &'static str>,
) {
    discovery_revisions.retain(|pane_id, _| registry_observes_pane(registry, *pane_id));
    storage_resync_revisions.retain(|pane_id, _| registry_observes_pane(registry, *pane_id));
    capture_transitions.retain(|pane_id, _| registry_observes_pane(registry, *pane_id));
    capture_setup_pending.retain(|pane_id, _| registry_observes_pane(registry, *pane_id));
}

fn allocate_capture_transition_revisions(
    transitioning_pane_ids: &[u64],
    last_discovery_revision: &mut u64,
    discovery_revisions: &mut HashMap<u64, DiscoveryRevision>,
    capture_transitions: &mut HashMap<u64, CaptureTransitionDescriptor>,
    unresolved_barrier_predecessors: &mut HashMap<u64, DiscoveryRevision>,
    storage_resync_revisions: &mut HashMap<u64, DiscoveryRevision>,
) {
    for &pane_id in transitioning_pane_ids {
        let barrier_predecessor = unresolved_barrier_predecessors.get(&pane_id).copied();
        let predecessor_revision =
            barrier_predecessor.or_else(|| discovery_revisions.get(&pane_id).copied());
        match allocate_discovery_revision(last_discovery_revision) {
            Some(revision) => {
                discovery_revisions.insert(pane_id, revision);
                capture_transitions.insert(
                    pane_id,
                    CaptureTransitionDescriptor {
                        desired_revision: revision,
                        predecessor_revision,
                    },
                );
                if barrier_predecessor.is_some() {
                    unresolved_barrier_predecessors.remove(&pane_id);
                    storage_resync_revisions.insert(pane_id, revision);
                }
            }
            None => {
                discovery_revisions.remove(&pane_id);
                capture_transitions.remove(&pane_id);
                storage_resync_revisions.remove(&pane_id);
                error!(
                    pane_id,
                    "Discovery revision namespace exhausted; refusing capture admission"
                );
            }
        }
    }
}

fn capture_publication_view(
    registry: &PaneRegistry,
    revisions: &HashMap<u64, DiscoveryRevision>,
    storage_resync_revisions: &HashMap<u64, DiscoveryRevision>,
    excluded: &HashSet<u64>,
) -> Arc<HashMap<u64, ObservedCapturePane>> {
    let observed_ids = registry.observed_pane_ids();
    let mut published = HashMap::with_capacity(observed_ids.len());
    for pane_id in observed_ids {
        if excluded.contains(&pane_id) {
            continue;
        }
        let Some(entry) = registry.get_entry(pane_id) else {
            continue;
        };
        let Some(revision) = revisions.get(&pane_id).copied() else {
            continue;
        };
        published.insert(
            pane_id,
            ObservedCapturePane {
                info: entry.info.clone(),
                generation: entry.generation,
                pane_uuid: entry.pane_uuid.clone(),
                revision,
                requires_storage_resync: storage_resync_revisions.get(&pane_id).copied()
                    == Some(revision),
            },
        );
    }
    Arc::new(published)
}

fn capture_publication_identity_matches(
    left: &HashMap<u64, ObservedCapturePane>,
    right: &HashMap<u64, ObservedCapturePane>,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(pane_id, left_pane)| {
            right.get(pane_id).is_some_and(|right_pane| {
                left_pane.generation == right_pane.generation
                    && left_pane.pane_uuid == right_pane.pane_uuid
                    && left_pane.revision == right_pane.revision
                    && left_pane.requires_storage_resync == right_pane.requires_storage_resync
            })
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureResyncRequirement {
    Exact(DiscoveryRevision),
    StorageAudit,
}

/// Bounded, lossless accounting for successor resync obligations.
///
/// Exact predecessor revisions use the in-memory durability checkpoint fast
/// path. When that bounded tier is full, the affected pane moves into an
/// explicit storage-audit set retained only while the pane is observed or in a
/// published transition. The fallback therefore survives a predecessor commit
/// that races an earlier discovery read without poisoning unrelated future
/// panes with a process-lifetime global mode.
struct PendingCaptureResyncs {
    exact: HashMap<u64, DiscoveryRevision>,
    storage_audit: HashSet<u64>,
    exact_capacity: usize,
}

impl PendingCaptureResyncs {
    fn new(exact_capacity: usize) -> Self {
        let exact_capacity = exact_capacity.max(1);
        Self {
            exact: HashMap::with_capacity(exact_capacity),
            storage_audit: HashSet::new(),
            exact_capacity,
        }
    }

    /// Remember an obligation without evicting an earlier one.
    ///
    /// Returns `true` when this pane is using the storage-audit tier. An
    /// overflow loses only the exact-checkpoint fast path, never the resync
    /// obligation itself.
    fn remember(&mut self, pane_id: u64, revision: DiscoveryRevision) -> bool {
        if self.storage_audit.contains(&pane_id) {
            return true;
        }
        if let Some(existing) = self.exact.get_mut(&pane_id) {
            *existing = revision;
            return false;
        }
        if self.exact.len() < self.exact_capacity {
            self.exact.insert(pane_id, revision);
            return false;
        }
        self.storage_audit.insert(pane_id);
        true
    }

    fn requirement(
        &self,
        pane_id: u64,
        requires_storage_resync: bool,
    ) -> Option<CaptureResyncRequirement> {
        self.exact
            .get(&pane_id)
            .copied()
            .map(CaptureResyncRequirement::Exact)
            .or_else(|| {
                (self.storage_audit.contains(&pane_id) || requires_storage_resync)
                    .then_some(CaptureResyncRequirement::StorageAudit)
            })
    }

    fn acknowledge(&mut self, pane_id: u64) {
        self.exact.remove(&pane_id);
        self.storage_audit.remove(&pane_id);
    }

    /// Force an unproven storage recovery for a failed successor resync.
    ///
    /// Storage rows are keyed by numeric pane ID, not discovery revision. A
    /// caller-supplied successor revision therefore cannot relabel the loaded
    /// predecessor tail into positive continuity evidence. Dropping the exact
    /// tier here guarantees the retry emits a full successor snapshot.
    fn require_storage_audit(&mut self, pane_id: u64) {
        self.exact.remove(&pane_id);
        self.storage_audit.insert(pane_id);
    }

    fn retain_authoritative(
        &mut self,
        publication: &DiscoveryCapturePublication,
    ) {
        let retain = |pane_id: &u64| {
            publication.observed_panes.contains_key(pane_id)
                || publication.transitioning_pane_ids.contains(pane_id)
                || publication.transitions.contains_key(pane_id)
        };
        self.exact.retain(|pane_id, _| retain(pane_id));
        self.storage_audit.retain(retain);
    }

    /// Import exact predecessor continuity from a fully ready publication.
    ///
    /// The discovery watch coalesces updates, so capture may observe the ready
    /// successor without observing the transition-pending publication or the
    /// predecessor binding. Descriptor values are consumed only when the
    /// published successor revision matches: post-list barriers intentionally
    /// carry the prior descriptor map while the replacement revision is still
    /// being allocated.
    fn observe_ready_transitions(
        &mut self,
        publication: &DiscoveryCapturePublication,
        completed: &HashMap<u64, DiscoveryRevision>,
    ) {
        for (&pane_id, descriptor) in publication.transitions.iter() {
            let successor_is_ready = publication
                .observed_panes
                .get(&pane_id)
                .is_some_and(|pane| pane.revision == descriptor.desired_revision);
            if !successor_is_ready
                || completed.get(&pane_id).copied() == Some(descriptor.desired_revision)
            {
                continue;
            }
            let Some(predecessor_revision) = descriptor.predecessor_revision else {
                continue;
            };
            if predecessor_revision == descriptor.desired_revision {
                self.require_storage_audit(pane_id);
            } else {
                self.remember(pane_id, predecessor_revision);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct CaptureDurabilityCheckpoint {
    revision: DiscoveryRevision,
    next_seq: u64,
    raw_tail: String,
}

#[derive(Clone, Debug)]
enum CachedCaptureCheckpoint {
    Certain(CaptureDurabilityCheckpoint),
    Uncertain { revision: DiscoveryRevision },
}

type CaptureCheckpointCache = Arc<StdMutex<LruCache<u64, CachedCaptureCheckpoint>>>;

struct CaptureCheckpointWrite {
    revision: DiscoveryRevision,
    baseline: Option<CaptureDurabilityCheckpoint>,
}

fn begin_capture_checkpoint_write(
    checkpoints: &CaptureCheckpointCache,
    pane_id: u64,
    revision: DiscoveryRevision,
) -> CaptureCheckpointWrite {
    let Ok(mut cache) = checkpoints.lock() else {
        error!(pane_id, "Capture durability checkpoint cache is poisoned");
        return CaptureCheckpointWrite {
            revision,
            baseline: None,
        };
    };
    let baseline = match cache.get(&pane_id) {
        Some(CachedCaptureCheckpoint::Certain(checkpoint))
            if checkpoint.revision == revision =>
        {
            Some(checkpoint.clone())
        }
        Some(CachedCaptureCheckpoint::Certain(_)
        | CachedCaptureCheckpoint::Uncertain { .. })
        | None => None,
    };
    let _ = cache.put(
        pane_id,
        CachedCaptureCheckpoint::Uncertain { revision },
    );
    CaptureCheckpointWrite { revision, baseline }
}

fn confirm_capture_checkpoint(
    checkpoints: &CaptureCheckpointCache,
    pane_id: u64,
    write: &CaptureCheckpointWrite,
    persisted_seq: u64,
    raw_content: &str,
) {
    let Some(next_seq) = persisted_seq.checked_add(1) else {
        error!(pane_id, persisted_seq, "Durable capture sequence exhausted");
        return;
    };
    let Ok(mut cache) = checkpoints.lock() else {
        error!(pane_id, "Capture durability checkpoint cache is poisoned");
        return;
    };
    let current_write_is_live = matches!(
        cache.get(&pane_id),
        Some(CachedCaptureCheckpoint::Uncertain { revision })
            if *revision == write.revision
    );
    if !current_write_is_live {
        error!(
            pane_id,
            revision = write.revision.get(),
            "Capture checkpoint write lost its exact cache generation"
        );
        return;
    }
    let Some(baseline) = write.baseline.as_ref() else {
        // A missing, evicted, or previously uncertain baseline cannot be
        // reconstructed from one later segment.  Keep the cache uncertain so
        // the next transition performs an authoritative storage read.
        return;
    };
    if baseline.next_seq != persisted_seq {
        error!(
            pane_id,
            expected_seq = baseline.next_seq,
            persisted_seq,
            "Capture checkpoint sequence correction requires storage reconciliation"
        );
        return;
    }
    let mut raw_tail = baseline.raw_tail.clone();
    raw_tail.push_str(raw_content);
    raw_tail = crate::ingest::resume_anchor_tail(
        &raw_tail,
        crate::ingest::RESUME_ANCHOR_BYTES,
    )
    .to_string();
    let _ = cache.put(
        pane_id,
        CachedCaptureCheckpoint::Certain(CaptureDurabilityCheckpoint {
            revision: write.revision,
            next_seq,
            raw_tail,
        }),
    );
}

fn certain_capture_checkpoint(
    checkpoints: &CaptureCheckpointCache,
    pane_id: u64,
    revision: DiscoveryRevision,
) -> Option<CaptureDurabilityCheckpoint> {
    let Ok(mut cache) = checkpoints.lock() else {
        error!(pane_id, "Capture durability checkpoint cache is poisoned");
        return None;
    };
    match cache.get(&pane_id) {
        Some(CachedCaptureCheckpoint::Certain(checkpoint))
            if checkpoint.revision == revision =>
        {
            Some(checkpoint.clone())
        }
        Some(CachedCaptureCheckpoint::Certain(_)
        | CachedCaptureCheckpoint::Uncertain { .. })
        | None => None,
    }
}

#[derive(Clone)]
struct CapturePaneMetadata {
    pane_uuid: String,
    discovery_generation: u32,
    discovery_revision: DiscoveryRevision,
}

struct ActiveCaptureBinding {
    generation: u32,
    pane_uuid: String,
    revision: DiscoveryRevision,
    identity: ActivePaneIdentity,
    polling_lease: CaptureLease,
    /// Present while the successor's mandatory resync is queued or being
    /// persisted. The binding is not exposed to normal producers until this
    /// receipt is durably committed.
    resync_receipt: Option<CaptureResyncReceipt>,
    #[cfg(feature = "native-wezterm")]
    native_lease: Option<CaptureLease>,
    #[cfg(all(feature = "vendored", unix))]
    streaming_lease: Option<CaptureLease>,
}

struct PendingCaptureResyncBinding {
    binding: ActiveCaptureBinding,
    queued_at: Instant,
}

#[derive(Debug, Eq, PartialEq)]
enum PendingCaptureResyncDisposition {
    Wait,
    Publish(u64),
    RetireFailed(String),
    RetireSuperseded {
        committed: bool,
        failure_reason: Option<String>,
    },
}

fn pending_capture_resync_disposition(
    still_current: bool,
    outcome: Option<std::result::Result<u64, String>>,
) -> PendingCaptureResyncDisposition {
    match (still_current, outcome) {
        (_, None) => PendingCaptureResyncDisposition::Wait,
        (true, Some(Ok(sequence))) => PendingCaptureResyncDisposition::Publish(sequence),
        (true, Some(Err(reason))) => PendingCaptureResyncDisposition::RetireFailed(reason),
        (false, Some(Ok(_))) => PendingCaptureResyncDisposition::RetireSuperseded {
            committed: true,
            failure_reason: None,
        },
        (false, Some(Err(reason))) => PendingCaptureResyncDisposition::RetireSuperseded {
            committed: false,
            failure_reason: Some(reason),
        },
    }
}

impl ActiveCaptureBinding {
    fn matches_observed(&self, pane: &ObservedCapturePane) -> bool {
        self.generation == pane.generation
            && self.pane_uuid == pane.pane_uuid
            && self.revision == pane.revision
    }
}

fn allocate_discovery_revision(last_revision: &mut u64) -> Option<DiscoveryRevision> {
    let next = last_revision.checked_add(1)?;
    *last_revision = next;
    Some(DiscoveryRevision(next))
}

fn allocate_discovery_publication_epoch(last_epoch: &mut u64) -> Option<u64> {
    let next = last_epoch.checked_add(1)?;
    *last_epoch = next;
    Some(next)
}

fn publish_discovery_capture_view(
    publication_tx: &watch::Sender<DiscoveryCapturePublication>,
    authority: &CaptureAuthority,
    last_epoch: &mut u64,
    last_view: &mut Arc<HashMap<u64, ObservedCapturePane>>,
    observed_panes: Arc<HashMap<u64, ObservedCapturePane>>,
    transitioning_pane_ids: Arc<HashSet<u64>>,
    transitions: Arc<HashMap<u64, CaptureTransitionDescriptor>>,
    phase: &'static str,
) -> Result<()> {
    let authority_gate_uninitialized = *last_epoch == 0;
    let Some(epoch) = allocate_discovery_publication_epoch(last_epoch) else {
        return Err(runtime_backend_error(
            "capture.discovery.publish",
            format!(
                "{phase}: discovery publication namespace exhausted; capture remains on its last complete view"
            ),
        ));
    };
    let Some(authority_epoch) = CaptureViewEpoch::new(epoch) else {
        return Err(runtime_backend_error(
            "capture.discovery.publish",
            format!("{phase}: discovery produced an invalid zero authority-view epoch"),
        ));
    };
    if authority_gate_uninitialized
        || !capture_publication_identity_matches(last_view, &observed_panes)
    {
        let mut desired_revisions = HashMap::with_capacity(observed_panes.len());
        for (pane_id, pane) in observed_panes.iter() {
            let Some(revision) = CaptureRevision::new(pane.revision.get()) else {
                return Err(runtime_backend_error(
                    "capture.discovery.publish",
                    format!(
                        "{phase}: pane {pane_id} has an invalid zero capture revision"
                    ),
                ));
            };
            desired_revisions.insert(*pane_id, revision);
        }
        authority
            .install_desired_revisions(authority_epoch, &desired_revisions)
            .map_err(|error| {
                runtime_backend_error(
                    "capture.discovery.publish",
                    format!("{phase}: failed to install discovery revision gate: {error}"),
                )
            })?;
    }
    let publication = DiscoveryCapturePublication {
        epoch,
        observed_panes: Arc::clone(&observed_panes),
        transitioning_pane_ids,
        transitions,
    };
    publication_tx.send(publication).map_err(|_| {
        runtime_backend_error(
            "capture.discovery.publish",
            format!(
                "{phase}: capture publication receiver closed before epoch {epoch} was delivered"
            ),
        )
    })?;
    *last_view = observed_panes;
    Ok(())
}

fn capture_sync_due(
    now: Instant,
    next_sync_tick: Instant,
    publication_rx: &watch::Receiver<DiscoveryCapturePublication>,
) -> bool {
    now >= next_sync_tick || publication_rx.has_changed()
}

fn capture_publication_matches(
    publication_rx: &watch::Receiver<DiscoveryCapturePublication>,
    pane_id: u64,
    revision: DiscoveryRevision,
) -> bool {
    publication_rx
        .borrow()
        .observed_panes
        .get(&pane_id)
        .is_some_and(|pane| pane.revision == revision)
}

async fn relay_capture_event_with_cx(
    cx: &RuntimeLoopCx,
    capture_ring_tx: &SpscProducer<CaptureEvent>,
    event: CaptureEvent,
) -> Result<()> {
    capture_ring_tx
        .send_with_cx(cx, event)
        .await
        .map_err(|_| {
            if cx.checkpoint().is_err() {
                runtime_cx_error(
                    "capture.relay",
                    cx,
                    "capability context failed while relaying capture",
                )
            } else {
                runtime_backend_error("capture.relay", "persistence ring closed")
            }
        })
}

/// Acquire the complete persistence-side admission before any semantic lookup
/// or side effect.  Keeping authority, immutable incarnation metadata, and the
/// latest discovery publication behind one seam makes it impossible for a
/// caller to validate only the queue stamp and then accidentally process a
/// discovery-superseded event.
async fn admit_capture_event_for_persistence(
    authority: &CaptureAuthority,
    capture_metadata: &RwLock<HashMap<PaneIncarnation, CapturePaneMetadata>>,
    publication_rx: &watch::Receiver<DiscoveryCapturePublication>,
    event: &CaptureEvent,
) -> Result<(CapturePersistenceGuard, CapturePaneMetadata)> {
    let stamp = event.stamp();
    let guard = authority.try_acquire_persistence(stamp, event.segment.pane_id)?;
    let pane_incarnation = stamp.pane_incarnation();
    let metadata = capture_metadata
        .read()
        .await
        .get(&pane_incarnation)
        .cloned()
        .ok_or_else(|| {
            runtime_backend_error(
                "capture.persistence.admission",
                format!(
                    "pane {} incarnation {} has no immutable capture metadata",
                    event.segment.pane_id,
                    pane_incarnation.get()
                ),
            )
        })?;
    let is_current = {
        let publication = publication_rx.borrow();
        publication
            .observed_panes
            .get(&event.segment.pane_id)
            .is_some_and(|pane| {
                (
                    pane.pane_uuid.as_str(),
                    pane.generation,
                    pane.revision,
                ) == (
                    metadata.pane_uuid.as_str(),
                    metadata.discovery_generation,
                    metadata.discovery_revision,
                )
            })
    };
    if !is_current {
        return Err(runtime_backend_error(
            "capture.persistence.admission",
            format!(
                "pane {} incarnation {} discovery revision {} is no longer current",
                event.segment.pane_id,
                pane_incarnation.get(),
                metadata.discovery_revision.get()
            ),
        ));
    }
    Ok((guard, metadata))
}

const CAPTURE_AUTHORITY_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const CAPTURE_TRANSITION_QUEUE_TIMEOUT: Duration = Duration::from_millis(100);
const CAPTURE_PROMPT_DRAIN_WINDOW: Duration = Duration::from_secs(5);
const CAPTURE_FAST_RETRY_WINDOW: Duration = Duration::from_millis(100);
const CAPTURE_WARM_RETRY_WINDOW: Duration = Duration::from_secs(1);
const CAPTURE_COOL_RETRY_WINDOW: Duration = Duration::from_secs(5);
const CAPTURE_FAST_RETRY_DELAY: Duration = Duration::from_millis(10);
const CAPTURE_WARM_RETRY_DELAY: Duration = Duration::from_millis(50);
const CAPTURE_COOL_RETRY_DELAY: Duration = Duration::from_millis(250);
const CAPTURE_IDLE_RETRY_DELAY: Duration = Duration::from_secs(1);
const CAPTURE_POLL_REAP_BUDGET: usize = 64;

fn capture_transition_retry_delay(started_at: Instant, now: Instant) -> Duration {
    let age = now.saturating_duration_since(started_at);
    if age < CAPTURE_FAST_RETRY_WINDOW {
        CAPTURE_FAST_RETRY_DELAY
    } else if age < CAPTURE_WARM_RETRY_WINDOW {
        CAPTURE_WARM_RETRY_DELAY
    } else if age < CAPTURE_COOL_RETRY_WINDOW {
        CAPTURE_COOL_RETRY_DELAY
    } else {
        CAPTURE_IDLE_RETRY_DELAY
    }
}

fn bounded_capture_transition_retry_delay(
    started_at: Instant,
    now: Instant,
) -> Option<Duration> {
    let age = now.saturating_duration_since(started_at);
    let remaining = CAPTURE_PROMPT_DRAIN_WINDOW.checked_sub(age)?;
    (!remaining.is_zero()).then(|| capture_transition_retry_delay(started_at, now).min(remaining))
}

async fn rollback_empty_capture_binding(
    cx: &RuntimeLoopCx,
    authority: &CaptureAuthority,
    identity: ActivePaneIdentity,
) -> bool {
    let Ok(revocation) = authority.begin_pane_revocation(identity) else {
        return false;
    };
    match revocation
        .wait_with_cx(cx, CAPTURE_AUTHORITY_DRAIN_TIMEOUT)
        .await
    {
        Ok(_) => true,
        Err(error) => {
            error!(
                pane_id = identity.global_pane_id(),
                error = %error,
                "Failed to roll back partially activated capture binding"
            );
            false
        }
    }
}

async fn activate_capture_binding(
    cx: &RuntimeLoopCx,
    authority: &CaptureAuthority,
    global_pane_id: u64,
    generation: u32,
    capture_metadata: &Arc<RwLock<HashMap<PaneIncarnation, CapturePaneMetadata>>>,
    metadata: CapturePaneMetadata,
) -> Result<ActiveCaptureBinding> {
    let capture_revision = CaptureRevision::new(metadata.discovery_revision.get()).ok_or_else(|| {
        runtime_backend_error(
            "capture.transition.activate",
            format!("pane {global_pane_id} has an invalid zero discovery revision"),
        )
    })?;
    let identity = authority.activate_pane_for_revision(global_pane_id, capture_revision)?;
    let pane_uuid = metadata.pane_uuid.clone();
    let revision = metadata.discovery_revision;
    capture_metadata
        .write()
        .await
        .insert(identity.pane_incarnation(), metadata);
    let polling_lease = match authority.issue_source(identity, CaptureSourceKind::Polling) {
        Ok(lease) => lease,
        Err(error) => {
            if rollback_empty_capture_binding(cx, authority, identity).await {
                capture_metadata
                    .write()
                    .await
                    .remove(&identity.pane_incarnation());
            }
            return Err(error.into());
        }
    };

    Ok(ActiveCaptureBinding {
        generation,
        pane_uuid,
        revision,
        identity,
        polling_lease,
        resync_receipt: None,
        #[cfg(feature = "native-wezterm")]
        native_lease: None,
        #[cfg(all(feature = "vendored", unix))]
        streaming_lease: None,
    })
}

fn enable_native_capture_source(
    authority: &CaptureAuthority,
    binding: &mut ActiveCaptureBinding,
    native_enabled: bool,
) -> Result<()> {
    #[cfg(feature = "native-wezterm")]
    {
        if native_enabled && binding.native_lease.is_none() {
            binding.native_lease = Some(
                authority.issue_source(binding.identity, CaptureSourceKind::NativePush)?,
            );
        }
    }
    #[cfg(not(feature = "native-wezterm"))]
    {
        let _ = (authority, binding, native_enabled);
    }
    Ok(())
}

async fn retire_or_quarantine_capture_binding(
    authority: &CaptureAuthority,
    capture_metadata: &Arc<RwLock<HashMap<PaneIncarnation, CapturePaneMetadata>>>,
    backpressure: &BackpressureMetrics,
    draining_bindings: &mut HashMap<u64, ActiveCaptureBinding>,
    draining_since: &mut HashMap<u64, Instant>,
    binding: ActiveCaptureBinding,
    context: &'static str,
) -> bool {
    let pane_id = binding.identity.global_pane_id();
    match authority.retire_pane_if_drained(binding.identity) {
        Ok(true) => {
            draining_since.remove(&pane_id);
            capture_metadata
                .write()
                .await
                .remove(&binding.identity.pane_incarnation());
            // Per-pane attribution is generation-scoped even though its
            // storage key is the muxer's reusable numeric pane ID. Exact
            // authority drain proves no predecessor producer can recreate the
            // entry after this cleanup; a successor therefore starts at zero.
            let _ = backpressure.cleanup_pane(pane_id);
            true
        }
        Ok(false) => {
            debug!(
                pane_id,
                context,
                "Capture binding quarantined while exact revocation drains"
            );
            let replaced = draining_bindings.insert(pane_id, binding);
            debug_assert!(replaced.is_none(), "one exact draining binding per pane");
            draining_since.entry(pane_id).or_insert_with(Instant::now);
            false
        }
        Err(error) => {
            error!(
                pane_id,
                context,
                error = %error,
                "Capture binding retirement failed; exact binding quarantined"
            );
            let replaced = draining_bindings.insert(pane_id, binding);
            debug_assert!(replaced.is_none(), "one exact draining binding per pane");
            draining_since.entry(pane_id).or_insert_with(Instant::now);
            false
        }
    }
}

async fn load_capture_checkpoint_from_storage(
    cx: &RuntimeLoopCx,
    storage: &StorageHandle,
    pane_id: u64,
    revision: DiscoveryRevision,
) -> Result<CaptureDurabilityCheckpoint> {
    let max_seq = storage.get_max_seq_with_cx(cx, pane_id).await?;
    let next_seq = match max_seq {
        Some(seq) => seq.checked_add(1).ok_or_else(|| {
            runtime_backend_error(
                "capture.transition.checkpoint",
                format!("pane {pane_id} durable sequence exhausted"),
            )
        })?,
        None => 0,
    };
    let segments = storage
        .get_segments_with_cx(cx, pane_id, RESUME_ANCHOR_SEGMENT_LIMIT)
        .await?;
    Ok(CaptureDurabilityCheckpoint {
        revision,
        next_seq,
        raw_tail: assemble_resume_anchor(segments),
    })
}

async fn recover_capture_checkpoint(
    cx: &RuntimeLoopCx,
    storage: &StorageHandle,
    checkpoints: &CaptureCheckpointCache,
    pane_id: u64,
    predecessor_revision: DiscoveryRevision,
) -> Result<CaptureDurabilityCheckpoint> {
    if let Some(checkpoint) =
        certain_capture_checkpoint(checkpoints, pane_id, predecessor_revision)
    {
        return Ok(checkpoint);
    }
    load_capture_checkpoint_from_storage(cx, storage, pane_id, predecessor_revision).await
}

/// Install the storage-proven baseline for a pane that has not yet been
/// exposed to capture in this runtime.
///
/// All fallible guards are acquired before mutation.  If a cursor already
/// exists, a transition coordinator owns it and this setup must not overwrite
/// its potentially newer in-flight state; that coordinator will perform the
/// exact drain/reset before exposing the new revision.
async fn initialize_capture_state_from_checkpoint(
    pane_id: u64,
    checkpoint: &CaptureDurabilityCheckpoint,
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
    detection_contexts: &Arc<RwLock<HashMap<u64, DetectionContext>>>,
    pane_activity_tracker: &Arc<RwLock<HashMap<u64, PaneActivityState>>>,
    checkpoints: &CaptureCheckpointCache,
) -> Result<bool> {
    let mut cursor_guard = cursors.write().await;
    let mut context_guard = detection_contexts.write().await;
    let mut activity_guard = pane_activity_tracker.write().await;
    let mut cache = checkpoints.lock().map_err(|_| {
        runtime_backend_error(
            "capture.transition.checkpoint",
            "capture durability checkpoint cache is poisoned",
        )
    })?;
    if cursor_guard.contains_key(&pane_id) {
        return Ok(false);
    }

    let cursor = PaneCursor::from_seq(pane_id, checkpoint.next_seq)
        .with_resume_anchor(checkpoint.raw_tail.clone());
    let mut context = DetectionContext::new();
    context.pane_id = Some(pane_id);
    let _ = cache.put(
        pane_id,
        CachedCaptureCheckpoint::Certain(checkpoint.clone()),
    );
    cursor_guard.insert(pane_id, cursor);
    context_guard.insert(pane_id, context);
    activity_guard.remove(&pane_id);
    Ok(true)
}

async fn reset_capture_state_from_checkpoint(
    pane_id: u64,
    desired_revision: DiscoveryRevision,
    checkpoint: &CaptureDurabilityCheckpoint,
    preserve_durable_anchor: bool,
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
    detection_contexts: &Arc<RwLock<HashMap<u64, DetectionContext>>>,
    pane_activity_tracker: &Arc<RwLock<HashMap<u64, PaneActivityState>>>,
    checkpoints: &CaptureCheckpointCache,
) -> Result<()> {
    let replacement_raw_tail = if preserve_durable_anchor {
        checkpoint.raw_tail.clone()
    } else {
        String::new()
    };
    let replacement_cursor = if preserve_durable_anchor {
        PaneCursor::from_seq(pane_id, checkpoint.next_seq)
            .with_resume_anchor(replacement_raw_tail.clone())
    } else {
        PaneCursor::from_seq(pane_id, checkpoint.next_seq)
    };
    let mut replacement_context = DetectionContext::new();
    replacement_context.pane_id = Some(pane_id);
    // Acquire every fallible/awaiting guard before mutating any member.  Once
    // all four guards are held, the commit below contains no await and cannot
    // return a partial cursor/context/activity reset merely because the
    // checkpoint cache was poisoned.
    let mut cursor_guard = cursors.write().await;
    let mut context_guard = detection_contexts.write().await;
    let mut activity_guard = pane_activity_tracker.write().await;
    let mut cache = checkpoints.lock().map_err(|_| {
        runtime_backend_error(
            "capture.transition.checkpoint",
            "capture durability checkpoint cache is poisoned",
        )
    })?;
    let _ = cache.put(
        pane_id,
        CachedCaptureCheckpoint::Certain(CaptureDurabilityCheckpoint {
            revision: desired_revision,
            next_seq: checkpoint.next_seq,
            raw_tail: replacement_raw_tail,
        }),
    );
    cursor_guard.insert(pane_id, replacement_cursor);
    context_guard.insert(pane_id, replacement_context);
    activity_guard.remove(&pane_id);
    Ok(())
}

async fn emit_capture_generation_resync(
    cx: &RuntimeLoopCx,
    source: &WeztermHandleSource,
    capture_tx: &mpsc::Sender<CaptureEvent>,
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
    binding: &ActiveCaptureBinding,
) -> Result<CaptureResyncReceipt> {
    let pane_id = binding.identity.global_pane_id();
    // Reserve bounded-queue capacity before acquiring authority or mutating
    // the cursor.  Backpressure therefore cannot create a speculative hole.
    let permit = runtime_timeout(
        cx,
        CAPTURE_TRANSITION_QUEUE_TIMEOUT,
        capture_tx.reserve(cx),
    )
    .await
    .map_err(|failure| match failure {
        RuntimeTimeoutFailure::Elapsed => runtime_backend_error(
            "capture.transition.reserve",
            format!("pane {pane_id} capture ingress stayed full"),
        ),
        RuntimeTimeoutFailure::Context(_) => runtime_cx_error(
            "capture.transition.reserve",
            cx,
            "capability context failed while reserving capture ingress",
        ),
    })?
    .map_err(|error| runtime_backend_error("capture.transition.reserve", error))?;
    let producer_guard = binding.polling_lease.try_acquire_producer(
        binding.polling_lease.stamp(),
        pane_id,
    )?;
    let text = runtime_timeout(
        cx,
        Duration::from_secs(2),
        source.get_text_with_cx(cx, pane_id, false),
    )
    .await
    .map_err(|failure| match failure {
        RuntimeTimeoutFailure::Elapsed => runtime_backend_error(
            "capture.transition.snapshot",
            format!("pane {pane_id} resync snapshot timed out"),
        ),
        RuntimeTimeoutFailure::Context(_) => runtime_cx_error(
            "capture.transition.snapshot",
            cx,
            "capability context failed while reading resync snapshot",
        ),
    })??;
    let segment = {
        let mut cursor_guard = cursors.write_with_cx(cx).await.map_err(|_| {
            runtime_cx_error(
                "capture.transition.cursor",
                cx,
                "cursor lock acquisition failed during capture resync",
            )
        })?;
        cx.checkpoint().map_err(|_| {
            runtime_cx_error(
                "capture.transition.cursor",
                cx,
                "capability checkpoint failed before capture resync mutation",
            )
        })?;
        let cursor = cursor_guard.get_mut(&pane_id).ok_or_else(|| {
            runtime_backend_error(
                "capture.transition.cursor",
                format!("pane {pane_id} has no reset cursor"),
            )
        })?;
        cursor.capture_generation_resync(&text, "capture_generation_resync")
    };
    let (resync_decision, resync_receipt) = CaptureResyncDecision::channel();
    let event = CaptureEvent::from_producer(segment, &producer_guard)?
        .with_resync_decision(resync_decision);
    permit.send(event);
    drop(producer_guard);
    Ok(resync_receipt)
}

#[cfg(all(feature = "vendored", unix))]
fn allocate_streaming_task_token(next: &mut u64) -> Option<StreamingTaskToken> {
    let token = StreamingTaskToken(*next);
    *next = (*next).checked_add(1)?;
    Some(token)
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

#[cfg(all(test, feature = "vendored", unix))]
fn vendored_streaming_identity_for_pane(
    socket_paths: &[PathBuf],
    global_pane_id: u64,
    generation: u32,
    capture_stamp: crate::capture_authority::CaptureStamp,
) -> Option<StreamingSubscriptionIdentity> {
    let (socket_shard, local_pane_id, socket_path) =
        vendored_streaming_route_for_pane(socket_paths, global_pane_id)?;
    Some(StreamingSubscriptionIdentity {
        global_pane_id,
        local_pane_id,
        socket_shard,
        socket_path,
        generation,
        capture_stamp,
    })
}

#[cfg(all(feature = "vendored", unix))]
fn vendored_streaming_route_for_pane(
    socket_paths: &[PathBuf],
    global_pane_id: u64,
) -> Option<(ShardId, u64, PathBuf)> {
    if socket_paths.is_empty() {
        return None;
    }
    if socket_paths.len() == 1 {
        let (shard_id, local_pane_id) = try_decode_sharded_pane_id(global_pane_id).ok()?;
        if shard_id != ShardId(0) {
            return None;
        }
        return Some((ShardId(0), local_pane_id, socket_paths[0].clone()));
    }
    let (shard_id, local_pane_id) = try_decode_sharded_pane_id(global_pane_id).ok()?;
    socket_paths
        .get(shard_id.0)
        .cloned()
        .map(|socket_path| (shard_id, local_pane_id, socket_path))
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
    ///
    /// Poison-recovery (parity with the `unwrap_or_else(|e| e.into_inner())`
    /// pattern used for every global-state lock in runtime_telemetry.rs): a
    /// poisoned lock previously caused `if let Ok` to SILENTLY DROP the update,
    /// taking lock/memory telemetry permanently dark with no signal. The write
    /// critical section is a single infallible assignment, so the guarded data
    /// is never torn — recover the guard and apply the update instead of losing
    /// it.
    pub fn update_global(snapshot: Self) {
        let lock = GLOBAL_RUNTIME_LOCK_MEMORY_TELEMETRY.get_or_init(|| StdRwLock::new(None));
        // ft-lo0ip: recover a poisoned lock AND record it (counter + warn) so
        // the update still lands and the poison is observable, not silent.
        let mut guard = lock
            .write()
            .unwrap_or_else(record_lock_memory_telemetry_poison_and_recover);
        *guard = Some(snapshot);
    }

    /// Get the latest lock/memory telemetry snapshot.
    ///
    /// Poison-recovery sibling of [`Self::update_global`]: a poisoned lock
    /// previously made `.ok()` return `None`, masking a present snapshot as
    /// "no telemetry". Recover the guard and return the value.
    #[must_use]
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_RUNTIME_LOCK_MEMORY_TELEMETRY.get_or_init(|| StdRwLock::new(None));
        // ft-lo0ip: recover a poisoned lock AND record it so a present snapshot
        // is returned (not masked as None) and the poison is observable.
        let guard = lock
            .read()
            .unwrap_or_else(record_lock_memory_telemetry_poison_and_recover);
        guard.clone()
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
    /// Count of queued capture events rejected by incarnation/source fencing.
    capture_authority_rejections: ShardedCounter,
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
    /// Last observed capture pipeline queue depth.
    capture_queue_depth: AtomicUsize,
    /// ft-u6zfw: crash-loop detector fed by observed agent/pane restarts so
    /// `HealthSnapshot` surfaces real crash loops instead of hardcoded healthy
    /// zeros. Fed from the discovery tick's `new_generations` (panes that
    /// respawned with a new generation) — the runtime's available restart
    /// signal — so managed-agent crash loops become visible to `ft status` /
    /// robot health, which were previously invisible. A process-level
    /// watcher-restart source would need cross-process persistence (follow-up).
    crash_detector: StdMutex<crate::crash::CrashLoopDetector>,
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
            capture_authority_rejections: ShardedCounter::new(),
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
            capture_queue_depth: AtomicUsize::new(0),
            crash_detector: StdMutex::new(crate::crash::CrashLoopDetector::new(
                crate::crash::CrashLoopConfig::default(),
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
    /// ft-u6zfw: feed observed agent/pane restarts (panes that gained a new
    /// generation this discovery tick) into the crash-loop detector. `now_secs`
    /// is wall-clock epoch seconds. Recording each restart lets the detector's
    /// windowed `is_crash_loop` + `restart_count` reflect real respawn storms.
    pub fn record_observed_restarts(&self, restarts: usize, now_secs: u64) {
        if restarts == 0 {
            return;
        }
        if let Ok(mut detector) = self.crash_detector.lock() {
            for _ in 0..restarts {
                detector.record_crash(now_secs);
            }
        }
    }

    /// ft-u6zfw: note a discovery tick with no restarts — a clean run that
    /// resets the consecutive-crash counter (the windowed `restart_count` /
    /// `in_crash_loop` still age out on their own).
    pub fn note_clean_observation(&self) {
        if let Ok(mut detector) = self.crash_detector.lock() {
            detector.record_success();
        }
    }

    /// ft-u6zfw: crash-loop diagnostics for `HealthSnapshot`. Falls back to a
    /// healthy default if the detector lock is poisoned rather than panicking on
    /// the snapshot publish path.
    #[must_use]
    pub fn crash_loop_diagnostics(&self) -> crate::crash::CrashLoopDiagnostics {
        self.crash_detector.lock().map_or_else(
            |_| crate::crash::CrashLoopDiagnostics {
                restart_count: 0,
                last_crash_at: None,
                consecutive_crashes: 0,
                current_backoff_ms: 0,
                in_crash_loop: false,
            },
            |detector| detector.diagnostics(),
        )
    }

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

    /// Record the latest capture pipeline depth observed by the relay task.
    pub fn record_capture_queue_depth(&self, depth: usize) {
        self.capture_queue_depth.store(depth, Ordering::Relaxed);
    }

    /// Last capture pipeline depth observed by the relay task.
    #[must_use]
    pub fn capture_queue_depth(&self) -> usize {
        self.capture_queue_depth.load(Ordering::Relaxed)
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

    /// Get queued capture events rejected before semantic side effects.
    pub fn capture_authority_rejections(&self) -> u64 {
        self.capture_authority_rejections.get()
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
    /// Sole runtime authority for pane incarnations and capture source epochs.
    capture_authority: CaptureAuthority,
    /// Immutable metadata keyed by runtime-monotonic pane incarnation.
    /// Entries outlive registry replacement until producer and persistence
    /// guards for the predecessor have drained.
    capture_metadata: Arc<RwLock<HashMap<PaneIncarnation, CapturePaneMetadata>>>,
    // LOCK ORDER (deadlock-avoidance doctrine): when more than one of the
    // three per-pane maps below is held at once, they MUST be acquired in this
    // declaration order — `cursors` → `detection_contexts` →
    // `pane_activity_tracker`. Every multi-lock site already obeys it (e.g.
    // `remove_runtime_pane_state_for_panes` and the checkpoint initialization
    // and reset helpers). Acquiring them in a different order in a new call
    // site would risk a lock-order-inversion deadlock under concurrency; add
    // new multi-map critical sections in this order, or take one lock at a time.
    /// Per-pane cursors for delta extraction. Lock-order rank 1 (acquire first).
    cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
    /// Per-pane detection contexts for deduplication. Lock-order rank 2.
    detection_contexts: Arc<RwLock<HashMap<u64, DetectionContext>>>,
    /// Best-effort per-pane output activity tracker for health snapshots.
    /// Lock-order rank 3 (acquire last).
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
    /// Runtime-owned inbound connector bridge feeding the live event bus.
    connector_inbound_bridge: Option<Arc<StdMutex<ConnectorInboundBridge>>>,
    /// Configuration used when constructing the inbound connector bridge.
    ///
    /// Carries the operator ingress classifier settings/policies (ft-pzxsr);
    /// set via [`Self::with_connector_inbound_bridge_config`] before or after
    /// [`Self::with_event_bus`].
    connector_inbound_bridge_config: ConnectorInboundBridgeConfig,
    /// Runtime-owned outbound connector bridge draining live EventBus traffic.
    connector_outbound_bridge: Option<Arc<StdMutex<ConnectorOutboundBridge>>>,
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

        // Seed the hot-reload channel from the exact validated startup policy;
        // waiting for a later file-change notification would silently run the
        // built-in tier defaults during the most important first cleanup.
        let hot_config = initial_hot_reloadable_config(&config);
        let (config_tx, config_rx) = watch::channel(hot_config);

        Self {
            config,
            wezterm_handle: wezterm_handle_with_timeout(5),
            storage,
            pattern_engine,
            registry: Arc::new(RwLock::new(registry)),
            capture_authority: CaptureAuthority::new(),
            capture_metadata: Arc::new(RwLock::new(HashMap::new())),
            cursors: Arc::new(RwLock::new(HashMap::new())),
            detection_contexts: Arc::new(RwLock::new(HashMap::new())),
            pane_activity_tracker: Arc::new(RwLock::new(HashMap::new())),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            metrics,
            config_tx: Arc::new(config_tx),
            config_rx,
            event_bus: None,
            connector_inbound_bridge: None,
            connector_inbound_bridge_config: ConnectorInboundBridgeConfig::default(),
            connector_outbound_bridge: None,
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
        self.connector_inbound_bridge = Some(Arc::new(StdMutex::new(ConnectorInboundBridge::new(
            Arc::clone(&event_bus),
            self.connector_inbound_bridge_config.clone(),
        ))));
        self.connector_outbound_bridge.get_or_insert_with(|| {
            Arc::new(StdMutex::new(ConnectorOutboundBridge::new(
                ConnectorOutboundBridgeConfig::default(),
            )))
        });
        self.event_bus = Some(event_bus);
        self
    }

    /// Configure the runtime-owned inbound connector bridge.
    ///
    /// This is how operator `[safety.data_classifier]` settings and
    /// classification policies reach the connector ingress path (ft-pzxsr):
    /// pass a config whose `classifier` field was built from the operator
    /// `SafetyConfig`. Call before [`Self::start`]; order relative to
    /// [`Self::with_event_bus`] does not matter — if the bridge already
    /// exists it is rebuilt with the new config.
    #[must_use]
    pub fn with_connector_inbound_bridge_config(
        mut self,
        config: ConnectorInboundBridgeConfig,
    ) -> Self {
        self.connector_inbound_bridge_config = config;
        if self.connector_inbound_bridge.is_some()
            && let Some(event_bus) = self.event_bus.as_ref()
        {
            self.connector_inbound_bridge =
                Some(Arc::new(StdMutex::new(ConnectorInboundBridge::new(
                    Arc::clone(event_bus),
                    self.connector_inbound_bridge_config.clone(),
                ))));
        }
        self
    }

    /// Configure the runtime-owned outbound connector bridge.
    ///
    /// Call before [`Self::start`] so the spawned production subscriber uses
    /// this bridge config from its first EventBus receive.
    #[must_use]
    pub fn with_connector_outbound_bridge_config(
        mut self,
        config: ConnectorOutboundBridgeConfig,
    ) -> Self {
        self.connector_outbound_bridge = Some(Arc::new(StdMutex::new(
            ConnectorOutboundBridge::new(config),
        )));
        self
    }

    /// Register an outbound connector routing rule on the runtime-owned bridge.
    ///
    /// If no bridge was configured yet, a default bridge is created. The rule is
    /// then consumed by the production EventBus subscriber spawned by
    /// [`Self::start`] when this runtime also has an event bus.
    #[must_use]
    pub fn with_connector_outbound_rule(self, rule: OutboundRoutingRule) -> Self {
        let mut runtime = self;
        let bridge = runtime.connector_outbound_bridge.get_or_insert_with(|| {
            Arc::new(StdMutex::new(ConnectorOutboundBridge::new(
                ConnectorOutboundBridgeConfig::default(),
            )))
        });
        match bridge.lock() {
            Ok(mut guard) => guard.add_rule(rule),
            Err(_) => {
                warn!("connector outbound bridge lock poisoned while registering routing rule");
            }
        }
        runtime
    }

    /// Route an inbound connector signal into the runtime's live event bus.
    ///
    /// This is the production ingress path for `ConnectorInboundBridge`: callers
    /// feed connector webhooks, streams, polls, and lifecycle signals here after
    /// constructing the observation runtime with [`Self::with_event_bus`].
    pub fn route_connector_signal(
        &self,
        signal: &ConnectorSignal,
    ) -> std::result::Result<BridgeRouteResult, ConnectorBridgeError> {
        route_connector_signal_through_bridge(self.connector_inbound_bridge.as_ref(), signal)
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
    /// ft-tr5a0 Cx-first sibling of [`Self::start`].
    ///
    /// Pre-flight checkpoint gate: if the caller's cx is already cancelled
    /// the runtime startup is short-circuited before any task spawns. The
    /// runtime's internal sub-tasks then run under their own request-cx
    /// (see runtime_loop_cx() in shutdown_with_summary), so the seal is
    /// preserved at every boundary the runtime crosses.
    // This source-level `async fn` identity is part of runtime-proof coverage.
    // Startup currently completes synchronously once polled; adding an
    // artificial yield would change task-admission and cancellation ordering.
    #[allow(clippy::unused_async_trait_impl)]
    pub async fn start_with_cx(&mut self, cx: &crate::cx::Cx) -> Result<RuntimeHandle> {
        cx.checkpoint()
            .map_err(|_| {
                runtime_cx_error(
                    "runtime.start",
                    cx,
                    "capability checkpoint failed before runtime startup",
                )
            })?;
        self.start_impl()
    }

    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<RuntimeHandle> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.start_with_cx(&cx).await
    }

    fn start_impl(&self) -> Result<RuntimeHandle> {
        info!("Starting observation runtime");

        // Stage 1 ingress: multi-producer capture tasks write into bounded MPSC.
        let (capture_ingress_tx, capture_ingress_rx) =
            mpsc::channel::<CaptureEvent>(self.config.channel_buffer);
        // Stage 2 handoff: single relay task forwards ingress into lock-free SPSC
        // consumed by the persistence task.
        let (capture_ring_tx, capture_ring_rx) =
            spsc_channel::<CaptureEvent>(self.config.channel_buffer);
        // Discovery publishes immutable, revision-stamped pending and ready
        // views around its fallible identity work. Cursor/checkpoint setup is
        // deliberately deferred to capture after exact predecessor drain. A
        // watch channel retains the latest view across startup scheduling and
        // coalesces superseded ticks without losing transition descriptors.
        let (discovery_publication_tx, discovery_publication_rx) =
            watch::channel(DiscoveryCapturePublication::default());
        // The hot path resets a successor cursor from the most recent
        // durability-confirmed raw tail without touching SQLite.  Eviction is
        // safe: transitions fall back to a strict storage read.
        let capture_checkpoints: CaptureCheckpointCache = Arc::new(StdMutex::new(LruCache::new(
            self.config.channel_buffer.max(1),
        )));

        // Clone ingress sender for queue depth instrumentation before moving it.
        let capture_tx_probe = capture_ingress_tx.clone();
        let capture_queue_capacity = capture_ingress_tx
            .capacity()
            .saturating_add(capture_ring_tx.capacity())
            .saturating_add(1);

        // Spawn discovery task
        let discovery_handle = self.spawn_discovery_task(discovery_publication_tx);

        let native_socket = self.config.native_event_socket.clone();

        // Always spawn the capture task. It owns the polling supervisor and,
        // when vendored direct-mux sockets are configured, per-pane streaming
        // subscriptions. Native push events can still feed the same ingress
        // channel in parallel, with polling remaining the safety net.
        let capture_handle = self.spawn_capture_task(
            capture_ingress_tx.clone(),
            discovery_publication_rx.clone(),
            Arc::clone(&capture_checkpoints),
        );

        // Spawn the native event listener only when configuration explicitly
        // enabled it and resolution therefore produced a socket path. The
        // resolver deliberately ignores environment overrides while disabled.
        #[cfg(feature = "native-wezterm")]
        let native_handle = native_socket.map(|socket| {
            info!(
                path = %socket.display(),
                "Starting explicitly enabled native event listener"
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
            discovery_publication_rx,
            capture_checkpoints,
        );

        // Spawn maintenance task
        let maintenance_handle = self.spawn_maintenance_task(capture_queue_capacity);

        // Spawn outbound connector bridge task. It is dormant unless both an
        // EventBus and runtime-owned bridge exist.
        let connector_outbound = self.spawn_connector_outbound_task();

        // Spawn snapshot engine task (session persistence) if configured
        let (
            snapshot_handle,
            snapshot_shutdown_tx,
            snapshot_triggers,
            snapshot_shutdown_clean,
            snapshot_engine,
            snapshot_scheduler_status,
            snapshot_shutdown_requested,
        ) =
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
                    let snapshot_shutdown_clean = Arc::new(AtomicBool::new(false));
                    let snapshot_scheduler_status =
                        Arc::new(AtomicU8::new(SNAPSHOT_SCHEDULER_RUNNING));
                    let task_snapshot_scheduler_status = Arc::clone(&snapshot_scheduler_status);
                    let snapshot_shutdown_requested = Arc::new(AtomicBool::new(false));
                    let task_snapshot_shutdown_requested =
                        Arc::clone(&snapshot_shutdown_requested);
                    let scheduler_engine = Arc::clone(&engine);

                    let loop_cx = runtime_loop_cx();
                    let handle = spawn_runtime_task(&loop_cx, move |task_cx| async move {
                        let pane_provider_cx = task_cx.clone();
                        let scheduler_wezterm = wezterm.clone();
                        let scheduler_result = scheduler_engine
                            .run_periodic_with_cx(&task_cx, shutdown_rx, move || {
                                let wez = scheduler_wezterm.clone();
                                let pane_provider_cx = pane_provider_cx.clone();
                                async move {
                                    wez.list_panes_with_cx(&pane_provider_cx)
                                        .await
                                        .map_err(|error| {
                                            crate::snapshot_engine::SnapshotError::PaneList(
                                                error.to_string(),
                                            )
                                        })
                                }
                            })
                            .await;
                        let shutdown_was_requested =
                            task_snapshot_shutdown_requested.load(Ordering::Acquire);
                        let scheduler_status = match scheduler_result {
                            Ok(()) if shutdown_was_requested => {
                                info!("snapshot scheduler acknowledged runtime shutdown");
                                SNAPSHOT_SCHEDULER_SHUTDOWN_ACKNOWLEDGED
                            }
                            Ok(()) => {
                                error!(
                                    "snapshot scheduler returned before runtime shutdown was requested"
                                );
                                SNAPSHOT_SCHEDULER_UNEXPECTED_RETURN
                            }
                            Err(error) => {
                                error!(
                                    error = %error,
                                    shutdown_was_requested,
                                    "snapshot scheduler failed before publishing a clean shutdown acknowledgement"
                                );
                                SNAPSHOT_SCHEDULER_FAILED
                            }
                        };
                        task_snapshot_scheduler_status.store(scheduler_status, Ordering::Release);
                    });
                    info!("Snapshot engine started");
                    (
                        Some(handle),
                        Some(shutdown_tx),
                        snapshot_triggers,
                        Some(snapshot_shutdown_clean),
                        Some(engine),
                        Some(snapshot_scheduler_status),
                        Some(snapshot_shutdown_requested),
                    )
                } else {
                    (None, None, None, None, None, None, None)
                }
            } else {
                (None, None, None, None, None, None, None)
            };

        info!("Observation runtime started");

        Ok(RuntimeHandle {
            discovery: Some(discovery_handle),
            capture: Some(capture_handle),
            relay: Some(relay_handle),
            persistence: Some(persistence_handle),
            maintenance: Some(maintenance_handle),
            connector_outbound,
            snapshot: snapshot_handle,
            snapshot_triggers,
            snapshot_shutdown: snapshot_shutdown_tx,
            snapshot_shutdown_clean,
            snapshot_engine,
            snapshot_scheduler_status,
            snapshot_shutdown_requested,
            shutdown_flag: Arc::clone(&self.shutdown_flag),
            storage: self.storage.clone(),
            metrics: Arc::clone(&self.metrics),
            registry: Arc::clone(&self.registry),
            cursors: Arc::clone(&self.cursors),
            pane_activity_tracker: Arc::clone(&self.pane_activity_tracker),
            start_time: Instant::now(),
            config_tx: Arc::clone(&self.config_tx),
            event_bus: self.event_bus.clone(),
            connector_inbound_bridge: self.connector_inbound_bridge.clone(),
            heartbeats: Arc::clone(&self.heartbeats),
            capture_tx: capture_tx_probe,
            capture_queue_capacity,
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
            let started_at = crate::runtime_async::timer_now_with_cx(&loop_cx);
            let mut last_activity = started_at;
            let mut last_idle_trigger = started_at;
            let mut last_memory_trigger = None;

            loop {
                if shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }
                if loop_cx.checkpoint().is_err() {
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
                            if shutdown_flag.load(Ordering::SeqCst) {
                                break;
                            }
                            if loop_cx.checkpoint().is_err() {
                                record_runtime_wait_failure(
                                    "snapshot_trigger_bridge_event",
                                    runtime_context_failure_kind(&loop_cx),
                                );
                                break;
                            }
                            tick_only = false;
                            match recv {
                                Ok(event) => {
                                    if event_counts_as_activity(&event) {
                                        last_activity =
                                            crate::runtime_async::timer_now_with_cx(&loop_cx);
                                    }
                                    if let Some(trigger) = snapshot_trigger_from_event(&event) {
                                        if loop_cx.checkpoint().is_err() {
                                            record_runtime_wait_failure(
                                                "snapshot_trigger_bridge_emit",
                                                runtime_context_failure_kind(&loop_cx),
                                            );
                                            break;
                                        }
                                        if !snapshot_engine.emit_trigger(trigger) {
                                            debug!(
                                                trigger = ?trigger,
                                                event_type = event.type_name(),
                                                "snapshot trigger dropped (queue full or inactive)"
                                            );
                                        }
                                    }
                                }
                                Err(crate::events::RecvError::Lagged { missed_count }) => {
                                    last_activity =
                                        crate::runtime_async::timer_now_with_cx(&loop_cx);
                                    warn!(
                                        missed = missed_count,
                                        "snapshot trigger bridge lagged on event bus"
                                    );
                                }
                                Err(crate::events::RecvError::Cancelled) => {
                                    debug!("snapshot trigger bridge subscriber cancelled");
                                    subscriber_closed = true;
                                }
                                Err(crate::events::RecvError::Closed) => {
                                    subscriber_closed = true;
                                }
                            }
                        }
                        Err(RuntimeTimeoutFailure::Context(failure)) => {
                            record_runtime_wait_failure(
                                "snapshot_trigger_bridge_recv",
                                failure,
                            );
                            break;
                        }
                        Err(RuntimeTimeoutFailure::Elapsed) => {}
                    }
                } else if let Err(failure) = runtime_sleep(
                        &loop_cx,
                        Duration::from_secs(SNAPSHOT_TRIGGER_BRIDGE_TICK_SECS),
                    )
                    .await
                {
                    record_runtime_wait_failure("snapshot_trigger_bridge", failure);
                    break;
                }

                if shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }
                if loop_cx.checkpoint().is_err() {
                    record_runtime_wait_failure(
                        "snapshot_trigger_bridge_tick",
                        runtime_context_failure_kind(&loop_cx),
                    );
                    break;
                }

                if subscriber_closed {
                    subscriber = None;
                }

                if !tick_only {
                    continue;
                }

                let now = crate::runtime_async::timer_now_with_cx(&loop_cx);

                if idle_enabled
                    && runtime_time_elapsed_at_least(
                        now,
                        last_activity,
                        Duration::from_secs(SNAPSHOT_IDLE_WINDOW_SECS),
                    )
                    && runtime_time_elapsed_at_least(
                        now,
                        last_idle_trigger,
                        Duration::from_secs(SNAPSHOT_IDLE_WINDOW_SECS),
                    )
                {
                    let observed_panes = {
                        let reg = registry.read().await;
                        reg.observed_pane_ids().len()
                    };
                    if loop_cx.checkpoint().is_err() {
                        record_runtime_wait_failure(
                            "snapshot_trigger_bridge_idle_emit",
                            runtime_context_failure_kind(&loop_cx),
                        );
                        break;
                    }
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
                    && last_memory_trigger.is_none_or(|last_trigger| {
                        runtime_time_elapsed_at_least(
                            now,
                            last_trigger,
                            Duration::from_secs(SNAPSHOT_MEMORY_TRIGGER_COOLDOWN_SECS),
                        )
                    })
                {
                    if loop_cx.checkpoint().is_err() {
                        record_runtime_wait_failure(
                            "snapshot_trigger_bridge_memory_emit",
                            runtime_context_failure_kind(&loop_cx),
                        );
                        break;
                    }
                    if !snapshot_engine
                        .emit_trigger(crate::snapshot_engine::SnapshotTrigger::MemoryPressure)
                    {
                        debug!("snapshot memory-pressure trigger dropped (queue full or inactive)");
                    }
                    last_memory_trigger = Some(now);
                }
            }
        })
    }

    /// Spawn the production outbound connector bridge subscriber.
    ///
    /// The task is intentionally separated from capture/persistence: it listens
    /// to already-redacted EventBus traffic, routes eligible events through
    /// `ConnectorOutboundBridge`, drains queued connector actions into the
    /// connector host runtime, and feeds completion status back into the
    /// bridge's governor/reliability controllers.
    fn spawn_connector_outbound_task(&self) -> Option<JoinHandle<()>> {
        let event_bus = self.event_bus.clone()?;
        let bridge = self.connector_outbound_bridge.clone()?;
        let shutdown_flag = Arc::clone(&self.shutdown_flag);

        let loop_cx = runtime_loop_cx();
        Some(spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let mut subscriber = event_bus.subscribe();

            loop {
                if shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }
                if loop_cx.checkpoint().is_err() {
                    record_runtime_wait_failure(
                        "connector_outbound_bridge",
                        runtime_context_failure_kind(&loop_cx),
                    );
                    break;
                }

                match runtime_timeout(
                    &loop_cx,
                    Duration::from_millis(CONNECTOR_OUTBOUND_BRIDGE_TICK_MS),
                    subscriber.recv(),
                )
                .await
                {
                    Ok(Ok(event)) => {
                        if shutdown_flag.load(Ordering::SeqCst) {
                            break;
                        }
                        if loop_cx.checkpoint().is_err() {
                            record_runtime_wait_failure(
                                "connector_outbound_bridge_event",
                                runtime_context_failure_kind(&loop_cx),
                            );
                            break;
                        }
                        let now_ms = epoch_ms_u64();
                        match bridge.lock() {
                            Ok(mut guard) => {
                                if shutdown_flag.load(Ordering::SeqCst) {
                                    break;
                                }
                                if loop_cx.checkpoint().is_err() {
                                    record_runtime_wait_failure(
                                        "connector_outbound_bridge_dispatch",
                                        runtime_context_failure_kind(&loop_cx),
                                    );
                                    break;
                                }
                                process_connector_outbound_runtime_event(
                                    &mut guard, &event, now_ms,
                                );
                            }
                            Err(_) => warn!(
                                event_type = event.type_name(),
                                "connector outbound bridge lock poisoned; dropping runtime event"
                            ),
                        }
                    }
                    Ok(Err(crate::events::RecvError::Lagged { missed_count })) => {
                        warn!(
                            missed = missed_count,
                            "connector outbound bridge lagged on event bus"
                        );
                    }
                    Ok(Err(crate::events::RecvError::Cancelled)) => {
                        debug!("connector outbound bridge subscriber cancelled");
                        break;
                    }
                    Ok(Err(crate::events::RecvError::Closed)) => break,
                    Err(RuntimeTimeoutFailure::Context(failure)) => {
                        record_runtime_wait_failure(
                            "connector_outbound_bridge_recv",
                            failure,
                        );
                        break;
                    }
                    Err(RuntimeTimeoutFailure::Elapsed) => {}
                }
            }
        }))
    }

    /// Spawn the maintenance task.
    fn spawn_maintenance_task(&self, capture_queue_capacity: usize) -> JoinHandle<()> {
        let storage = self.storage.clone();
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let wezterm_handle = self.wezterm_handle.clone();
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let pane_activity_tracker = Arc::clone(&self.pane_activity_tracker);
        let metrics = Arc::clone(&self.metrics);
        let scheduler_snapshot = Arc::clone(&self.scheduler_snapshot);

        let initial_retention_days = self.config.retention_days;
        let initial_retention_policy = Arc::clone(&self.config.retention_policy);
        let initial_retention_max_mb = self.config.retention_max_mb;
        let initial_checkpoint_secs = self.config.checkpoint_interval_secs;
        let initial_cache_gc_settings = self.config.gc;

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let mut retention_days = initial_retention_days;
            let mut retention_policy = initial_retention_policy;
            let mut retention_max_mb = initial_retention_max_mb;
            let mut checkpoint_secs = initial_checkpoint_secs;
            let mut cache_gc_settings = initial_cache_gc_settings;
            let initial_health_completion = Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(Instant::now);
            let health_interval = Duration::from_secs(30);
            let mut health_schedule =
                CompletionTimedSchedule::new(initial_health_completion);

            // Run maintenance every minute, but only do expensive ops when needed.
            // Keep first tick immediate to preserve prior interval behavior.
            let maintenance_interval = Duration::from_secs(60);
            let mut first_tick = true;
            let mut retention_schedule = RetentionMaintenanceSchedule::new(Instant::now());
            let mut checkpoint_schedule = CompletionTimedSchedule::new(Instant::now());
            let mut cache_gc_schedule = CompletionTimedSchedule::new(Instant::now());
            // A receipt failure must not erase the exact durable size-eviction
            // outcome. Retain it across maintenance attempts and do not admit
            // further size deletions until the bounded receipt succeeds.
            let mut pending_size_retention_receipt = None::<PendingSizeRetentionReceipt>;
            // The tiered cleanup path returns an exact finalization record when
            // updating its transactionally advanced in-progress receipt fails.
            // Preserve it across ticks and finalize that same positive row ID
            // before admitting another age-retention mutation.
            let mut pending_cleanup_receipt = None::<MaintenanceRecord>;
            let backpressure_manager = BackpressureManager::new(BackpressureConfig::default());
            let memory_pressure_monitor =
                MemoryPressureMonitor::new(MemoryPressureConfig::default());
            // Per-pane logical memory budget used to derive the fleet
            // budget-pressure dimension from PaneRegistry arena accounting on
            // each health tick (ft-6n7hs). The RSS/cgroup MemoryBudgetManager
            // is not wired into the observation runtime; the scrollback
            // coordinator is driven from the same per-pane *logical* accounting
            // sampled here, which is the eviction-relevant signal.
            let pane_budget_config = MemoryBudgetConfig::default();

            // Fleet scrollback coordinator (ft-dwjtm): evaluates fleet memory
            // pressure from queue depths and triggers warm-page eviction when
            // TieredScrollback instances are available.
            let mut fleet_coordinator = FleetScrollbackCoordinator::new(
                CoordinatorConfig::default(),
                FleetMemoryConfig::default(),
            );
            let mut last_fleet_coordinator_maintenance_state =
                None::<FleetCoordinatorMaintenanceState>;
            let mut last_fleet_coordinator_observed_state =
                None::<FleetCoordinatorMaintenanceState>;

            loop {
                if !first_tick {
                    match runtime_sleep_until_shutdown(
                        &loop_cx,
                        shutdown_flag.as_ref(),
                        maintenance_interval,
                    )
                    .await
                    {
                        ShutdownAwareSleepOutcome::Elapsed => {}
                        ShutdownAwareSleepOutcome::ShutdownRequested => break,
                        ShutdownAwareSleepOutcome::WaitFailed(failure) => {
                            record_runtime_wait_failure("maintenance_interval", failure);
                            break;
                        }
                    }
                }
                first_tick = false;
                heartbeats.record_maintenance();

                if shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }
                if loop_cx.checkpoint().is_err() {
                    break;
                }

                // Check for config updates
                if config_update_pending(&config_rx) {
                    let new_config = config_take_update(&mut config_rx);
                    let mut retention_changed = new_config.retention_days != retention_days;
                    if retention_changed {
                        info!(
                            old = retention_days,
                            new = new_config.retention_days,
                            "Retention policy updated"
                        );
                        retention_days = new_config.retention_days;
                    }
                    if new_config.retention_policy.as_ref() != retention_policy.as_ref() {
                        info!(
                            old = retention_policy.tiers().len(),
                            new = new_config.retention_policy.tiers().len(),
                            "Retention tiers updated"
                        );
                        retention_policy = Arc::clone(&new_config.retention_policy);
                        retention_changed = true;
                    }
                    if new_config.retention_max_mb != retention_max_mb {
                        info!(
                            old = retention_max_mb,
                            new = new_config.retention_max_mb,
                            "Size-based retention cap updated"
                        );
                        retention_max_mb = new_config.retention_max_mb;
                        retention_changed = true;
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
                    if retention_changed {
                        retention_schedule.mark_due();
                    }
                }

                let now = Instant::now();

                // Run retention cleanup every hour and immediately on startup
                // or a policy/size-cap update. Failed or cancelled attempts
                // remain due, with a one-maintenance-tick retry delay.
                if retention_schedule.should_attempt(now) {
                    // Treat age- and size-retention as one policy epoch: if
                    // either component fails, retry both so a later success
                    // certifies the complete active policy. This can repeat a
                    // successful component, but never more than once per
                    // maintenance interval because the aggregate schedule
                    // applies RETENTION_MAINTENANCE_RETRY_DELAY after failure.
                    let mut retention_succeeded = true;
                    if let Some(pending) = pending_cleanup_receipt.clone() {
                        let expected_maintenance_id = pending.id;
                        let receipt_cx = crate::cx::Cx::for_request_with_budget(
                            crate::cx::Budget::MINIMAL,
                        );
                        match storage
                            .record_maintenance_with_cx(&receipt_cx, pending)
                            .await
                        {
                            Ok(maintenance_id)
                                if maintenance_id == expected_maintenance_id =>
                            {
                                pending_cleanup_receipt = None;
                                info!(
                                    maintenance_id,
                                    "Retried pending tiered-cleanup audit receipt"
                                );
                            }
                            Ok(_) => {
                                retention_succeeded = false;
                                error!(
                                    error_class = "tiered_cleanup_receipt_identity_mismatch",
                                    "Pending tiered-cleanup audit receipt returned a different identity; deferring new age-retention cleanup"
                                );
                            }
                            Err(_error) => {
                                retention_succeeded = false;
                                error!(
                                    error_class = "tiered_cleanup_receipt_retry_failed",
                                    "Pending tiered-cleanup audit receipt still failed; deferring new age-retention cleanup"
                                );
                            }
                        }
                    }
                    if let Some(pending) = pending_size_retention_receipt {
                        match record_size_retention_receipt(&storage, pending).await {
                            Ok(maintenance_id) => {
                                pending_size_retention_receipt = None;
                                info!(
                                    maintenance_id,
                                    deleted_segments = pending.outcome.deleted_segments,
                                    attempt_status = pending.status.as_str(),
                                    "Retried pending size-retention audit receipt"
                                );
                            }
                            Err(_error) => {
                                retention_succeeded = false;
                                error!(
                                    error_class = "size_retention_receipt_retry_failed",
                                    deleted_segments = pending.outcome.deleted_segments,
                                    attempt_status = pending.status.as_str(),
                                    "Pending size-retention audit receipt still failed; deferring new size eviction"
                                );
                            }
                        }
                    }
                    // ft-tkke8: tier-aware cleanup. This previously called the
                    // flat storage.retention_cleanup(cutoff) (output_segments
                    // only) plus a separate audit purge, ignoring
                    // config.retention_tiers entirely — so a configured tier
                    // policy (e.g. keep critical 90d, info 7d) was silently
                    // dropped and everything pruned at the flat retention_days.
                    // The compiled cleanup path evaluates per-tier event retention
                    // AND prunes output_segments / audit_actions / usage_metrics
                    // / notification_history at the global cutoff, threading the
                    // loop Cx for cancellation. Run whenever a flat retention OR
                    // any tier rule is active.
                    if pending_cleanup_receipt.is_none()
                        && pending_size_retention_receipt.is_none()
                        && (retention_days > 0 || !retention_policy.tiers().is_empty())
                    {
                        let outcome = crate::cleanup::cleanup_apply_with_compiled_retention_with_cx(
                            &loop_cx,
                            &storage,
                            retention_days,
                            Arc::clone(&retention_policy),
                        )
                        .await;
                        if let crate::cleanup::CleanupAuditStatus::Failed {
                            pending_record, ..
                        } = &outcome.audit
                        {
                            // Preserve failed finalizations for completed,
                            // cancelled, and failed attempts alike. The row is
                            // already durable and transactionally owns every
                            // committed deletion prefix; retry its positive ID.
                            pending_cleanup_receipt = Some(pending_record.clone());
                        }
                        match (&outcome.termination, &outcome.audit) {
                            (
                                crate::cleanup::CleanupTermination::Completed,
                                crate::cleanup::CleanupAuditStatus::Recorded { .. },
                            ) => {
                                debug!(
                                    deleted = outcome.plan.total_deleted,
                                    tables = outcome.plan.tables.len(),
                                    "Tiered retention cleanup completed"
                                );
                            }
                            (
                                crate::cleanup::CleanupTermination::Completed,
                                crate::cleanup::CleanupAuditStatus::Failed {
                                    error: _error,
                                    ..
                                },
                            ) => {
                                retention_succeeded = false;
                                error!(
                                    error_class = "tiered_cleanup_finalization_failed",
                                    deleted = outcome.plan.total_deleted,
                                    "Retention cleanup completed but its audit receipt failed"
                                );
                            }
                            (
                                crate::cleanup::CleanupTermination::Cancelled { error: _error },
                                _audit,
                            ) => {
                                retention_succeeded = false;
                                warn!(
                                    error_class = "tiered_cleanup_cancelled",
                                    reported_deleted = outcome.plan.total_deleted,
                                    "Retention cleanup cancelled; durable receipt owns committed prefix"
                                );
                            }
                            (
                                crate::cleanup::CleanupTermination::Failed { error: _error },
                                _audit,
                            ) => {
                                retention_succeeded = false;
                                error!(
                                    error_class = "tiered_cleanup_failed",
                                    reported_deleted = outcome.plan.total_deleted,
                                    "Retention cleanup failed; durable receipt owns committed prefix"
                                );
                            }
                            (
                                crate::cleanup::CleanupTermination::Completed,
                                crate::cleanup::CleanupAuditStatus::NotRequired,
                            ) => {
                                retention_succeeded = false;
                                error!(
                                    "Retention cleanup completed without its required audit receipt"
                                );
                            }
                        }
                    }

                    // Size-based retention (ft-rrqhm): evict oldest segments
                    // until the live DB size is under storage.retention_max_mb.
                    // Runs independently of retention_days so an operator who
                    // sets only a size cap (retention_days=0, retention_max_mb>0)
                    // still gets a bounded database instead of unbounded growth.
                    if retention_max_mb > 0
                        && pending_cleanup_receipt.is_none()
                        && pending_size_retention_receipt.is_none()
                    {
                        // Preserve one immutable attempt timestamp across any
                        // receipt retries; a later successful audit write must
                        // not make the original deletion appear to have
                        // happened at retry time.
                        let size_retention_attempt_timestamp = epoch_ms();
                        match storage
                            .enforce_size_limit_progress_with_cx(&loop_cx, retention_max_mb)
                            .await
                        {
                            crate::storage::DurableMutationProgress::Complete(outcome) => {
                                if outcome.deleted_segments > 0 {
                                    info!(
                                        deleted_segments = outcome.deleted_segments,
                                        used_bytes_after = outcome.used_bytes_after,
                                        cap_mb = retention_max_mb,
                                        "Size-based retention evicted oldest segments"
                                    );
                                    let pending = PendingSizeRetentionReceipt {
                                        outcome,
                                        status: SizeRetentionReceiptStatus::Completed,
                                        retention_max_mb,
                                        attempt_timestamp: size_retention_attempt_timestamp,
                                    };
                                    if let Err(_error) =
                                        record_size_retention_receipt(&storage, pending).await
                                    {
                                        retention_succeeded = false;
                                        pending_size_retention_receipt = Some(pending);
                                        error!(
                                            error_class = "size_retention_receipt_failed",
                                            deleted_segments = outcome.deleted_segments,
                                            "Completed size retention but its audit receipt failed"
                                        );
                                    }
                                }
                                if outcome.over_limit_after {
                                    warn!(
                                        cap_mb = retention_max_mb,
                                        used_bytes_after = outcome.used_bytes_after,
                                        "Database still over size cap after eviction \
                                         (non-segment data dominates)"
                                    );
                                }
                            }
                            crate::storage::DurableMutationProgress::Interrupted {
                                durable,
                                error: _error,
                            } => {
                                retention_succeeded = false;
                                let deleted_segments =
                                    durable.map_or(0, |outcome| outcome.deleted_segments);
                                let mut receipt_failed = false;
                                if let Some(outcome) =
                                    durable.filter(|value| value.deleted_segments > 0)
                                {
                                    let pending = PendingSizeRetentionReceipt {
                                        outcome,
                                        status: SizeRetentionReceiptStatus::InterruptedPartial,
                                        retention_max_mb,
                                        attempt_timestamp: size_retention_attempt_timestamp,
                                    };
                                    if let Err(_receipt_failure) =
                                        record_size_retention_receipt(&storage, pending).await
                                    {
                                        pending_size_retention_receipt = Some(pending);
                                        receipt_failed = true;
                                    }
                                }
                                error!(
                                    error_class = "size_retention_interrupted",
                                    deleted_segments,
                                    receipt_failed,
                                    "Size-based retention interrupted"
                                );
                            }
                        }
                    }
                    // Cadence and retry delays are measured from completion,
                    // not the stale pre-I/O tick instant. Large cleanup passes
                    // must not shorten the next hourly/retry window.
                    retention_schedule.finish_attempt(Instant::now(), retention_succeeded);
                }

                // Run WAL checkpoint + PRAGMA optimize (lightweight)
                if checkpoint_schedule.should_run(
                    Instant::now(),
                    Duration::from_secs(u64::from(checkpoint_secs)),
                ) {
                    match storage.checkpoint_with_cx(&loop_cx).await {
                        Ok(result) => {
                            debug!(
                                wal_pages = result.wal_pages,
                                optimized = result.optimized,
                                "WAL checkpoint completed"
                            );
                        }
                        Err(error) if is_runtime_cancellation(&error) => break,
                        Err(_error) => {
                            error!(
                                error_class = "wal_checkpoint_failed",
                                "WAL checkpoint failed"
                            );
                        }
                    }

                    // Throttle both success and failure from the actual
                    // completion boundary. Slow I/O must not shorten the next
                    // interval, and failures must not be retried every tick.
                    checkpoint_schedule.finish(Instant::now());
                }

                if cache_gc_settings.enabled
                    && cache_gc_schedule.should_run(
                        Instant::now(),
                        Duration::from_secs(cache_gc_settings.interval_seconds),
                    )
                {
                    let mut page_count = 0_i64;
                    let mut free_pages = 0_i64;
                    let mut free_ratio = 0.0_f64;
                    let mut manual_vacuum_advised = false;
                    let mut page_stats_available = true;

                    match storage.database_page_stats_with_cx(&loop_cx).await {
                        Ok(stats) => {
                            page_count = stats.page_count;
                            free_pages = stats.free_pages;
                            free_ratio = stats.free_ratio();

                            manual_vacuum_advised = should_vacuum(
                                stats.page_count,
                                stats.free_pages,
                                cache_gc_settings.vacuum_threshold,
                            );
                        }
                        Err(error) if is_runtime_cancellation(&error) => break,
                        Err(_error) => {
                            page_stats_available = false;
                            error!(
                                error_class = "database_page_stats_unavailable",
                                "Cache GC failed to read database page stats"
                            );
                        }
                    }

                    let metadata = serde_json::json!({
                        // Runtime pane-state teardown cannot be driven by a
                        // maintenance snapshot: it races same-ID successor
                        // initialization. Capture reconciliation removes exact
                        // state after authority drain; this cycle owns only the
                        // SQLite page-reclamation policy.
                        "runtime_state_cleanup_owner": "capture_reconciliation",
                        "page_count": page_count,
                        "free_pages": free_pages,
                        "free_ratio": free_ratio,
                        // Periodic automatic VACUUM is intentionally disabled:
                        // it rewrites the database and can monopolize the
                        // single writer during large ongoing sessions.
                        "automatic_vacuum": false,
                        "manual_vacuum_advised": manual_vacuum_advised,
                        "manual_vacuum_advisory_threshold": cache_gc_settings.vacuum_threshold,
                        "page_stats_available": page_stats_available,
                        "log_report": cache_gc_settings.log_report,
                    });
                    if let Err(error) = storage
                        .record_maintenance_with_cx(
                            &loop_cx,
                            MaintenanceRecord {
                                id: 0,
                                event_type: "cache_gc".to_string(),
                                message: Some("Periodic database cache GC cycle".to_string()),
                                metadata: Some(metadata.to_string()),
                                timestamp: epoch_ms(),
                            },
                        )
                        .await
                    {
                        if is_runtime_cancellation(&error) {
                            break;
                        }
                        error!(
                            error_class = "cache_gc_maintenance_record_failed",
                            page_count,
                            free_pages,
                            free_ratio,
                            manual_vacuum_advised,
                            page_stats_available,
                            "Failed to record database cache GC advisory"
                        );
                    }

                    if cache_gc_settings.log_report {
                        info!(
                            free_ratio,
                            manual_vacuum_advised,
                            "Database cache GC cycle completed"
                        );
                    } else {
                        debug!(
                            free_ratio,
                            manual_vacuum_advised,
                            "Database cache GC cycle completed"
                        );
                    }

                    cache_gc_schedule.finish(Instant::now());
                }

                if health_schedule.should_run(Instant::now(), health_interval) {
                    let (health_panes, leak_risk_inventory, worst_pane_budget) = {
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
                        let worst_pane_budget = worst_pane_budget_level(
                            reg.pane_arena_stats_snapshot()
                                .iter()
                                .map(|s| u64::try_from(s.stats.tracked_bytes).unwrap_or(u64::MAX)),
                            pane_budget_config.default_budget_bytes,
                            pane_budget_config.high_ratio,
                        );
                        (health_panes, leak_risk_inventory, worst_pane_budget)
                    };
                    let observed_panes = health_panes.observed_panes;
                    let last_activity_by_pane = health_panes.last_activity_by_pane;
                    let last_seq_by_pane = health_panes.last_seq_by_pane;
                    let cursor_snapshot_bytes = health_panes.cursor_snapshot_bytes;
                    if loop_cx.checkpoint().is_err() {
                        break;
                    }
                    metrics.record_cursor_snapshot_memory(cursor_snapshot_bytes);

                    let capture_cap = capture_queue_capacity;
                    let capture_depth = metrics.capture_queue_depth();

                    let (write_depth, write_cap, db_writable) = {
                        let wd = storage.write_queue_depth();
                        let wc = storage.write_queue_capacity();
                        let writable = match storage.is_writable_with_cx(&loop_cx).await {
                            Ok(writable) => writable,
                            Err(error) if is_runtime_cancellation(&error) => break,
                            Err(_error) => false,
                        };
                        (wd, wc, writable)
                    };
                    if loop_cx.checkpoint().is_err() {
                        break;
                    }

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
                    match wezterm_handle.watchdog_warnings_with_cx(&loop_cx).await {
                        Ok(wezterm_warnings) => {
                            append_bounded_watchdog_warnings(&mut warnings, wezterm_warnings);
                        }
                        Err(error) if is_runtime_cancellation(&error) => break,
                        Err(_error) => {
                            if loop_cx.checkpoint().is_err() {
                                break;
                            }
                            warnings.push(
                                "Mux health warning probe failed: backend_unavailable".to_string(),
                            );
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
                        worst_pane_budget,
                        observed_panes,
                    );
                    let observed_pane_ids = {
                        let reg = registry.read().await;
                        reg.observed_pane_ids()
                    };
                    let observed_pane_count = observed_pane_ids.len();
                    let tiered_scrollback_fetch =
                        match collect_pane_tiered_scrollback_summaries(
                            &loop_cx,
                            &wezterm_handle,
                            &observed_pane_ids,
                        )
                        .await
                        {
                            Ok(fetch) => fetch,
                            Err(error) if is_runtime_cancellation(&error) => break,
                            Err(_error) => {
                                warn!(
                                    error_class = "pane_summary_collection_failed",
                                    "Failed to collect mux-side tiered scrollback telemetry"
                                );
                                PaneTieredScrollbackFetch::default()
                            }
                        };
                    if loop_cx.checkpoint().is_err() {
                        break;
                    }
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
                    if loop_cx.checkpoint().is_err() {
                        break;
                    }
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

                    // Persist only state transitions and actual reclamation
                    // activity. The live health snapshot below still records
                    // every evaluation, but writing a full maintenance row on
                    // every 30-second no-op tick permanently amplified the
                    // single-writer workload and grew the table/index without
                    // bound during long-running sessions.
                    {
                        let telem = fleet_coordinator.telemetry_snapshot();
                        let telemetry_blind =
                            tiered_scrollback_fetch.telemetry_blind(observed_pane_count);
                        let telemetry_partial =
                            tiered_scrollback_fetch.telemetry_partial(observed_pane_count);
                        let blind_reason = telemetry_blind.then_some(
                            "vendored tiered scrollback telemetry unavailable; runtime is operating without mux-side pane telemetry",
                        );
                        let pane_status_error_samples = tiered_scrollback_fetch.error_samples();
                        let current_state = FleetCoordinatorMaintenanceState {
                            pressure: fleet_eval.compound_tier,
                            telemetry_blind,
                            telemetry_partial,
                            recommended_actions: fleet_eval.actions.len(),
                        };
                        let observed_state_changed =
                            last_fleet_coordinator_observed_state != Some(current_state);

                        if loop_cx.checkpoint().is_err() {
                            break;
                        }

                        if observed_state_changed && telemetry_blind {
                            warn!(
                                observed_panes = observed_pane_count,
                                pane_status_errors = tiered_scrollback_fetch.errors,
                                error_samples = ?pane_status_error_samples,
                                "Tiered scrollback telemetry is blind; vendored mux compatibility or health has degraded"
                            );
                        } else if observed_state_changed && telemetry_partial {
                            warn!(
                                observed_panes = observed_pane_count,
                                pane_status_snapshots = tiered_scrollback_fetch.summaries.len(),
                                pane_status_errors = tiered_scrollback_fetch.errors,
                                error_samples = ?pane_status_error_samples,
                                "Tiered scrollback telemetry is partially unavailable"
                            );
                        }
                        last_fleet_coordinator_observed_state = Some(current_state);

                        let audit_state_changed =
                            last_fleet_coordinator_maintenance_state != Some(current_state);
                        let actions = fleet_eval.actions.len();
                        if fleet_coordinator_maintenance_is_noteworthy(
                            last_fleet_coordinator_maintenance_state,
                            current_state,
                            fleet_eval.pages_evicted,
                            fleet_eval.bytes_reclaimed,
                            fleet_eval.targets_applied,
                        ) {
                            let audit_reason = if audit_state_changed {
                                "state_transition"
                            } else {
                                "reclamation_activity"
                            };
                            let metadata = serde_json::json!({
                                "audit_reason": audit_reason,
                                "compound_tier": format!("{:?}", fleet_eval.compound_tier),
                                "pages_evicted": fleet_eval.pages_evicted,
                                "bytes_reclaimed": fleet_eval.bytes_reclaimed,
                                "targets_applied": fleet_eval.targets_applied,
                                "actions": actions,
                                "observed_panes": observed_panes,
                                "pane_status_snapshots": tiered_scrollback_fetch.summaries.len(),
                                "pane_scrollback_snapshots": pane_scrollback_snapshot_count,
                                "pane_status_errors": tiered_scrollback_fetch.errors,
                                "pane_status_error_samples": pane_status_error_samples,
                                "tiered_scrollback_telemetry_blind": telemetry_blind,
                                "tiered_scrollback_telemetry_partial": telemetry_partial,
                                "tiered_scrollback_blind_reason": blind_reason,
                                "cumulative_ticks": telem.ticks,
                                "cumulative_elevated_ticks": telem.elevated_ticks,
                                "cumulative_pages_evicted": telem.pages_evicted,
                                "cumulative_bytes_reclaimed": telem.bytes_reclaimed,
                            });
                            match storage
                                .record_maintenance_with_cx(
                                    &loop_cx,
                                    MaintenanceRecord {
                                        id: 0,
                                        event_type: "fleet_scrollback_coordinator".to_string(),
                                        message: Some(if telemetry_blind {
                                            format!(
                                                "Fleet coordinator {audit_reason}: tier={:?}, evicted={} pages; tiered scrollback telemetry blind",
                                                fleet_eval.compound_tier, fleet_eval.pages_evicted,
                                            )
                                        } else {
                                            format!(
                                                "Fleet coordinator {audit_reason}: tier={:?}, evicted={} pages",
                                                fleet_eval.compound_tier, fleet_eval.pages_evicted,
                                            )
                                        }),
                                        metadata: Some(metadata.to_string()),
                                        timestamp: epoch_ms(),
                                    },
                                )
                                .await
                            {
                                Ok(_) => {
                                    last_fleet_coordinator_maintenance_state = Some(current_state);
                                }
                                Err(error) if is_runtime_cancellation(&error) => break,
                                Err(_error) => {
                                    warn!(
                                        error_class = "maintenance_record_failed",
                                        "Failed to record fleet coordinator maintenance transition"
                                    );
                                }
                            }
                        }
                    }
                    // ── end fleet coordinator tick ──────────────────────────

                    let snapshot_timestamp = epoch_ms_u64();
                    if loop_cx.checkpoint().is_err() {
                        break;
                    }
                    let snapshot = HealthSnapshot {
                        timestamp: snapshot_timestamp,
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
                        // ft-u6zfw: real crash-loop diagnostics (was hardcoded zeros).
                        restart_count: metrics.crash_loop_diagnostics().restart_count,
                        last_crash_at: metrics.crash_loop_diagnostics().last_crash_at,
                        consecutive_crashes: metrics.crash_loop_diagnostics().consecutive_crashes,
                        current_backoff_ms: metrics.crash_loop_diagnostics().current_backoff_ms,
                        in_crash_loop: metrics.crash_loop_diagnostics().in_crash_loop,
                        fleet_pressure_tier: Some(format!("{:?}", fleet_eval.compound_tier)),
                        swarm_capacity: Some(
                            crate::runtime_telemetry::live_swarm_capacity_operator_summary(
                                snapshot_timestamp,
                                observed_panes,
                                3,
                            ),
                        ),
                        leak_risk_inventory,
                    };

                    if loop_cx.checkpoint().is_err() {
                        break;
                    }
                    HealthSnapshot::update_global(snapshot);
                    RuntimeLockMemoryTelemetrySnapshot::update_global(
                        metrics.lock_memory_snapshot(),
                    );
                    // Health work includes several locks, storage probes, mux
                    // probes, and coordinator I/O; cadence starts only once
                    // that complete snapshot has actually been published.
                    health_schedule.finish(Instant::now());
                }
            }
        })
    }

    /// Spawn the pane discovery task.
    fn spawn_discovery_task(
        &self,
        discovery_publication_tx: watch::Sender<DiscoveryCapturePublication>,
    ) -> JoinHandle<()> {
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        // ft-u6zfw: clone the metrics handle so the discovery loop can feed
        // observed agent/pane restarts into the crash-loop detector.
        let crash_metrics = Arc::clone(&self.metrics);
        let storage = self.storage.clone();
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let initial_interval = self.config.discovery_interval;
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let wezterm = Arc::clone(&self.wezterm_handle);
        let replay_capture = self.replay_capture.clone();
        let capture_authority = self.capture_authority.clone();

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let mut current_interval = initial_interval;
            let mut first_tick = true;
            let mut last_discovery_revision = 0_u64;
            let mut last_publication_epoch = 0_u64;
            let mut discovery_revisions = HashMap::<u64, DiscoveryRevision>::new();
            let mut storage_resync_revisions = HashMap::<u64, DiscoveryRevision>::new();
            let mut capture_transitions =
                HashMap::<u64, CaptureTransitionDescriptor>::new();
            let mut last_capture_view = Arc::new(HashMap::<u64, ObservedCapturePane>::new());
            let mut unresolved_barrier_predecessors =
                HashMap::<u64, DiscoveryRevision>::new();
            let mut capture_setup_pending = HashMap::<u64, &'static str>::new();

            'discovery: loop {
                if first_tick {
                    first_tick = false;
                } else {
                    // Wait for interval, checking shutdown periodically to ensure responsiveness
                    let deadline = crate::runtime_async::timer_now_with_cx(&loop_cx)
                        + current_interval;
                    loop {
                        if shutdown_flag.load(Ordering::SeqCst) {
                            break;
                        }
                        let now = crate::runtime_async::timer_now_with_cx(&loop_cx);
                        if now >= deadline {
                            break;
                        }
                        // Sleep in short bursts to remain responsive to shutdown signals
                        let remaining = Duration::from_nanos(deadline.duration_since(now));
                        if let Err(failure) = runtime_sleep(
                            &loop_cx,
                            remaining.min(Duration::from_millis(100)),
                        )
                        .await
                        {
                            record_runtime_wait_failure("discovery_interval", failure);
                            break 'discovery;
                        }
                    }
                }

                // Check shutdown flag
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Discovery task: shutdown signal received");
                    break;
                }
                if loop_cx.checkpoint().is_err() {
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
                        // This must be the first operation after the listing:
                        // do not cross even a contended async registry lock
                        // while an identity contradicted by the fresh mux view
                        // can still admit producer or persistence guards.
                        let previous_capture_view = Arc::clone(&last_capture_view);
                        let (pre_storage_view, pre_storage_transitioning) =
                            conservative_capture_view_before_storage(
                                &previous_capture_view,
                                &panes,
                            );
                        if let Err(error) = publish_discovery_capture_view(
                            &discovery_publication_tx,
                            &capture_authority,
                            &mut last_publication_epoch,
                            &mut last_capture_view,
                            Arc::clone(&pre_storage_view),
                            pre_storage_transitioning,
                            Arc::new(capture_transitions.clone()),
                            "post-list-barrier",
                        ) {
                            error!(
                                error = %error,
                                "Discovery cannot install the post-list capture barrier; stopping the runtime fail-closed"
                            );
                            shutdown_flag.store(true, Ordering::SeqCst);
                            break;
                        }
                        remember_withheld_barrier_predecessors(
                            &mut unresolved_barrier_predecessors,
                            &previous_capture_view,
                            &pre_storage_view,
                        );
                        // Resolve stored UUIDs before the first ready publication. Capture
                        // snapshots the UUID and generation under the registry
                        // lock; publishing a generated UUID and adopting the
                        // stable one after an await lets a producer advance its
                        // cursor under metadata that persistence must reject.
                        let candidate_new_panes = {
                            let reg = registry.read().await;
                            panes
                                .iter()
                                .filter(|pane| reg.get_entry(pane.pane_id).is_none())
                                .map(|pane| pane.pane_id)
                                .collect::<Vec<_>>()
                        };
                        let stable_pane_records = if candidate_new_panes.is_empty() {
                            Vec::new()
                        } else {
                            match storage
                                .get_panes_by_ids_with_cx(&loop_cx, &candidate_new_panes)
                                .await
                            {
                                Ok(records) => records,
                                Err(error) => {
                                    warn!(
                                        candidate_count = candidate_new_panes.len(),
                                        error = %error,
                                        "Discovery remains pending because stable pane UUID recovery failed"
                                    );
                                    continue;
                                }
                            }
                        };
                        let mut stable_uuids = stable_pane_records
                            .into_iter()
                            .filter_map(|record| {
                                record
                                    .pane_uuid
                                    .map(|pane_uuid| (record.pane_id, pane_uuid))
                            })
                            .collect::<HashMap<_, _>>();
                        let (diff, new_entries, pending_capture_view) = {
                            let mut reg = registry.write().await;
                            let diff = reg.discovery_tick(panes);
                            let (confirmed_terminal, forced_transitions) =
                                classify_unresolved_barrier_predecessors(
                                    &unresolved_barrier_predecessors,
                                    &reg,
                                );
                            for pane_id in confirmed_terminal {
                                unresolved_barrier_predecessors.remove(&pane_id);
                            }
                            for pane_id in &diff.new_panes {
                                let Some(stable_uuid) = stable_uuids.remove(pane_id) else {
                                    continue;
                                };
                                let Some(generated_uuid) = reg
                                    .get_entry(*pane_id)
                                    .map(|entry| entry.pane_uuid.clone())
                                else {
                                    continue;
                                };
                                if stable_uuid == generated_uuid {
                                    continue;
                                }
                                debug!(
                                    pane_id,
                                    old_uuid = %generated_uuid,
                                    new_uuid = %stable_uuid,
                                    "Adopting stable UUID before registry publication"
                                );
                                if !reg.adopt_uuid(*pane_id, stable_uuid.clone()) {
                                    warn!(
                                        pane_id,
                                        rejected_uuid = %stable_uuid,
                                        "Rejected stable UUID adoption due to registry collision"
                                    );
                                }
                            }
                            // Observation can end without a numeric pane close
                            // (for example after a title/domain filter change).
                            // Such panes are terminal for capture: stale
                            // transition descriptors would otherwise retain
                            // resync and teardown obligations forever.
                            retain_observed_capture_bookkeeping(
                                &reg,
                                &mut discovery_revisions,
                                &mut storage_resync_revisions,
                                &mut capture_transitions,
                                &mut capture_setup_pending,
                            );
                            for pane_id in &diff.closed_panes {
                                discovery_revisions.remove(pane_id);
                                storage_resync_revisions.remove(pane_id);
                                capture_transitions.remove(pane_id);
                                capture_setup_pending.remove(pane_id);
                            }
                            for pane_id in &diff.new_panes {
                                if registry_observes_pane(&reg, *pane_id) {
                                    capture_setup_pending.insert(*pane_id, "observation_started");
                                }
                            }
                            for pane_id in &diff.re_observed_panes {
                                capture_setup_pending.insert(*pane_id, "observation_resumed");
                            }
                            let mut transitioning_pane_ids = diff
                                .new_panes
                                .iter()
                                .chain(&diff.new_generations)
                                .chain(&diff.re_observed_panes)
                                .copied()
                                .collect::<Vec<_>>();
                            transitioning_pane_ids.extend(forced_transitions);
                            transitioning_pane_ids
                                .retain(|pane_id| registry_observes_pane(&reg, *pane_id));
                            transitioning_pane_ids.sort_unstable();
                            transitioning_pane_ids.dedup();
                            for pane_id in &diff.new_panes {
                                storage_resync_revisions.remove(pane_id);
                            }
                            allocate_capture_transition_revisions(
                                &transitioning_pane_ids,
                                &mut last_discovery_revision,
                                &mut discovery_revisions,
                                &mut capture_transitions,
                                &mut unresolved_barrier_predecessors,
                                &mut storage_resync_revisions,
                            );
                            for pane_id in diff
                                .new_generations
                                .iter()
                                .chain(&diff.re_observed_panes)
                            {
                                if let Some(revision) = discovery_revisions.get(pane_id).copied() {
                                    storage_resync_revisions.insert(*pane_id, revision);
                                }
                            }
                            let pending_capture_view = if transitioning_pane_ids.is_empty()
                                && diff.closed_panes.is_empty()
                            {
                                None
                            } else {
                                let transitioning =
                                    transitioning_pane_ids.into_iter().collect::<HashSet<_>>();
                                Some((
                                    capture_publication_view(
                                        &reg,
                                        &discovery_revisions,
                                        &storage_resync_revisions,
                                        &transitioning,
                                    ),
                                    Arc::new(transitioning),
                                ))
                            };
                            let new_entries: Vec<_> = diff
                                .new_panes
                                .iter()
                                .filter_map(|pane_id| {
                                    reg.get_entry(*pane_id)
                                        .cloned()
                                        .map(|entry| (*pane_id, entry))
                                })
                                .collect();
                            (diff, new_entries, pending_capture_view)
                        };

                        // Publish a transition-pending identity view before any
                        // storage/cursor setup awaits.  Changed, closed, newly
                        // observed, and replacement pane IDs are absent here,
                        // so predecessor persistence admission closes before a
                        // poll can read a reused live mux pane and attribute it
                        // to the old revision.
                        if let Some((pending_capture_view, transitioning_pane_ids)) =
                            pending_capture_view
                            && let Err(error) = publish_discovery_capture_view(
                                &discovery_publication_tx,
                                &capture_authority,
                                &mut last_publication_epoch,
                                &mut last_capture_view,
                                pending_capture_view,
                                transitioning_pane_ids,
                                Arc::new(capture_transitions.clone()),
                                "transition-pending",
                            )
                        {
                            error!(
                                error = %error,
                                "Discovery cannot install the transition barrier; stopping the runtime fail-closed"
                            );
                            shutdown_flag.store(true, Ordering::SeqCst);
                            break;
                        }

                        // Handle new panes
                        for (pane_id, entry) in new_entries {
                            if let Some(ref adapter) = replay_capture {
                                if let Err(error) = adapter.capture_lifecycle(
                                    pane_id,
                                    crate::recording::RecorderLifecyclePhase::PaneOpened,
                                    None,
                                    serde_json::json!({
                                        "domain": entry.info.inferred_domain().to_string(),
                                        "title": entry.info.title.clone(),
                                        "cwd": entry.info.cwd.clone(),
                                    }),
                                ) {
                                    adapter.record_sequence_error(
                                        "runtime.pane_opened",
                                        error,
                                    );
                                }
                            }

                            if let Some(reason) = entry.observation.ignore_reason() {
                                info!(
                                    pane_id = pane_id,
                                    reason = reason,
                                    "Pane ignored by observation filter"
                                );
                            }
                        }

                        // Handle closed panes
                        for pane_id in &diff.closed_panes {
                            // Capture reconciliation owns terminal runtime-state
                            // teardown. Registry closure revokes admission via
                            // the publication barrier, but cursor/context and
                            // backpressure state must survive until exact
                            // predecessor producer/persistence guards drain.

                            if let Some(ref adapter) = replay_capture {
                                if let Err(error) = adapter.capture_lifecycle(
                                    *pane_id,
                                    crate::recording::RecorderLifecyclePhase::CaptureStopped,
                                    Some("pane_closed".to_string()),
                                    serde_json::json!({}),
                                ) {
                                    adapter.record_sequence_error(
                                        "runtime.capture_stopped",
                                        error,
                                    );
                                }
                                if let Err(error) = adapter.capture_lifecycle(
                                    *pane_id,
                                    crate::recording::RecorderLifecyclePhase::PaneClosed,
                                    Some("pane_closed".to_string()),
                                    serde_json::json!({}),
                                ) {
                                    adapter.record_sequence_error(
                                        "runtime.pane_closed",
                                        error,
                                    );
                                }
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

                        // New and re-admitted panes remain absent until their
                        // durable pane identity is upserted. Cursor/context and
                        // checkpoint state deliberately belongs to the capture
                        // coordinator: only it can prove an exact same-ID
                        // predecessor has drained before reading and certifying
                        // the successor baseline.
                        let setup_pane_ids =
                            capture_setup_pending.keys().copied().collect::<Vec<_>>();
                        for pane_id in setup_pane_ids {
                            let Some(reason) = capture_setup_pending.get(&pane_id).copied() else {
                                continue;
                            };
                            let setup_entry = {
                                let reg = registry.read().await;
                                reg.get_entry(pane_id).cloned().map(|entry| {
                                    let revision = discovery_revisions.get(&pane_id).copied();
                                    (entry, revision)
                                })
                            };
                            let Some((entry, revision)) = setup_entry else {
                                capture_setup_pending.remove(&pane_id);
                                continue;
                            };

                            if let Err(error) = storage
                                .upsert_pane_with_cx(&loop_cx, entry.to_pane_record())
                                .await
                            {
                                error!(
                                    pane_id,
                                    error = %error,
                                    "Capture setup remains pending because pane upsert failed"
                                );
                                continue;
                            }
                            if !entry.should_observe() {
                                capture_setup_pending.remove(&pane_id);
                                continue;
                            }
                            let Some(revision) = revision else {
                                error!(
                                    pane_id,
                                    "Capture setup remains pending because no checked discovery revision exists"
                                );
                                continue;
                            };
                            capture_setup_pending.remove(&pane_id);
                            info!(
                                pane_id,
                                revision = revision.get(),
                                reason,
                                "Prepared durable pane identity; capture baseline remains post-drain"
                            );
                            if let Some(ref adapter) = replay_capture {
                                if let Err(error) = adapter.capture_lifecycle(
                                    pane_id,
                                    crate::recording::RecorderLifecyclePhase::CaptureStarted,
                                    None,
                                    serde_json::json!({ "reason": reason }),
                                ) {
                                    adapter.record_sequence_error(
                                        "runtime.capture_started",
                                        error,
                                    );
                                }
                            }
                        }

                        // Snapshot only the capture-advanced fields needed by
                        // the registry. Semantic teardown belongs exclusively to
                        // capture reconciliation, which also amortizes sparse
                        // map capacity after exact removal. Avoiding discovery-time
                        // write locks on all three runtime maps materially cuts
                        // q200 lock contention.
                        let live_cursor_state = {
                            let cursors_guard = cursors.read().await;
                            cursors_guard
                                .values()
                                .map(crate::ingest::LiveCursorState::from)
                                .collect::<Vec<_>>()
                        };

                        // Publish the live capture state into the registry's
                        // cursors. Without this the registry's copy stays at
                        // its initial values forever, and `plan.rs` hands the
                        // policy engine `alt_screen: Some(false)` /
                        // `has_recent_gap: false` for every pane — a
                        // fail-open on exactly the two conditions those gates
                        // exist to catch.
                        if !live_cursor_state.is_empty() {
                            registry
                                .write()
                                .await
                                .publish_live_cursor_state(&live_cursor_state);
                        }
                        if !diff.new_panes.is_empty()
                            || !diff.closed_panes.is_empty()
                            || !diff.new_generations.is_empty()
                            || !diff.re_observed_panes.is_empty()
                        {
                            debug!(
                                new = diff.new_panes.len(),
                                closed = diff.closed_panes.len(),
                                restarted = diff.new_generations.len(),
                                re_observed = diff.re_observed_panes.len(),
                                "Pane discovery tick"
                            );
                        }
                        // ft-u6zfw: feed observed agent/pane restarts into the
                        // crash-loop detector so HealthSnapshot reflects real
                        // crash loops; a tick with no respawns is a clean run.
                        let crash_now_secs = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |elapsed| elapsed.as_secs());
                        if diff.new_generations.is_empty() {
                            crash_metrics.note_clean_observation();
                        } else {
                            crash_metrics.record_observed_restarts(
                                diff.new_generations.len(),
                                crash_now_secs,
                            );
                        }

                        let setup_excluded = capture_setup_pending
                            .keys()
                            .copied()
                            .collect::<HashSet<_>>();
                        let observed_panes = {
                            let reg = registry.read().await;
                            capture_publication_view(
                                &reg,
                                &discovery_revisions,
                                &storage_resync_revisions,
                                &setup_excluded,
                            )
                        };
                        if let Err(error) = publish_discovery_capture_view(
                            &discovery_publication_tx,
                            &capture_authority,
                            &mut last_publication_epoch,
                            &mut last_capture_view,
                            observed_panes,
                            Arc::new(setup_excluded),
                            Arc::new(capture_transitions.clone()),
                            "ready",
                        ) {
                            error!(
                                error = %error,
                                "Discovery cannot publish the ready capture view; stopping the runtime fail-closed"
                            );
                            shutdown_flag.store(true, Ordering::SeqCst);
                            break;
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
    fn spawn_capture_task(
        &self,
        capture_tx: mpsc::Sender<CaptureEvent>,
        discovery_publication_rx: watch::Receiver<DiscoveryCapturePublication>,
        capture_checkpoints: CaptureCheckpointCache,
    ) -> JoinHandle<()> {
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let pane_activity_tracker = Arc::clone(&self.pane_activity_tracker);
        let storage = self.storage.clone();
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let discovery_interval = self.config.discovery_interval;
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let wezterm_handle = Arc::clone(&self.wezterm_handle);
        let scheduler_snapshot = Arc::clone(&self.scheduler_snapshot);
        let capture_authority = self.capture_authority.clone();
        let capture_metadata = Arc::clone(&self.capture_metadata);
        let backpressure = Arc::clone(self.metrics.backpressure_metrics());
        let native_capture_enabled = self.config.native_event_socket.is_some();
        let transition_cache_capacity = self.config.channel_buffer.max(1);
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
            max_concurrent: capture_concurrency_usize(self.config.max_concurrent_captures),
            overlap_size,
            send_timeout: Duration::from_millis(100),
            capture_timeout: Duration::from_secs(2),
        };

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let source = Arc::new(WeztermHandleSource::new(wezterm_handle));
            let resync_source = Arc::clone(&source);
            let capture_tx_for_supervisor = capture_tx.clone();
            // Create tailer supervisor with budget enforcement
            let initial_budget = config_rx.borrow().capture_budgets.clone();
            let mut supervisor = TailerSupervisor::with_budget(
                initial_config,
                capture_tx_for_supervisor,
                Arc::clone(&cursors),
                Arc::clone(&registry), // Pass registry for authoritative state
                Arc::clone(&shutdown_flag),
                source,
                initial_budget,
            );

            // Cache hot-reloadable pane priority config for scheduling.
            let mut pane_priorities = config_rx.borrow().pane_priorities.clone();
            #[cfg(all(feature = "vendored", unix))]
            let mut vendored_subscription_config = initial_vendored_subscription_config;
            #[cfg(all(feature = "vendored", unix))]
            let (stream_exit_tx, mut stream_exit_rx) =
                mpsc::channel::<StreamingTaskExit>(vendored_channel_capacity);
            // Per-pane vendored streaming subtasks. Normal removal transfers
            // handles into a settlement set; clean shutdown bounded-drains it.
            // Dropping the capture future remains an abort-and-telemetry fallback
            // for panic/outer-cancellation paths that cannot await.
            #[cfg(all(feature = "vendored", unix))]
            let mut streaming_tasks: StreamingTasks = StreamingTasks::new();
            #[cfg(all(feature = "vendored", unix))]
            let mut observed_panes_cache =
                Arc::new(HashMap::<u64, ObservedCapturePane>::new());
            let mut capture_bindings = HashMap::<u64, ActiveCaptureBinding>::new();
            let mut pending_resync_bindings =
                HashMap::<u64, PendingCaptureResyncBinding>::new();
            let mut draining_bindings = HashMap::<u64, ActiveCaptureBinding>::new();
            let mut draining_since = HashMap::<u64, Instant>::new();
            let mut retired_state_candidates = HashMap::<u64, Instant>::new();
            let mut pending_resyncs = PendingCaptureResyncs::new(transition_cache_capacity);
            let mut completed_resyncs = HashMap::<u64, DiscoveryRevision>::new();
            #[cfg(all(feature = "vendored", unix))]
            let mut next_stream_task_token = 1_u64;

            // Sync tailers periodically with discovery interval.
            // Keep the first sync immediate to preserve prior interval behavior.
            let mut next_sync_tick = Instant::now();
            let tick_duration = Duration::from_millis(10);
            let mut next_spawn_tick =
                runtime_deadline_after(Instant::now(), tick_duration, "tailer spawn tick");
            let mut poll_tasks = TailerPollTaskSet::new();

            loop {
                #[cfg(all(feature = "vendored", unix))]
                streaming_tasks.reap_completed();

                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Capture task: shutdown signal received");
                    break;
                }

                #[cfg(all(feature = "vendored", unix))]
                while let Ok(exit) = stream_exit_rx.try_recv() {
                    let pane_id = exit.identity.global_pane_id;
                    let is_current_exit = streaming_tasks
                        .get(&pane_id)
                        .is_some_and(|task| task.matches_exit(&exit));
                    if !is_current_exit {
                        debug!(
                            pane_id,
                            generation = exit.identity.generation,
                            local_pane_id = exit.identity.local_pane_id,
                            socket_shard = exit.identity.socket_shard.0,
                            task_token = exit.token.0,
                            reason = %exit.reason,
                            "Ignoring stale vendored streaming task exit"
                        );
                        continue;
                    }

                    let task = streaming_tasks
                        .remove_for_settlement(pane_id, false)
                        .expect("current streaming exit has an active task");
                    let task_stamp = task.lease.stamp();
                    let binding_identity = capture_bindings.get(&pane_id).and_then(|binding| {
                        binding
                            .streaming_lease
                            .as_ref()
                            .is_some_and(|lease| lease.stamp() == task_stamp)
                            .then_some(binding.identity)
                    });
                    if let Some(binding_identity) = binding_identity {
                        match capture_authority
                            .begin_source_revocation(binding_identity, task_stamp)
                        {
                            Ok(revocation) => match revocation
                                .wait_with_cx(&loop_cx, CAPTURE_AUTHORITY_DRAIN_TIMEOUT)
                                .await
                            {
                                Ok(_) => {
                                    if let Some(binding) = capture_bindings.get_mut(&pane_id)
                                        && binding.streaming_lease.as_ref().is_some_and(|lease| {
                                            lease.stamp() == task_stamp
                                        })
                                    {
                                        binding.streaming_lease = None;
                                    }
                                }
                                Err(error) => {
                                    error!(
                                        pane_id,
                                        error = %error,
                                        "Vendored streaming source remained draining after exit"
                                    );
                                }
                            },
                            Err(error) => {
                                error!(
                                    pane_id,
                                    error = %error,
                                    "Failed to begin vendored streaming source revocation"
                                );
                            }
                        }
                    } else {
                        debug!(
                            pane_id,
                            source_epoch = task_stamp.source_epoch().get(),
                            "Streaming exit no longer matches the pane's active source lease"
                        );
                    }
                    if observed_panes_cache.contains_key(&pane_id)
                        && should_record_streaming_fallback(&exit.reason)
                    {
                        warn!(
                            pane_id,
                            generation = exit.identity.generation,
                            local_pane_id = exit.identity.local_pane_id,
                            socket_shard = exit.identity.socket_shard.0,
                            task_token = exit.token.0,
                            reason = %exit.reason,
                            "Vendored pane streaming ended; falling back to polling"
                        );
                    } else {
                        debug!(
                            pane_id,
                            generation = exit.identity.generation,
                            local_pane_id = exit.identity.local_pane_id,
                            socket_shard = exit.identity.socket_shard.0,
                            task_token = exit.token.0,
                            reason = %exit.reason,
                            "Vendored pane streaming ended"
                        );
                    }

                    let polling_observed_panes: HashMap<u64, PaneInfo> = observed_panes_cache
                        .iter()
                        .filter(|(pane_id, _)| !streaming_tasks.contains_key(*pane_id))
                        .filter(|(pane_id, pane)| {
                            capture_bindings
                                .get(*pane_id)
                                .is_some_and(|binding| binding.matches_observed(pane))
                        })
                        .map(|(pane_id, pane)| (*pane_id, pane.info.clone()))
                        .collect();
                    let polling_leases = polling_observed_panes
                        .keys()
                        .filter_map(|pane_id| {
                            capture_bindings
                                .get(pane_id)
                                .map(|binding| (*pane_id, binding.polling_lease.clone()))
                        })
                        .collect();
                    if let Err(error) = supervisor
                        .sync_authorized_tailers(&polling_observed_panes, &polling_leases)
                    {
                        error!(error = %error, "Failed to authorize polling fallback");
                    }
                }

                let now = Instant::now();
                if capture_sync_due(now, next_sync_tick, &discovery_publication_rx) {
                    next_sync_tick = now + discovery_interval;
                    heartbeats.record_capture();

                    // Check for config updates
                    if config_update_pending(&config_rx) {
                        let new_config = config_take_update(&mut config_rx);
                        let new_tailer_config = TailerConfig {
                            min_interval: Duration::from_millis(new_config.min_poll_interval_ms),
                            max_interval: Duration::from_millis(new_config.poll_interval_ms),
                            backoff_multiplier: 1.5,
                            max_concurrent: capture_concurrency_usize(
                                new_config.max_concurrent_captures,
                            ),
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

                    // Consume one fully-prepared immutable publication.  The
                    // channel retains an update sent before this task first
                    // runs and coalesces superseded ticks to their latest
                    // monotonic revision-stamped view.
                    let publication = discovery_publication_rx.borrow_and_clone();
                    let publication_epoch = publication.epoch;
                    #[cfg(all(feature = "vendored", unix))]
                    {
                        observed_panes_cache = Arc::clone(&publication.observed_panes);
                    }
                    #[cfg(not(all(feature = "vendored", unix)))]
                    let observed_panes_cache = Arc::clone(&publication.observed_panes);
                    let observed_pane_count = observed_panes_cache.len();
                    pending_resyncs.retain_authoritative(&publication);
                    completed_resyncs.retain(|pane_id, revision| {
                        observed_panes_cache
                            .get(pane_id)
                            .is_some_and(|pane| pane.revision == *revision)
                    });
                    pending_resyncs.observe_ready_transitions(
                        &publication,
                        &completed_resyncs,
                    );

                    // A successor resync is submitted without blocking fleet
                    // reconciliation on storage.  Persistence decides the
                    // receipt after durable commit plus cursor correction.
                    // Until then the provisional binding remains unpublished,
                    // but unrelated panes continue to reconcile normally.
                    let pending_resync_pane_ids =
                        pending_resync_bindings.keys().copied().collect::<Vec<_>>();
                    for pane_id in pending_resync_pane_ids {
                        let Some(pending_binding) = pending_resync_bindings.remove(&pane_id) else {
                            continue;
                        };
                        let PendingCaptureResyncBinding {
                            mut binding,
                            queued_at,
                        } = pending_binding;
                        let outcome = binding
                            .resync_receipt
                            .as_ref()
                            .and_then(CaptureResyncReceipt::outcome);
                        let still_current = capture_publication_matches(
                            &discovery_publication_rx,
                            pane_id,
                            binding.revision,
                        );
                        let disposition =
                            pending_capture_resync_disposition(still_current, outcome);
                        match disposition {
                            PendingCaptureResyncDisposition::Wait => {
                                // Queue admission transfers one drop-safe
                                // decision into persistence. Keep exactly this
                                // provisional binding until terminal, even if
                                // discovery supersedes it. Stale publication
                                // preflight settles the receipt; retiring early
                                // would permit duplicate queued resyncs.
                                pending_resync_bindings.insert(
                                    pane_id,
                                    PendingCaptureResyncBinding { binding, queued_at },
                                );
                            }
                            PendingCaptureResyncDisposition::RetireSuperseded {
                                committed,
                                failure_reason,
                            } => {
                                if committed {
                                    if pending_resyncs.remember(pane_id, binding.revision) {
                                        error!(
                                            exact_capacity = transition_cache_capacity,
                                            "Superseded durable resync will use storage-audited recovery"
                                        );
                                    }
                                } else {
                                    debug!(
                                        pane_id,
                                        reason = ?failure_reason,
                                        "Superseded resync reached its terminal failure decision"
                                    );
                                    pending_resyncs.require_storage_audit(pane_id);
                                }
                                let retired = retire_or_quarantine_capture_binding(
                                    &capture_authority,
                                    &capture_metadata,
                                    &backpressure,
                                    &mut draining_bindings,
                                    &mut draining_since,
                                    binding,
                                    "provisional resync superseded",
                                )
                                .await;
                                if retired {
                                    retired_state_candidates
                                        .entry(pane_id)
                                        .or_insert_with(Instant::now);
                                }
                            }
                            PendingCaptureResyncDisposition::Publish(sequence) => {
                                pending_resyncs.acknowledge(pane_id);
                                completed_resyncs.insert(pane_id, binding.revision);
                                binding.resync_receipt = None;
                                if let Err(error) = enable_native_capture_source(
                                    &capture_authority,
                                    &mut binding,
                                    native_capture_enabled,
                                ) {
                                    error!(
                                        pane_id,
                                        sequence,
                                        error = %error,
                                        "Failed to activate native source after durable resync"
                                    );
                                    let retired = retire_or_quarantine_capture_binding(
                                        &capture_authority,
                                        &capture_metadata,
                                        &backpressure,
                                        &mut draining_bindings,
                                        &mut draining_since,
                                        binding,
                                        "native source activation failed after resync",
                                    )
                                    .await;
                                    if retired {
                                        retired_state_candidates
                                            .entry(pane_id)
                                            .or_insert_with(Instant::now);
                                    }
                                    continue;
                                }
                                retired_state_candidates.remove(&pane_id);
                                capture_bindings.insert(pane_id, binding);
                            }
                            PendingCaptureResyncDisposition::RetireFailed(reason) => {
                                error!(
                                    pane_id,
                                    reason = %reason,
                                    "Capture successor resync failed before durable acknowledgement"
                                );
                                pending_resyncs.require_storage_audit(pane_id);
                                let retired = retire_or_quarantine_capture_binding(
                                    &capture_authority,
                                    &capture_metadata,
                                    &backpressure,
                                    &mut draining_bindings,
                                    &mut draining_since,
                                    binding,
                                    "resync persistence failed",
                                )
                                .await;
                                if retired {
                                    retired_state_candidates
                                        .entry(pane_id)
                                        .or_insert_with(Instant::now);
                                }
                            }
                        }
                    }

                    // A timed-out revocation is quarantined here rather than
                    // reinserted as an active binding.  Retry exact drain on
                    // each reconciliation; no successor can be admitted while
                    // a same-ID predecessor remains in this map.
                    let draining_pane_ids = draining_bindings.keys().copied().collect::<Vec<_>>();
                    for pane_id in draining_pane_ids {
                        let Some(binding) = draining_bindings.remove(&pane_id) else {
                            continue;
                        };
                        match capture_authority.retire_pane_if_drained(binding.identity) {
                            Ok(true) => {
                                draining_since.remove(&pane_id);
                                capture_metadata
                                    .write()
                                    .await
                                    .remove(&binding.identity.pane_incarnation());
                                let _ = backpressure.cleanup_pane(pane_id);
                                retired_state_candidates
                                    .entry(pane_id)
                                    .or_insert_with(Instant::now);
                            }
                            Ok(false) => {
                                debug!(
                                    pane_id,
                                    "Capture binding remains quarantined while exact revocation drains"
                                );
                                draining_since.entry(pane_id).or_insert_with(Instant::now);
                                draining_bindings.insert(pane_id, binding);
                            }
                            Err(error) => {
                                error!(pane_id, error = %error, "Failed to retry quarantined capture revocation");
                                draining_since.entry(pane_id).or_insert_with(Instant::now);
                                draining_bindings.insert(pane_id, binding);
                            }
                        }
                    }

                    let obsolete_bindings = capture_bindings
                        .iter()
                        .filter_map(|(pane_id, binding)| {
                            let keep = observed_panes_cache
                                .get(pane_id)
                                .is_some_and(|pane| binding.matches_observed(pane));
                            (!keep).then_some(*pane_id)
                        })
                        .collect::<Vec<_>>();

                    #[cfg(all(feature = "vendored", unix))]
                    for pane_id in &obsolete_bindings {
                        let _ = streaming_tasks.remove_for_settlement(*pane_id, true);
                    }

                    // Stop polling admission before revoking an obsolete pane.
                    let retained_polling_panes = observed_panes_cache
                        .iter()
                        .filter(|(pane_id, pane)| {
                            capture_bindings
                                .get(*pane_id)
                                .is_some_and(|binding| binding.matches_observed(pane))
                        })
                        .map(|(pane_id, pane)| (*pane_id, pane.info.clone()))
                        .collect::<HashMap<_, _>>();
                    let retained_polling_leases = retained_polling_panes
                        .keys()
                        .filter_map(|pane_id| {
                            capture_bindings
                                .get(pane_id)
                                .map(|binding| (*pane_id, binding.polling_lease.clone()))
                        })
                        .collect::<HashMap<_, _>>();
                    if let Err(error) = supervisor.sync_authorized_tailers(
                        &retained_polling_panes,
                        &retained_polling_leases,
                    ) {
                        error!(error = %error, "Failed to suspend obsolete polling bindings");
                    }

                    for pane_id in obsolete_bindings {
                        let Some(binding) = capture_bindings.remove(&pane_id) else {
                            continue;
                        };
                        // Always retain the obsolete revision until a fresh
                        // publication proves the pane terminal. A successor can
                        // be published after the borrowed snapshot above, so a
                        // snapshot-conditioned remember would lose its exact
                        // predecessor obligation in that race.
                        if pending_resyncs.remember(pane_id, binding.revision) {
                            error!(
                                exact_capacity = transition_cache_capacity,
                                "Capture transition exact fast path saturated; successor will use storage-audited resync"
                            );
                        }
                        let retired = retire_or_quarantine_capture_binding(
                            &capture_authority,
                            &capture_metadata,
                            &backpressure,
                            &mut draining_bindings,
                            &mut draining_since,
                            binding,
                            "obsolete publication binding",
                        )
                        .await;
                        if retired {
                            retired_state_candidates
                                .entry(pane_id)
                                .or_insert_with(Instant::now);
                        }
                    }

                    for (&pane_id, observed) in observed_panes_cache.iter() {
                        if capture_bindings.contains_key(&pane_id)
                            || pending_resync_bindings.contains_key(&pane_id)
                            || draining_bindings.contains_key(&pane_id)
                        {
                            continue;
                        }
                        let desired_is_current = capture_publication_matches(
                            &discovery_publication_rx,
                            pane_id,
                            observed.revision,
                        );
                        if !desired_is_current {
                            next_sync_tick = Instant::now();
                            continue;
                        }

                        // Discovery never certifies a cursor baseline. Do the
                        // storage read here, after every exact same-ID
                        // predecessor has left both active and draining maps.
                        // A late predecessor commit therefore cannot race this
                        // successor baseline into false certainty.
                        let capture_state_preexisted =
                            cursors.read().await.contains_key(&pane_id);
                        if capture_state_preexisted {
                            // Until a current authority binding is installed,
                            // this state is provisional. Tracking it from the
                            // first unbound observation gives every later
                            // recovery/currentness/activation failure a
                            // terminal cleanup path.
                            retired_state_candidates
                                .entry(pane_id)
                                .or_insert_with(Instant::now);
                        }
                        let initialized_checkpoint = if capture_state_preexisted {
                            None
                        } else {
                            let checkpoint = match load_capture_checkpoint_from_storage(
                                &loop_cx,
                                &storage,
                                pane_id,
                                observed.revision,
                            )
                            .await
                            {
                                Ok(checkpoint) => checkpoint,
                                Err(error) => {
                                    error!(
                                        pane_id,
                                        error = %error,
                                        "Capture successor remains unpublished because post-drain baseline recovery failed"
                                    );
                                    continue;
                                }
                            };
                            match initialize_capture_state_from_checkpoint(
                                pane_id,
                                &checkpoint,
                                &cursors,
                                &detection_contexts,
                                &pane_activity_tracker,
                                &capture_checkpoints,
                            )
                            .await
                            {
                                Ok(initialized) => {
                                    retired_state_candidates
                                        .entry(pane_id)
                                        .or_insert_with(Instant::now);
                                    initialized.then_some(checkpoint)
                                }
                                Err(error) => {
                                    error!(
                                        pane_id,
                                        error = %error,
                                        "Capture successor remains unpublished because post-drain state initialization failed"
                                    );
                                    continue;
                                }
                            }
                        };

                        let resync_completed = completed_resyncs.get(&pane_id).copied()
                            == Some(observed.revision);
                        let mut resync_requirement = if resync_completed {
                            None
                        } else {
                            pending_resyncs.requirement(
                                pane_id,
                                observed.requires_storage_resync,
                            )
                        };
                        if !resync_completed
                            && resync_requirement.is_none()
                            && (capture_state_preexisted
                                || initialized_checkpoint
                                    .as_ref()
                                    .is_some_and(|checkpoint| checkpoint.next_seq > 0))
                        {
                            resync_requirement = Some(CaptureResyncRequirement::StorageAudit);
                        }
                        if let Some(requirement) = resync_requirement {
                            let checkpoint_result = match requirement {
                                CaptureResyncRequirement::Exact(predecessor_revision) => {
                                    recover_capture_checkpoint(
                                        &loop_cx,
                                        &storage,
                                        &capture_checkpoints,
                                        pane_id,
                                        predecessor_revision,
                                    )
                                    .await
                                }
                                CaptureResyncRequirement::StorageAudit => {
                                    if let Some(checkpoint) = initialized_checkpoint.as_ref() {
                                        Ok(checkpoint.clone())
                                    } else {
                                        load_capture_checkpoint_from_storage(
                                            &loop_cx,
                                            &storage,
                                            pane_id,
                                            observed.revision,
                                        )
                                        .await
                                    }
                                }
                            };
                            let checkpoint = match checkpoint_result {
                                Ok(checkpoint) => checkpoint,
                                Err(error) => {
                                    error!(
                                        pane_id,
                                        desired_revision = observed.revision.get(),
                                        error = %error,
                                        "Capture successor remains unpublished because durable recovery failed"
                                    );
                                    continue;
                                }
                            };
                            if let Err(error) = reset_capture_state_from_checkpoint(
                                pane_id,
                                observed.revision,
                                &checkpoint,
                                false,
                                &cursors,
                                &detection_contexts,
                                &pane_activity_tracker,
                                &capture_checkpoints,
                            )
                            .await
                            {
                                error!(
                                    pane_id,
                                    error = %error,
                                    "Capture successor remains unpublished because cursor reset failed"
                                );
                                continue;
                            }

                            let still_current = capture_publication_matches(
                                &discovery_publication_rx,
                                pane_id,
                                observed.revision,
                            );
                            if !still_current {
                                next_sync_tick = Instant::now();
                                continue;
                            }
                        }
                        match activate_capture_binding(
                            &loop_cx,
                            &capture_authority,
                            pane_id,
                            observed.generation,
                            &capture_metadata,
                            CapturePaneMetadata {
                                pane_uuid: observed.pane_uuid.clone(),
                                discovery_generation: observed.generation,
                                discovery_revision: observed.revision,
                            },
                        )
                        .await
                        {
                            Ok(mut binding) => {
                                if !capture_publication_matches(
                                    &discovery_publication_rx,
                                    pane_id,
                                    observed.revision,
                                ) {
                                    next_sync_tick = Instant::now();
                                    if pending_resyncs.remember(pane_id, observed.revision) {
                                        error!(
                                            exact_capacity = transition_cache_capacity,
                                            "Capture transition exact fast path saturated; successor will use storage-audited resync"
                                        );
                                    }
                                    let retired = retire_or_quarantine_capture_binding(
                                        &capture_authority,
                                        &capture_metadata,
                                        &backpressure,
                                        &mut draining_bindings,
                                        &mut draining_since,
                                        binding,
                                        "superseded before exposure",
                                    )
                                    .await;
                                    if retired {
                                        retired_state_candidates
                                            .entry(pane_id)
                                            .or_insert_with(Instant::now);
                                    }
                                    continue;
                                }
                                if resync_requirement.is_some() {
                                    match emit_capture_generation_resync(
                                        &loop_cx,
                                        &resync_source,
                                        &capture_tx,
                                        &cursors,
                                        &binding,
                                    )
                                    .await
                                    {
                                        Ok(receipt) => {
                                            binding.resync_receipt = Some(receipt);
                                            pending_resync_bindings.insert(
                                                pane_id,
                                                PendingCaptureResyncBinding {
                                                    binding,
                                                    queued_at: Instant::now(),
                                                },
                                            );
                                        }
                                        Err(error) => {
                                            error!(
                                                pane_id,
                                                desired_revision = observed.revision.get(),
                                                error = %error,
                                                "Capture successor resync could not enter the durable pipeline"
                                            );
                                            pending_resyncs.require_storage_audit(pane_id);
                                            let retired = retire_or_quarantine_capture_binding(
                                                &capture_authority,
                                                &capture_metadata,
                                                &backpressure,
                                                &mut draining_bindings,
                                                &mut draining_since,
                                                binding,
                                                "resync submission failed",
                                            )
                                            .await;
                                            if retired {
                                                retired_state_candidates
                                                    .entry(pane_id)
                                                    .or_insert_with(Instant::now);
                                            }
                                        }
                                    }
                                    continue;
                                }
                                if !capture_publication_matches(
                                    &discovery_publication_rx,
                                    pane_id,
                                    observed.revision,
                                ) {
                                    next_sync_tick = Instant::now();
                                    if pending_resyncs.remember(pane_id, observed.revision) {
                                        error!(
                                            exact_capacity = transition_cache_capacity,
                                            "Capture transition exact fast path saturated; successor will use storage-audited resync"
                                        );
                                    }
                                    let retired = retire_or_quarantine_capture_binding(
                                        &capture_authority,
                                        &capture_metadata,
                                        &backpressure,
                                        &mut draining_bindings,
                                        &mut draining_since,
                                        binding,
                                        "durable successor superseded before exposure",
                                    )
                                    .await;
                                    if retired {
                                        retired_state_candidates
                                            .entry(pane_id)
                                            .or_insert_with(Instant::now);
                                    }
                                    continue;
                                }
                                if let Err(error) = enable_native_capture_source(
                                    &capture_authority,
                                    &mut binding,
                                    native_capture_enabled,
                                ) {
                                    error!(
                                        pane_id,
                                        error = %error,
                                        "Failed to activate native capture source"
                                    );
                                    let retired = retire_or_quarantine_capture_binding(
                                        &capture_authority,
                                        &capture_metadata,
                                        &backpressure,
                                        &mut draining_bindings,
                                        &mut draining_since,
                                        binding,
                                        "native source activation failed",
                                    )
                                    .await;
                                    if retired {
                                        retired_state_candidates
                                            .entry(pane_id)
                                            .or_insert_with(Instant::now);
                                    }
                                    continue;
                                }
                                retired_state_candidates.remove(&pane_id);
                                capture_bindings.insert(pane_id, binding);
                            }
                            Err(error) => {
                                error!(
                                    pane_id,
                                    generation = observed.generation,
                                    error = %error,
                                    "Failed to activate capture pane authority"
                                );
                            }
                        }
                    }

                    // The capture coordinator is the sole owner of terminal
                    // cursor/context/activity teardown. Run this after every
                    // retirement path above, then borrow the latest publication
                    // so a same-ID successor published during drain cannot lose
                    // its state.
                    let terminal_publication = discovery_publication_rx.borrow_and_clone();
                    let terminal_candidates = retired_state_candidates
                        .keys()
                        .copied()
                        .filter(|pane_id| {
                            !terminal_publication.observed_panes.contains_key(pane_id)
                                && !terminal_publication
                                    .transitioning_pane_ids
                                    .contains(pane_id)
                                && !terminal_publication.transitions.contains_key(pane_id)
                        })
                        .collect::<Vec<_>>();
                    if !terminal_candidates.is_empty() {
                        remove_runtime_pane_state_for_panes(
                            &terminal_candidates,
                            &cursors,
                            &detection_contexts,
                            &pane_activity_tracker,
                        )
                        .await;
                        let _ = backpressure.cleanup_panes(&terminal_candidates);
                        match capture_checkpoints.lock() {
                            Ok(mut checkpoints) => {
                                for pane_id in &terminal_candidates {
                                    checkpoints.remove(pane_id);
                                }
                            }
                            Err(_) => {
                                error!(
                                    pane_count = terminal_candidates.len(),
                                    "Capture checkpoint cache is poisoned during batched terminal teardown"
                                );
                            }
                        }
                        for pane_id in terminal_candidates {
                            pending_resyncs.acknowledge(pane_id);
                            completed_resyncs.remove(&pane_id);
                            retired_state_candidates.remove(&pane_id);
                        }
                    }

                    #[cfg(all(feature = "vendored", unix))]
                    {
                        let obsolete_streams: Vec<(u64, bool)> = streaming_tasks
                            .iter()
                            .filter_map(|(pane_id, task)| {
                                let desired_identity = observed_panes_cache.get(pane_id).and_then(
                                    |pane| {
                                        capture_bindings.get(pane_id).and_then(|binding| {
                                            binding.streaming_lease.as_ref().map(|lease| {
                                                (*pane_id, pane.generation, lease.stamp())
                                            })
                                        })
                                    },
                                );
                                match streaming_task_reconcile_action(
                                    &task.identity,
                                    desired_identity,
                                ) {
                                    StreamingTaskReconcileAction::Keep => None,
                                    StreamingTaskReconcileAction::Remove => {
                                        Some((*pane_id, false))
                                    }
                                    StreamingTaskReconcileAction::Replace => {
                                        Some((*pane_id, true))
                                    }
                                }
                            })
                            .collect();
                        for (pane_id, has_replacement) in obsolete_streams {
                            if let Some(task) =
                                streaming_tasks.remove_for_settlement(pane_id, true)
                            {
                                let task_stamp = task.lease.stamp();
                                let binding_identity = capture_bindings.get(&pane_id).and_then(
                                    |binding| {
                                        binding
                                            .streaming_lease
                                            .as_ref()
                                            .is_some_and(|lease| lease.stamp() == task_stamp)
                                            .then_some(binding.identity)
                                    },
                                );
                                if let Some(binding_identity) = binding_identity {
                                    match capture_authority
                                        .begin_source_revocation(binding_identity, task_stamp)
                                    {
                                        Ok(revocation) => match revocation
                                            .wait_with_cx(
                                                &loop_cx,
                                                CAPTURE_AUTHORITY_DRAIN_TIMEOUT,
                                            )
                                            .await
                                        {
                                            Ok(_) => {
                                                if let Some(binding) =
                                                    capture_bindings.get_mut(&pane_id)
                                                    && binding.streaming_lease.as_ref().is_some_and(
                                                        |lease| lease.stamp() == task_stamp,
                                                    )
                                                {
                                                    binding.streaming_lease = None;
                                                }
                                            }
                                            Err(error) => {
                                                error!(
                                                    pane_id,
                                                    error = %error,
                                                    "Obsolete stream source remained draining"
                                                );
                                            }
                                        },
                                        Err(error) => {
                                            error!(
                                                pane_id,
                                                error = %error,
                                                "Failed to revoke obsolete stream source"
                                            );
                                        }
                                    }
                                }
                                debug!(
                                    pane_id,
                                    generation = task.identity.generation,
                                    local_pane_id = task.identity.local_pane_id,
                                    socket_shard = task.identity.socket_shard.0,
                                    task_token = task.token.0,
                                    has_replacement,
                                    "Cancelled obsolete vendored streaming task"
                                );
                            }
                        }

                        if let Some(subscription_config) = vendored_subscription_config.clone() {
                            for (&pane_id, observed_pane) in observed_panes_cache.iter() {
                                if streaming_tasks.contains_key(&pane_id) {
                                    continue;
                                }

                                let Some((socket_shard, local_pane_id, socket_path)) =
                                    vendored_streaming_route_for_pane(
                                        &vendored_mux_socket_paths,
                                        pane_id,
                                    )
                                else {
                                    continue;
                                };

                                if !socket_path.exists() {
                                    debug!(
                                        pane_id,
                                        generation = observed_pane.generation,
                                        local_pane_id,
                                        socket_shard = socket_shard.0,
                                        path = %socket_path.display(),
                                        "Skipping vendored pane streaming because mux socket is missing"
                                    );
                                    continue;
                                }

                                let Some(token) =
                                    allocate_streaming_task_token(&mut next_stream_task_token)
                                else {
                                    error!(
                                        pane_id,
                                        generation = observed_pane.generation,
                                        "Vendored streaming task token space exhausted; refusing to start an unidentifiable task"
                                    );
                                    continue;
                                };

                                let Some((binding_identity, existing_streaming_lease)) =
                                    capture_bindings.get(&pane_id).and_then(|binding| {
                                        binding.matches_observed(observed_pane).then(|| {
                                            (binding.identity, binding.streaming_lease.clone())
                                        })
                                    })
                                else {
                                    continue;
                                };

                                // A prior task may have exited while its
                                // source drain timed out. Resume that exact
                                // fail-closed transition before issuing a
                                // replacement epoch.
                                if let Some(existing_lease) = existing_streaming_lease {
                                    let existing_stamp = existing_lease.stamp();
                                    let drained = match capture_authority
                                        .begin_source_revocation(
                                            binding_identity,
                                            existing_stamp,
                                        )
                                    {
                                        Ok(revocation) => revocation
                                            .wait_with_cx(
                                                &loop_cx,
                                                CAPTURE_AUTHORITY_DRAIN_TIMEOUT,
                                            )
                                            .await,
                                        Err(error) => Err(error),
                                    };
                                    match drained {
                                        Ok(_) => {
                                            if let Some(binding) =
                                                capture_bindings.get_mut(&pane_id)
                                                && binding.streaming_lease.as_ref().is_some_and(
                                                    |lease| lease.stamp() == existing_stamp,
                                                )
                                            {
                                                binding.streaming_lease = None;
                                            }
                                        }
                                        Err(error) => {
                                            error!(
                                                pane_id,
                                                error = %error,
                                                "Vendored stream replacement remains fail-closed"
                                            );
                                            continue;
                                        }
                                    }
                                }

                                let streaming_lease = match capture_authority.issue_source(
                                    binding_identity,
                                    CaptureSourceKind::VendoredStreaming,
                                ) {
                                    Ok(lease) => lease,
                                    Err(error) => {
                                        error!(
                                            pane_id,
                                            error = %error,
                                            "Failed to issue vendored streaming authority"
                                        );
                                        continue;
                                    }
                                };
                                let identity = StreamingSubscriptionIdentity {
                                    global_pane_id: pane_id,
                                    local_pane_id,
                                    socket_shard,
                                    socket_path,
                                    generation: observed_pane.generation,
                                    capture_stamp: streaming_lease.stamp(),
                                };
                                let installed = capture_bindings
                                    .get_mut(&pane_id)
                                    .filter(|binding| binding.identity == binding_identity)
                                    .is_some_and(|binding| {
                                        binding.streaming_lease = Some(streaming_lease.clone());
                                        true
                                });
                                if !installed {
                                    let cleanup = match capture_authority
                                        .begin_source_revocation(
                                            binding_identity,
                                            streaming_lease.stamp(),
                                        )
                                    {
                                        Ok(revocation) => revocation
                                            .wait_with_cx(
                                                &loop_cx,
                                                CAPTURE_AUTHORITY_DRAIN_TIMEOUT,
                                            )
                                            .await,
                                        Err(error) => Err(error),
                                    };
                                    if let Err(error) = cleanup {
                                        error!(
                                            pane_id,
                                            capture_stamp = ?streaming_lease.stamp(),
                                            error = %error,
                                            "Uninstalled stream authority revocation did not drain; capture remains fail-closed"
                                        );
                                    }
                                    error!(
                                        pane_id,
                                        "Capture binding changed while installing stream source"
                                    );
                                    continue;
                                }

                                let capture_tx = capture_tx.clone();
                                let stream_exit_tx = stream_exit_tx.clone();
                                let shutdown_flag = Arc::clone(&shutdown_flag);
                                let subscription_config = subscription_config.clone();
                                let identity_for_task = identity.clone();
                                let identity_for_exit = identity.clone();
                                let lease_for_task = streaming_lease.clone();
                                let stream_task_cx = loop_cx.clone();
                                let handle = spawn_runtime_task(
                                    &stream_task_cx,
                                    move |stream_task_cx| async move {
                                        let exit_reason = Box::pin(run_vendored_streaming_capture(
                                            identity_for_task,
                                            vendored_mux_compression,
                                            subscription_config,
                                            capture_tx,
                                            lease_for_task,
                                        ))
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
                                                identity: identity_for_exit,
                                                token,
                                                reason: final_reason,
                                            },
                                        )
                                        .await;
                                    },
                                );
                                streaming_tasks.insert_active(
                                    pane_id,
                                    StreamingTask {
                                        identity,
                                        lease: streaming_lease,
                                        token,
                                        handle,
                                    },
                                );
                            }
                        }

                        let polling_observed_panes: HashMap<u64, PaneInfo> = observed_panes_cache
                            .iter()
                            .filter(|(pane_id, _)| !streaming_tasks.contains_key(*pane_id))
                            .filter(|(pane_id, pane)| {
                                capture_bindings
                                    .get(*pane_id)
                                    .is_some_and(|binding| binding.matches_observed(pane))
                            })
                            .map(|(pane_id, pane)| (*pane_id, pane.info.clone()))
                            .collect();
                        let polling_leases = polling_observed_panes
                            .keys()
                            .filter_map(|pane_id| {
                                capture_bindings
                                    .get(pane_id)
                                    .map(|binding| (*pane_id, binding.polling_lease.clone()))
                            })
                            .collect::<HashMap<_, _>>();
                        if let Err(error) = supervisor
                            .sync_authorized_tailers(&polling_observed_panes, &polling_leases)
                        {
                            error!(error = %error, "Failed to authorize polling capture");
                        }
                    }
                    #[cfg(not(all(feature = "vendored", unix)))]
                    {
                        let polling_observed_panes = observed_panes_cache
                            .iter()
                            .filter(|(pane_id, pane)| {
                                capture_bindings
                                    .get(*pane_id)
                                    .is_some_and(|binding| binding.matches_observed(pane))
                            })
                            .map(|(pane_id, pane)| (*pane_id, pane.info.clone()))
                            .collect::<HashMap<_, _>>();
                        let polling_leases = polling_observed_panes
                            .keys()
                            .filter_map(|pane_id| {
                                capture_bindings
                                    .get(pane_id)
                                    .map(|binding| (*pane_id, binding.polling_lease.clone()))
                            })
                            .collect::<HashMap<_, _>>();
                        if let Err(error) = supervisor
                            .sync_authorized_tailers(&polling_observed_panes, &polling_leases)
                        {
                            error!(error = %error, "Failed to authorize polling capture");
                        }
                    }

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

                    // Prompt transition retries must not starve poll futures:
                    // those futures own producer guards whose release is often
                    // the condition for `draining_bindings` to make progress.
                    // Polling once also registers wakeups for pending work;
                    // draining ready completions is bounded per sync tick.
                    let poll_reap_limit = poll_tasks.len().min(CAPTURE_POLL_REAP_BUDGET);
                    for _ in 0..poll_reap_limit {
                        let Some((pane_id, outcome)) = poll_tasks.try_join_next() else {
                            break;
                        };
                        supervisor.handle_poll_result(pane_id, outcome);
                    }

                    draining_since.retain(|pane_id, _| draining_bindings.contains_key(pane_id));
                    debug_assert!(
                        draining_bindings
                            .keys()
                            .all(|pane_id| draining_since.contains_key(pane_id)),
                        "every quarantined binding must carry a bounded prompt-retry deadline"
                    );
                    let retry_now = Instant::now();
                    let retry_delay = pending_resync_bindings
                        .values()
                        .map(|pending| {
                            capture_transition_retry_delay(pending.queued_at, retry_now)
                        })
                        .chain(draining_since.values().filter_map(|started_at| {
                            bounded_capture_transition_retry_delay(*started_at, retry_now)
                        }))
                        .chain(retired_state_candidates.values().filter_map(|started_at| {
                            bounded_capture_transition_retry_delay(*started_at, retry_now)
                        }))
                        .min();
                    if let Some(retry_delay) = retry_delay {
                        let prompt_retry = runtime_deadline_after(
                            retry_now,
                            retry_delay,
                            "capture transition retry",
                        );
                        if next_sync_tick > prompt_retry {
                            next_sync_tick = prompt_retry;
                        }
                    }

                    debug!(
                        discovery_publication_epoch = publication_epoch,
                        active_tailers = supervisor.active_count(),
                        observed_panes = observed_pane_count,
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
                    match runtime_timeout(&loop_cx, wait_duration, poll_tasks.join_next()).await {
                        Ok(Some((pane_id, outcome))) => {
                            supervisor.handle_poll_result(pane_id, outcome);
                        }
                        Ok(None) => {}
                        Err(RuntimeTimeoutFailure::Context(failure)) => {
                            record_runtime_wait_failure("capture_poll_wait", failure);
                            break;
                        }
                        // The bounded wait expiring is the normal scheduler
                        // wakeup: keep the in-flight poll futures owned and
                        // let the next loop iteration run sync/spawn work.
                        Err(RuntimeTimeoutFailure::Elapsed) => {}
                    }
                } else if let Err(failure) = runtime_sleep(&loop_cx, wait_duration).await {
                    record_runtime_wait_failure("capture_scheduler_wait", failure);
                    break;
                }
            }

            // TailerPollTaskSet owns futures directly rather than detached
            // executor tasks. Drop it before supervisor shutdown so every
            // pending poll future is synchronously cancelled and releases its
            // producer/semaphore guards; retaining the set across shutdown can
            // otherwise make the supervisor wait on work it no longer polls.
            drop(poll_tasks);

            #[cfg(all(feature = "vendored", unix))]
            match streaming_tasks.abort_and_settle_all().await {
                StreamingTaskDrainOutcome::Settled => {}
                StreamingTaskDrainOutcome::SettledWithFailure { failure } => {
                    warn!(
                        failure_class = ?failure,
                        "Vendored streaming tasks settled after a task failure"
                    );
                }
                StreamingTaskDrainOutcome::TimedOut {
                    active_tasks,
                    unacknowledged_tasks,
                } => {
                    error!(
                        event = "streaming_task_drain_timeout",
                        active_tasks,
                        unacknowledged_tasks,
                        remaining_tasks = streaming_tasks.settling.len(),
                        orphan_risk = true,
                        "Vendored streaming tasks missed bounded terminal settlement"
                    );
                }
                StreamingTaskDrainOutcome::Incomplete {
                    active_tasks,
                    unacknowledged_tasks,
                    drain_failure,
                } => {
                    error!(
                        event = "streaming_task_settlement_incomplete",
                        active_tasks,
                        unacknowledged_tasks,
                        drain_failure = ?drain_failure,
                        remaining_tasks = streaming_tasks.settling.len(),
                        orphan_risk = true,
                        "Vendored streaming task drain ended without terminal settlement"
                    );
                }
            }

            // Graceful shutdown of all tailers
            supervisor.shutdown().await;
        })
    }

    #[cfg(feature = "native-wezterm")]
    fn native_event_listener_error_class(
        error: &crate::native_events::NativeEventError,
    ) -> &'static str {
        match error {
            crate::native_events::NativeEventError::EmptySocketPath => "empty_socket_path",
            crate::native_events::NativeEventError::ContextUnavailable => {
                "context_unavailable"
            }
            crate::native_events::NativeEventError::SocketAlreadyExists(_) => {
                "socket_already_exists"
            }
            crate::native_events::NativeEventError::Security(_) => "security_failure",
            crate::native_events::NativeEventError::Io(_) => "io_failure",
            crate::native_events::NativeEventError::ConnectionTaskAdmissionFailed => {
                "connection_task_admission_failed"
            }
            crate::native_events::NativeEventError::ConnectionTaskDrainTimedOut => {
                "connection_task_drain_timeout"
            }
            crate::native_events::NativeEventError::ConnectionTaskDrainIncomplete => {
                "connection_task_drain_incomplete"
            }
        }
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
        let storage = self.storage.clone();
        let metrics = Arc::clone(&self.metrics);
        let event_bus = self.event_bus.clone();
        let pane_filter = self.config.pane_filter.clone();
        let capture_authority = self.capture_authority.clone();

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let listener = match NativeEventListener::bind_with_cx(&loop_cx, socket_path.clone())
                .await
            {
                Ok(listener) => {
                    if loop_cx.checkpoint().is_err() {
                        record_runtime_wait_failure(
                            "native_event_bind",
                            runtime_context_failure_kind(&loop_cx),
                        );
                        return;
                    }
                    info!(
                        path = %socket_path.display(),
                        "Native event listener bound — waiting for GUI connections"
                    );
                    listener
                }
                Err(_) if loop_cx.checkpoint().is_err() => {
                    record_runtime_wait_failure(
                        "native_event_bind",
                        runtime_context_failure_kind(&loop_cx),
                    );
                    return;
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
            let mut accept_task = AbortOnDropNativeAcceptTask::new(spawn_runtime_task(
                &loop_cx,
                move |accept_cx| async move {
                    let listener_result = listener
                        .run_with_cx(
                            &accept_cx,
                            event_tx,
                            Arc::clone(&accept_shutdown_flag),
                        )
                        .await;
                    // `run_with_cx` reports observed cancellation and requested
                    // shutdown as `Ok(())`. Every error here is therefore an
                    // actionable listener or terminal-settlement failure, even
                    // if shutdown became visible concurrently with its return.
                    match listener_result {
                        Ok(()) => {}
                        Err(error) => {
                            warn!(
                                event = "native_event_listener_failed",
                                failure_class = Self::native_event_listener_error_class(&error),
                                "Native event listener stopped after a typed failure"
                            );
                        }
                    }
                },
            ));

            let mut coalescer = NativeOutputCoalescer::new(
                NATIVE_OUTPUT_COALESCE_WINDOW_MS,
                NATIVE_OUTPUT_COALESCE_MAX_DELAY_MS,
                NATIVE_OUTPUT_COALESCE_MAX_BYTES,
            );
            // Keep coalescing deadlines in the same clock domain as the Cx
            // timer used by runtime_timeout. Mixing std::Instant with a lab
            // timer can turn an elapsed virtual timeout into a wall-clock spin.
            let start = crate::runtime_async::timer_now_with_cx(&loop_cx);
            let flush_interval = Duration::from_millis(NATIVE_OUTPUT_COALESCE_WINDOW_MS / 2)
                .max(Duration::from_millis(5));
            let mut next_flush = start + flush_interval;
            let mut drain_pending_output_on_shutdown = false;
            let mut shutdown_drain_state = NativeEventShutdownDrainState::Running;

            'native_events: loop {
                if loop_cx.checkpoint().is_err() {
                    break;
                }
                let now = crate::runtime_async::timer_now_with_cx(&loop_cx);
                match shutdown_drain_state
                    .advance(now, shutdown_flag.load(Ordering::SeqCst))
                {
                    NativeEventShutdownDrainAction::None => {}
                    NativeEventShutdownDrainAction::BeginGraceful => {
                        // Stop admitting new work through the shared shutdown
                        // flag, then receive until the producer closes. This
                        // drains every event admitted before graceful shutdown.
                        drain_pending_output_on_shutdown = true;
                    }
                    NativeEventShutdownDrainAction::AbortProducer => {
                        warn!(
                            grace_ms = NATIVE_ACCEPT_TASK_GRACEFUL_TIMEOUT.as_millis(),
                            "Native event listener missed graceful shutdown deadline; escalating to abort"
                        );
                        // Retain the receiver after abort so every event already
                        // admitted to the bounded channel can still be drained.
                        accept_task.abort();
                    }
                    NativeEventShutdownDrainAction::Abandon => {
                        warn!(
                            event = "native_event_queue_drain_timeout",
                            queued_events_observed = event_rx.len(),
                            drain_ms = NATIVE_EVENT_QUEUE_DRAIN_TIMEOUT.as_millis(),
                            data_loss_indeterminate = true,
                            loss_scope = "queued_or_inflight_native_events",
                            "Native event queue remained open after producer abort; abandoning the bounded drain with explicit loss telemetry"
                        );
                        break;
                    }
                    NativeEventShutdownDrainAction::ProducerClosed => {
                        debug_assert!(shutdown_drain_state.producer_closed());
                        break;
                    }
                }
                if now >= next_flush {
                    next_flush = now + flush_interval;
                    let now_ms = now.duration_since(start) / 1_000_000;
                    for item in coalescer.drain_due(now_ms) {
                        if loop_cx.checkpoint().is_err() {
                            break 'native_events;
                        }
                        metrics.record_native_output_batch(item.input_events, item.bytes.len());
                        emit_native_output_delta(
                            &loop_cx,
                            item.pane_id,
                            item.bytes,
                            item.timestamp_ms,
                            &capture_tx,
                            &cursors,
                            metrics.backpressure_metrics(),
                            &item.producer_guard,
                        )
                        .await;
                    }

                    continue;
                }

                let flush_wait = Duration::from_nanos(next_flush.duration_since(now));
                match runtime_timeout(&loop_cx, flush_wait, recv_event(&loop_cx, &mut event_rx))
                    .await
                {
                    Ok(RecvEvent::Item(event)) => {
                        if loop_cx.checkpoint().is_err() {
                            break;
                        }

                        match event {
                            NativeEvent::PaneOutput {
                                pane_id,
                                data,
                                timestamp_ms,
                                dropped_bytes,
                            } => {
                                let Some(producer_guard) = acquire_native_producer(
                                    &capture_authority,
                                    &metrics,
                                    pane_id,
                                ) else {
                                    continue;
                                };
                                metrics.record_native_output_input(data.len());
                                if data.is_empty() {
                                    if dropped_bytes > 0 {
                                        if loop_cx.checkpoint().is_err() {
                                            break 'native_events;
                                        }
                                        emit_native_output_gap(
                                            &loop_cx,
                                            pane_id,
                                            &crate::native_events::native_output_truncation_gap_reason(
                                                dropped_bytes,
                                            ),
                                            &capture_tx,
                                            &cursors,
                                            &producer_guard,
                                        )
                                        .await;
                                    }
                                    continue;
                                }
                                let now_ms = crate::runtime_async::timer_now_with_cx(&loop_cx)
                                    .duration_since(start)
                                    / 1_000_000;
                                if let Some(item) =
                                    coalescer.push(
                                        pane_id,
                                        data,
                                        timestamp_ms,
                                        now_ms,
                                        producer_guard,
                                    )
                                {
                                    if loop_cx.checkpoint().is_err() {
                                        break 'native_events;
                                    }
                                    metrics.record_native_output_batch(
                                        item.input_events,
                                        item.bytes.len(),
                                    );
                                    emit_native_output_delta(
                                        &loop_cx,
                                        item.pane_id,
                                        item.bytes,
                                        item.timestamp_ms,
                                        &capture_tx,
                                        &cursors,
                                        metrics.backpressure_metrics(),
                                        &item.producer_guard,
                                    )
                                    .await;
                                }
                                if dropped_bytes > 0 {
                                    // ft-wtd5g: the wire frame was truncated at the
                                    // decode bound. Flush any buffered delta for this
                                    // pane first so the partial data is recorded in
                                    // order, then inject an explicit capture gap so
                                    // replay records the loss instead of treating the
                                    // holed stream as complete (mirrors the capture-
                                    // tiering explicit_gap invariant).
                                    if let Some(item) = coalescer.flush_pane(pane_id) {
                                        if loop_cx.checkpoint().is_err() {
                                            break 'native_events;
                                        }
                                        metrics.record_native_output_batch(
                                            item.input_events,
                                            item.bytes.len(),
                                        );
                                        emit_native_output_delta(
                                            &loop_cx,
                                            item.pane_id,
                                            item.bytes,
                                            item.timestamp_ms,
                                            &capture_tx,
                                            &cursors,
                                            metrics.backpressure_metrics(),
                                            &item.producer_guard,
                                        )
                                        .await;
                                        if loop_cx.checkpoint().is_err() {
                                            break 'native_events;
                                        }
                                        emit_native_output_gap(
                                            &loop_cx,
                                            pane_id,
                                            &crate::native_events::native_output_truncation_gap_reason(
                                                dropped_bytes,
                                            ),
                                            &capture_tx,
                                            &cursors,
                                            &item.producer_guard,
                                        )
                                        .await;
                                    }
                                }
                            }
                            NativeEvent::StateChange { pane_id, .. }
                            | NativeEvent::PaneDestroyed { pane_id, .. } => {
                                if let Some(item) = coalescer.flush_pane(pane_id) {
                                    if loop_cx.checkpoint().is_err() {
                                        break 'native_events;
                                    }
                                    metrics.record_native_output_batch(
                                        item.input_events,
                                        item.bytes.len(),
                                    );
                                    emit_native_output_delta(
                                        &loop_cx,
                                        item.pane_id,
                                        item.bytes,
                                        item.timestamp_ms,
                                        &capture_tx,
                                        &cursors,
                                        metrics.backpressure_metrics(),
                                        &item.producer_guard,
                                    )
                                    .await;
                                }

                                if loop_cx.checkpoint().is_err() {
                                    break 'native_events;
                                }
                                handle_native_event(
                                    &loop_cx,
                                    event,
                                    &capture_tx,
                                    &cursors,
                                    &storage,
                                    event_bus.as_ref(),
                                    &pane_filter,
                                    metrics.backpressure_metrics(),
                                    &capture_authority,
                                    &metrics,
                                )
                                .await;
                            }
                            _ => {
                                if loop_cx.checkpoint().is_err() {
                                    break 'native_events;
                                }
                                handle_native_event(
                                    &loop_cx,
                                    event,
                                    &capture_tx,
                                    &cursors,
                                    &storage,
                                    event_bus.as_ref(),
                                    &pane_filter,
                                    metrics.backpressure_metrics(),
                                    &capture_authority,
                                    &metrics,
                                )
                                .await;
                            }
                        }
                    }
                    Ok(RecvEvent::Closed) => {
                        // Producer completion is a clean terminal boundary;
                        // preserve already-coalesced bytes before exit.
                        let producer_closed = shutdown_drain_state.mark_producer_closed();
                        debug_assert_eq!(
                            producer_closed,
                            NativeEventShutdownDrainAction::ProducerClosed,
                        );
                        drain_pending_output_on_shutdown = true;
                        break;
                    }
                    Ok(RecvEvent::Cancelled) => {
                        debug!("Native event coalescer recv cancelled");
                        break;
                    }
                    Err(RuntimeTimeoutFailure::Context(failure)) => {
                        record_runtime_wait_failure("native_event_recv", failure);
                        break;
                    }
                    Err(RuntimeTimeoutFailure::Elapsed) => {
                        // Timer elapsed; next loop iteration drains due batches.
                    }
                }
            }

            // A graceful shutdown deliberately flushes already-admitted output.
            // Direct Cx cancellation is different: it forbids new output side
            // effects, so dropping the coalescer releases pending guards/data.
            if drain_pending_output_on_shutdown {
                for item in coalescer.drain_all() {
                    if loop_cx.checkpoint().is_err() {
                        break;
                    }
                    metrics.record_native_output_batch(item.input_events, item.bytes.len());
                    emit_native_output_delta(
                        &loop_cx,
                        item.pane_id,
                        item.bytes,
                        item.timestamp_ms,
                        &capture_tx,
                        &cursors,
                        metrics.backpressure_metrics(),
                        &item.producer_guard,
                    )
                    .await;
                }
            }

            let accept_settlement = if shutdown_drain_state.producer_closed() {
                // Sender closure proves `run_with_cx` returned after its own
                // nested connection drain. Let the small wrapper task finish
                // logging that typed result instead of aborting it at the last
                // instruction.
                accept_task.settle().await
            } else {
                accept_task.abort_and_settle().await
            };
            match accept_settlement {
                NativeAcceptTaskSettlement::Settled => {}
                NativeAcceptTaskSettlement::SettledWithFailure { failure } => {
                    warn!(
                        failure_class = ?failure,
                        "Native event listener settled after a task failure"
                    );
                }
                NativeAcceptTaskSettlement::TimedOut {
                    active_tasks,
                    unacknowledged_tasks,
                } => {
                    warn!(
                        event = "native_accept_task_drain_timeout",
                        active_tasks,
                        unacknowledged_tasks,
                        orphan_risk = true,
                        "Native event listener missed bounded terminal settlement"
                    );
                }
                NativeAcceptTaskSettlement::Incomplete {
                    active_tasks,
                    unacknowledged_tasks,
                    drain_failure,
                } => {
                    warn!(
                        event = "native_accept_task_settlement_incomplete",
                        active_tasks,
                        unacknowledged_tasks,
                        drain_failure = ?drain_failure,
                        orphan_risk = true,
                        "Native event listener drain failed before terminal settlement"
                    );
                }
            }
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
        let metrics = Arc::clone(&self.metrics);

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            loop {
                // Graceful shutdown intentionally drains already-admitted
                // ingress below. Direct capability cancellation does not: it
                // must stop before the next persistence-side mutation.
                if loop_cx.checkpoint().is_err() {
                    break;
                }
                record_capture_pipeline_depth(&metrics, &capture_ingress_rx, &capture_ring_tx, 0);
                match runtime_timeout(
                    &loop_cx,
                    Duration::from_millis(25),
                    recv_event(&loop_cx, &mut capture_ingress_rx),
                )
                .await
                {
                    Ok(RecvEvent::Item(event)) => {
                        if loop_cx.checkpoint().is_err() {
                            break;
                        }
                        record_capture_pipeline_depth(
                            &metrics,
                            &capture_ingress_rx,
                            &capture_ring_tx,
                            1,
                        );
                        if shutdown_flag.load(Ordering::SeqCst) {
                            debug!(
                                "Capture relay: shutdown signal received, draining remaining events"
                            );
                        }

                        // ft-xbnl0.2.3 tick 267: cx-first capture relay enqueue.
                        // `SpscProducer::send_with_cx` performs another
                        // checkpoint immediately before the queue push, closing
                        // the final race between this boundary and mutation.
                        if let Err(error) =
                            relay_capture_event_with_cx(&loop_cx, &capture_ring_tx, event).await
                        {
                            metrics.record_capture_queue_depth(0);
                            if is_runtime_cancellation(&error) {
                                debug!("Capture relay: capability context cancelled");
                            } else {
                                debug!("Capture relay: persistence ring closed");
                            }
                            return;
                        }
                        record_capture_pipeline_depth(
                            &metrics,
                            &capture_ingress_rx,
                            &capture_ring_tx,
                            0,
                        );
                    }
                    Ok(RecvEvent::Closed) => break,
                    Ok(RecvEvent::Cancelled) => {
                        debug!("Capture relay recv cancelled");
                        break;
                    }
                    Err(RuntimeTimeoutFailure::Context(failure)) => {
                        record_runtime_wait_failure("capture_relay_recv", failure);
                        break;
                    }
                    Err(RuntimeTimeoutFailure::Elapsed) => {
                        if shutdown_flag.load(Ordering::SeqCst) && capture_ingress_rx.is_empty() {
                            break;
                        }
                    }
                }
            }

            metrics.record_capture_queue_depth(capture_ring_tx.depth());
            capture_ring_tx.close();
            debug!("Capture relay exited");
        })
    }

    /// Spawn the persistence and detection task.
    fn spawn_persistence_task(
        &self,
        capture_rx: SpscConsumer<CaptureEvent>,
        cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
        discovery_publication_rx: watch::Receiver<DiscoveryCapturePublication>,
        capture_checkpoints: CaptureCheckpointCache,
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
        let wezterm_handle = Arc::clone(&self.wezterm_handle);
        let mut config_rx = self.config_rx.clone();
        let mut current_patterns = self.config.patterns.clone();
        let patterns_root = self.config.patterns_root.clone();
        let semantic_zone_cache_ttl = self.config.capture_interval.max(Duration::from_millis(1));
        let capture_authority = self.capture_authority.clone();
        let capture_metadata = Arc::clone(&self.capture_metadata);
        let replay_capture = self.replay_capture.clone();

        let loop_cx = runtime_loop_cx();
        spawn_runtime_task(&loop_cx, move |loop_cx| async move {
            let max_persist_segment_bytes = tuning.ingest.max_persist_segment_bytes;
            let mut semantic_zone_cache =
                HashMap::<CapturePaneCacheKey, CachedSemanticZoneSnapshot>::new();
            let mut latest_incarnation_by_pane = HashMap::<u64, PaneIncarnation>::new();
            let mut bocpd_manager =
                crate::bocpd::BocpdManager::new(crate::bocpd::BocpdConfig::default());
            let mut bocpd_last_capture_at = HashMap::<u64, i64>::new();

            // Process events until producer is closed and the ring is drained.
            while let Some(mut event) = capture_rx.recv().await {
                let mut resync_decision = event.take_resync_decision();
                let stamp = event.stamp();
                let (persistence_guard, capture_pane_metadata) = match
                    admit_capture_event_for_persistence(
                        &capture_authority,
                        &capture_metadata,
                        &discovery_publication_rx,
                        &event,
                    )
                    .await
                {
                    Ok(admission) => admission,
                    Err(error) => {
                        metrics.capture_authority_rejections.increment();
                        debug!(
                            pane_id = event.segment.pane_id,
                            pane_incarnation = stamp.pane_incarnation().get(),
                            source_kind = ?stamp.source_kind(),
                            error = %error,
                            "Rejected stale capture event before semantic side effects"
                        );
                        if let Some(decision) = resync_decision.as_mut() {
                            decision.finish(Err(error.to_string()));
                        }
                        continue;
                    }
                };
                let pane_incarnation = stamp.pane_incarnation();
                metrics.record_capture_queue_depth(capture_rx.depth());
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
                if let Some(previous_incarnation) =
                    latest_incarnation_by_pane.insert(pane_id, pane_incarnation)
                    && previous_incarnation != pane_incarnation
                {
                    semantic_zone_cache.retain(|(cached_pane_id, cached_incarnation), _| {
                        *cached_pane_id != pane_id || *cached_incarnation == pane_incarnation
                    });
                    bocpd_manager.unregister_pane(pane_id);
                    bocpd_last_capture_at.remove(&pane_id);
                    let mut contexts = detection_contexts.write().await;
                    contexts.remove(&pane_id);
                }
                let bounded_segment =
                    bounded_segment_for_persistence(&event.segment, max_persist_segment_bytes);
                let captured_at = bounded_segment.captured_at;
                let captured_seq = bounded_segment.seq;
                let zone_type = semantic_zone_type_for_captured_segment(
                    &loop_cx,
                    &wezterm_handle,
                    &mut semantic_zone_cache,
                    semantic_zone_cache_ttl,
                    &bounded_segment,
                    pane_incarnation,
                    true,
                )
                .await;

                // Persist the segment
                // ft-xbnl0.2.3 tick 254: cx-first segment persist.
                let checkpoint_write = begin_capture_checkpoint_write(
                    &capture_checkpoints,
                    pane_id,
                    capture_pane_metadata.discovery_revision,
                );
                match persist_captured_segment_for_runtime(
                    &loop_cx,
                    &storage,
                    &bounded_segment,
                    max_persist_segment_bytes,
                    zone_type.as_deref(),
                    &persistence_guard,
                )
                .await
                {
                    Ok(persisted) => {
                        confirm_capture_checkpoint(
                            &capture_checkpoints,
                            pane_id,
                            &checkpoint_write,
                            persisted.segment.seq,
                            &bounded_segment.content,
                        );
                        // Check for sequence discontinuity and resync cursor if needed
                        if persisted.segment.seq != captured_seq {
                            warn!(
                                pane_id,
                                expected_seq = captured_seq,
                                actual_seq = persisted.segment.seq,
                                "Sequence discontinuity detected, resyncing cursor"
                            );
                            let mut cursors_guard = cursors.write().await;
                            let Some(cursor) = cursors_guard.get_mut(&pane_id) else {
                                let error = runtime_backend_error(
                                    "capture.persistence.cursor",
                                    format!(
                                        "pane {pane_id} has no cursor for mandatory sequence correction"
                                    ),
                                );
                                error!(
                                    pane_id,
                                    error = %error,
                                    "Durable capture cannot continue semantic fanout without cursor correction"
                                );
                                if let Some(decision) = resync_decision.as_mut() {
                                    decision.finish(Err(error.to_string()));
                                }
                                continue;
                            };
                            cursor.resync_seq(persisted.segment.seq);
                        }

                        // This acknowledgement is deliberately tied to the
                        // durable segment commit and mandatory cursor sequence
                        // reconciliation, not to the independent
                        // recording/detection fanout below.  Acknowledging
                        // before a storage-assigned sequence correction can
                        // expose the successor with a stale next sequence;
                        // delaying it through downstream fanout can instead
                        // time out after the gap is durable and duplicate that
                        // gap on retry.  The persistence guard remains alive
                        // for the entire semantic chain, so revocation still
                        // waits for every admitted predecessor side effect.
                        if let Some(decision) = resync_decision.as_mut() {
                            decision.finish(Ok(persisted.segment.seq));
                        }
                        if let Some(ref adapter) = replay_capture {
                            if let Err(error) = record_authorized_replay_egress(
                                adapter,
                                &bounded_segment,
                                persisted.segment.seq,
                                &persistence_guard,
                            ) {
                                adapter.record_sequence_error("runtime.authorized_egress", error);
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
                                .record_segment_with_cx(&loop_cx, &bounded_segment)
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
                        let mut detections = {
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

                        if let Some(detection) = observe_bocpd_segment_for_runtime(
                            &mut bocpd_manager,
                            &mut bocpd_last_capture_at,
                            &bounded_segment,
                            persisted.gap.is_some(),
                        ) {
                            detections.push(detection);
                        }

                        if !detections.is_empty() {
                            debug!(
                                pane_id = pane_id,
                                count = detections.len(),
                                "Pattern detections"
                            );

                            let pane_uuid = Some(capture_pane_metadata.pane_uuid.clone());

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
                                let delegated_hold = match persistence_guard.delegate_storage() {
                                    Ok(hold) => hold,
                                    Err(error) => {
                                        error!(
                                            pane_id,
                                            rule_id = detection.rule_id,
                                            error = %error,
                                            "Failed to delegate capture authority to event storage"
                                        );
                                        continue;
                                    }
                                };
                                match storage
                                    .record_capture_event_outcome_with_cx(
                                        &loop_cx,
                                        stored_event,
                                        delegated_hold,
                                    )
                                    .await
                                {
                                    Ok(outcome) => {
                                        if let Some(event_id) = outcome.inserted_event_id() {
                                            metrics.events_recorded.increment();

                                            // Publish to event bus for workflow runners (if configured)
                                            if let Some(ref bus) = event_bus {
                                                // FND-010 / INV-RED-1: the stored row is
                                                // redacted, but the live event bus was
                                                // publishing the RAW in-memory detection
                                                // (matched_text can contain a secret) to
                                                // subscribers (web SSE, workflow runners).
                                                // Redact the emitted copy too.
                                                let event = crate::events::Event::PatternDetected {
                                                    pane_id,
                                                    pane_uuid: pane_uuid.clone(),
                                                    detection: redact_detection(&detection),
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
                                        } else {
                                            debug!(
                                                pane_id,
                                                rule_id = %detection.rule_id,
                                                event_id = outcome.event_id(),
                                                "Suppressed duplicate detection after durable dedupe"
                                            );
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
                        if let Some(decision) = resync_decision.as_mut() {
                            decision.finish(Err(e.to_string()));
                        }
                    }
                }
            }

            metrics.record_capture_queue_depth(0);
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
// Native event handling keeps runtime subsystems explicit at the mux boundary.
#[allow(clippy::too_many_arguments)]
async fn handle_native_event(
    runtime_cx: &RuntimeLoopCx,
    event: NativeEvent,
    capture_tx: &mpsc::Sender<CaptureEvent>,
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
    storage: &StorageHandle,
    event_bus: Option<&Arc<EventBus>>,
    pane_filter: &PaneFilterConfig,
    backpressure: &Arc<BackpressureMetrics>,
    capture_authority: &CaptureAuthority,
    metrics: &RuntimeMetrics,
) {
    if runtime_cx.checkpoint().is_err() {
        return;
    }
    match event {
        NativeEvent::PaneOutput {
            pane_id,
            data,
            timestamp_ms,
            dropped_bytes,
        } => {
            let Some(producer_guard) =
                acquire_native_producer(capture_authority, metrics, pane_id)
            else {
                return;
            };
            emit_native_output_delta(
                runtime_cx,
                pane_id,
                data,
                timestamp_ms,
                capture_tx,
                cursors,
                backpressure,
                &producer_guard,
            )
            .await;
            if dropped_bytes > 0 {
                // ft-wtd5g: mirror the main loop — a truncated frame must leave
                // an explicit capture gap so replay records the loss.
                emit_native_output_gap(
                    runtime_cx,
                    pane_id,
                    &crate::native_events::native_output_truncation_gap_reason(dropped_bytes),
                    capture_tx,
                    cursors,
                    &producer_guard,
                )
                .await;
            }
        }
        NativeEvent::StateChange { pane_id, state, .. } => {
            let Some(producer_guard) =
                acquire_native_producer(capture_authority, metrics, pane_id)
            else {
                return;
            };
            let mut gap_segment = None;
            {
                let mut cursors_guard = match cursors.write_with_cx(runtime_cx).await {
                    Ok(guard) => guard,
                    Err(_) => {
                        record_runtime_wait_failure(
                            "native_state_change_cursor_lock",
                            if runtime_cx.checkpoint().is_err() {
                                runtime_context_failure_kind(runtime_cx)
                            } else {
                                RuntimeWaitFailureKind::ContextFailure
                            },
                        );
                        return;
                    }
                };
                if runtime_cx.checkpoint().is_err() {
                    return;
                }
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
                let event = match CaptureEvent::from_producer(segment, &producer_guard) {
                    Ok(event) => event,
                    Err(error) => {
                        debug!(pane_id, error = %error, "Rejected native state-change gap");
                        return;
                    }
                };
                if capture_tx.try_send(event).is_err() {
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
                        if runtime_cx.checkpoint().is_err() {
                            return;
                        }
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
        }
        NativeEvent::PaneDestroyed { pane_id, .. } => {
            if runtime_cx.checkpoint().is_err() {
                return;
            }
            // Native lifecycle frames do not own cursor/context teardown.
            // Discovery first withdraws the pane from capture publication;
            // capture reconciliation removes semantic runtime state only after
            // the exact predecessor authority has drained. Drop attribution is
            // safe to clear eagerly: a concurrent late drop recreates its entry,
            // and the coordinator's terminal cleanup clears it again.
            let _ = backpressure.cleanup_pane(pane_id);
            // [ft-pp7jk] Publish Event::PaneDisappeared so downstream
            // subscribers (workflow runners, policy engines, any future
            // long-lived subsystem that accumulates per-pane state)
            // can release their own caches. The event variant was
            // declared in events.rs (line 181) but never emitted from
            // the production runtime — every consumer that matched on
            // it would stay dormant forever. This closes the contract
            // so PaneDisappeared behaves symmetrically with
            // PaneDiscovered (already published elsewhere in the
            // runtime). Ignores the delivered-count return because
            // best-effort fanout is the contract for all pane
            // lifecycle events on this bus.
            if let Some(bus) = event_bus {
                let _ = bus.publish(Event::PaneDisappeared { pane_id });
            }
        }
    }
}

#[cfg(feature = "native-wezterm")]
async fn emit_native_output_delta(
    runtime_cx: &RuntimeLoopCx,
    pane_id: u64,
    data: Vec<u8>,
    timestamp_ms: i64,
    capture_tx: &mpsc::Sender<CaptureEvent>,
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
    backpressure: &Arc<BackpressureMetrics>,
    producer_guard: &CaptureProducerGuard,
) {
    if data.is_empty() || runtime_cx.checkpoint().is_err() {
        return;
    }

    let content = String::from_utf8_lossy(&data).to_string();
    let segment = {
        let mut cursors_guard = match cursors.write_with_cx(runtime_cx).await {
            Ok(guard) => guard,
            Err(_) => {
                record_runtime_wait_failure(
                    "native_output_cursor_lock",
                    if runtime_cx.checkpoint().is_err() {
                        runtime_context_failure_kind(runtime_cx)
                    } else {
                        RuntimeWaitFailureKind::ContextFailure
                    },
                );
                return;
            }
        };
        if runtime_cx.checkpoint().is_err() {
            return;
        }
        cursors_guard
            .get_mut(&pane_id)
            .map(|cursor| cursor.capture_delta(content, timestamp_ms))
    };

    if let Some(segment) = segment {
        let event = match CaptureEvent::from_producer(segment, producer_guard) {
            Ok(event) => event,
            Err(error) => {
                debug!(pane_id, error = %error, "Rejected native output capture event");
                return;
            }
        };
        if capture_tx.try_send(event).is_err() {
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

/// ft-wtd5g: inject an explicit capture gap for a pane after a native-output
/// truncation/drop, so replay/recorder records the loss instead of treating the
/// holed stream as complete (mirrors the capture-tiering `explicit_gap`
/// invariant). Best-effort under capture backpressure: if the capture queue is
/// itself full the gap marker is dropped with a debug line — the truncation is
/// still surfaced by the upstream `dropped_bytes` accounting.
#[cfg(feature = "native-wezterm")]
async fn emit_native_output_gap(
    runtime_cx: &RuntimeLoopCx,
    pane_id: u64,
    reason: &str,
    capture_tx: &mpsc::Sender<CaptureEvent>,
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
    producer_guard: &CaptureProducerGuard,
) {
    if runtime_cx.checkpoint().is_err() {
        return;
    }
    let gap = {
        let mut cursors_guard = match cursors.write_with_cx(runtime_cx).await {
            Ok(guard) => guard,
            Err(_) => {
                record_runtime_wait_failure(
                    "native_output_gap_cursor_lock",
                    if runtime_cx.checkpoint().is_err() {
                        runtime_context_failure_kind(runtime_cx)
                    } else {
                        RuntimeWaitFailureKind::ContextFailure
                    },
                );
                return;
            }
        };
        if runtime_cx.checkpoint().is_err() {
            return;
        }
        cursors_guard
            .get_mut(&pane_id)
            .map(|cursor| cursor.emit_gap(reason))
    };
    match gap {
        Some(segment) => {
            let event = match CaptureEvent::from_producer(segment, producer_guard) {
                Ok(event) => event,
                Err(error) => {
                    debug!(pane_id, error = %error, "Rejected native output gap event");
                    return;
                }
            };
            if capture_tx.try_send(event).is_err() {
                debug!(
                    pane_id,
                    "native output gap marker dropped; capture queue full"
                );
            }
        }
        None => {
            debug!(
                pane_id,
                "native output gap: cursor not initialized; loss unrecorded"
            );
        }
    }
}

#[cfg(all(feature = "vendored", unix))]
async fn run_vendored_streaming_capture(
    identity: StreamingSubscriptionIdentity,
    compression_mode: crate::config::VendoredCompressionMode,
    subscription_config: SubscriptionConfig,
    capture_tx: mpsc::Sender<CaptureEvent>,
    capture_lease: CaptureLease,
) -> String {
    let runtime_cx = runtime_loop_cx();

    let mut bridge = StreamingBridge::new();
    let mut client_config =
        DirectMuxClientConfig::default().with_socket_path(identity.socket_path.clone());
    client_config.compression_mode = compression_mode;

    let client =
        match Box::pin(DirectMuxClient::connect_with_cx(&runtime_cx, client_config)).await {
            Ok(client) => client,
            Err(err) => {
                bridge.record_fallback();
                return format!("connect error: {err}");
            }
        };

    info!(
        pane_id = identity.global_pane_id,
        local_pane_id = identity.local_pane_id,
        socket_shard = identity.socket_shard.0,
        generation = identity.generation,
        socket = %identity.socket_path.display(),
        "Started vendored pane streaming subscription"
    );

    let mut subscription = subscribe_pane_output_with_inherited_cx(
        &runtime_cx,
        client,
        identity.local_pane_id,
        subscription_config.clone(),
    );

    let exit_reason = loop {
        match subscription.next_with_cx(&runtime_cx).await {
            Some(delta) => {
                if let Some(reason) = forward_vendored_streaming_delta(
                    &runtime_cx,
                    &mut bridge,
                    &capture_tx,
                    &identity,
                    &capture_lease,
                    delta,
                )
                .await
                {
                    break reason;
                }
            }
            None if runtime_cx.checkpoint().is_err() => break "cancelled".to_string(),
            None => break "subscription channel closed".to_string(),
        }
    };

    if should_record_streaming_fallback(&exit_reason) {
        bridge.record_fallback();
    }
    subscription.shutdown_with_cx(&runtime_cx).await;
    exit_reason
}

/// Handle to the running observation runtime.
pub struct RuntimeHandle {
    /// Discovery task handle. Wrapped in `Option` so the consumer
    /// shutdown methods (`join`, `shutdown_with_timeout`,
    /// `shutdown_with_summary`) can `.take()` the handle and
    /// `.await` it, without conflicting with the defensive
    /// [`Drop`] impl that aborts any handle still in place when
    /// the runtime exits abnormally.
    pub discovery: Option<JoinHandle<()>>,
    /// Capture task handle. See [`Self::discovery`] for the
    /// `Option` rationale.
    pub capture: Option<JoinHandle<()>>,
    /// Relay task handle (capture ingress -> SPSC persistence
    /// queue). See [`Self::discovery`] for the `Option` rationale.
    pub relay: Option<JoinHandle<()>>,
    /// Native events listener task handle (optional by design,
    /// not by the orphan-on-drop pattern).
    pub native_events: Option<JoinHandle<()>>,
    /// Persistence task handle. See [`Self::discovery`] for the
    /// `Option` rationale.
    pub persistence: Option<JoinHandle<()>>,
    /// Maintenance task handle (retention, checkpointing)
    pub maintenance: Option<JoinHandle<()>>,
    /// Connector outbound bridge task handle (EventBus → host runtime dispatch).
    pub connector_outbound: Option<JoinHandle<()>>,
    /// Snapshot engine task handle (session persistence)
    pub snapshot: Option<JoinHandle<()>>,
    /// Snapshot trigger bridge task handle (event/health → snapshot trigger)
    pub snapshot_triggers: Option<JoinHandle<()>>,
    /// Snapshot engine shutdown sender (bridges AtomicBool → watch channel).
    ///
    /// The `let _ = tx.send(true)` calls are intentional best-effort wake-ups:
    /// a send error means every receiver has already gone away, so retrying
    /// cannot help. Receiver loss is not itself a clean-shutdown receipt;
    /// `snapshot_scheduler_status` distinguishes an acknowledged shutdown from
    /// an unexpected return or failure after the task join settles.
    snapshot_shutdown: Option<watch::Sender<bool>>,
    /// Set only by the unique RuntimeHandle shutdown owner after every runtime
    /// task has settled, storage has flushed, and the terminal snapshot helper
    /// returns both its final-checkpoint and clean-mark receipt.
    /// `None` means this runtime did not enable the duplicate snapshot surface.
    snapshot_shutdown_clean: Option<Arc<AtomicBool>>,
    /// Snapshot engine retained by the unique shutdown owner. The scheduler
    /// task never finalizes this engine merely because its loop returns.
    snapshot_engine: Option<Arc<crate::snapshot_engine::SnapshotEngine>>,
    /// Finite terminal status published by the scheduler task. A successful
    /// task join is insufficient because an unexpected `Ok(())` return is still
    /// a lifecycle failure.
    snapshot_scheduler_status: Option<Arc<AtomicU8>>,
    /// Explicit shutdown intent shared with the scheduler task. This separates
    /// an expected watch-channel acknowledgement from a spontaneous return.
    snapshot_shutdown_requested: Option<Arc<AtomicBool>>,
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
    /// Runtime-owned inbound connector bridge feeding the live event bus.
    connector_inbound_bridge: Option<Arc<StdMutex<ConnectorInboundBridge>>>,
    /// Heartbeat registry for watchdog monitoring
    pub heartbeats: Arc<HeartbeatRegistry>,
    /// Capture ingress sender retained for runtime-handle lifetime and capacity checks.
    capture_tx: mpsc::Sender<CaptureEvent>,
    /// Combined capacity of the capture ingress queue, relay slot, and persistence ring.
    capture_queue_capacity: usize,
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
const CONNECTOR_OUTBOUND_BRIDGE_TICK_MS: u64 = 250;
const DEFAULT_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneActivityState {
    last_seq: i64,
    last_output_at_ms: u64,
    generation: u32,
    first_seen_at_ms: u64,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct RuntimePaneStateCompaction {
    cursors: CacheCompactionStats,
    detection_contexts: CacheCompactionStats,
    pane_activity_tracker: CacheCompactionStats,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RuntimePaneStateRemoval {
    cursor_removed: bool,
    detection_context_removed: bool,
    pane_activity_removed: bool,
}

const RUNTIME_PANE_STATE_MIN_RETAINED_CAPACITY: usize = 64;

fn shrink_runtime_pane_map_if_sparse<V>(map: &mut HashMap<u64, V>) {
    let capacity = map.capacity();
    let sparse_threshold = map.len().saturating_mul(4);
    if capacity > RUNTIME_PANE_STATE_MIN_RETAINED_CAPACITY && capacity > sparse_threshold {
        let retained_capacity = map
            .len()
            .saturating_mul(2)
            .max(RUNTIME_PANE_STATE_MIN_RETAINED_CAPACITY);
        map.shrink_to(retained_capacity);
    }
}

#[cfg(test)]
fn remove_runtime_pane_state(
    pane_id: u64,
    cursors: &mut HashMap<u64, PaneCursor>,
    detection_contexts: &mut HashMap<u64, DetectionContext>,
    pane_activity_tracker: &mut HashMap<u64, PaneActivityState>,
) -> RuntimePaneStateRemoval {
    let removal = RuntimePaneStateRemoval {
        cursor_removed: cursors.remove(&pane_id).is_some(),
        detection_context_removed: detection_contexts.remove(&pane_id).is_some(),
        pane_activity_removed: pane_activity_tracker.remove(&pane_id).is_some(),
    };
    if removal.cursor_removed {
        shrink_runtime_pane_map_if_sparse(cursors);
    }
    if removal.detection_context_removed {
        shrink_runtime_pane_map_if_sparse(detection_contexts);
    }
    if removal.pane_activity_removed {
        shrink_runtime_pane_map_if_sparse(pane_activity_tracker);
    }
    removal
}

fn route_connector_signal_through_bridge(
    bridge: Option<&Arc<StdMutex<ConnectorInboundBridge>>>,
    signal: &ConnectorSignal,
) -> std::result::Result<BridgeRouteResult, ConnectorBridgeError> {
    let bridge = bridge.ok_or_else(|| ConnectorBridgeError::BridgeUnavailable {
        reason: "runtime was not configured with an event bus".to_string(),
    })?;
    let mut guard = bridge
        .lock()
        .map_err(|_| ConnectorBridgeError::BridgeUnavailable {
            reason: "connector inbound bridge lock poisoned".to_string(),
        })?;
    guard.route_signal(signal)
}

fn process_connector_outbound_runtime_event(
    bridge: &mut ConnectorOutboundBridge,
    event: &Event,
    now_ms: u64,
) {
    let Some(outbound_event) = OutboundEvent::from_runtime_event(event, now_ms) else {
        return;
    };

    match bridge.process_event(&outbound_event) {
        Ok(result) => {
            if result.deduplicated {
                debug!(
                    pane_id = ?outbound_event.pane_id,
                    source = %outbound_event.source,
                    "connector outbound event deduplicated"
                );
            }
        }
        Err(_error) => {
            warn!(
                pane_id = ?outbound_event.pane_id,
                source = %outbound_event.source,
                error_class = "connector_outbound_bridge_rejected",
                "connector outbound bridge rejected runtime event"
            );
            return;
        }
    }

    for action in bridge.drain_actions() {
        dispatch_connector_outbound_action(bridge, &action, now_ms);
    }
}

fn dispatch_connector_outbound_action(
    bridge: &mut ConnectorOutboundBridge,
    action: &ConnectorAction,
    now_ms: u64,
) {
    let operation_name = format!(
        "connector.{}.{}",
        action.target_connector,
        action.action_kind.as_str()
    );
    let mut request = crate::connector_host_runtime::ConnectorOperationRequest::new(
        operation_name,
        action.correlation_id.clone(),
        action.dispatch_capability(),
    );
    if let Some(target) = action.sandbox_target() {
        request = request.with_target(target);
    }
    let result = bridge
        .policy_engine_mut()
        .route_connector_operation_through_mesh(action.target_connector.clone(), request, now_ms);

    match result {
        Ok(dispatch) => {
            bridge.record_action_success(action, now_ms);
            info!(
                connector = %action.target_connector,
                action = %action.action_kind,
                correlation_id = %action.correlation_id,
                operation_id = %dispatch.operation_envelope.operation_id,
                host_id = %dispatch.routing_decision.host_id,
                zone_id = %dispatch.routing_decision.zone_id,
                "connector outbound action accepted by mesh-routed host runtime"
            );
        }
        Err(err) => {
            let kind = connector_operation_dispatch_error_kind(&err);
            let dlq_entry_id = bridge.record_action_failure(action, err.to_string(), kind, now_ms);
            warn!(
                connector = %action.target_connector,
                action = %action.action_kind,
                correlation_id = %action.correlation_id,
                error = %err,
                error_kind = %kind,
                dlq_entry_id = ?dlq_entry_id,
                "connector outbound action rejected by host runtime"
            );
        }
    }
}

fn connector_operation_dispatch_error_kind(
    err: &crate::policy::ConnectorOperationDispatchError,
) -> ConnectorErrorKind {
    if err.is_retryable() {
        ConnectorErrorKind::ServiceUnavailable
    } else {
        ConnectorErrorKind::Permanent
    }
}

async fn remove_runtime_pane_state_for_panes(
    pane_ids: &[u64],
    cursors: &Arc<RwLock<HashMap<u64, PaneCursor>>>,
    detection_contexts: &Arc<RwLock<HashMap<u64, DetectionContext>>>,
    pane_activity_tracker: &Arc<RwLock<HashMap<u64, PaneActivityState>>>,
) {
    if pane_ids.is_empty() {
        return;
    }
    let mut cursors_guard = cursors.write().await;
    let mut contexts_guard = detection_contexts.write().await;
    let mut tracker_guard = pane_activity_tracker.write().await;
    for &pane_id in pane_ids {
        let removal = RuntimePaneStateRemoval {
            cursor_removed: cursors_guard.remove(&pane_id).is_some(),
            detection_context_removed: contexts_guard.remove(&pane_id).is_some(),
            pane_activity_removed: tracker_guard.remove(&pane_id).is_some(),
        };

        debug_assert!(
            !cursors_guard.contains_key(&pane_id)
                && !contexts_guard.contains_key(&pane_id)
                && !tracker_guard.contains_key(&pane_id),
            "pane {pane_id} leaked runtime pane state after teardown: {removal:?}"
        );
    }
    shrink_runtime_pane_map_if_sparse(&mut cursors_guard);
    shrink_runtime_pane_map_if_sparse(&mut contexts_guard);
    shrink_runtime_pane_map_if_sparse(&mut tracker_guard);
}

#[cfg(test)]
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

/// How many of a pane's most recent stored segments to read when building a
/// resume anchor (ft-6lso5).
///
/// Segments are capped at `IngestTuning::DEFAULT_MAX_PERSIST_SEGMENT_BYTES`
/// (64 KiB) but are usually far smaller, so this is a bound on the read rather
/// than a target: the assembled text is truncated to
/// [`crate::ingest::RESUME_ANCHOR_BYTES`] regardless.
const RESUME_ANCHOR_SEGMENT_LIMIT: usize = 32;

/// Concatenate stored segments (newest first, as `get_segments` returns them)
/// into the trailing text used as a resume anchor (ft-6lso5).
fn assemble_resume_anchor(segments: Vec<crate::storage::Segment>) -> String {
    let mut ordered = segments;
    // `get_segments` returns `seq DESC`; the anchor is the text in capture
    // order.
    ordered.sort_by_key(|segment| segment.seq);

    let mut tail = String::new();
    for segment in ordered {
        tail.push_str(&segment.content);
        if tail.len() > crate::ingest::RESUME_ANCHOR_BYTES * 2 {
            // Keep the working string bounded on panes with large segments;
            // only the trailing window can ever matter.
            let trimmed =
                crate::ingest::resume_anchor_tail(&tail, crate::ingest::RESUME_ANCHOR_BYTES)
                    .to_string();
            tail = trimmed;
        }
    }

    crate::ingest::resume_anchor_tail(&tail, crate::ingest::RESUME_ANCHOR_BYTES).to_string()
}

/// Sequence number a re-admitted pane must resume capture at (ft-0kdi9).
///
/// Two independent records survive an observation gap and either can be the
/// further-along one:
///
/// * `max_persisted_seq` — the highest seq actually committed to storage for
///   this pane. Authoritative for collision avoidance: resuming at or below it
///   would re-use a sequence number a row already owns.
/// * `resume_next_seq` — what the registry retired when the pane went
///   unobserved. Fed from the capture pipeline once per discovery tick, so it
///   can lag storage by an interval, but it can also *lead* storage when
///   segments were captured and not yet flushed.
///
/// Taking the maximum keeps `next_seq` monotonic against both.
#[cfg(test)]
fn resumed_capture_next_seq(max_persisted_seq: Option<u64>, resume_next_seq: u64) -> u64 {
    max_persisted_seq
        .map_or(0, |seq| seq.saturating_add(1))
        .max(resume_next_seq)
}

/// Re-create the capture-side state for a pane the observation filter
/// re-admitted (ft-0kdi9).
///
/// The inverse of [`compact_runtime_pane_state`], which drops this state for
/// every unobserved pane. Returns `true` when a cursor was created. An existing
/// cursor is left untouched: it means capture never actually stopped (the
/// Ignored window closed before any compaction ran), and overwriting it with a
/// cursor derived from storage would rewind `next_seq` and re-emit sequence
/// numbers that are already persisted.
///
/// Callers must hold the `cursors` guard before the `detection_contexts` guard
/// (lock-order rank 1 first).
#[cfg(test)]
fn resume_runtime_pane_state(
    pane_id: u64,
    next_seq: u64,
    resume_anchor: String,
    cursors: &mut HashMap<u64, PaneCursor>,
    detection_contexts: &mut HashMap<u64, DetectionContext>,
) -> bool {
    let created = match cursors.entry(pane_id) {
        std::collections::hash_map::Entry::Vacant(vacant) => {
            vacant
                .insert(PaneCursor::from_seq(pane_id, next_seq).with_resume_anchor(resume_anchor));
            true
        }
        std::collections::hash_map::Entry::Occupied(_) => false,
    };

    detection_contexts.entry(pane_id).or_insert_with(|| {
        let mut ctx = DetectionContext::new();
        ctx.pane_id = Some(pane_id);
        ctx
    });

    created
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

/// Classify a single pane's tracked logical bytes against its budget,
/// mirroring [`crate::memory_budget::PaneBudget`] thresholds:
/// `>= budget_bytes` is `OverBudget`, `>= budget_bytes * high_ratio` is
/// `Throttled`, otherwise `Normal`. A zero budget is always `Normal`.
fn classify_pane_budget_level(
    tracked_bytes: u64,
    budget_bytes: u64,
    high_ratio: f64,
) -> BudgetLevel {
    if budget_bytes == 0 {
        return BudgetLevel::Normal;
    }
    // Mirror memory_budget::normalize_high_ratio: NaN falls back to the
    // default soft-limit ratio, otherwise clamp into [0.0, 1.0].
    let high_ratio = if high_ratio.is_nan() {
        MemoryBudgetConfig::default().high_ratio
    } else {
        high_ratio.clamp(0.0, 1.0)
    };
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let high_bytes = (budget_bytes as f64 * high_ratio) as u64;
    if tracked_bytes >= budget_bytes {
        BudgetLevel::OverBudget
    } else if tracked_bytes >= high_bytes {
        BudgetLevel::Throttled
    } else {
        BudgetLevel::Normal
    }
}

/// Derive the worst per-pane memory budget level from the registry's
/// logical arena accounting (ft-6n7hs).
///
/// The worst (highest) level across all reserved panes is returned; an
/// empty iterator is `BudgetLevel::Normal`.
///
/// This feeds `PressureSignals.worst_budget`, which
/// `fleet_memory_controller::map_budget_level` folds into the compound
/// fleet tier driving scrollback eviction. Previously the runtime passed
/// a literal `BudgetLevel::Normal`, freezing this dimension so per-pane
/// budget pressure could never raise the fleet tier on its own. The
/// RSS/cgroup `MemoryBudgetManager` is not wired into the observation
/// runtime; the logical arena accounting sampled here is the
/// eviction-relevant per-pane signal.
fn worst_pane_budget_level<I>(
    tracked_bytes_iter: I,
    budget_bytes: u64,
    high_ratio: f64,
) -> BudgetLevel
where
    I: IntoIterator<Item = u64>,
{
    tracked_bytes_iter
        .into_iter()
        .map(|tracked| classify_pane_budget_level(tracked, budget_bytes, high_ratio))
        .max()
        .unwrap_or(BudgetLevel::Normal)
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
    error_sample_pane_ids: Vec<u64>,
}

/// Bound maintenance telemetry fan-out below the default mux-pool pipeline
/// depth while avoiding one serial round trip per pane. This is deliberately
/// independent of host core count: the work is mux I/O, and a finite cap keeps
/// a large fleet's health pass from competing with interactive traffic.
const PANE_TIERED_SCROLLBACK_FETCH_CONCURRENCY: usize = 16;

impl PaneTieredScrollbackFetch {
    fn note_error(&mut self, pane_id: u64) {
        self.errors = self.errors.saturating_add(1);
        // Concurrent probes may finish in any order. Retain the four smallest
        // failing pane identities so persisted diagnostics remain stable and
        // do not depend on scheduler timing.
        match self.error_sample_pane_ids.binary_search(&pane_id) {
            Ok(_) => {}
            Err(index) if index < 4 => {
                self.error_sample_pane_ids.insert(index, pane_id);
                self.error_sample_pane_ids.truncate(4);
            }
            Err(_) => {}
        }
    }

    fn error_samples(&self) -> Vec<String> {
        // Never reflect backend error text: it can contain paths, pane
        // content, or arbitrarily large caller-controlled strings. Pane ids
        // plus a finite failure class retain enough information to locate the
        // failing probes.
        self.error_sample_pane_ids
            .iter()
            .map(|pane_id| format!("pane {pane_id}: summary_unavailable"))
            .collect()
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
        Err(_error) => {
            fetch.note_error(pane_id);
            debug!(
                pane_id,
                error_class = "pane_summary_unavailable",
                "failed to collect tiered scrollback summary"
            );
        }
    }
}

async fn collect_pane_tiered_scrollback_summaries(
    runtime_cx: &RuntimeLoopCx,
    wezterm_handle: &WeztermHandle,
    pane_ids: &[u64],
) -> Result<PaneTieredScrollbackFetch> {
    runtime_cx.checkpoint().map_err(|_| {
        runtime_cx_error(
            "tiered scrollback collection",
            runtime_cx,
            "capability checkpoint failed before dispatch",
        )
    })?;
    let mut fetch = PaneTieredScrollbackFetch::default();
    let mut remaining = pane_ids.iter().copied();
    let probe = |pane_id| {
        let wezterm_handle = Arc::clone(wezterm_handle);
        async move {
            let result = wezterm_handle
                .pane_tiered_scrollback_summary_with_cx(runtime_cx, pane_id)
                .await;
            (pane_id, result)
        }
    };
    let mut pending = FuturesUnordered::new();
    for pane_id in remaining
        .by_ref()
        .take(PANE_TIERED_SCROLLBACK_FETCH_CONCURRENCY)
    {
        pending.push(probe(pane_id));
    }

    while let Some((pane_id, result)) = pending.next().await {
        match result {
            Err(error) if is_runtime_cancellation(&error) => return Err(error),
            result => record_pane_tiered_scrollback_summary_result(&mut fetch, pane_id, result),
        }
        runtime_cx.checkpoint().map_err(|_| {
            runtime_cx_error(
                "tiered scrollback collection",
                runtime_cx,
                "capability checkpoint failed after pane summary",
            )
        })?;
        if let Some(next_pane_id) = remaining.next() {
            pending.push(probe(next_pane_id));
        }
    }

    Ok(fetch)
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

    // Match BackpressureManager::classify saturation semantics. The absolute
    // "within N slots of full" guard is only meaningful once the queue is
    // already highly filled; otherwise tiny capacities trip `saturating_sub`
    // (e.g. write_capacity=1 → saturating_sub(100)=0 → write_depth>=0 is always
    // true) and classify an EMPTY queue as BLACK. Require a high fill ratio
    // before the absolute margin can escalate to BLACK — this mirrors the
    // resource-types manager's ft-5 fix that the runtime copy had missed
    // (regression: classify_backpressure_tier_matches_manager_semantics).
    let capture_saturated = capture_capacity > 0
        && (capture_ratio >= 0.995
            || (capture_ratio >= 0.95 && capture_depth >= capture_capacity.saturating_sub(5)));
    let write_saturated = write_capacity > 0
        && (write_ratio >= 0.995
            || (write_ratio >= 0.95 && write_depth >= write_capacity.saturating_sub(100)));

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
    tx.capacity()
}

fn record_capture_pipeline_depth(
    metrics: &RuntimeMetrics,
    capture_ingress_rx: &mpsc::Receiver<CaptureEvent>,
    capture_ring_tx: &SpscProducer<CaptureEvent>,
    relay_in_flight: usize,
) {
    let depth = capture_ingress_rx
        .len()
        .saturating_add(capture_ring_tx.depth())
        .saturating_add(relay_in_flight);
    metrics.record_capture_queue_depth(depth);
}

impl RuntimeHandle {
    fn take_task_join_set(&mut self) -> JoinSet<()> {
        let mut tasks = JoinSet::new();
        for handle in [
            self.discovery.take(),
            self.capture.take(),
            self.relay.take(),
            self.native_events.take(),
            self.persistence.take(),
            self.maintenance.take(),
            self.connector_outbound.take(),
            self.snapshot.take(),
            self.snapshot_triggers.take(),
        ]
        .into_iter()
        .flatten()
        {
            tasks.insert_handle(handle);
        }
        tasks
    }

    /// Route an inbound connector signal into the runtime's live event bus.
    ///
    /// Connector host/SDK ingress paths should call this handle after
    /// [`ObservationRuntime::start`] so the bridge's deduplication,
    /// classification, redaction, and `PatternDetected` fanout are shared with
    /// the observation runtime.
    pub fn route_connector_signal(
        &self,
        signal: &ConnectorSignal,
    ) -> std::result::Result<BridgeRouteResult, ConnectorBridgeError> {
        route_connector_signal_through_bridge(self.connector_inbound_bridge.as_ref(), signal)
    }

    /// Current capture channel queue depth (pending items waiting for persistence).
    #[must_use]
    pub fn capture_queue_depth(&self) -> usize {
        self.metrics.capture_queue_depth()
    }

    /// Maximum capture channel capacity.
    #[must_use]
    pub fn capture_queue_capacity(&self) -> usize {
        debug_assert!(self.capture_queue_capacity >= mpsc_max_capacity(&self.capture_tx));
        self.capture_queue_capacity
    }

    /// Current write queue depth (pending commands for the storage writer thread).
    // Intentionally `async`: part of the Cx-first async surface and the sibling
    // pair below; the signature is the public contract even though the current
    // body delegates to a sync read.
    #[allow(unknown_lints)]
    #[allow(clippy::unused_async_trait_impl)]
    pub async fn write_queue_depth(&self) -> usize {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.write_queue_depth_with_cx(&cx).await
    }

    /// ft-tr5a0 Cx-first sibling of [`Self::write_queue_depth`].
    #[allow(unknown_lints)]
    #[allow(clippy::unused_async_trait_impl)]
    pub async fn write_queue_depth_with_cx(&self, _cx: &crate::cx::Cx) -> usize {
        self.storage.write_queue_depth()
    }

    /// ft-tr5a0 Cx-first sibling of [`Self::join`]. Pre-flight checkpoint
    /// only — once the runtime has signaled shutdown, the join itself
    /// must run to completion to avoid leaking the spawned tasks.
    pub async fn join_with_cx(self, cx: &crate::cx::Cx) {
        let _ = cx.checkpoint();
        self.join_impl().await;
    }

    /// Wait for all tasks to complete.
    ///
    /// Each `.take()` empties the corresponding field so the
    /// defensive [`Drop`] impl on [`RuntimeHandle`] sees `None`
    /// and skips the redundant `abort()` once the join has
    /// completed cleanly.
    pub async fn join(self) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.join_with_cx(&cx).await;
    }

    async fn join_impl(mut self) {
        let mut tasks = self.take_task_join_set();
        while let Some(result) = tasks.drain_next_trusted().await {
            match result {
                Ok(()) => {}
                Err(error) => match error.kind() {
                    JoinErrorKind::WakerRegistrationFailed => {
                        warn!(
                            failure_class = ?error.kind(),
                            "Top-level runtime task join observation failed; retaining trusted drain authority"
                        );
                    }
                    _ => {
                        warn!(
                            failure_class = ?error.kind(),
                            "Top-level runtime task failed while joining"
                        );
                    }
                },
            }
        }
    }

    /// ft-tr5a0 Cx-first sibling of [`Self::shutdown_with_summary`].
    /// Pre-flight checkpoint only — shutdown itself runs to completion
    /// so storage flushes and task joins always happen, regardless of
    /// caller cancellation.
    pub async fn shutdown_with_summary_with_cx(self, cx: &crate::cx::Cx) -> ShutdownSummary {
        self.shutdown_with_timeout_with_cx(cx, DEFAULT_RUNTIME_SHUTDOWN_TIMEOUT)
            .await
    }

    /// Request graceful shutdown and collect a summary.
    ///
    /// This method:
    /// 1. Sets the shutdown flag to signal all tasks
    /// 2. Waits for tasks to complete (with timeout)
    /// 3. Flushes storage
    /// 4. Persists the terminal mux snapshot and clean mark, when configured
    /// 5. Collects and returns a shutdown summary
    pub async fn shutdown_with_summary(self) -> ShutdownSummary {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.shutdown_with_summary_with_cx(&cx).await
    }

    /// ft-e87u6.13 Cx-first sibling of [`Self::shutdown_with_timeout`].
    /// The caller checkpoint is deliberately observational rather than a gate:
    /// once cleanup is requested, an already-cancelled caller must not prevent
    /// bounded joins and storage flushes. Each cleanup phase therefore uses a
    /// fresh, independently bounded context.
    pub async fn shutdown_with_timeout_with_cx(
        self,
        cx: &crate::cx::Cx,
        shutdown_timeout: Duration,
    ) -> ShutdownSummary {
        let _ = cx.checkpoint();
        self.shutdown_with_timeout_impl(shutdown_timeout).await
    }

    /// Request graceful shutdown with an explicit task-join timeout.
    ///
    /// This is the bounded shutdown primitive used by the default shutdown
    /// paths. A stubborn background task can make the returned summary
    /// unclean, but it must not hold operator shutdown forever.
    ///
    /// `shutdown_timeout` independently bounds the graceful task-join phase,
    /// the forced-cancellation settlement phase (when needed), the final
    /// cursor snapshot, the subsequent storage flush, and terminal pane
    /// discovery. The terminal snapshot mutation uses the smaller of this
    /// timeout and five seconds. A stubborn writer task that misses both join
    /// windows — and is therefore still running concurrently with the flush —
    /// cannot wedge the flush either. Total shutdown latency may therefore
    /// span multiple timeout budgets.
    pub async fn shutdown_with_timeout(self, shutdown_timeout: Duration) -> ShutdownSummary {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.shutdown_with_timeout_with_cx(&cx, shutdown_timeout)
            .await
    }

    async fn shutdown_with_timeout_impl(
        mut self,
        shutdown_timeout: Duration,
    ) -> ShutdownSummary {
        let elapsed_secs = self.start_time.elapsed().as_secs();
        let mut warnings = Vec::new();
        let mut clean = true;

        // Signal shutdown
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Some(requested) = self.snapshot_shutdown_requested.as_ref() {
            requested.store(true, Ordering::Release);
        }
        if let Some(ref tx) = self.snapshot_shutdown {
            // Intentional best-effort wake-up. The joined scheduler status,
            // not channel receiver presence, is the shutdown receipt.
            let _ = tx.send(true);
        }
        info!("Shutdown signal sent");

        // Transfer every top-level handle into one trusted-drain owner. A raw
        // JoinHandle cannot recover terminal authority after persistent
        // caller-waker registration failure: polling it again merely repeats
        // the same observation error. JoinSet quarantines that handle, retains
        // it, and re-polls only through its stable internal completion waker.
        let mut task_handles = self.take_task_join_set();
        // Shutdown phases must never inherit an already-cancelled ambient Cx:
        // each phase gets a fresh context and its own bounded timeout.
        let mut graceful_join_failures = 0_usize;
        let mut graceful_join_unacknowledged = 0_usize;
        let join_cx = crate::cx::for_request();
        let join_result = runtime_timeout(&join_cx, shutdown_timeout, async {
            loop {
                match task_handles.drain_next_with_cx(&join_cx).await {
                    Ok(Some(Ok(()))) => {}
                    Ok(Some(Err(error))) => match error.kind() {
                        JoinErrorKind::WakerRegistrationFailed => {
                            graceful_join_failures = graceful_join_failures.saturating_add(1);
                            graceful_join_unacknowledged =
                                graceful_join_unacknowledged.saturating_add(1);
                        }
                        _ => {
                            graceful_join_failures = graceful_join_failures.saturating_add(1);
                        }
                    },
                    Ok(None) => return Ok(()),
                    Err(error) => return Err(error.kind()),
                }
            }
        })
        .await;

        let (graceful_timed_out, graceful_wait_failure, graceful_drain_failure) =
            match join_result {
                Ok(Ok(())) => (false, None, None),
                Ok(Err(failure)) => (false, None, Some(failure)),
                Err(RuntimeTimeoutFailure::Elapsed) => (true, None, None),
                Err(RuntimeTimeoutFailure::Context(failure)) => {
                    record_runtime_wait_failure("runtime_shutdown_graceful_join", failure);
                    (false, Some(failure), None)
                }
            };

        if graceful_join_failures > 0 {
            clean = false;
            warnings.push(format!(
                "{graceful_join_failures} top-level runtime task join failure observation(s) occurred during graceful shutdown"
            ));
        }
        if graceful_join_unacknowledged > 0 {
            warnings.push(format!(
                "{graceful_join_unacknowledged} caller-waker registration failure observation(s) entered trusted quarantine before terminal settlement"
            ));
        }
        if let Some(failure) = graceful_drain_failure {
            clean = false;
            warnings.push(format!(
                "Graceful top-level trusted drain failed with class {failure:?}"
            ));
        }

        let graceful_has_unsettled_handles =
            task_handles.settlement() != JoinSetSettlement::Settled;
        if graceful_timed_out
            || graceful_wait_failure.is_some()
            || graceful_drain_failure.is_some()
            || graceful_has_unsettled_handles
        {
            clean = false;
            task_handles.abort_all();

            let cancellation_cx = crate::cx::for_request();
            let mut cancellation_unacknowledged = 0_usize;
            let cancellation_result = runtime_timeout(&cancellation_cx, shutdown_timeout, async {
                loop {
                    match task_handles.drain_next_with_cx(&cancellation_cx).await {
                        Ok(Some(Ok(()))) => {}
                        Ok(Some(Err(error))) => {
                            if error.kind() == JoinErrorKind::WakerRegistrationFailed {
                                cancellation_unacknowledged =
                                    cancellation_unacknowledged.saturating_add(1);
                            }
                        }
                        Ok(None) => return Ok(()),
                        Err(error) => return Err(error.kind()),
                    }
                }
            })
            .await;
            let (cancellation_wait_failure, cancellation_drain_failure) =
                match cancellation_result {
                    Ok(Ok(())) => (None, None),
                    Ok(Err(failure)) => (None, Some(failure)),
                    Err(RuntimeTimeoutFailure::Elapsed) => {
                        (Some(RuntimeTimeoutFailure::Elapsed), None)
                    }
                    Err(RuntimeTimeoutFailure::Context(failure)) => {
                        record_runtime_wait_failure(
                            "runtime_shutdown_cancellation_join",
                            failure,
                        );
                        (Some(RuntimeTimeoutFailure::Context(failure)), None)
                    }
                };
            let cancellation_settled = cancellation_wait_failure.is_none()
                && cancellation_drain_failure.is_none()
                && task_handles.settlement() == JoinSetSettlement::Settled;

            if cancellation_settled && graceful_timed_out {
                warnings.push(
                    "Tasks exceeded graceful shutdown timeout; every retained top-level task acknowledged cancellation, but independently delegated nested or blocking work is not proven stopped"
                        .to_string(),
                );
            } else if cancellation_settled
                && let Some(failure) = graceful_wait_failure
            {
                warnings.push(format!(
                    "Graceful task-join wait failed with class {}; every retained top-level task subsequently acknowledged cancellation, but independently delegated nested or blocking work is not proven stopped",
                    failure.as_str()
                ));
            } else if cancellation_settled
                && let Some(failure) = graceful_drain_failure
            {
                warnings.push(format!(
                    "Graceful trusted drain failed with class {failure:?}; every retained top-level task subsequently acknowledged cancellation, but independently delegated nested or blocking work is not proven stopped"
                ));
            } else if cancellation_settled {
                let registration_note = if cancellation_unacknowledged > 0 {
                    format!(
                        " after {cancellation_unacknowledged} caller-waker registration failure(s)"
                    )
                } else {
                    String::new()
                };
                warnings.push(format!(
                    "Retained cancellation authority subsequently settled every top-level task{registration_note}, but independently delegated nested or blocking work is not proven stopped"
                ));
            } else if let Some(RuntimeTimeoutFailure::Context(failure)) =
                cancellation_wait_failure
            {
                warnings.push(format!(
                    "Cancellation settlement wait failed with class {}; terminal acknowledgement remained incomplete, so orphan risk remains",
                    failure.as_str()
                ));
            } else if matches!(
                cancellation_wait_failure,
                Some(RuntimeTimeoutFailure::Elapsed)
            ) {
                warnings.push(
                    "Cancellation settlement exceeded its timeout; terminal acknowledgement remained incomplete, so orphan risk remains"
                        .to_string(),
                );
            } else if let Some(failure) = cancellation_drain_failure {
                warnings.push(format!(
                    "Cancellation trusted drain failed with class {failure:?}; terminal acknowledgement remained incomplete, so orphan risk remains"
                ));
            } else if graceful_join_unacknowledged > 0 {
                warnings.push(format!(
                    "Cancellation was requested after {graceful_join_unacknowledged} top-level join acknowledgement failure(s), but bounded terminal settlement remained incomplete; orphan risk remains"
                ));
            } else {
                warnings.push(
                    "Tasks exceeded graceful shutdown timeout; cancellation was requested but terminal acknowledgement also timed out, so orphan risk remains"
                        .to_string(),
                );
            }
        }

        if let Some(status) = self.snapshot_scheduler_status.as_ref() {
            let status = status.load(Ordering::Acquire);
            if snapshot_scheduler_shutdown_acknowledged(status) {
                // The explicit shutdown watch was observed and the scheduler
                // returned without a typed failure.
            } else {
                match status {
                    SNAPSHOT_SCHEDULER_RUNNING => {
                        clean = false;
                        warnings.push(
                            "RuntimeBuilder snapshot scheduler did not publish a terminal acknowledgement"
                                .to_string(),
                        );
                    }
                    SNAPSHOT_SCHEDULER_UNEXPECTED_RETURN => {
                        clean = false;
                        warnings.push(
                            "RuntimeBuilder snapshot scheduler returned before shutdown was requested"
                                .to_string(),
                        );
                    }
                    SNAPSHOT_SCHEDULER_FAILED => {
                        clean = false;
                        warnings.push(
                            "RuntimeBuilder snapshot scheduler failed before clean shutdown acknowledgement"
                                .to_string(),
                        );
                    }
                    unknown => {
                        clean = false;
                        warnings.push(format!(
                            "RuntimeBuilder snapshot scheduler published unknown terminal status {unknown}"
                        ));
                    }
                }
            }
        } else if self.snapshot_engine.is_some() {
            clean = false;
            warnings.push(
                "RuntimeBuilder snapshot engine has no scheduler acknowledgement authority"
                    .to_string(),
            );
        }

        // Get final metrics
        let segments_persisted = self.metrics.segments_persisted.get();
        let events_recorded = self.metrics.events_recorded.get();

        // Get last seq per pane
        let cursor_cx = crate::cx::for_request();
        let last_seq_by_pane = match runtime_timeout(&cursor_cx, shutdown_timeout, async {
            self.cursors
                .read_with_cx(&cursor_cx)
                .await
                .map(|cursors| {
                    cursors
                        .iter()
                        .map(|(pane_id, cursor)| (*pane_id, cursor.last_seq()))
                        .collect::<Vec<_>>()
                })
        })
        .await
        {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(_)) => {
                warnings.push("Final cursor snapshot failed".to_string());
                clean = false;
                Vec::new()
            }
            Err(RuntimeTimeoutFailure::Elapsed) => {
                warnings.push("Final cursor snapshot did not complete within timeout".to_string());
                clean = false;
                Vec::new()
            }
            Err(RuntimeTimeoutFailure::Context(failure)) => {
                record_runtime_wait_failure("runtime_shutdown_cursor_snapshot", failure);
                warnings.push(format!(
                    "Final cursor snapshot wait failed with class {}",
                    failure.as_str()
                ));
                clean = false;
                Vec::new()
            }
        };

        // Flush storage under the same bounded timeout. A storage backend
        // that wedges (e.g. a SQLite checkpoint blocked on a held lock, a
        // filesystem hang, or a still-running persistence writer racing
        // the flush after a join timeout) must not stall operator
        // shutdown indefinitely.
        //
        // Cloning the StorageHandle (an `Arc`-backed clone, no real copy) is
        // required because a type implementing Drop cannot move out one of its
        // fields. The defensive Drop does not flush storage.
        let storage = self.storage.clone();
        let flush_cx = crate::cx::for_request();
        let flush_result = runtime_timeout(&flush_cx, shutdown_timeout, async {
            storage.shutdown_with_cx(&flush_cx).await
        })
        .await;
        match flush_result {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                warnings.push("Storage shutdown failed".to_string());
                clean = false;
            }
            Err(RuntimeTimeoutFailure::Elapsed) => {
                warnings.push("Storage flush did not complete within timeout".to_string());
                clean = false;
            }
            Err(RuntimeTimeoutFailure::Context(failure)) => {
                record_runtime_wait_failure("runtime_shutdown_storage_flush", failure);
                warnings.push(format!(
                    "Storage flush wait failed with class {}",
                    failure.as_str()
                ));
                clean = false;
            }
        }

        // These are point-in-time observations, not inferred values. In
        // particular, an unclean join or flush must not claim that either
        // queue is empty merely because shutdown has reached its summary
        // phase. `capture_queue_depth` is the aggregate ingress/ring/in-flight
        // observation maintained by the relay and persistence tasks;
        // `write_queue_depth` reads the storage writer's live bounded channel.
        let final_capture_queue = self.capture_queue_depth();
        let final_write_queue = self.storage.write_queue_depth();
        if final_capture_queue > 0 {
            warnings.push(format!(
                "Shutdown ended with {final_capture_queue} capture queue item(s) still observed"
            ));
            clean = false;
        }
        if final_write_queue > 0 {
            warnings.push(format!(
                "Shutdown ended with {final_write_queue} storage write queue item(s) still observed"
            ));
            clean = false;
        }

        // The consuming RuntimeHandle is the sole terminal-snapshot owner. A
        // clean path here proves that the scheduler and every top-level
        // producer/persistence task settled, storage flushed, and the live
        // queue observations above were zero. Never convert a degraded earlier
        // phase into `shutdown_clean = 1`.
        if let Some(snapshot_engine) = self.snapshot_engine.as_ref() {
            if clean {
                let snapshot_cx = crate::cx::for_request();
                let pane_list_result = runtime_timeout(
                    &snapshot_cx,
                    shutdown_timeout,
                    self.wezterm_handle.list_panes_with_cx(&snapshot_cx),
                )
                .await;
                match pane_list_result {
                    Ok(Ok(panes)) => {
                        let checkpoint_timeout = shutdown_timeout.min(Duration::from_secs(5));
                        match snapshot_engine
                            .shutdown_checkpoint_with_cx(
                                &snapshot_cx,
                                &panes,
                                checkpoint_timeout,
                            )
                            .await
                        {
                            Ok(checkpoint) => {
                                info!(
                                    checkpoint_id = checkpoint.checkpoint_id,
                                    pane_count = checkpoint.pane_count,
                                    total_bytes = checkpoint.total_bytes,
                                    "runtime snapshot terminal checkpoint and clean mark committed after storage settlement"
                                );
                                if let Some(receipt) = self.snapshot_shutdown_clean.as_ref() {
                                    receipt.store(true, Ordering::Release);
                                }
                            }
                            Err(error) => {
                                clean = false;
                                if let Some(checkpoint) =
                                    error.committed_shutdown_checkpoint()
                                {
                                    let session_id = checkpoint
                                        .session_id
                                        .chars()
                                        .take(64)
                                        .collect::<String>();
                                    warn!(
                                        error = %error,
                                        %session_id,
                                        checkpoint_id = checkpoint.checkpoint_id,
                                        pane_count = checkpoint.pane_count,
                                        total_bytes = checkpoint.total_bytes,
                                        "runtime terminal checkpoint committed but clean mark failed"
                                    );
                                    warnings.push(format!(
                                        "RuntimeBuilder terminal checkpoint {} committed, but its clean mark failed",
                                        checkpoint.checkpoint_id
                                    ));
                                } else {
                                    warn!(
                                        error = %error,
                                        "runtime terminal snapshot failed before a clean receipt"
                                    );
                                    warnings.push(
                                        "RuntimeBuilder terminal snapshot failed before publishing a clean receipt"
                                            .to_string(),
                                    );
                                }
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        clean = false;
                        warn!(
                            error = %error,
                            "runtime terminal snapshot pane listing failed after storage settlement"
                        );
                        warnings.push(
                            "RuntimeBuilder terminal snapshot pane listing failed".to_string(),
                        );
                    }
                    Err(RuntimeTimeoutFailure::Elapsed) => {
                        clean = false;
                        warnings.push(
                            "RuntimeBuilder terminal snapshot pane listing exceeded its timeout"
                                .to_string(),
                        );
                    }
                    Err(RuntimeTimeoutFailure::Context(failure)) => {
                        clean = false;
                        record_runtime_wait_failure(
                            "runtime_shutdown_snapshot_pane_list",
                            failure,
                        );
                        warnings.push(format!(
                            "RuntimeBuilder terminal snapshot pane-list wait failed with class {}",
                            failure.as_str()
                        ));
                    }
                }
            } else {
                warnings.push(
                    "RuntimeBuilder terminal snapshot clean mark was suppressed because an earlier shutdown phase was unclean"
                        .to_string(),
                );
            }
        }

        if self
            .snapshot_shutdown_clean
            .as_ref()
            .is_some_and(|receipt| !receipt.load(Ordering::Acquire))
        {
            clean = false;
            warnings.push(
                "RuntimeBuilder snapshot session did not publish both final-checkpoint and clean-mark receipts"
                    .to_string(),
            );
        }

        // Derive proof-related diagnostics only after both queue observations
        // have updated the final clean state. Zero observations remain
        // non-authoritative after any earlier unclean phase.
        let managed_queue_quiescence_proven =
            clean && final_capture_queue == 0 && final_write_queue == 0;
        if !managed_queue_quiescence_proven
            && final_capture_queue == 0
            && final_write_queue == 0
        {
            warnings.push(
                "Managed queue quiescence was not proven because an earlier shutdown phase was unclean"
                    .to_string(),
            );
        }

        ShutdownSummary::from_runtime_observation(
            elapsed_secs,
            final_capture_queue,
            final_write_queue,
            segments_persisted,
            events_recorded,
            last_seq_by_pane,
            clean,
            warnings,
        )
    }

    /// Request graceful shutdown.
    ///
    /// Sets the shutdown flag, waits for tasks with the default bounded
    /// timeout, and flushes storage. Use [`Self::shutdown_with_summary`] when
    /// the caller needs to inspect warnings or distinguish clean from timed-out
    /// shutdown.
    pub async fn shutdown(self) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.shutdown_with_cx(&cx).await;
    }

    /// ft-tr5a0 Cx-first sibling of [`Self::shutdown`]. Pre-flight
    /// checkpoint only — once `shutdown` is invoked, the join runs to
    /// completion regardless of caller cancellation (otherwise tasks
    /// leak).
    pub async fn shutdown_with_cx(self, cx: &crate::cx::Cx) {
        let summary = self.shutdown_with_summary_with_cx(cx).await;
        if !summary.is_clean() || !summary.warnings.is_empty() {
            warn!(
                clean = summary.is_clean(),
                warnings = ?summary.warnings,
                "runtime shutdown completed with warnings"
            );
        }
    }

    /// Signal shutdown without waiting.
    pub fn signal_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Some(requested) = self.snapshot_shutdown_requested.as_ref() {
            requested.store(true, Ordering::Release);
        }
        if let Some(ref tx) = self.snapshot_shutdown {
            // Intentional best-effort wake-up. The joined scheduler status,
            // not channel receiver presence, is the shutdown receipt.
            let _ = tx.send(true);
        }
    }

    /// ft-tr5a0 Cx-first sibling of [`Self::update_health_snapshot`].
    /// Pre-flight checkpoint; on cancel the snapshot is skipped this
    /// tick (next periodic call will retry).
    pub async fn update_health_snapshot_with_cx(&self, cx: &crate::cx::Cx) {
        if cx.checkpoint().is_err() {
            return;
        }
        self.update_health_snapshot_impl(cx).await;
    }

    /// Update the global health snapshot from current runtime state.
    ///
    /// Call this periodically (e.g., every 30s) to keep crash reports useful.
    pub async fn update_health_snapshot(&self) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.update_health_snapshot_with_cx(&cx).await;
    }

    async fn update_health_snapshot_impl(&self, cx: &crate::cx::Cx) {
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
        if cx.checkpoint().is_err() {
            return;
        }

        // Measure queue depths for backpressure visibility
        let capture_depth = self.capture_queue_depth();
        let capture_cap = self.capture_queue_capacity();

        let (write_depth, write_cap, db_writable) = {
            let wd = self.storage.write_queue_depth();
            let wc = self.storage.write_queue_capacity();
            let writable = match self.storage.is_writable_with_cx(cx).await {
                Ok(writable) => writable,
                Err(error) if is_runtime_cancellation(&error) => return,
                Err(_) => false,
            };

            (wd, wc, writable)
        };
        if cx.checkpoint().is_err() {
            return;
        }

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
        match self.wezterm_handle.watchdog_warnings_with_cx(cx).await {
            Ok(wezterm_warnings) => {
                append_bounded_watchdog_warnings(&mut warnings, wezterm_warnings);
            }
            Err(error) if is_runtime_cancellation(&error) => return,
            Err(_error) => {
                if cx.checkpoint().is_err() {
                    return;
                }
                warnings.push("Mux health warning probe failed: backend_unavailable".to_string());
            }
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

        let snapshot_timestamp = epoch_ms_u64();
        let snapshot = HealthSnapshot {
            timestamp: snapshot_timestamp,
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
            // ft-u6zfw: real crash-loop diagnostics (was hardcoded zeros).
            restart_count: self.metrics.crash_loop_diagnostics().restart_count,
            last_crash_at: self.metrics.crash_loop_diagnostics().last_crash_at,
            consecutive_crashes: self.metrics.crash_loop_diagnostics().consecutive_crashes,
            current_backoff_ms: self.metrics.crash_loop_diagnostics().current_backoff_ms,
            in_crash_loop: self.metrics.crash_loop_diagnostics().in_crash_loop,
            fleet_pressure_tier: None,
            swarm_capacity: Some(
                crate::runtime_telemetry::live_swarm_capacity_operator_summary(
                    snapshot_timestamp,
                    observed_panes,
                    3,
                ),
            ),
            leak_risk_inventory,
        };

        // Cancellation at any point in the sample invalidates the whole
        // observation; never publish a partially degraded snapshot.
        if cx.checkpoint().is_err() {
            return;
        }
        HealthSnapshot::update_global(snapshot);
        RuntimeLockMemoryTelemetrySnapshot::update_global(self.metrics.lock_memory_snapshot());
    }

    /// Clone the storage handle for external shutdown coordination.
    ///
    /// `RuntimeHandle` has a defensive `Drop` impl, so this method cannot move
    /// the field out directly. Consuming `self` still drops the runtime handle
    /// and requests abort on any wrapper handles left in place. That abort does
    /// not stop an already-detached underlying task; the returned
    /// `StorageHandle` is the caller's handle for follow-up storage shutdown.
    #[must_use]
    pub fn take_storage(self) -> StorageHandle {
        self.storage.clone()
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
            .map_err(|e| runtime_backend_error("runtime.apply_config_update", e))
    }

    /// Get the current hot-reloadable config.
    #[must_use]
    pub fn current_config(&self) -> HotReloadableConfig {
        self.config_tx.borrow().clone()
    }
}

impl Drop for RuntimeHandle {
    /// Defensive drop for callers that forget to invoke
    /// [`Self::shutdown_with_summary`] / [`Self::shutdown_with_timeout`].
    ///
    /// This Drop fires only on abnormal exit paths — early return,
    /// panic, or end-of-scope without an explicit shutdown call. We
    /// cannot `.await` here, so we cannot drain channels or flush
    /// storage; operators who need a clean summary MUST still call
    /// the explicit shutdown methods. What we *can* do, defensively:
    ///
    ///   - Flip `shutdown_flag` so any background task that is
    ///     still polling it observes cancellation on its next tick.
    ///   - Send the snapshot-shutdown wake-up so the snapshot task
    ///     breaks out of any pending `watch` / `select` wait.
    ///   - Call `JoinHandle::abort` on every wrapper handle, signalling the
    ///     abortable task future. Drop cannot await terminal acknowledgement,
    ///     so the shared shutdown signal remains the complementary cooperative
    ///     path while the runtime schedules those cancellations.
    ///
    /// A `tracing::warn` records the unclean exit so the leak shows
    /// up in operator logs instead of failing silently.
    fn drop(&mut self) {
        let already_signalled = self.shutdown_flag.swap(true, Ordering::SeqCst);
        if !already_signalled {
            tracing::warn!(
                target: "ft.runtime",
                event = "runtime_handle_dropped_without_shutdown",
                "RuntimeHandle dropped without explicit shutdown — \
                 background cancellation is not drained; storage may not be flushed; \
                 call shutdown_with_summary or shutdown_with_timeout for a \
                 clean exit"
            );
        }
        if let Some(ref tx) = self.snapshot_shutdown {
            // Same best-effort wake-up as the explicit path. Deliberately do
            // not set `snapshot_shutdown_requested`: Drop is an abnormal exit,
            // never a clean scheduler acknowledgement.
            let _ = tx.send(true);
        }
        for handle in [
            self.discovery.as_ref(),
            self.capture.as_ref(),
            self.relay.as_ref(),
            self.persistence.as_ref(),
            self.native_events.as_ref(),
            self.maintenance.as_ref(),
            self.connector_outbound.as_ref(),
            self.snapshot.as_ref(),
            self.snapshot_triggers.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            handle.abort();
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

// =============================================================================
// br-ft-ykkig: runtime telemetry sample buffer Mutex poison-recovery counter
// =============================================================================
//
// Pre-fix the 2 production lock-sites on the percentile-sample
// VecDeques (`StdMutex<VecDeque<u64>>`) used
// `unwrap_or_else(std::sync::PoisonError::into_inner)` — fail-soft
// recovery from poison was correct, but invisible. Operators had no
// signal when the runtime telemetry sample buffers degraded.
//
// Same defect class as ft-ky7nf / ft-gbv7s — silent recovery without
// observability. Hot-path: every latency sample recording goes
// through `record_bounded_sample`; every p50/p95/p99 query goes
// through `percentile_from_samples`.
static RUNTIME_SAMPLES_LOCK_POISONED_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Read the current count of recovered runtime sample-buffer
/// Mutex-poison events. Non-zero values mean a prior thread
/// panicked while holding a percentile-window VecDeque lock; the
/// telemetry sampler continued (fail-soft) after recovering.
#[must_use]
pub fn runtime_samples_lock_poisoned_count() -> u64 {
    RUNTIME_SAMPLES_LOCK_POISONED_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Test-only reset of the counter so tests don't observe
/// cross-test pollution.
#[cfg(test)]
pub fn reset_runtime_samples_lock_poisoned_count_for_test() {
    RUNTIME_SAMPLES_LOCK_POISONED_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Recover a poisoned `StdMutex<VecDeque<u64>>` guard via
/// [`std::sync::PoisonError::into_inner`] and bump the
/// `RUNTIME_SAMPLES_LOCK_POISONED_COUNT` observability counter on
/// recovery. [ft-ykkig]
fn record_samples_poison_and_recover<T>(poison: std::sync::PoisonError<T>) -> T {
    RUNTIME_SAMPLES_LOCK_POISONED_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    poison.into_inner()
}

// =============================================================================
// ft-lo0ip: global lock/memory telemetry RwLock poison-recovery counter
// =============================================================================
//
// `RuntimeLockMemoryTelemetrySnapshot::update_global`/`get_global` recover a
// poisoned `GLOBAL_RUNTIME_LOCK_MEMORY_TELEMETRY` RwLock via into_inner (so the
// update lands / the snapshot is returned instead of being dropped), but the
// recovery was invisible — operators got no signal that the global telemetry
// lock had been poisoned. Same defect class as ft-ykkig (silent fail-soft
// recovery): surface it via an observability counter.
static RUNTIME_LOCK_MEMORY_TELEMETRY_POISONED_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Read the current count of recovered global lock/memory telemetry RwLock
/// poison events. Non-zero means a prior thread panicked while holding the
/// `GLOBAL_RUNTIME_LOCK_MEMORY_TELEMETRY` lock; `update_global`/`get_global`
/// continued (fail-soft) after recovering the guard. [ft-lo0ip]
#[must_use]
pub fn runtime_lock_memory_telemetry_poisoned_count() -> u64 {
    RUNTIME_LOCK_MEMORY_TELEMETRY_POISONED_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Test-only reset so tests don't observe cross-test pollution of the
/// process-global counter.
#[cfg(test)]
pub fn reset_runtime_lock_memory_telemetry_poisoned_count_for_test() {
    RUNTIME_LOCK_MEMORY_TELEMETRY_POISONED_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Recover a poisoned global lock/memory telemetry RwLock guard via
/// [`std::sync::PoisonError::into_inner`] and bump the observability counter
/// on recovery. Also emits a `tracing::warn` so the poison shows up in logs,
/// not just the counter. [ft-lo0ip]
fn record_lock_memory_telemetry_poison_and_recover<T>(poison: std::sync::PoisonError<T>) -> T {
    RUNTIME_LOCK_MEMORY_TELEMETRY_POISONED_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tracing::warn!(
        target: "ft.runtime.telemetry",
        event = "runtime_lock_memory_telemetry_lock_poisoned",
        "global lock/memory telemetry RwLock was poisoned; recovered guard and continued (ft-lo0ip)"
    );
    poison.into_inner()
}

fn record_bounded_sample(samples: &StdMutex<VecDeque<u64>>, value: u64) {
    // br-ft-ykkig: hot path — every latency sample recording.
    let mut guard = samples
        .lock()
        .unwrap_or_else(record_samples_poison_and_recover);
    if guard.len() == TELEMETRY_PERCENTILE_WINDOW_CAPACITY {
        let _ = guard.pop_front();
    }
    guard.push_back(value);
}

fn percentile_from_samples(samples: &StdMutex<VecDeque<u64>>, percentile: usize) -> u64 {
    debug_assert!((1..=100).contains(&percentile));
    let mut values: Vec<u64> = {
        // br-ft-ykkig.
        let guard = samples
            .lock()
            .unwrap_or_else(record_samples_poison_and_recover);
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
///
/// br-ft-0n4nx: route through the shared clock-anomaly helper so a
/// pre-epoch host clock surfaces in operator telemetry instead of
/// silently flattening every runtime telemetry timestamp to 0.
fn epoch_ms() -> i64 {
    crate::clock_anomaly::epoch_ms_i64("ft.runtime.clock")
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
        #[cfg(feature = "subprocess-bridge")]
        Event::MissionAudit { .. } => None,
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
            | BOCPD_CHANGE_POINT_EVENT_TYPE
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

fn observe_bocpd_segment_for_runtime(
    manager: &mut crate::bocpd::BocpdManager,
    last_capture_at: &mut HashMap<u64, i64>,
    segment: &CapturedSegment,
    reset_pane_state: bool,
) -> Option<Detection> {
    if reset_pane_state {
        manager.unregister_pane(segment.pane_id);
        last_capture_at.remove(&segment.pane_id);
    }

    if segment.content.is_empty() {
        return None;
    }

    let elapsed =
        elapsed_since_last_bocpd_segment(last_capture_at, segment.pane_id, segment.captured_at);

    manager
        .observe_text_chunk(segment.pane_id, segment.content.as_str(), elapsed)
        .map(|change_point| bocpd_change_point_to_detection(&change_point))
}

fn elapsed_since_last_bocpd_segment(
    last_capture_at: &mut HashMap<u64, i64>,
    pane_id: u64,
    captured_at: i64,
) -> Duration {
    let previous = last_capture_at.insert(pane_id, captured_at);
    let Some(previous_capture_at) = previous else {
        return Duration::from_secs(1);
    };

    let elapsed_ms = captured_at.saturating_sub(previous_capture_at).max(1);
    let elapsed_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
    Duration::from_millis(elapsed_ms)
}

fn bocpd_change_point_to_detection(change_point: &crate::bocpd::PaneChangePoint) -> Detection {
    Detection {
        rule_id: BOCPD_CHANGE_POINT_RULE_ID.to_string(),
        agent_type: AgentType::Unknown,
        event_type: BOCPD_CHANGE_POINT_EVENT_TYPE.to_string(),
        severity: Severity::Info,
        confidence: change_point.posterior_probability.clamp(0.0, 1.0),
        extracted: serde_json::json!({
            "pane_id": change_point.pane_id,
            "observation_index": change_point.observation_index,
            "posterior_probability": change_point.posterior_probability,
            "timestamp_secs": change_point.timestamp_secs,
            "features_at_change": &change_point.features_at_change,
        }),
        matched_text: format!(
            "BOCPD change point pane={} observation={} posterior={:.3}",
            change_point.pane_id,
            change_point.observation_index,
            change_point.posterior_probability
        ),
        span: (0, 0),
    }
}

/// Redact all string leaves in a JSON Value (for extracted capture groups that may contain secrets).
fn redact_json_leaves(value: &mut serde_json::Value, redactor: &Redactor) {
    match value {
        serde_json::Value::String(s) => {
            *s = redactor.redact(s);
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_json_leaves(v, redactor);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                redact_json_leaves(v, redactor);
            }
        }
        _ => {}
    }
}

/// Return a copy of `detection` with `matched_text` and the string leaves of
/// `extracted` redacted (rule_id / severity / span etc. are left intact).
///
/// `matched_text` is the full regex match span (`patterns.rs` `m.as_str()`), so a
/// rule whose pattern reaches past its anchor can capture adjacent secret bytes.
/// Apply this anywhere a `Detection` is persisted OR emitted on the event bus so
/// that no consumer — storage rows, `ft robot events`, web `/events`, live
/// `EventBus` subscribers (SSE), distributed aggregators — ever sees a raw match.
/// Redaction is idempotent. (gauntlet FND-010 / INV-RED-1.)
pub fn redact_detection(detection: &Detection) -> Detection {
    let redactor = Redactor::new();
    let mut redacted = detection.clone();
    redacted.matched_text = redactor.redact(&detection.matched_text);
    redact_json_leaves(&mut redacted.extracted, &redactor);
    redacted
}

/// Convert a Detection to a StoredEvent for persistence.
/// Redacts matched_text and string values inside extracted at write time so that
/// all downstream consumers (storage rows, wa.events, ft robot events, web /events,
/// replay, etc.) see only redacted content. This makes the "rows in storage are
/// already clean" invariant true and closes the previous gap where only the
/// dedupe_key was redacted.
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

    let redacted = redact_detection(detection);

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
        extracted: Some(redacted.extracted),
        matched_text: Some(redacted.matched_text),
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
    use crate::runtime_async::{CompatRuntime, sleep};
    use crate::storage::PaneRecord;
    use tempfile::TempDir;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for runtime tests");
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        crate::runtime_async::clear_runtime_handle();
        if let Err(payload) = test_result {
            std::panic::resume_unwind(payload);
        }
    }

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn native_accept_settlement_truth_gives_terminal_authority_precedence() {
        assert_eq!(
            classify_native_accept_task_settlement(
                true,
                None,
                None,
                JoinSetSettlement::Settled,
            ),
            NativeAcceptTaskSettlement::Settled,
        );
        assert_eq!(
            classify_native_accept_task_settlement(
                true,
                None,
                None,
                JoinSetSettlement::Incomplete {
                    active_tasks: 1,
                    unacknowledged_tasks: 1,
                },
            ),
            NativeAcceptTaskSettlement::TimedOut {
                active_tasks: 1,
                unacknowledged_tasks: 1,
            },
        );
        assert_eq!(
            classify_native_accept_task_settlement(
                false,
                Some(JoinErrorKind::ContextFailure),
                None,
                JoinSetSettlement::Incomplete {
                    active_tasks: 0,
                    unacknowledged_tasks: 1,
                },
            ),
            NativeAcceptTaskSettlement::Incomplete {
                active_tasks: 0,
                unacknowledged_tasks: 1,
                drain_failure: Some(JoinErrorKind::ContextFailure),
            },
        );
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn streaming_task_drain_truth_gives_terminal_authority_precedence() {
        assert_eq!(
            classify_streaming_task_drain(
                true,
                None,
                None,
                JoinSetSettlement::Settled,
            ),
            StreamingTaskDrainOutcome::Settled,
        );
        assert_eq!(
            classify_streaming_task_drain(
                true,
                None,
                None,
                JoinSetSettlement::Incomplete {
                    active_tasks: 2,
                    unacknowledged_tasks: 1,
                },
            ),
            StreamingTaskDrainOutcome::TimedOut {
                active_tasks: 2,
                unacknowledged_tasks: 1,
            },
        );
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn streaming_task_owner_trusted_drains_registration_failure() {
        run_async_test(async {
            let mut tasks = StreamingTasks::new();
            tasks.settling.spawn(std::future::pending::<()>());
            tasks.settling.force_join_registration_failure_for_test();

            assert_eq!(
                tasks.abort_and_settle_all().await,
                StreamingTaskDrainOutcome::SettledWithFailure {
                    failure: JoinErrorKind::WakerRegistrationFailed,
                },
                "waker-registration failure must remain observable after trusted settlement",
            );
            assert_eq!(tasks.settling.settlement(), JoinSetSettlement::Settled);
        });
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn streaming_task_steady_reaper_retains_quarantined_terminal_authority() {
        run_async_test(async {
            let mut tasks = StreamingTasks::new();
            tasks.settling.spawn(std::future::pending::<()>());
            tasks.settling.force_join_registration_failure_for_test();
            tasks.settling.abort_all();

            for _ in 0..4_096 {
                tasks.reap_completed();
                if tasks.settling.settlement() == JoinSetSettlement::Settled {
                    break;
                }
                crate::runtime_async::yield_now().await;
            }

            assert_eq!(
                tasks.settling.settlement(),
                JoinSetSettlement::Settled,
                "steady-state reaping must trusted-poll quarantined handles after abort acknowledgement",
            );
        });
    }

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn native_accept_guard_trusted_drain_settles_registration_failure() {
        run_async_test(async {
            let handle = crate::runtime_async::task::spawn(std::future::pending::<()>());
            let mut guard = AbortOnDropNativeAcceptTask::new(handle);
            guard.force_registration_failure_for_test();

            assert_eq!(
                guard.abort_and_settle().await,
                NativeAcceptTaskSettlement::SettledWithFailure {
                    failure: JoinErrorKind::WakerRegistrationFailed,
                },
                "waker-registration failure must remain observable after trusted settlement",
            );
        });
    }

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn native_accept_guard_graceful_settlement_does_not_abort_listener() {
        run_async_test(async {
            let completed = Arc::new(AtomicBool::new(false));
            let completed_task = Arc::clone(&completed);
            let handle = crate::runtime_async::task::spawn(async move {
                crate::runtime_async::yield_now().await;
                completed_task.store(true, Ordering::Release);
            });
            let mut guard = AbortOnDropNativeAcceptTask::new(handle);

            assert_eq!(
                guard.settle().await,
                NativeAcceptTaskSettlement::Settled
            );
            assert!(
                completed.load(Ordering::Acquire),
                "graceful settlement must let the listener reach its own terminal path"
            );
        });
    }

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn graceful_native_shutdown_aborts_then_drains_every_admitted_event() {
        run_async_test(async {
            let (event_tx, mut event_rx) = mpsc::channel(4);
            event_tx
                .try_send(NativeEvent::PaneDestroyed {
                    pane_id: 41,
                    timestamp_ms: 1,
                })
                .expect("first admitted native event");
            event_tx
                .try_send(NativeEvent::PaneDestroyed {
                    pane_id: 42,
                    timestamp_ms: 2,
                })
                .expect("second admitted native event");

            let held_sender = event_tx.clone();
            drop(event_tx);
            let producer = crate::runtime_async::task::spawn(async move {
                let _held_sender = held_sender;
                std::future::pending::<()>().await;
            });
            let mut guard = AbortOnDropNativeAcceptTask::new(producer);

            let start = RuntimeTime::ZERO;
            let mut drain_state = NativeEventShutdownDrainState::Running;
            assert_eq!(
                drain_state.advance(start, true),
                NativeEventShutdownDrainAction::BeginGraceful,
            );
            assert_eq!(
                drain_state.advance(
                    start + NATIVE_ACCEPT_TASK_GRACEFUL_TIMEOUT,
                    true,
                ),
                NativeEventShutdownDrainAction::AbortProducer,
            );
            assert_eq!(
                guard.abort_and_settle().await,
                NativeAcceptTaskSettlement::Settled
            );

            let cx = crate::cx::for_testing();
            assert!(matches!(
                recv_event(&cx, &mut event_rx).await,
                RecvEvent::Item(NativeEvent::PaneDestroyed { pane_id: 41, .. })
            ));
            assert!(matches!(
                recv_event(&cx, &mut event_rx).await,
                RecvEvent::Item(NativeEvent::PaneDestroyed { pane_id: 42, .. })
            ));
            assert!(matches!(
                recv_event(&cx, &mut event_rx).await,
                RecvEvent::Closed
            ));
            assert_eq!(
                drain_state.mark_producer_closed(),
                NativeEventShutdownDrainAction::ProducerClosed,
            );
            assert!(drain_state.producer_closed());
            assert_eq!(
                drain_state.advance(
                    start
                        + NATIVE_ACCEPT_TASK_GRACEFUL_TIMEOUT
                        + NATIVE_EVENT_QUEUE_DRAIN_TIMEOUT,
                    true,
                ),
                NativeEventShutdownDrainAction::None,
                "clean producer closure after draining must not become abandonment",
            );
        });
    }

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn native_shutdown_drain_abandonment_is_bounded_and_explicit() {
        let start = RuntimeTime::ZERO;
        let graceful_deadline = start + NATIVE_ACCEPT_TASK_GRACEFUL_TIMEOUT;
        let forced_deadline = graceful_deadline + NATIVE_EVENT_QUEUE_DRAIN_TIMEOUT;
        let mut drain_state = NativeEventShutdownDrainState::Running;

        assert_eq!(
            drain_state.advance(start, false),
            NativeEventShutdownDrainAction::None,
        );
        assert_eq!(
            drain_state.advance(start, true),
            NativeEventShutdownDrainAction::BeginGraceful,
        );
        assert_eq!(
            drain_state.advance(graceful_deadline, true),
            NativeEventShutdownDrainAction::AbortProducer,
        );
        assert_eq!(
            drain_state.advance(
                graceful_deadline + NATIVE_EVENT_QUEUE_DRAIN_TIMEOUT / 2,
                true,
            ),
            NativeEventShutdownDrainAction::None,
            "admitted events remain drainable throughout the forced window",
        );
        assert_eq!(
            drain_state.advance(forced_deadline, true),
            NativeEventShutdownDrainAction::Abandon,
        );
        assert_eq!(
            drain_state,
            NativeEventShutdownDrainState::Abandoned,
        );
        assert_eq!(
            drain_state.mark_producer_closed(),
            NativeEventShutdownDrainAction::None,
            "a late closure must not erase the already-recorded loss classification",
        );
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
                let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
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
                crate::runtime_async::clear_runtime_handle();

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

    fn discovery_publication(epoch: u64) -> DiscoveryCapturePublication {
        DiscoveryCapturePublication {
            epoch,
            observed_panes: Arc::new(HashMap::new()),
            transitioning_pane_ids: Arc::new(HashSet::new()),
            transitions: Arc::new(HashMap::new()),
        }
    }

    #[test]
    fn discovery_revision_and_publication_namespaces_never_wrap() {
        let mut revision = 0;
        assert_eq!(allocate_discovery_revision(&mut revision).map(DiscoveryRevision::get), Some(1));
        revision = u64::MAX;
        assert_eq!(allocate_discovery_revision(&mut revision), None);
        assert_eq!(revision, u64::MAX);

        let mut epoch = 0;
        assert_eq!(allocate_discovery_publication_epoch(&mut epoch), Some(1));
        epoch = u64::MAX;
        assert_eq!(allocate_discovery_publication_epoch(&mut epoch), None);
        assert_eq!(epoch, u64::MAX);
    }

    #[test]
    fn capture_transition_retries_back_off_and_prompting_has_a_hard_deadline() {
        let started_at = Instant::now();
        assert_eq!(
            capture_transition_retry_delay(started_at, started_at),
            CAPTURE_FAST_RETRY_DELAY,
        );
        let at_warm_window = started_at
            .checked_add(CAPTURE_FAST_RETRY_WINDOW)
            .expect("warm retry boundary");
        assert_eq!(
            capture_transition_retry_delay(started_at, at_warm_window),
            CAPTURE_WARM_RETRY_DELAY,
        );
        let at_cool_window = started_at
            .checked_add(CAPTURE_WARM_RETRY_WINDOW)
            .expect("cool retry boundary");
        assert_eq!(
            capture_transition_retry_delay(started_at, at_cool_window),
            CAPTURE_COOL_RETRY_DELAY,
        );
        let at_idle_window = started_at
            .checked_add(CAPTURE_COOL_RETRY_WINDOW)
            .expect("idle retry boundary");
        assert_eq!(
            capture_transition_retry_delay(started_at, at_idle_window),
            CAPTURE_IDLE_RETRY_DELAY,
        );

        let drain_window_minus_one = CAPTURE_PROMPT_DRAIN_WINDOW
            .checked_sub(Duration::from_nanos(1))
            .expect("prompt drain window exceeds one nanosecond");
        let before_drain_window = started_at
            .checked_add(drain_window_minus_one)
            .expect("drain pre-deadline");
        let at_drain_window = started_at
            .checked_add(CAPTURE_PROMPT_DRAIN_WINDOW)
            .expect("drain deadline");
        assert_eq!(
            bounded_capture_transition_retry_delay(started_at, before_drain_window),
            Some(Duration::from_nanos(1)),
        );
        assert_eq!(
            bounded_capture_transition_retry_delay(started_at, at_drain_window),
            None,
        );
    }

    #[test]
    fn pending_resync_requires_a_terminal_receipt_even_when_superseded() {
        use PendingCaptureResyncDisposition::{
            Publish, RetireFailed, RetireSuperseded, Wait,
        };

        assert_eq!(pending_capture_resync_disposition(true, None), Wait);
        assert_eq!(pending_capture_resync_disposition(false, None), Wait);
        assert_eq!(
            pending_capture_resync_disposition(true, Some(Ok(7))),
            Publish(7),
        );
        assert_eq!(
            pending_capture_resync_disposition(true, Some(Err("failed".to_string()))),
            RetireFailed("failed".to_string()),
        );
        assert_eq!(
            pending_capture_resync_disposition(false, Some(Ok(7))),
            RetireSuperseded {
                committed: true,
                failure_reason: None,
            },
        );
        assert_eq!(
            pending_capture_resync_disposition(false, Some(Err("stale".to_string()))),
            RetireSuperseded {
                committed: false,
                failure_reason: Some("stale".to_string()),
            },
        );

        // Wall-clock age is deliberately absent from disposition: neither the
        // former five-second boundary nor a much longer backlog authorizes a
        // duplicate submission while the decision remains outstanding.
        for elapsed in [Duration::from_secs(5), Duration::from_secs(60)] {
            let started_at = Instant::now();
            let _later = started_at
                .checked_add(elapsed)
                .expect("representable backlog duration");
            assert_eq!(pending_capture_resync_disposition(false, None), Wait);
        }
    }

    #[test]
    fn conservative_pre_storage_view_revokes_only_unproven_identities() {
        let retained_pane = ObservedCapturePane {
            info: make_pane(1, "retained"),
            generation: 2,
            pane_uuid: "retained-uuid".to_string(),
            revision: DiscoveryRevision(10),
            requires_storage_resync: false,
        };
        let closed_pane = ObservedCapturePane {
            info: make_pane(2, "closed"),
            generation: 3,
            pane_uuid: "closed-uuid".to_string(),
            revision: DiscoveryRevision(11),
            requires_storage_resync: false,
        };
        let replaced_pane = ObservedCapturePane {
            info: make_pane(3, "predecessor"),
            generation: 4,
            pane_uuid: "predecessor-uuid".to_string(),
            revision: DiscoveryRevision(12),
            requires_storage_resync: false,
        };
        let previous = HashMap::from([
            (1, retained_pane.clone()),
            (2, closed_pane),
            (3, replaced_pane),
        ]);
        let panes = vec![
            make_pane(1, "retained"),
            make_pane(3, "successor"),
            make_pane(4, "new"),
        ];

        let (view, transitioning) =
            conservative_capture_view_before_storage(&previous, &panes);

        assert_eq!(view.len(), 1);
        let retained = view.get(&1).expect("unchanged pane remains admissible");
        assert_eq!(retained.revision, retained_pane.revision);
        assert!(!view.contains_key(&2), "closed predecessor is revoked");
        assert!(!view.contains_key(&3), "changed predecessor is revoked");
        assert_eq!(transitioning, Arc::new(HashSet::from([3, 4])));
    }

    #[test]
    fn failed_uuid_lookup_cannot_re_admit_withheld_revision_after_same_fingerprint_return() {
        let pane_id = 42;
        let predecessor_revision = DiscoveryRevision(7);
        let predecessor_info = make_pane(pane_id, "stable");
        let unrelated_new_pane = make_pane(99, "new-pane-needing-uuid-lookup");
        let mut registry = PaneRegistry::new();
        let initial = registry.discovery_tick(vec![predecessor_info.clone()]);
        assert_eq!(initial.new_panes, vec![pane_id]);
        let previous = HashMap::from([(
            pane_id,
            ObservedCapturePane {
                info: predecessor_info.clone(),
                generation: 0,
                pane_uuid: registry
                    .get_entry(pane_id)
                    .expect("initial pane")
                    .pane_uuid
                    .clone(),
                revision: predecessor_revision,
                requires_storage_resync: false,
            },
        )]);

        // The mux omits pane 42 while also adding pane 99. Discovery publishes
        // the conservative barrier, then its batched UUID lookup fails. Model
        // that failed await by deliberately not applying this listing to the
        // registry.
        let (withheld_view, _) = conservative_capture_view_before_storage(
            &previous,
            std::slice::from_ref(&unrelated_new_pane),
        );
        let mut unresolved = HashMap::new();
        remember_withheld_barrier_predecessors(&mut unresolved, &previous, &withheld_view);
        assert_eq!(unresolved.get(&pane_id), Some(&predecessor_revision));

        // On the next successful tick the same physical-looking pane returns.
        // The registry never consumed the absence, so its ordinary diff reports
        // no transition for 42. The barrier ledger must force one anyway.
        let recovered_diff =
            registry.discovery_tick(vec![predecessor_info, unrelated_new_pane.clone()]);
        assert!(
            !recovered_diff.new_panes.contains(&pane_id)
                && !recovered_diff.new_generations.contains(&pane_id)
                && !recovered_diff.re_observed_panes.contains(&pane_id),
            "the registry deliberately has no native transition evidence for the ABA return"
        );
        let (confirmed_terminal, forced_transitions) =
            classify_unresolved_barrier_predecessors(&unresolved, &registry);
        assert!(confirmed_terminal.is_empty());
        assert_eq!(forced_transitions, vec![pane_id]);

        let mut last_revision = predecessor_revision.get();
        let mut revisions = HashMap::from([(pane_id, predecessor_revision)]);
        let mut transitions = HashMap::new();
        let mut storage_resyncs = HashMap::new();
        allocate_capture_transition_revisions(
            &forced_transitions,
            &mut last_revision,
            &mut revisions,
            &mut transitions,
            &mut unresolved,
            &mut storage_resyncs,
        );
        let successor_revision = DiscoveryRevision(predecessor_revision.get() + 1);
        assert_eq!(revisions.get(&pane_id), Some(&successor_revision));
        assert_eq!(storage_resyncs.get(&pane_id), Some(&successor_revision));
        assert_eq!(
            transitions.get(&pane_id),
            Some(&CaptureTransitionDescriptor {
                desired_revision: successor_revision,
                predecessor_revision: Some(predecessor_revision),
            })
        );
        assert!(!unresolved.contains_key(&pane_id));

        let _closed_diff = registry.discovery_tick(vec![unrelated_new_pane]);
        let terminal_unresolved = HashMap::from([(pane_id, successor_revision)]);
        let (confirmed_terminal, forced_transitions) =
            classify_unresolved_barrier_predecessors(&terminal_unresolved, &registry);
        assert_eq!(confirmed_terminal, vec![pane_id]);
        assert!(forced_transitions.is_empty());
    }

    #[test]
    fn filter_transition_is_terminal_for_capture_bookkeeping() {
        let pane_id = 42;
        let revision = DiscoveryRevision(7);
        let filter = PaneFilterConfig {
            include: Vec::new(),
            exclude: vec![crate::config::PaneFilterRule {
                id: "ignore-title".to_string(),
                domain: None,
                title: Some("ignore-".to_string()),
                cwd: None,
            }],
        };
        let mut registry = PaneRegistry::with_filter(filter);
        registry.discovery_tick(vec![make_pane(pane_id, "observed")]);
        assert!(registry_observes_pane(&registry, pane_id));

        let transition = CaptureTransitionDescriptor {
            desired_revision: revision,
            predecessor_revision: None,
        };
        let mut revisions = HashMap::from([(pane_id, revision)]);
        let mut storage_resyncs = HashMap::from([(pane_id, revision)]);
        let mut transitions = HashMap::from([(pane_id, transition)]);
        let mut setup = HashMap::from([(pane_id, "observation_started")]);

        let diff = registry.discovery_tick(vec![make_pane(pane_id, "ignore-now")]);
        assert!(diff.new_generations.contains(&pane_id));
        assert!(!registry_observes_pane(&registry, pane_id));
        retain_observed_capture_bookkeeping(
            &registry,
            &mut revisions,
            &mut storage_resyncs,
            &mut transitions,
            &mut setup,
        );
        assert!(revisions.is_empty());
        assert!(storage_resyncs.is_empty());
        assert!(transitions.is_empty());
        assert!(setup.is_empty());

        let mut transitioning = diff.new_generations;
        transitioning.retain(|id| registry_observes_pane(&registry, *id));
        assert!(
            transitioning.is_empty(),
            "an ignored pane cannot remain in a permanent capture transition"
        );
    }

    #[test]
    fn discovery_publication_channel_loss_fails_closed() {
        let authority = CaptureAuthority::new();
        let (tx, rx) = watch::channel(DiscoveryCapturePublication::default());
        drop(rx);
        let mut last_epoch = 0;
        let mut last_view = Arc::new(HashMap::new());

        let result = publish_discovery_capture_view(
            &tx,
            &authority,
            &mut last_epoch,
            &mut last_view,
            Arc::new(HashMap::new()),
            Arc::new(HashSet::new()),
            Arc::new(HashMap::new()),
            "test-closed-channel",
        );

        assert!(result.is_err());
        assert_eq!(last_epoch, 1, "failed epoch remains consumed, never reused");
        assert!(last_view.is_empty());
    }

    #[test]
    fn discovery_publication_wakes_capture_and_coalesces_without_spin() {
        let now = Instant::now();
        let deadline = runtime_deadline_after(now, Duration::from_secs(3_600), "test sync");
        let (tx, rx) = watch::channel(DiscoveryCapturePublication::default());

        assert!(!capture_sync_due(now, deadline, &rx));
        tx.send(discovery_publication(1)).expect("publish epoch 1");
        tx.send(discovery_publication(2)).expect("publish epoch 2");
        tx.send(discovery_publication(3)).expect("publish epoch 3");
        assert!(capture_sync_due(now, deadline, &rx));
        assert_eq!(rx.borrow_and_clone().epoch, 3);
        assert!(
            !capture_sync_due(now, deadline, &rx),
            "consuming the latest publication must clear the wakeup"
        );

        drop(tx);
        assert!(
            !capture_sync_due(now, deadline, &rx),
            "a closed publication channel must retain timer fallback without busy-spin"
        );
        assert!(capture_sync_due(deadline, deadline, &rx));
    }

    #[test]
    fn durability_checkpoint_is_bounded_and_uncertainty_fails_closed() {
        let checkpoints = Arc::new(StdMutex::new(LruCache::new(2)));
        let revision = DiscoveryRevision(7);

        let unseeded = begin_capture_checkpoint_write(&checkpoints, 42, revision);
        confirm_capture_checkpoint(&checkpoints, 42, &unseeded, 4, "unseeded");
        assert!(
            certain_capture_checkpoint(&checkpoints, 42, revision).is_none(),
            "one later segment cannot certify missing durable history"
        );

        let raw = "x".repeat(crate::ingest::RESUME_ANCHOR_BYTES * 2);
        let _ = checkpoints.lock().expect("checkpoint cache").put(
            42,
            CachedCaptureCheckpoint::Certain(CaptureDurabilityCheckpoint {
                revision,
                next_seq: 4,
                raw_tail: raw,
            }),
        );
        let contiguous = begin_capture_checkpoint_write(&checkpoints, 42, revision);
        confirm_capture_checkpoint(&checkpoints, 42, &contiguous, 4, "tail");
        let confirmed = certain_capture_checkpoint(&checkpoints, 42, revision)
            .expect("contiguous confirmed checkpoint");
        assert_eq!(confirmed.next_seq, 5);
        assert_eq!(confirmed.raw_tail.len(), crate::ingest::RESUME_ANCHOR_BYTES);
        assert!(confirmed.raw_tail.ends_with("tail"));

        let ambiguous = begin_capture_checkpoint_write(&checkpoints, 42, revision);
        drop(ambiguous);
        assert!(certain_capture_checkpoint(&checkpoints, 42, revision).is_none());
        let after_ambiguous = begin_capture_checkpoint_write(&checkpoints, 42, revision);
        confirm_capture_checkpoint(&checkpoints, 42, &after_ambiguous, 5, "later");
        assert!(
            certain_capture_checkpoint(&checkpoints, 42, revision).is_none(),
            "a later success cannot erase an ambiguous predecessor write"
        );

        let _ = checkpoints.lock().expect("checkpoint cache").put(
            42,
            CachedCaptureCheckpoint::Certain(CaptureDurabilityCheckpoint {
                revision,
                next_seq: 8,
                raw_tail: "base".to_string(),
            }),
        );
        let corrected = begin_capture_checkpoint_write(&checkpoints, 42, revision);
        confirm_capture_checkpoint(&checkpoints, 42, &corrected, 9, "corrected");
        assert!(
            certain_capture_checkpoint(&checkpoints, 42, revision).is_none(),
            "storage-assigned sequence correction requires authoritative reconciliation"
        );

        assert!(
            certain_capture_checkpoint(&checkpoints, 42, DiscoveryRevision(8)).is_none(),
            "a same-ID successor cannot consume a predecessor revision without transition reset"
        );
    }

    #[test]
    fn post_drain_capture_setup_bootstraps_checkpoint_without_overwrite() {
        run_async_test(async {
            let pane_id = 42;
            let revision = DiscoveryRevision(7);
            let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
            let contexts = Arc::new(RwLock::new(HashMap::<u64, DetectionContext>::new()));
            let activity = Arc::new(RwLock::new(HashMap::<u64, PaneActivityState>::new()));
            let checkpoints = Arc::new(StdMutex::new(LruCache::new(2)));
            let checkpoint = CaptureDurabilityCheckpoint {
                revision,
                next_seq: 5,
                raw_tail: "durable tail".to_string(),
            };

            assert!(
                initialize_capture_state_from_checkpoint(
                    pane_id,
                    &checkpoint,
                    &cursors,
                    &contexts,
                    &activity,
                    &checkpoints,
                )
                .await
                .expect("initialize post-drain storage-proven capture state")
            );
            assert_eq!(
                cursors
                    .read()
                    .await
                    .get(&pane_id)
                    .expect("initialized cursor")
                    .next_seq,
                5
            );
            assert!(contexts.read().await.contains_key(&pane_id));
            let cached = certain_capture_checkpoint(&checkpoints, pane_id, revision)
                .expect("post-drain storage-proven checkpoint is immediately certain");
            assert_eq!(cached.next_seq, 5);
            assert_eq!(cached.raw_tail, "durable tail");

            let superseding_setup = CaptureDurabilityCheckpoint {
                revision: DiscoveryRevision(8),
                next_seq: 99,
                raw_tail: "must not overwrite live state".to_string(),
            };
            assert!(
                !initialize_capture_state_from_checkpoint(
                    pane_id,
                    &superseding_setup,
                    &cursors,
                    &contexts,
                    &activity,
                    &checkpoints,
                )
                .await
                .expect("existing cursor is coordinator-owned")
            );
            assert_eq!(
                cursors
                    .read()
                    .await
                    .get(&pane_id)
                    .expect("retained live cursor")
                    .next_seq,
                5
            );
            assert!(
                certain_capture_checkpoint(
                    &checkpoints,
                    pane_id,
                    superseding_setup.revision,
                )
                .is_none(),
                "post-drain setup must not certify over an existing coordinator-owned cursor"
            );
        });
    }

    #[test]
    fn checkpoint_eviction_cannot_be_recertified_from_one_later_segment() {
        let checkpoints = Arc::new(StdMutex::new(LruCache::new(1)));
        let revision = DiscoveryRevision(3);
        let _ = checkpoints.lock().expect("checkpoint cache").put(
            1,
            CachedCaptureCheckpoint::Certain(CaptureDurabilityCheckpoint {
                revision,
                next_seq: 10,
                raw_tail: "pane one history".to_string(),
            }),
        );
        let _ = checkpoints.lock().expect("checkpoint cache").put(
            2,
            CachedCaptureCheckpoint::Certain(CaptureDurabilityCheckpoint {
                revision,
                next_seq: 20,
                raw_tail: "pane two history".to_string(),
            }),
        );

        let evicted = begin_capture_checkpoint_write(&checkpoints, 1, revision);
        confirm_capture_checkpoint(&checkpoints, 1, &evicted, 10, "short prompt");

        assert!(
            certain_capture_checkpoint(&checkpoints, 1, revision).is_none(),
            "eviction must force storage fallback instead of inventing a short anchor"
        );
    }

    #[test]
    fn pending_resync_overflow_never_becomes_direct_admission() {
        let mut pending = PendingCaptureResyncs::new(1);
        assert!(!pending.remember(1, DiscoveryRevision(10)));
        assert!(pending.remember(2, DiscoveryRevision(20)));
        assert_eq!(
            pending.requirement(1, false),
            Some(CaptureResyncRequirement::Exact(DiscoveryRevision(10)))
        );
        assert_eq!(
            pending.requirement(2, false),
            Some(CaptureResyncRequirement::StorageAudit),
            "capacity-plus-one transition must fail over to storage, never disappear"
        );

        pending.acknowledge(1);
        assert_eq!(
            pending.requirement(1, false),
            None,
            "settled transitions must not poison unrelated future panes"
        );
        assert_eq!(
            pending.requirement(2, false),
            Some(CaptureResyncRequirement::StorageAudit),
            "overflow fallback must not depend on discovery's earlier storage observation"
        );
        pending.acknowledge(2);
        assert_eq!(pending.requirement(2, false), None);
        assert!(!pending.remember(3, DiscoveryRevision(30)));
        pending.require_storage_audit(3);
        assert_eq!(
            pending.requirement(3, false),
            Some(CaptureResyncRequirement::StorageAudit),
            "an unacknowledged retry must never infer continuity from revision equality"
        );
    }

    #[test]
    fn failed_resync_retry_discards_predecessor_anchor_and_emits_full_successor_snapshot() {
        run_async_test(async {
            let pane_id = 42;
            let predecessor_revision = DiscoveryRevision(30);
            let successor_revision = DiscoveryRevision(31);
            let mut pending = PendingCaptureResyncs::new(4);
            assert!(!pending.remember(pane_id, predecessor_revision));
            pending.require_storage_audit(pane_id);
            assert_eq!(
                pending.requirement(pane_id, false),
                Some(CaptureResyncRequirement::StorageAudit)
            );

            let cursors = Arc::new(RwLock::new(HashMap::from([(
                pane_id,
                PaneCursor::from_seq(pane_id, 8)
                    .with_resume_anchor("shared prompt> ".to_string()),
            )])));
            let contexts = Arc::new(RwLock::new(HashMap::from([(
                pane_id,
                DetectionContext::new(),
            )])));
            let activity = Arc::new(RwLock::new(HashMap::<u64, PaneActivityState>::new()));
            let checkpoints = Arc::new(StdMutex::new(LruCache::new(4)));
            let storage_audited = CaptureDurabilityCheckpoint {
                revision: predecessor_revision,
                next_seq: 8,
                raw_tail: "shared prompt> ".to_string(),
            };

            reset_capture_state_from_checkpoint(
                pane_id,
                successor_revision,
                &storage_audited,
                false,
                &cursors,
                &contexts,
                &activity,
                &checkpoints,
            )
            .await
            .expect("reset failed resync retry without predecessor continuity");
            let segment = cursors
                .write()
                .await
                .get_mut(&pane_id)
                .expect("successor cursor")
                .capture_generation_resync(
                    "shared prompt> successor output",
                    "capture_generation_resync",
                );
            assert_eq!(segment.content, "shared prompt> successor output");
            let checkpoint = certain_capture_checkpoint(
                &checkpoints,
                pane_id,
                successor_revision,
            )
            .expect("successor checkpoint");
            assert!(
                checkpoint.raw_tail.is_empty(),
                "failed retry cannot relabel predecessor storage text as successor continuity"
            );
        });
    }

    #[test]
    fn ready_transition_descriptor_survives_watch_coalescing_without_rearming_completion() {
        let pane_id = 42;
        let predecessor_revision = DiscoveryRevision(10);
        let successor_revision = DiscoveryRevision(11);
        let transition = CaptureTransitionDescriptor {
            desired_revision: successor_revision,
            predecessor_revision: Some(predecessor_revision),
        };
        let mut pending = PendingCaptureResyncs::new(1);

        let pending_publication = DiscoveryCapturePublication {
            epoch: 1,
            observed_panes: Arc::new(HashMap::new()),
            transitioning_pane_ids: Arc::new(HashSet::from([pane_id])),
            transitions: Arc::new(HashMap::from([(pane_id, transition)])),
        };
        pending.retain_authoritative(&pending_publication);
        assert_eq!(
            pending.requirement(pane_id, false),
            None,
            "an unready post-list publication cannot import a possibly stale descriptor"
        );

        let ready_publication = DiscoveryCapturePublication {
            epoch: 2,
            observed_panes: Arc::new(HashMap::from([(
                pane_id,
                ObservedCapturePane {
                    info: make_pane(pane_id, "successor"),
                    generation: 1,
                    pane_uuid: "successor-uuid".to_string(),
                    revision: successor_revision,
                    requires_storage_resync: false,
                },
            )])),
            transitioning_pane_ids: Arc::new(HashSet::new()),
            transitions: Arc::new(HashMap::from([(pane_id, transition)])),
        };
        pending.retain_authoritative(&ready_publication);
        pending.observe_ready_transitions(&ready_publication, &HashMap::new());
        assert_eq!(
            pending.requirement(pane_id, false),
            Some(CaptureResyncRequirement::Exact(predecessor_revision)),
            "watch coalescing directly to ready must retain exact predecessor continuity"
        );

        pending.acknowledge(pane_id);
        pending.observe_ready_transitions(
            &ready_publication,
            &HashMap::from([(pane_id, successor_revision)]),
        );
        assert_eq!(
            pending.requirement(pane_id, false),
            None,
            "a durable successor cannot be re-armed by its retained descriptor"
        );

        pending.retain_authoritative(&DiscoveryCapturePublication::default());
        assert_eq!(
            pending.requirement(pane_id, false),
            None,
            "a confirmed terminal close releases the transition obligation"
        );
    }

    #[test]
    fn held_binding_is_quarantined_without_blocking_unrelated_retirement() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let first_identity = authority.activate_pane(1).expect("first pane");
            let second_identity = authority.activate_pane(2).expect("second pane");
            let first_lease = authority
                .issue_source(first_identity, CaptureSourceKind::Polling)
                .expect("first polling source");
            let second_lease = authority
                .issue_source(second_identity, CaptureSourceKind::Polling)
                .expect("second polling source");
            let held = first_lease
                .try_acquire_persistence(first_lease.stamp(), 1)
                .expect("held first persistence guard");
            let revision = DiscoveryRevision(1);
            let first_binding = ActiveCaptureBinding {
                generation: 0,
                pane_uuid: "first".to_string(),
                revision,
                identity: first_identity,
                polling_lease: first_lease,
                resync_receipt: None,
                #[cfg(feature = "native-wezterm")]
                native_lease: None,
                #[cfg(all(feature = "vendored", unix))]
                streaming_lease: None,
            };
            let second_binding = ActiveCaptureBinding {
                generation: 0,
                pane_uuid: "second".to_string(),
                revision,
                identity: second_identity,
                polling_lease: second_lease,
                resync_receipt: None,
                #[cfg(feature = "native-wezterm")]
                native_lease: None,
                #[cfg(all(feature = "vendored", unix))]
                streaming_lease: None,
            };
            let metadata = Arc::new(RwLock::new(HashMap::from([
                (
                    first_identity.pane_incarnation(),
                    CapturePaneMetadata {
                        pane_uuid: "first".to_string(),
                        discovery_generation: 0,
                        discovery_revision: revision,
                    },
                ),
                (
                    second_identity.pane_incarnation(),
                    CapturePaneMetadata {
                        pane_uuid: "second".to_string(),
                        discovery_generation: 0,
                        discovery_revision: revision,
                    },
                ),
            ])));
            let backpressure = BackpressureMetrics::default();
            backpressure.record_segment_dropped(1);
            backpressure.record_segment_dropped(2);
            let mut draining = HashMap::new();
            let mut draining_since = HashMap::new();

            assert!(!retire_or_quarantine_capture_binding(
                &authority,
                &metadata,
                &backpressure,
                &mut draining,
                &mut draining_since,
                first_binding,
                "test held predecessor",
            )
            .await);
            assert!(draining.contains_key(&1));
            assert!(draining_since.contains_key(&1));
            assert_eq!(
                backpressure.segments_dropped_for_pane(1),
                1,
                "attribution remains until the exact producer guard drains"
            );
            assert!(
                metadata
                    .read()
                    .await
                    .contains_key(&first_identity.pane_incarnation()),
                "quarantined binding retains immutable metadata until exact drain"
            );

            assert!(retire_or_quarantine_capture_binding(
                &authority,
                &metadata,
                &backpressure,
                &mut draining,
                &mut draining_since,
                second_binding,
                "test unrelated predecessor",
            )
            .await);
            assert!(!draining.contains_key(&2));
            assert!(!draining_since.contains_key(&2));
            assert_eq!(backpressure.segments_dropped_for_pane(2), 0);
            assert!(
                !metadata
                    .read()
                    .await
                    .contains_key(&second_identity.pane_incarnation()),
                "unrelated drained binding retires immediately"
            );

            drop(held);
            let first_binding = draining.remove(&1).expect("quarantined first binding");
            assert!(retire_or_quarantine_capture_binding(
                &authority,
                &metadata,
                &backpressure,
                &mut draining,
                &mut draining_since,
                first_binding,
                "test retry after drain",
            )
            .await);
            assert!(draining.is_empty());
            assert!(draining_since.is_empty());
            assert_eq!(backpressure.segments_dropped_for_pane(1), 0);
            assert!(
                !metadata
                    .read()
                    .await
                    .contains_key(&first_identity.pane_incarnation())
            );
        });
    }

    #[test]
    fn stale_mpsc_and_spsc_events_fail_full_persistence_preflight() {
        run_async_test(async {
            let pane_id = 77;
            let predecessor_revision = DiscoveryRevision(10);
            let successor_revision = DiscoveryRevision(11);
            let authority = CaptureAuthority::new();
            let predecessor = authority
                .activate_pane(pane_id)
                .expect("predecessor pane");
            let predecessor_lease = authority
                .issue_source(predecessor, CaptureSourceKind::Polling)
                .expect("predecessor source");
            let capture_metadata = Arc::new(RwLock::new(HashMap::from([(
                predecessor.pane_incarnation(),
                CapturePaneMetadata {
                    pane_uuid: "predecessor-uuid".to_string(),
                    discovery_generation: 0,
                    discovery_revision: predecessor_revision,
                },
            )])));
            let predecessor_observed = ObservedCapturePane {
                info: make_pane(pane_id, "predecessor"),
                generation: 0,
                pane_uuid: "predecessor-uuid".to_string(),
                revision: predecessor_revision,
                requires_storage_resync: false,
            };
            let (publication_tx, publication_rx) = watch::channel(
                DiscoveryCapturePublication {
                    epoch: 1,
                    observed_panes: Arc::new(HashMap::from([(
                        pane_id,
                        predecessor_observed,
                    )])),
                    transitioning_pane_ids: Arc::new(HashSet::new()),
                    transitions: Arc::new(HashMap::new()),
                },
            );

            let publication_only = test_capture_event_for_lease(
                pane_id,
                0,
                &predecessor_lease,
            );
            let (mpsc_decision, mpsc_receipt) = CaptureResyncDecision::channel();
            let mpsc_event = test_capture_event_for_lease(
                pane_id,
                1,
                &predecessor_lease,
            )
            .with_resync_decision(mpsc_decision);
            let (spsc_decision, spsc_receipt) = CaptureResyncDecision::channel();
            let spsc_event = test_capture_event_for_lease(
                pane_id,
                2,
                &predecessor_lease,
            )
            .with_resync_decision(spsc_decision);
            let (ingress_tx, mut ingress_rx) = mpsc::channel(2);
            let (ring_tx, ring_rx) = spsc_channel(3);
            ingress_tx.try_send(mpsc_event).expect("queue old MPSC event");
            ring_tx.try_send(spsc_event).expect("queue old SPSC event");

            let successor_observed = ObservedCapturePane {
                info: make_pane(pane_id, "successor"),
                generation: 1,
                pane_uuid: "successor-uuid".to_string(),
                revision: successor_revision,
                requires_storage_resync: true,
            };
            publication_tx
                .send(DiscoveryCapturePublication {
                    epoch: 2,
                    observed_panes: Arc::new(HashMap::from([(
                        pane_id,
                        successor_observed,
                    )])),
                    transitioning_pane_ids: Arc::new(HashSet::new()),
                    transitions: Arc::new(HashMap::new()),
                })
                .expect("publish successor before revocation");
            assert!(
                admit_capture_event_for_persistence(
                    &authority,
                    &capture_metadata,
                    &publication_rx,
                    &publication_only,
                )
                .await
                .is_err(),
                "publication supersession alone must close semantic admission"
            );

            assert!(
                authority
                    .retire_pane_if_drained(predecessor)
                    .expect("retire predecessor")
            );
            let successor = authority.activate_pane(pane_id).expect("successor pane");
            let successor_lease = authority
                .issue_source(successor, CaptureSourceKind::Polling)
                .expect("successor source");
            capture_metadata.write().await.insert(
                successor.pane_incarnation(),
                CapturePaneMetadata {
                    pane_uuid: "successor-uuid".to_string(),
                    discovery_generation: 1,
                    discovery_revision: successor_revision,
                },
            );

            let loop_cx = runtime_loop_cx();
            let relayed = recv_mpsc(&mut ingress_rx).await;
            relay_capture_event_with_cx(&loop_cx, &ring_tx, relayed)
                .await
                .expect("relay old MPSC event into SPSC");

            let mut semantic_side_effects = 0_u64;
            let mut rejected = 0_u64;
            let mut direct_ring_event = ring_rx.try_recv().expect("old SPSC event");
            let mut direct_ring_decision = direct_ring_event
                .take_resync_decision()
                .expect("SPSC resync decision");
            match admit_capture_event_for_persistence(
                &authority,
                &capture_metadata,
                &publication_rx,
                &direct_ring_event,
            )
            .await
            {
                Ok((_guard, _metadata)) => semantic_side_effects += 1,
                Err(error) => {
                    rejected += 1;
                    direct_ring_decision.finish(Err(error.to_string()));
                }
            }
            assert!(
                spsc_receipt
                    .outcome()
                    .expect("SPSC rejection decision")
                    .is_err()
            );

            let mut relayed_ingress_event =
                ring_rx.try_recv().expect("relayed MPSC event");
            let mut relayed_ingress_decision = relayed_ingress_event
                .take_resync_decision()
                .expect("MPSC resync decision");
            match admit_capture_event_for_persistence(
                &authority,
                &capture_metadata,
                &publication_rx,
                &relayed_ingress_event,
            )
            .await
            {
                Ok((_guard, _metadata)) => semantic_side_effects += 1,
                Err(error) => {
                    rejected += 1;
                    relayed_ingress_decision.finish(Err(error.to_string()));
                }
            }
            assert!(
                mpsc_receipt
                    .outcome()
                    .expect("MPSC rejection decision")
                    .is_err()
            );

            let successor_event =
                test_capture_event_for_lease(pane_id, 3, &successor_lease);
            let successor_admission = admit_capture_event_for_persistence(
                &authority,
                &capture_metadata,
                &publication_rx,
                &successor_event,
            )
            .await
            .expect("successor event admission");
            semantic_side_effects += 1;
            drop(successor_admission);

            assert_eq!(rejected, 2);
            assert_eq!(
                semantic_side_effects, 1,
                "only the exact successor may cross the semantic side-effect boundary"
            );
        });
    }

    #[test]
    fn queued_predecessor_events_on_both_queues_have_zero_real_persistence_side_effects() {
        run_async_test_isolated(|| async {
            let pane_id = 77;
            let predecessor_revision = DiscoveryRevision(10);
            let successor_revision = DiscoveryRevision(11);
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.expect("test storage");
            storage
                .upsert_pane(test_pane_record(pane_id))
                .await
                .expect("persist test pane");

            let event_bus = Arc::new(EventBus::new(16));
            let mut event_subscriber = event_bus.subscribe();
            let wezterm: WeztermHandle = Arc::new(crate::wezterm::MockWezterm::new());
            let runtime = ObservationRuntime::new(
                RuntimeConfig::default(),
                storage.clone(),
                Arc::new(RwLock::new(PatternEngine::new())),
            )
            .with_event_bus(Arc::clone(&event_bus))
            .with_wezterm_handle(wezterm);

            let predecessor = runtime
                .capture_authority
                .activate_pane(pane_id)
                .expect("predecessor pane");
            let predecessor_lease = runtime
                .capture_authority
                .issue_source(predecessor, CaptureSourceKind::Polling)
                .expect("predecessor source");
            runtime.capture_metadata.write().await.insert(
                predecessor.pane_incarnation(),
                CapturePaneMetadata {
                    pane_uuid: "predecessor-uuid".to_string(),
                    discovery_generation: 0,
                    discovery_revision: predecessor_revision,
                },
            );

            let (publication_tx, publication_rx) = watch::channel(
                DiscoveryCapturePublication {
                    epoch: 1,
                    observed_panes: Arc::new(HashMap::from([(
                        pane_id,
                        ObservedCapturePane {
                            info: make_pane(pane_id, "predecessor"),
                            generation: 0,
                            pane_uuid: "predecessor-uuid".to_string(),
                            revision: predecessor_revision,
                            requires_storage_resync: false,
                        },
                    )])),
                    transitioning_pane_ids: Arc::new(HashSet::new()),
                    transitions: Arc::new(HashMap::new()),
                },
            );

            let (mpsc_decision, mpsc_receipt) = CaptureResyncDecision::channel();
            let stale_mpsc = test_capture_event_for_lease(pane_id, 0, &predecessor_lease)
                .with_resync_decision(mpsc_decision);
            let (spsc_decision, spsc_receipt) = CaptureResyncDecision::channel();
            let stale_spsc = test_capture_event_for_lease(pane_id, 1, &predecessor_lease)
                .with_resync_decision(spsc_decision);

            publication_tx
                .send(DiscoveryCapturePublication {
                    epoch: 2,
                    observed_panes: Arc::new(HashMap::from([(
                        pane_id,
                        ObservedCapturePane {
                            info: make_pane(pane_id, "successor"),
                            generation: 1,
                            pane_uuid: "successor-uuid".to_string(),
                            revision: successor_revision,
                            requires_storage_resync: true,
                        },
                    )])),
                    transitioning_pane_ids: Arc::new(HashSet::new()),
                    transitions: Arc::new(HashMap::new()),
                })
                .expect("publish successor view");
            assert!(
                runtime
                    .capture_authority
                    .retire_pane_if_drained(predecessor)
                    .expect("retire predecessor"),
                "predecessor has no live guards after both events own their immutable stamps"
            );

            let successor = runtime
                .capture_authority
                .activate_pane(pane_id)
                .expect("successor pane");
            let successor_lease = runtime
                .capture_authority
                .issue_source(successor, CaptureSourceKind::Polling)
                .expect("successor source");
            runtime.capture_metadata.write().await.insert(
                successor.pane_incarnation(),
                CapturePaneMetadata {
                    pane_uuid: "successor-uuid".to_string(),
                    discovery_generation: 1,
                    discovery_revision: successor_revision,
                },
            );

            let successor_segment = {
                let mut cursors = runtime.cursors.write().await;
                let cursor = cursors
                    .entry(pane_id)
                    .or_insert_with(|| PaneCursor::new(pane_id));
                cursor.capture_generation_resync(
                    "successor snapshot",
                    "capture_generation_resync",
                )
            };
            let (successor_decision, successor_receipt) = CaptureResyncDecision::channel();
            let successor_event = test_capture_event_from_segment_for_lease(
                successor_segment,
                &successor_lease,
            )
            .with_resync_decision(successor_decision);

            let (ingress_tx, ingress_rx) = mpsc::channel(2);
            let (ring_tx, ring_rx) = spsc_channel(4);
            ring_tx
                .try_send(stale_spsc)
                .expect("park predecessor event in SPSC");
            ingress_tx
                .try_send(stale_mpsc)
                .expect("park predecessor event in MPSC");
            ingress_tx
                .try_send(successor_event)
                .expect("queue successor resync behind predecessor");
            drop(ingress_tx);

            let checkpoints: CaptureCheckpointCache =
                Arc::new(StdMutex::new(LruCache::new(4)));
            let _ = checkpoints
                .lock()
                .expect("capture checkpoint cache")
                .put(
                    pane_id,
                    CachedCaptureCheckpoint::Certain(CaptureDurabilityCheckpoint {
                        revision: successor_revision,
                        next_seq: 0,
                        raw_tail: String::new(),
                    }),
                );
            let relay = runtime.spawn_capture_relay_task(ingress_rx, ring_tx);
            let persistence = runtime.spawn_persistence_task(
                ring_rx,
                Arc::clone(&runtime.cursors),
                publication_rx,
                Arc::clone(&checkpoints),
            );
            relay.await.expect("capture relay task");
            persistence.await.expect("capture persistence task");

            assert!(
                spsc_receipt
                    .outcome()
                    .expect("stale SPSC decision")
                    .is_err(),
                "the direct SPSC predecessor must fail closed"
            );
            assert!(
                mpsc_receipt
                    .outcome()
                    .expect("stale MPSC decision")
                    .is_err(),
                "the relayed MPSC predecessor must fail closed"
            );
            assert_eq!(
                successor_receipt.outcome(),
                Some(Ok(0)),
                "the exact successor gets one durable sequence acknowledgement"
            );

            let segments = storage
                .get_segments(pane_id, 10)
                .await
                .expect("read persisted successor");
            assert_eq!(segments.len(), 1, "stale events must not reach storage");
            assert_eq!(segments[0].seq, 0);
            assert_eq!(segments[0].content, "successor snapshot");
            let gaps = storage.get_gaps().await.expect("read successor gap");
            assert_eq!(gaps.len(), 1, "one successor resync emits one durable gap");
            assert_eq!(gaps[0].pane_id, pane_id);
            assert!(
                gaps[0].reason.contains("capture_generation_resync"),
                "the durable gap retains its resync justification"
            );
            assert!(
                storage
                    .get_events(crate::storage::EventQuery::default())
                    .await
                    .expect("read detection events")
                    .is_empty(),
                "stale text must not create detection rows"
            );

            let cursor = runtime
                .cursors
                .read()
                .await
                .get(&pane_id)
                .cloned()
                .expect("successor cursor");
            assert_eq!(cursor.next_seq, 1);
            assert_eq!(cursor.last_snapshot, "successor snapshot");
            assert!(cursor.in_gap);
            let checkpoint = certain_capture_checkpoint(&checkpoints, pane_id, successor_revision)
                .expect("durable successor checkpoint");
            assert_eq!(checkpoint.next_seq, 1);
            assert_eq!(checkpoint.raw_tail, "successor snapshot");

            assert_eq!(runtime.metrics.capture_authority_rejections(), 2);
            assert_eq!(runtime.metrics.segments_persisted(), 1);
            assert_eq!(runtime.metrics.events_recorded(), 0);
            assert!(runtime.detection_contexts.read().await.contains_key(&pane_id));

            let mut published = Vec::new();
            while let Some(event) = event_subscriber.try_recv() {
                published.push(event.expect("event bus receive"));
            }
            assert_eq!(published.len(), 2, "one segment and one gap are published");
            assert!(published.iter().any(|event| matches!(
                event,
                Event::SegmentCaptured { pane_id: id, seq: 0, .. } if *id == pane_id
            )));
            assert!(published.iter().any(|event| matches!(
                event,
                Event::GapDetected { pane_id: id, .. } if *id == pane_id
            )));

            storage.shutdown().await.expect("shutdown test storage");
        });
    }

    #[test]
    fn connector_outbound_runtime_event_dispatches_through_mesh_and_host_runtime() {
        let mut bridge = crate::connector_outbound_bridge::ConnectorOutboundBridge::new(
            crate::connector_outbound_bridge::ConnectorOutboundBridgeConfig::default(),
        );
        bridge.add_rule(crate::connector_outbound_bridge::OutboundRoutingRule {
            rule_id: "notify-usage".to_string(),
            source_filter: Some(
                crate::connector_outbound_bridge::OutboundEventSource::PatternDetected,
            ),
            event_type_prefix: Some("pattern.".to_string()),
            min_severity: None,
            target_connector: "worker".to_string(),
            action_kind: crate::connector_outbound_bridge::ConnectorActionKind::Invoke,
            enabled: true,
            priority: 0,
        });
        let detection = crate::patterns::Detection {
            rule_id: "codex.usage.reached".to_string(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: crate::patterns::Severity::Warning,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "[redacted]".to_string(),
            span: (0, 10),
        };
        let event = Event::PatternDetected {
            pane_id: 7,
            pane_uuid: None,
            detection,
            event_id: Some(123),
        };

        process_connector_outbound_runtime_event(&mut bridge, &event, 5_000);

        assert_eq!(bridge.pending_action_count(), 0);
        assert_eq!(bridge.telemetry().actions_dispatched, 1);
        let policy = bridge.policy_engine();
        let mesh = policy.connector_mesh().telemetry().snapshot();
        assert_eq!(mesh.zones_created, 1);
        assert_eq!(mesh.hosts_registered, 1);
        assert_eq!(mesh.heartbeats_received, 1);
        assert_eq!(mesh.routing_requests, 1);
        assert_eq!(mesh.routing_successes, 1);
        assert_eq!(policy.connector_mesh().health_snapshot().total_active, 0);
        assert_eq!(
            policy.connector_host_runtime().state().phase(),
            crate::connector_host_runtime::ConnectorLifecyclePhase::Running
        );
        assert_eq!(
            policy
                .connector_host_runtime()
                .sandbox_decision_history()
                .len(),
            1,
            "runtime dispatch must authorize through the policy-owned host runtime"
        );
        assert_eq!(
            policy.reliability_registry().total_dlq_depth(),
            0,
            "mesh-routed runtime dispatch should complete without DLQ feedback"
        );
        assert!(
            policy.reliability_registry().get("worker").is_some(),
            "successful dispatch should feed reliability success for the target connector"
        );
    }

    #[test]
    fn runtime_cx_error_uses_structured_content_free_cancellation() {
        let cx = crate::cx::for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("SECRET caller cancellation detail"),
        );
        let err = runtime_cx_error("runtime.test_start", &cx, "fallback");

        assert!(
            matches!(&err, Error::RuntimeOperation { .. }),
            "expected structured runtime operation, got {err:?}"
        );
        if let Error::RuntimeOperation { operation, source } = &err {
            assert_eq!(*operation, "runtime.test_start");
            assert_eq!(
                source,
                &RuntimeOperationSource::Cancelled("capability context cancelled".to_string())
            );
        }
        assert!(!format!("{err:?}").contains("SECRET"));
    }

    #[test]
    fn runtime_cancellation_classifier_covers_structured_and_legacy_errors() {
        let cx = crate::cx::for_testing();
        cx.cancel_with(crate::outcome::CancelKind::User, Some("SECRET structured cancel"));
        let structured = runtime_cx_error("runtime.test", &cx, "fallback");
        let legacy = Error::Cancelled("legacy cancel".to_string());
        let backend = runtime_backend_error("runtime.test", "backend failure");

        assert!(is_runtime_cancellation(&structured));
        assert!(is_runtime_cancellation(&legacy));
        assert!(!is_runtime_cancellation(&backend));
        assert!(!format!("{structured:?}").contains("SECRET"));
    }

    #[test]
    fn runtime_timeout_cancel_kind_is_classified_as_deadline_exhaustion() {
        let cx = crate::cx::for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::Timeout,
            Some("SECRET timeout detail"),
        );
        let error = runtime_cx_error("runtime.test", &cx, "fallback");
        assert!(matches!(
            &error,
            Error::RuntimeOperation {
                source: RuntimeOperationSource::Backend(message),
                ..
            } if message == "capability deadline exceeded"
        ));
        assert!(!is_runtime_cancellation(&error));
        assert!(!format!("{error:?}").contains("SECRET"));
    }

    #[test]
    fn runtime_wait_failure_counter_increment_saturates() {
        let counter = std::sync::atomic::AtomicU64::new(u64::MAX - 1);
        assert_eq!(increment_saturating_atomic(&counter), u64::MAX);
        assert_eq!(increment_saturating_atomic(&counter), u64::MAX);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn runtime_sleep_checks_context_after_successful_timer_completion() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("SECRET cancellation before zero-duration timer"),
            );

            let failure = runtime_sleep(&cx, Duration::ZERO)
                .await
                .expect_err("successful timer completion must not hide context cancellation");
            assert_eq!(failure, RuntimeWaitFailureKind::ContextCancelled);
        });
    }

    #[test]
    fn runtime_wait_classifier_detects_unmaterialized_budget_exhaustion() {
        let cases = [
            (
                crate::cx::Budget::new()
                    .with_deadline(crate::runtime_async::RuntimeTime::ZERO),
                RuntimeWaitFailureKind::DeadlineExceeded,
            ),
            (
                crate::cx::Budget::new().with_poll_quota(0),
                RuntimeWaitFailureKind::PollQuotaExhausted,
            ),
            (
                crate::cx::Budget::new().with_cost_quota(0),
                RuntimeWaitFailureKind::CostBudgetExhausted,
            ),
        ];

        for (budget, expected) in cases {
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            assert!(
                cx.root_cancel_cause().is_none(),
                "test precondition: budget failure must not yet be materialized as cancellation"
            );
            assert_eq!(runtime_context_failure_kind(&cx), expected);
        }
    }

    #[test]
    fn runtime_timeout_preserves_budget_failure_classes() {
        run_async_test(async {
            let cases = [
                (
                    crate::cx::Budget::new()
                        .with_deadline(crate::runtime_async::RuntimeTime::ZERO),
                    RuntimeWaitFailureKind::DeadlineExceeded,
                ),
                (
                    crate::cx::Budget::new().with_poll_quota(0),
                    RuntimeWaitFailureKind::PollQuotaExhausted,
                ),
                (
                    crate::cx::Budget::new().with_cost_quota(0),
                    RuntimeWaitFailureKind::CostBudgetExhausted,
                ),
            ];

            for (budget, expected) in cases {
                let cx = crate::cx::Cx::for_testing_with_budget(budget);
                let result = runtime_timeout(
                    &cx,
                    Duration::from_secs(1),
                    std::future::pending::<()>(),
                )
                .await;
                assert_eq!(result, Err(RuntimeTimeoutFailure::Context(expected)));
            }
        });
    }

    #[test]
    fn runtime_backend_error_uses_structured_runtime_operation() {
        let err = runtime_backend_error("runtime.test_update", "watch channel closed");

        assert!(
            matches!(&err, Error::RuntimeOperation { .. }),
            "expected structured runtime operation, got {err:?}"
        );
        if let Error::RuntimeOperation { operation, source } = err {
            assert_eq!(operation, "runtime.test_update");
            assert_eq!(
                source,
                RuntimeOperationSource::Backend("watch channel closed".to_string())
            );
        }
    }

    #[test]
    fn replay_egress_uses_the_durable_persistence_sequence() {
        let authority = CaptureAuthority::new();
        let pane = authority.activate_pane(9).expect("capture pane");
        let lease = authority
            .issue_source(pane, CaptureSourceKind::Polling)
            .expect("capture source");
        let persistence_guard = authority
            .try_acquire_persistence(lease.stamp(), 9)
            .expect("persistence guard");
        let sink = Arc::new(crate::replay_capture::CollectingCaptureSink::new());
        let adapter = crate::replay_capture::CaptureAdapter::new(
            sink.clone(),
            crate::replay_capture::CaptureConfig::default(),
        );
        let captured = CapturedSegment {
            pane_id: 9,
            seq: 3,
            content: "durable sequence".to_string(),
            kind: CapturedSegmentKind::Delta,
            captured_at: 1_700_000_000_000,
        };

        record_authorized_replay_egress(&adapter, &captured, 7, &persistence_guard)
            .expect("replay sequence is available");

        let events = sink.recorder_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 7);
    }

    #[test]
    fn semantic_zone_inference_uses_matching_zone_text() {
        let captured = CapturedSegment {
            pane_id: 9,
            seq: 1,
            content: "cargo test failed\n".to_string(),
            kind: CapturedSegmentKind::Delta,
            captured_at: 1_700_000_000_000,
        };
        let snapshot = MuxSemanticSnapshot {
            zones: vec![
                crate::wezterm::MuxSemanticZone {
                    start_y: 0,
                    start_x: 0,
                    end_y: 0,
                    end_x: 1,
                    semantic_type: MuxSemanticZoneKind::Prompt,
                    text: "$ ".to_string(),
                },
                crate::wezterm::MuxSemanticZone {
                    start_y: 1,
                    start_x: 0,
                    end_y: 1,
                    end_x: 18,
                    semantic_type: MuxSemanticZoneKind::Output,
                    text: "cargo test failed".to_string(),
                },
            ],
            last_exit_code: Some(101),
        };

        assert_eq!(
            infer_semantic_zone_type_for_segment(&captured, &snapshot),
            Some("output")
        );
    }

    #[test]
    fn semantic_zone_inference_leaves_gap_segments_untyped() {
        let captured = CapturedSegment {
            pane_id: 9,
            seq: 2,
            content: "full snapshot after overlap miss\n".to_string(),
            kind: CapturedSegmentKind::Gap {
                reason: "overlap_not_found".to_string(),
            },
            captured_at: 1_700_000_000_001,
        };
        let snapshot = MuxSemanticSnapshot {
            zones: vec![crate::wezterm::MuxSemanticZone {
                start_y: 1,
                start_x: 0,
                end_y: 1,
                end_x: 20,
                semantic_type: MuxSemanticZoneKind::Output,
                text: "some output".to_string(),
            }],
            last_exit_code: None,
        };

        assert_eq!(
            infer_semantic_zone_type_for_segment(&captured, &snapshot),
            None
        );
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

    #[cfg(any(all(feature = "vendored", unix), feature = "native-wezterm"))]
    fn test_capture_lease(global_pane_id: u64, source_kind: CaptureSourceKind) -> CaptureLease {
        let authority = CaptureAuthority::new();
        let pane = authority
            .activate_pane(global_pane_id)
            .expect("test pane authority");
        authority
            .issue_source(pane, source_kind)
            .expect("test source authority")
    }

    #[cfg(all(feature = "vendored", unix))]
    fn test_streaming_identity(
        global_pane_id: u64,
        local_pane_id: u64,
        socket_shard: usize,
        generation: u32,
        lease: &CaptureLease,
    ) -> StreamingSubscriptionIdentity {
        StreamingSubscriptionIdentity {
            global_pane_id,
            local_pane_id,
            socket_shard: ShardId(socket_shard),
            socket_path: PathBuf::from(format!("/tmp/wa-{socket_shard}.sock")),
            generation,
            capture_stamp: lease.stamp(),
        }
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn forward_vendored_streaming_delta_emits_capture_event() {
        run_async_test(async {
            let (capture_tx, mut capture_rx) = mpsc::channel(4);
            let loop_cx = runtime_loop_cx();
            let mut bridge = StreamingBridge::new();
            let lease = test_capture_lease(17, CaptureSourceKind::VendoredStreaming);
            let identity = test_streaming_identity(17, 17, 0, 0, &lease);

            let exit_reason = forward_vendored_streaming_delta(
                &loop_cx,
                &mut bridge,
                &capture_tx,
                &identity,
                &lease,
                PaneDelta::Output {
                    pane_id: 17,
                    seqno: 1,
                    delta_text: "hello from vendored stream".to_string(),
                    title: "bash".to_string(),
                    dirty_range_count: 1,
                    dirty_row_count: 2,
                },
            )
            .await;

            assert!(exit_reason.is_none());
            let event = recv_mpsc(&mut capture_rx).await;
            assert_eq!(event.segment.pane_id, 17);
            assert_eq!(event.segment.content, "hello from vendored stream");
            assert!(matches!(
                event.segment.kind,
                crate::ingest::CapturedSegmentKind::Delta
            ));
        });
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn forward_vendored_streaming_empty_projection_emits_no_capture_event() {
        run_async_test(async {
            let (capture_tx, mut capture_rx) = mpsc::channel(1);
            let loop_cx = runtime_loop_cx();
            let mut bridge = StreamingBridge::new();
            let lease = test_capture_lease(19, CaptureSourceKind::VendoredStreaming);
            let identity = test_streaming_identity(19, 19, 0, 0, &lease);

            let exit_reason = forward_vendored_streaming_delta(
                &loop_cx,
                &mut bridge,
                &capture_tx,
                &identity,
                &lease,
                PaneDelta::Output {
                    pane_id: 19,
                    seqno: 7,
                    delta_text: String::new(),
                    title: "metadata-only".to_string(),
                    dirty_range_count: 2,
                    dirty_row_count: 6,
                },
            )
            .await;

            assert!(exit_reason.is_none());
            assert!(capture_rx.try_recv().is_err());
            assert_eq!(bridge.events_processed(), 1);
            assert_eq!(bridge.ingester().active_panes(), 0);
            assert_eq!(
                bridge.render_metadata(19).map(|metadata| metadata.title.as_str()),
                Some("metadata-only")
            );
        });
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn forward_vendored_streaming_delta_returns_end_reason_and_emits_close_gap() {
        run_async_test(async {
            let (capture_tx, mut capture_rx) = mpsc::channel(4);
            let loop_cx = runtime_loop_cx();
            let mut bridge = StreamingBridge::new();
            let lease = test_capture_lease(21, CaptureSourceKind::VendoredStreaming);
            let identity = test_streaming_identity(21, 21, 0, 0, &lease);

            let output_reason = forward_vendored_streaming_delta(
                &loop_cx,
                &mut bridge,
                &capture_tx,
                &identity,
                &lease,
                PaneDelta::Output {
                    pane_id: 21,
                    seqno: 1,
                    delta_text: "seed".to_string(),
                    title: "bash".to_string(),
                    dirty_range_count: 1,
                    dirty_row_count: 1,
                },
            )
            .await;
            assert!(output_reason.is_none());
            let _ = recv_mpsc(&mut capture_rx).await;

            let exit_reason = forward_vendored_streaming_delta(
                &loop_cx,
                &mut bridge,
                &capture_tx,
                &identity,
                &lease,
                PaneDelta::Ended {
                    pane_id: 21,
                    reason: "mux socket disconnected".to_string(),
                },
            )
            .await;

            assert_eq!(exit_reason.as_deref(), Some("mux socket disconnected"));
            let event = recv_mpsc(&mut capture_rx).await;
            assert_eq!(event.segment.pane_id, 21);
            assert!(
                matches!(
                    &event.segment.kind,
                    crate::ingest::CapturedSegmentKind::Gap { reason }
                        if reason.contains("pane_closed")
                ),
                "ended delta must emit pane_closed gap"
            );
        });
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn forward_vendored_streaming_delta_reports_closed_capture_ingress() {
        run_async_test(async {
            let (capture_tx, capture_rx) = mpsc::channel::<CaptureEvent>(1);
            drop(capture_rx);

            let loop_cx = runtime_loop_cx();
            let mut bridge = StreamingBridge::new();
            let lease = test_capture_lease(9, CaptureSourceKind::VendoredStreaming);
            let identity = test_streaming_identity(9, 9, 0, 0, &lease);
            let exit_reason = forward_vendored_streaming_delta(
                &loop_cx,
                &mut bridge,
                &capture_tx,
                &identity,
                &lease,
                PaneDelta::Output {
                    pane_id: 9,
                    seqno: 1,
                    delta_text: "orphaned".to_string(),
                    title: "bash".to_string(),
                    dirty_range_count: 1,
                    dirty_row_count: 1,
                },
            )
            .await;

            assert_eq!(exit_reason.as_deref(), Some("capture ingress closed"));
        });
    }

    async fn recv_mpsc<T>(rx: &mut mpsc::Receiver<T>) -> T {
        let cx = crate::cx::for_testing();
        rx.recv(&cx).await.expect("test mpsc recv should succeed")
    }

    fn test_capture_event(seq: u64) -> CaptureEvent {
        let authority = CaptureAuthority::new();
        let pane = authority.activate_pane(1).expect("test pane authority");
        let lease = authority
            .issue_source(pane, CaptureSourceKind::Polling)
            .expect("test polling authority");
        test_capture_event_for_lease(1, seq, &lease)
    }

    fn test_capture_event_for_lease(
        pane_id: u64,
        seq: u64,
        lease: &CaptureLease,
    ) -> CaptureEvent {
        test_capture_event_from_segment_for_lease(
            CapturedSegment {
                pane_id,
                seq,
                content: "test".to_string(),
                kind: crate::ingest::CapturedSegmentKind::Delta,
                captured_at: epoch_ms(),
            },
            lease,
        )
    }

    fn test_capture_event_from_segment_for_lease(
        segment: CapturedSegment,
        lease: &CaptureLease,
    ) -> CaptureEvent {
        let guard = lease
            .try_acquire_producer(lease.stamp(), segment.pane_id)
            .expect("test producer authority");
        CaptureEvent::from_producer(segment, &guard).expect("stamped test capture event")
    }

    fn test_bocpd_segment(pane_id: u64, content: String, captured_at: i64) -> CapturedSegment {
        CapturedSegment {
            pane_id,
            seq: u64::try_from(captured_at).unwrap_or(0),
            content,
            kind: crate::ingest::CapturedSegmentKind::Delta,
            captured_at,
        }
    }

    fn temp_db_path() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db").to_string_lossy().to_string();
        (dir, path)
    }

    #[test]
    fn runtime_routes_connector_signal_to_live_event_bus() {
        run_async_test(async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let engine = PatternEngine::new();
            let bus = Arc::new(EventBus::new(16));
            let mut subscriber = bus.subscribe_detections();

            let runtime = ObservationRuntime::new(
                RuntimeConfig::default(),
                storage,
                Arc::new(RwLock::new(engine)),
            )
            .with_event_bus(Arc::clone(&bus));
            let signal = ConnectorSignal::new(
                "github",
                crate::connector_inbound_bridge::ConnectorSignalKind::Webhook,
                serde_json::json!({ "ref": "refs/heads/main" }),
            )
            .with_sub_type("push")
            .with_correlation_id("ft-7h5da.5.9-runtime-route")
            .with_pane_id(42);

            let result = runtime
                .route_connector_signal(&signal)
                .expect("runtime connector ingress should route signal");

            assert!(!result.deduplicated);
            assert_eq!(result.rule_id, "connector.github:webhook.push");
            assert_eq!(result.delivered_count, 1);

            let cx = crate::cx::for_testing();
            let event = subscriber
                .recv_cx(&cx)
                .await
                .expect("detection subscriber should receive routed connector signal");
            assert!(
                matches!(event, Event::PatternDetected { .. }),
                "expected PatternDetected from connector ingress, got {event:?}"
            );
            if let Event::PatternDetected {
                pane_id, detection, ..
            } = event
            {
                assert_eq!(pane_id, 42);
                assert_eq!(detection.rule_id, "connector.github:webhook.push");
                assert_eq!(detection.event_type, "connector.push");
                assert_eq!(
                    detection
                        .extracted
                        .get("source_connector")
                        .and_then(serde_json::Value::as_str),
                    Some("github")
                );
            }
        });
    }

    #[test]
    fn runtime_inbound_bridge_enforces_operator_classifier_config_ft_pzxsr() {
        use crate::connector_data_classification::{
            ClassificationPolicy, ClassificationRule, ClassifierConfig, DataSensitivity,
        };

        run_async_test(async {
            // Operator policy: `ssn` is Prohibited for "slack". The built-in
            // default policy classifies `ssn` as Restricted and would route
            // the signal (redacted), so a rejection proves the operator
            // config reached the bridge — the exact wiring ft-pzxsr fixed.
            let bridge_config = ConnectorInboundBridgeConfig {
                classifier: ClassifierConfig {
                    policies: vec![ClassificationPolicy {
                        policy_id: "slack-pii".to_string(),
                        connector_pattern: "slack".to_string(),
                        rules: vec![ClassificationRule::new(
                            "ssn-prohibited",
                            DataSensitivity::Prohibited,
                            vec!["ssn".to_string()],
                        )],
                        scan_for_secrets: false,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            };
            let signal = ConnectorSignal::new(
                "slack",
                crate::connector_inbound_bridge::ConnectorSignalKind::Webhook,
                serde_json::json!({ "ssn": "123-45-6789", "note": "hi" }),
            );

            // Order used by the production watcher paths in main.rs:
            // bridge config first, then the event bus.
            let (_dir_a, db_path_a) = temp_db_path();
            let storage_a = StorageHandle::new(&db_path_a).await.unwrap();
            let runtime_a = ObservationRuntime::new(
                RuntimeConfig::default(),
                storage_a,
                Arc::new(RwLock::new(PatternEngine::new())),
            )
            .with_connector_inbound_bridge_config(bridge_config.clone())
            .with_event_bus(Arc::new(EventBus::new(16)));
            let err = runtime_a
                .route_connector_signal(&signal)
                .expect_err("operator prohibited-field policy must reject ingress");
            assert!(matches!(
                err,
                crate::connector_inbound_bridge::ConnectorBridgeError::PrivacyRejected { .. }
            ));

            // Reverse order must behave identically (the setter rebuilds an
            // already-constructed bridge).
            let (_dir_b, db_path_b) = temp_db_path();
            let storage_b = StorageHandle::new(&db_path_b).await.unwrap();
            let runtime_b = ObservationRuntime::new(
                RuntimeConfig::default(),
                storage_b,
                Arc::new(RwLock::new(PatternEngine::new())),
            )
            .with_event_bus(Arc::new(EventBus::new(16)))
            .with_connector_inbound_bridge_config(bridge_config);
            let err = runtime_b
                .route_connector_signal(&signal)
                .expect_err("config applied after with_event_bus must also reject ingress");
            assert!(matches!(
                err,
                crate::connector_inbound_bridge::ConnectorBridgeError::PrivacyRejected { .. }
            ));
        });
    }

    struct StubbornRuntimeTaskDropFlag(Arc<AtomicBool>);

    impl Drop for StubbornRuntimeTaskDropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn stubborn_runtime_task(duration: Duration, dropped: Arc<AtomicBool>) -> JoinHandle<()> {
        let loop_cx = runtime_loop_cx();
        let drop_flag = StubbornRuntimeTaskDropFlag(dropped);
        spawn_runtime_task(&loop_cx, move |_task_cx| async move {
            let _drop_flag = drop_flag;
            sleep(duration).await;
        })
    }

    async fn runtime_handle_with_stubborn_tasks(
        duration: Duration,
    ) -> (TempDir, RuntimeHandle, Vec<Arc<AtomicBool>>) {
        let (dir, db_path) = temp_db_path();
        let storage = StorageHandle::new(&db_path).await.unwrap();
        let engine = PatternEngine::new();
        let runtime = ObservationRuntime::new(
            RuntimeConfig::default(),
            storage,
            Arc::new(RwLock::new(engine)),
        );
        let (capture_tx, _capture_rx) = mpsc::channel(1);
        let dropped = (0..4)
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect::<Vec<_>>();

        let handle = RuntimeHandle {
            discovery: Some(stubborn_runtime_task(duration, Arc::clone(&dropped[0]))),
            capture: Some(stubborn_runtime_task(duration, Arc::clone(&dropped[1]))),
            relay: Some(stubborn_runtime_task(duration, Arc::clone(&dropped[2]))),
            native_events: None,
            persistence: Some(stubborn_runtime_task(duration, Arc::clone(&dropped[3]))),
            maintenance: None,
            connector_outbound: None,
            snapshot: None,
            snapshot_triggers: None,
            snapshot_shutdown: None,
            snapshot_shutdown_clean: None,
            snapshot_engine: None,
            snapshot_scheduler_status: None,
            snapshot_shutdown_requested: None,
            shutdown_flag: Arc::clone(&runtime.shutdown_flag),
            storage: runtime.storage.clone(),
            metrics: Arc::clone(&runtime.metrics),
            registry: Arc::clone(&runtime.registry),
            cursors: Arc::clone(&runtime.cursors),
            pane_activity_tracker: Arc::clone(&runtime.pane_activity_tracker),
            start_time: Instant::now(),
            config_tx: Arc::clone(&runtime.config_tx),
            event_bus: None,
            connector_inbound_bridge: None,
            heartbeats: Arc::clone(&runtime.heartbeats),
            capture_tx,
            capture_queue_capacity: 1,
            wezterm_handle: runtime.wezterm_handle.clone(),
            scheduler_snapshot: Arc::clone(&runtime.scheduler_snapshot),
        };

        (dir, handle, dropped)
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
    fn bocpd_change_point_detection_uses_live_event_contract() {
        let change_point = crate::bocpd::PaneChangePoint {
            pane_id: 42,
            observation_index: 11,
            posterior_probability: 0.75,
            features_at_change: Some(crate::bocpd::OutputFeatures {
                output_rate: 123.0,
                byte_rate: 456.0,
                entropy: 4.5,
                unique_line_ratio: 0.25,
                ansi_density: 0.1,
            }),
            timestamp_secs: 99,
        };

        let detection = bocpd_change_point_to_detection(&change_point);

        assert_eq!(detection.rule_id, BOCPD_CHANGE_POINT_RULE_ID);
        assert_eq!(detection.event_type, BOCPD_CHANGE_POINT_EVENT_TYPE);
        assert_eq!(detection.agent_type, AgentType::Unknown);
        assert_eq!(detection.severity, Severity::Info);
        assert!((detection.confidence - 0.75).abs() < f64::EPSILON);
        assert_eq!(detection.extracted["pane_id"].as_u64(), Some(42));
        assert_eq!(detection.extracted["observation_index"].as_u64(), Some(11));
        assert_eq!(
            detection.extracted["features_at_change"]["output_rate"].as_f64(),
            Some(123.0)
        );
        assert!(!detection.matched_text.contains(&["s", "k", "-"].concat()));
    }

    #[test]
    fn observe_bocpd_segment_drops_empty_gap_without_retaining_pane() {
        let mut manager = crate::bocpd::BocpdManager::new(crate::bocpd::BocpdConfig::default());
        let mut last_capture_at = HashMap::new();
        let seed = test_bocpd_segment(7, "hello\n".to_string(), 1_000);
        assert!(
            observe_bocpd_segment_for_runtime(&mut manager, &mut last_capture_at, &seed, false)
                .is_none()
        );
        assert_eq!(manager.pane_count(), 1);

        let empty_gap = CapturedSegment {
            pane_id: 7,
            seq: 2,
            content: String::new(),
            kind: crate::ingest::CapturedSegmentKind::Gap {
                reason: "pane_closed".to_string(),
            },
            captured_at: 2_000,
        };

        assert!(
            observe_bocpd_segment_for_runtime(
                &mut manager,
                &mut last_capture_at,
                &empty_gap,
                true,
            )
            .is_none()
        );
        assert_eq!(
            manager.pane_count(),
            0,
            "empty teardown gaps must not re-register BOCPD panes"
        );
    }

    #[test]
    fn observe_bocpd_segment_emits_detection_on_synthetic_regime_shift() {
        let mut manager = crate::bocpd::BocpdManager::new(crate::bocpd::BocpdConfig {
            hazard_rate: 0.01,
            detection_threshold: 0.5,
            min_observations: 10,
            max_run_length: 100,
            ..Default::default()
        });
        let mut last_capture_at = HashMap::new();
        let mut captured_at = 0;

        for _ in 0..30 {
            captured_at += 1_000;
            let segment = test_bocpd_segment(9, "stable line\n".to_string(), captured_at);
            let detection = observe_bocpd_segment_for_runtime(
                &mut manager,
                &mut last_capture_at,
                &segment,
                false,
            );
            assert!(
                detection.is_none(),
                "stable warmup must not emit BOCPD change points"
            );
        }

        let mut detected = None;
        for _ in 0..10 {
            captured_at += 1_000;
            let segment = test_bocpd_segment(9, "shift\n".repeat(1_000), captured_at);
            detected = observe_bocpd_segment_for_runtime(
                &mut manager,
                &mut last_capture_at,
                &segment,
                false,
            );
            if detected.is_some() {
                break;
            }
        }

        let detection = detected.expect("synthetic output-rate shift should emit a change point");
        assert_eq!(detection.event_type, BOCPD_CHANGE_POINT_EVENT_TYPE);
        assert_eq!(detection.severity, Severity::Info);
        assert!(
            detection.confidence >= 0.5,
            "detection confidence should reflect threshold-crossing posterior"
        );
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

    #[test]
    fn detection_to_stored_event_redacts_secret_payloads_before_storage() {
        use crate::patterns::{AgentType, Severity};

        let redaction_prefix = ["s", "k", "-proj-"].concat();
        let redaction_sample = [
            redaction_prefix.as_str(),
            "abcdefghijklmnopqrstuvwxyz12345678901234567890",
        ]
        .concat();
        let detection = Detection {
            rule_id: "test.redaction".to_string(),
            agent_type: AgentType::Codex,
            event_type: "redaction.detected".to_string(),
            severity: Severity::Warning,
            confidence: 0.99,
            extracted: serde_json::json!({
                "nested": {
                    "sample_value": redaction_sample,
                    "safe": "ordinary"
                },
                "array": [
                    format!("sample={redaction_sample}"),
                    42,
                    true
                ]
            }),
            matched_text: format!("sample={redaction_sample}"),
            span: (0, 1),
        };

        let event = detection_to_stored_event(7, Some("pane-redaction"), &detection, Some(11));

        let matched_text = event
            .matched_text
            .as_deref()
            .expect("matched_text persisted");
        assert!(!matched_text.contains(&redaction_sample));
        assert!(matched_text.contains(crate::redactor::REDACTED_MARKER));

        let extracted = event.extracted.expect("extracted persisted");
        let rendered = extracted.to_string();
        assert!(!rendered.contains(&redaction_sample));
        assert!(rendered.contains(crate::redactor::REDACTED_MARKER));
        assert_eq!(extracted["nested"]["safe"], "ordinary");
        assert_eq!(extracted["array"][1].as_i64(), Some(42));
        assert_eq!(extracted["array"][2].as_bool(), Some(true));
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
    fn snapshot_trigger_from_detection_maps_bocpd_change_point() {
        let detection = test_detection(BOCPD_CHANGE_POINT_EVENT_TYPE, Severity::Info);
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

            let config = RuntimeConfig {
                discovery_interval: Duration::from_millis(10),
                capture_interval: Duration::from_millis(10),
                min_capture_interval: Duration::from_millis(5),
                channel_buffer: 64,
                ..Default::default()
            };

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
                summary.is_clean(),
                "shutdown should complete cleanly: {:?}",
                summary.warnings
            );
            assert!(
                summary.warnings.is_empty(),
                "shutdown should not emit warnings for mock lifecycle: {:?}",
                summary.warnings
            );
            assert!(
                summary.managed_queue_quiescence_proven(),
                "a fully clean zero-depth shutdown should prove managed-queue quiescence"
            );
        });
    }

    #[test]
    fn runtime_builder_snapshot_session_is_closed_after_scheduler_settles() {
        run_async_test_isolated(|| async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let patterns = PatternEngine::new();
            let config = RuntimeConfig {
                discovery_interval: Duration::from_millis(10),
                capture_interval: Duration::from_millis(10),
                min_capture_interval: Duration::from_millis(5),
                channel_buffer: 64,
                ..Default::default()
            };
            let snapshot_config = SnapshotConfig {
                enabled: true,
                interval_seconds: 3600,
                scheduling: crate::config::SnapshotSchedulingConfig {
                    mode: SnapshotSchedulingMode::Periodic,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mock = crate::wezterm::MockWezterm::new();
            mock.add_default_pane(0).await;
            let wezterm_handle: WeztermHandle = Arc::new(mock);
            let mut runtime = ObservationRuntime::new(
                config,
                storage,
                Arc::new(RwLock::new(patterns)),
            )
            .with_wezterm_handle(wezterm_handle)
            .with_snapshot_config(snapshot_config);

            let handle = runtime.start().await.expect("runtime should start");
            let startup_deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let checkpoint_exists = rusqlite::Connection::open(&db_path)
                    .and_then(|connection| {
                        connection.query_row(
                            "SELECT EXISTS(SELECT 1 FROM session_checkpoints)",
                            [],
                            |row| row.get::<_, bool>(0),
                        )
                    })
                    .unwrap_or(false);
                if checkpoint_exists {
                    break;
                }
                assert!(
                    Instant::now() < startup_deadline,
                    "snapshot scheduler did not publish its startup checkpoint"
                );
                sleep(Duration::from_millis(20)).await;
            }

            let summary = handle.shutdown_with_summary().await;
            assert!(
                summary.is_clean(),
                "runtime shutdown should settle cleanly: {:?}",
                summary.warnings
            );
            let connection = rusqlite::Connection::open(&db_path).unwrap();
            let (sessions, clean_sessions): (i64, i64) = connection
                .query_row(
                    "SELECT COUNT(*), COALESCE(SUM(shutdown_clean), 0) FROM mux_sessions",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert!(sessions > 0, "startup must create a snapshot session");
            assert_eq!(
                clean_sessions, sessions,
                "every RuntimeBuilder-owned snapshot session must close cleanly"
            );
        });
    }

    #[test]
    fn runtime_builder_empty_domain_persists_terminal_snapshot_receipt() {
        run_async_test_isolated(|| async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let patterns = PatternEngine::new();
            let config = RuntimeConfig {
                discovery_interval: Duration::from_millis(10),
                capture_interval: Duration::from_millis(10),
                min_capture_interval: Duration::from_millis(5),
                channel_buffer: 64,
                ..Default::default()
            };
            let snapshot_config = SnapshotConfig {
                enabled: true,
                interval_seconds: 3600,
                scheduling: crate::config::SnapshotSchedulingConfig {
                    mode: SnapshotSchedulingMode::Periodic,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut runtime = ObservationRuntime::new(
                config,
                storage,
                Arc::new(RwLock::new(patterns)),
            )
            .with_wezterm_handle(Arc::new(crate::wezterm::MockWezterm::new()))
            .with_snapshot_config(snapshot_config);

            let handle = runtime.start().await.expect("runtime should start");
            let summary = handle.shutdown_with_summary().await;
            assert!(
                summary.is_clean(),
                "empty-domain shutdown should settle cleanly: {:?}",
                summary.warnings
            );

            let connection = rusqlite::Connection::open(&db_path).unwrap();
            let (sessions, clean_sessions): (i64, i64) = connection
                .query_row(
                    "SELECT COUNT(*), COALESCE(SUM(shutdown_clean), 0) FROM mux_sessions",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(sessions, 1, "terminal checkpoint must create one session");
            assert_eq!(
                clean_sessions, sessions,
                "the empty-domain terminal session must carry a clean-mark receipt"
            );
            let empty_shutdown_checkpoints: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM session_checkpoints WHERE checkpoint_type = 'shutdown' AND pane_count = 0",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                empty_shutdown_checkpoints, 1,
                "empty-domain shutdown must persist an explicit zero-pane checkpoint"
            );
        });
    }

    #[test]
    fn runtime_shutdown_phases_do_not_inherit_cancelled_caller_context() {
        run_async_test_isolated(|| async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let engine = PatternEngine::new();
            let config = RuntimeConfig {
                discovery_interval: Duration::from_millis(10),
                capture_interval: Duration::from_millis(10),
                min_capture_interval: Duration::from_millis(5),
                channel_buffer: 64,
                ..Default::default()
            };
            let mut runtime = ObservationRuntime::new(
                config,
                storage,
                Arc::new(RwLock::new(engine)),
            )
            .with_wezterm_handle(Arc::new(crate::wezterm::MockWezterm::new()));
            let handle = runtime.start().await.expect("runtime should start");

            let caller_cx = crate::cx::for_testing();
            caller_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("cancel caller before mandatory runtime cleanup"),
            );
            let summary = handle
                .shutdown_with_timeout_with_cx(&caller_cx, Duration::from_secs(2))
                .await;

            assert!(
                summary.is_clean(),
                "fresh bounded cleanup contexts must complete despite caller cancellation: {:?}",
                summary.warnings
            );
            assert!(
                summary.warnings.is_empty(),
                "caller cancellation must not poison independent cleanup phases: {:?}",
                summary.warnings
            );
        });
    }

    #[test]
    fn runtime_shutdown_timeout_reports_stubborn_tasks_without_hanging() {
        run_async_test_isolated(|| async {
            let (_dir, handle, dropped) =
                runtime_handle_with_stubborn_tasks(Duration::from_millis(300)).await;
            // Pin a non-zero aggregate queue observation. The synthetic
            // stubborn tasks do not mutate runtime metrics, so shutdown must
            // preserve this observed depth rather than manufacture zero.
            handle.metrics.record_capture_queue_depth(7);

            let started = Instant::now();
            let summary = handle
                .shutdown_with_timeout(Duration::from_millis(50))
                .await;
            let elapsed = started.elapsed();

            assert!(
                !summary.is_clean(),
                "stubborn tasks should make shutdown summary unclean"
            );
            assert_eq!(
                summary.final_capture_queue(), 7,
                "shutdown summary must report the observed capture queue depth"
            );
            assert!(
                !summary.managed_queue_quiescence_proven(),
                "non-zero or unclean queue state must not claim proven quiescence"
            );
            assert!(
                summary.warnings.iter().any(|warning| {
                    warning.contains("7 capture queue item(s) still observed")
                }),
                "non-zero terminal capture depth must remain operator-visible: {:?}",
                summary.warnings
            );
            assert!(
                summary
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("timeout")),
                "shutdown summary should report timeout warning: {:?}",
                summary.warnings
            );
            assert!(
                elapsed < Duration::from_secs(1),
                "bounded shutdown should return promptly, took {elapsed:?}"
            );
            assert!(
                dropped.iter().all(|flag| flag.load(Ordering::SeqCst)),
                "shutdown timeout must abort and terminally settle every retained task future"
            );
        });
    }

    #[test]
    fn runtime_shutdown_observes_already_aborted_task_results() {
        run_async_test_isolated(|| async {
            let (_dir, handle, dropped) =
                runtime_handle_with_stubborn_tasks(Duration::from_secs(30)).await;
            for task in [
                handle.discovery.as_ref(),
                handle.capture.as_ref(),
                handle.relay.as_ref(),
                handle.persistence.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                task.abort();
            }

            let summary = handle.shutdown_with_timeout(Duration::from_secs(2)).await;

            assert!(
                !summary.is_clean(),
                "pre-aborted top-level task results must make shutdown unclean"
            );
            assert_eq!(
                (
                    summary.final_capture_queue(),
                    summary.final_write_queue(),
                ),
                (0, 0),
                "regression precondition: this path must exercise observed-zero but unclean shutdown"
            );
            assert!(
                !summary.managed_queue_quiescence_proven(),
                "observed zeroes must not become a proof after an unclean task-settlement phase"
            );
            assert!(
                summary
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("Managed queue quiescence was not proven")),
                "unknown zero-depth state must remain operator-visible: {:?}",
                summary.warnings
            );
            assert!(
                summary
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("top-level runtime task")),
                "shutdown must report observed task failures: {:?}",
                summary.warnings
            );
            assert!(
                dropped.iter().all(|flag| flag.load(Ordering::SeqCst)),
                "every pre-aborted task future must be terminally dropped"
            );
        });
    }

    #[test]
    fn runtime_shutdown_trusted_drains_persistent_join_registration_failure() {
        run_async_test_isolated(|| async {
            let (_dir, handle, dropped) =
                runtime_handle_with_stubborn_tasks(Duration::from_secs(30)).await;
            handle
                .discovery
                .as_ref()
                .expect("test runtime must own a discovery task")
                .force_registration_failure_for_test();

            let summary = handle.shutdown_with_timeout(Duration::from_secs(2)).await;

            assert!(
                !summary.is_clean(),
                "join observation failure must remain visible even after trusted settlement"
            );
            assert!(
                summary
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("top-level runtime task join failure")),
                "shutdown must report the finite observation failure: {:?}",
                summary.warnings
            );
            assert!(
                summary.warnings.iter().any(|warning| {
                    warning.contains("caller-waker registration failure")
                        && warning.contains("trusted quarantine")
                }),
                "the regression must exercise quarantine rather than only an immediately terminal abort: {:?}",
                summary.warnings
            );
            assert!(
                dropped.iter().all(|flag| flag.load(Ordering::SeqCst)),
                "trusted drain must retain and terminally settle every top-level task"
            );
            assert!(
                summary
                    .warnings
                    .iter()
                    .all(|warning| !warning.contains("orphan risk remains")),
                "terminally settled registration failure must not be mislabeled as orphan risk: {:?}",
                summary.warnings
            );
        });
    }

    #[test]
    fn runtime_shutdown_summary_rejects_missing_snapshot_close_receipt() {
        run_async_test_isolated(|| async {
            let (_dir, mut handle, _dropped) =
                runtime_handle_with_stubborn_tasks(Duration::ZERO).await;
            handle.snapshot_shutdown_clean = Some(Arc::new(AtomicBool::new(false)));

            let summary = handle.shutdown_with_timeout(Duration::from_secs(2)).await;
            assert!(
                !summary.is_clean(),
                "missing snapshot close authority must make the runtime summary unclean"
            );
            assert!(
                summary.warnings.iter().any(|warning| warning.contains(
                    "snapshot session did not publish both final-checkpoint and clean-mark receipts"
                )),
                "missing typed receipt must remain operator-visible: {:?}",
                summary.warnings
            );
        });
    }

    #[test]
    fn snapshot_scheduler_clean_exit_requires_explicit_shutdown_acknowledgement() {
        assert!(snapshot_scheduler_shutdown_acknowledged(
            SNAPSHOT_SCHEDULER_SHUTDOWN_ACKNOWLEDGED
        ));
        assert!(!snapshot_scheduler_shutdown_acknowledged(
            SNAPSHOT_SCHEDULER_RUNNING
        ));
        assert!(!snapshot_scheduler_shutdown_acknowledged(
            SNAPSHOT_SCHEDULER_UNEXPECTED_RETURN
        ));
        assert!(!snapshot_scheduler_shutdown_acknowledged(
            SNAPSHOT_SCHEDULER_FAILED
        ));
    }

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn native_event_listener_error_class_is_finite_and_content_free() {
        let cases = [
            (
                crate::native_events::NativeEventError::SocketAlreadyExists(
                    "SECRET socket path that must not enter runtime telemetry".to_string(),
                ),
                "socket_already_exists",
            ),
            (
                crate::native_events::NativeEventError::Io(std::io::Error::other(
                    "SECRET transport detail that must not enter runtime telemetry",
                )),
                "io_failure",
            ),
            (
                crate::native_events::NativeEventError::ConnectionTaskAdmissionFailed,
                "connection_task_admission_failed",
            ),
            (
                crate::native_events::NativeEventError::ConnectionTaskDrainTimedOut,
                "connection_task_drain_timeout",
            ),
            (
                crate::native_events::NativeEventError::ConnectionTaskDrainIncomplete,
                "connection_task_drain_incomplete",
            ),
        ];

        for (error, expected) in cases {
            let class = ObservationRuntime::native_event_listener_error_class(&error);
            assert_eq!(class, expected);
            assert!(!class.contains("SECRET"));
            assert!(class.len() <= 40, "telemetry class must remain finite");
        }
    }

    #[test]
    fn runtime_with_replay_capture_adapter_shuts_down_cleanly() {
        run_async_test_isolated(|| async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let engine = PatternEngine::new();

            let config = RuntimeConfig {
                discovery_interval: Duration::from_millis(10),
                capture_interval: Duration::from_millis(10),
                min_capture_interval: Duration::from_millis(5),
                channel_buffer: 64,
                ..Default::default()
            };

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
                summary.is_clean(),
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
            swarm_capacity: None,
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
        let lease = test_capture_lease(1, CaptureSourceKind::NativePush);

        for (bytes, timestamp_ms, now_ms) in [
            (b"a".to_vec(), 1_000, 0),
            (b"b".to_vec(), 1_001, 10),
            (b"c".to_vec(), 1_002, 20),
        ] {
            let guard = lease
                .try_acquire_producer(lease.stamp(), 1)
                .expect("native producer guard");
            assert!(
                c.push(1, bytes, timestamp_ms, now_ms, guard).is_none()
            );
        }

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
        let lease = test_capture_lease(7, CaptureSourceKind::NativePush);
        let guard = lease
            .try_acquire_producer(lease.stamp(), 7)
            .expect("native producer guard");
        c.push(7, b"x".to_vec(), 555, 0, guard);

        // Not due by window, but due by max_delay.
        assert!(c.drain_due(199).is_empty());
        let drained = c.drain_due(200);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].pane_id, 7);
    }

    #[test]
    fn backpressure_warn_ratio_is_valid() {
        const {
            assert!(BACKPRESSURE_WARN_RATIO > 0.0);
            assert!(BACKPRESSURE_WARN_RATIO < 1.0);
        }
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

    /// ft-0kdi9: the full Observed -> Ignored -> Observed round trip at the
    /// runtime-state layer. Compaction drops the cursor while the pane is
    /// unobserved (this part always worked); the resume must put it back at the
    /// right sequence number (this part did not exist, so capture stayed dead).
    #[test]
    fn ft_0kdi9_compaction_then_resume_restores_capture_cursor_at_next_seq() {
        let mut cursors = HashMap::from([(7_u64, PaneCursor::from_seq(7, 42))]);
        let mut detection_contexts = HashMap::from([(7_u64, DetectionContext::new())]);
        let mut pane_activity_tracker: HashMap<u64, PaneActivityState> = HashMap::new();

        // Pane 7's title starts matching an exclude rule: it is no longer in
        // the observed set, so the discovery tick compacts its state away.
        let stats = compact_runtime_pane_state(
            &mut cursors,
            &mut detection_contexts,
            &mut pane_activity_tracker,
            &HashSet::new(),
        );
        assert_eq!(stats.cursors.removed_entries, 1);
        assert!(!cursors.contains_key(&7));
        assert!(!detection_contexts.contains_key(&7));

        // The title reverts. The registry retired next_seq=42 and storage holds
        // seq 41, so both records agree the next segment is 42.
        let next_seq = resumed_capture_next_seq(Some(41), 42);
        let created = resume_runtime_pane_state(
            7,
            next_seq,
            "already stored tail\n".to_string(),
            &mut cursors,
            &mut detection_contexts,
        );

        assert!(created, "a compacted pane must get a fresh cursor");
        assert_eq!(
            cursors.get(&7).map(|cursor| cursor.next_seq),
            Some(42),
            "capture must resume where it left off, not restart at 0"
        );
        assert_eq!(
            detection_contexts.get(&7).and_then(|ctx| ctx.pane_id),
            Some(7),
            "the detection context compaction dropped must be rebuilt too"
        );
        // ft-6lso5: the rebuilt cursor has no snapshot baseline, so it must
        // carry an anchor into already-persisted output or the next capture
        // re-emits the pane's whole scrollback.
        assert!(
            cursors.get(&7).is_some_and(PaneCursor::has_resume_anchor),
            "a rebuilt cursor must be anchored against stored output"
        );
    }

    /// ft-0kdi9: resume must never rewind a cursor that is still live, which is
    /// what happens when the Ignored window closes inside a single discovery
    /// interval and no compaction ran in between.
    #[test]
    fn ft_0kdi9_resume_leaves_a_live_cursor_untouched() {
        let mut cursors = HashMap::from([(7_u64, PaneCursor::from_seq(7, 99))]);
        let mut detection_contexts: HashMap<u64, DetectionContext> = HashMap::new();

        let created = resume_runtime_pane_state(
            7,
            5,
            "tail\n".to_string(),
            &mut cursors,
            &mut detection_contexts,
        );

        assert!(!created);
        assert_eq!(
            cursors.get(&7).map(|cursor| cursor.next_seq),
            Some(99),
            "rewinding a live cursor would re-emit persisted sequence numbers"
        );
        assert!(
            detection_contexts.contains_key(&7),
            "a missing detection context is still rebuilt"
        );
    }

    /// ft-6lso5: the resume anchor is the tail of persisted output in capture
    /// order. `get_segments` returns newest-first, so assembling it in the
    /// returned order would produce text that never appears in the pane and the
    /// anchor would never match.
    #[test]
    fn ft_6lso5_assemble_resume_anchor_restores_capture_order() {
        fn segment(seq: u64, content: &str) -> crate::storage::Segment {
            crate::storage::Segment {
                id: i64::try_from(seq).expect("test seq fits i64"),
                pane_id: 3,
                seq,
                content: content.to_string(),
                content_len: content.len(),
                content_hash: None,
                captured_at: 0,
            }
        }

        // Newest-first, as the storage query returns them.
        let segments = vec![
            segment(2, "third\n"),
            segment(1, "second\n"),
            segment(0, "first\n"),
        ];

        assert_eq!(
            assemble_resume_anchor(segments),
            "first\nsecond\nthird\n",
            "anchor must read in capture order to match the pane's scrollback"
        );
        assert_eq!(assemble_resume_anchor(Vec::new()), "");
    }

    /// ft-6lso5: the anchor is bounded, and truncation keeps the *trailing*
    /// bytes — the newest output is what the next capture will still show.
    #[test]
    fn ft_6lso5_assemble_resume_anchor_keeps_bounded_tail() {
        let big = "x".repeat(crate::ingest::RESUME_ANCHOR_BYTES * 3);
        let segments = vec![crate::storage::Segment {
            id: 1,
            pane_id: 3,
            seq: 0,
            content: format!("{big}TAIL"),
            content_len: big.len() + 4,
            content_hash: None,
            captured_at: 0,
        }];

        let anchor = assemble_resume_anchor(segments);
        assert!(anchor.len() <= crate::ingest::RESUME_ANCHOR_BYTES);
        assert!(
            anchor.ends_with("TAIL"),
            "truncation must keep the newest output"
        );
    }

    /// ft-0kdi9: the resume sequence number is the max of both surviving
    /// records, because either one can be the further-along value.
    #[test]
    fn ft_0kdi9_resumed_capture_next_seq_takes_the_further_along_record() {
        // Nothing persisted, nothing retired: start at the beginning.
        assert_eq!(resumed_capture_next_seq(None, 0), 0);
        // Storage leads: the registry's retired value lagged by an interval.
        assert_eq!(resumed_capture_next_seq(Some(41), 0), 42);
        // The registry leads: segments were captured but not yet flushed.
        assert_eq!(resumed_capture_next_seq(None, 7), 7);
        assert_eq!(resumed_capture_next_seq(Some(41), 50), 50);
        // Equal records agree.
        assert_eq!(resumed_capture_next_seq(Some(41), 42), 42);
        // Saturation must not wrap to 0 and hand back a colliding seq.
        assert_eq!(resumed_capture_next_seq(Some(u64::MAX), 0), u64::MAX);
    }

    #[test]
    fn ft_l6v1r_pane_cleanup_removes_destroyed_pane_from_runtime_maps() {
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

        let removed = remove_runtime_pane_state(
            2,
            &mut cursors,
            &mut detection_contexts,
            &mut pane_activity_tracker,
        );

        assert_eq!(
            removed,
            RuntimePaneStateRemoval {
                cursor_removed: true,
                detection_context_removed: true,
                pane_activity_removed: true,
            }
        );
        assert!(cursors.contains_key(&1));
        assert!(!cursors.contains_key(&2));
        assert!(detection_contexts.contains_key(&1));
        assert!(!detection_contexts.contains_key(&2));
        assert!(pane_activity_tracker.contains_key(&1));
        assert!(!pane_activity_tracker.contains_key(&2));
    }

    #[test]
    fn correlated_terminal_cleanup_batches_runtime_map_locks() {
        run_async_test(async {
            let cursors = Arc::new(RwLock::new(HashMap::from([
                (1_u64, PaneCursor::from_seq(1, 4)),
                (2_u64, PaneCursor::from_seq(2, 9)),
                (3_u64, PaneCursor::from_seq(3, 12)),
            ])));
            let contexts = Arc::new(RwLock::new(HashMap::from([
                (1_u64, DetectionContext::new()),
                (2_u64, DetectionContext::new()),
                (3_u64, DetectionContext::new()),
            ])));
            let activity = Arc::new(RwLock::new(HashMap::from([
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
                (
                    3_u64,
                    PaneActivityState {
                        last_seq: 12,
                        last_output_at_ms: 3_000,
                        generation: 1,
                        first_seen_at_ms: 3_000,
                    },
                ),
            ])));

            remove_runtime_pane_state_for_panes(
                &[2, 3, 2],
                &cursors,
                &contexts,
                &activity,
            )
            .await;

            assert_eq!(
                cursors.read().await.keys().copied().collect::<Vec<_>>(),
                vec![1],
            );
            assert_eq!(
                contexts.read().await.keys().copied().collect::<Vec<_>>(),
                vec![1],
            );
            assert_eq!(
                activity.read().await.keys().copied().collect::<Vec<_>>(),
                vec![1],
            );
        });
    }

    /// [ft-pp7jk] `handle_native_event` on `NativeEvent::PaneDestroyed` must
    /// publish `Event::PaneDisappeared` when an `event_bus` is present.
    ///
    /// The event variant was declared in events.rs:181 and consumers
    /// (event_stream filters, wire_protocol serializer, main.rs handler)
    /// were already matching on it — but no production path ever emitted
    /// it. Every subscriber remained dormant forever, leaving every
    /// long-lived per-pane cache (policy rate-limiter state, connector
    /// bridge caches, future subsystems) unable to release state on
    /// pane destroy.
    ///
    /// Pin the fix: feed a PaneDestroyed event with an attached bus,
    /// assert the subscriber receives a matching PaneDisappeared, and retain
    /// coordinator-owned runtime state until exact capture retirement.
    #[cfg(feature = "native-wezterm")]
    #[test]
    fn ft_pp7jk_pane_destroyed_publishes_pane_disappeared_event() {
        run_async_test(async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let bus = Arc::new(crate::events::EventBus::new(16));
            let mut subscriber = bus.subscribe();

            let (capture_tx, _capture_rx) = mpsc::channel::<CaptureEvent>(4);
            let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::from([(
                42_u64,
                PaneCursor::from_seq(42, 0),
            )])));
            let detection_contexts = Arc::new(RwLock::new(HashMap::<u64, DetectionContext>::from(
                [(42_u64, DetectionContext::new())],
            )));
            let pane_activity_tracker =
                Arc::new(RwLock::new(HashMap::<u64, PaneActivityState>::from([(
                    42_u64,
                    PaneActivityState {
                        last_seq: 0,
                        last_output_at_ms: 0,
                        generation: 1,
                        first_seen_at_ms: 0,
                    },
                )])));
            let pane_filter = PaneFilterConfig::default();
            let backpressure = Arc::new(BackpressureMetrics::default());
            backpressure.record_segment_dropped(42);
            assert_eq!(backpressure.panes_with_drops(), 1);
            let capture_authority = CaptureAuthority::new();
            let pane_identity = capture_authority
                .activate_pane(42)
                .expect("native test pane authority");
            let _native_lease = capture_authority
                .issue_source(pane_identity, CaptureSourceKind::NativePush)
                .expect("native test source authority");
            let metrics = RuntimeMetrics::default();
            let runtime_cx = runtime_loop_cx();

            handle_native_event(
                &runtime_cx,
                NativeEvent::PaneDestroyed {
                    pane_id: 42,
                    timestamp_ms: 1_000,
                },
                &capture_tx,
                &cursors,
                &storage,
                Some(&bus),
                &pane_filter,
                &backpressure,
                &capture_authority,
                &metrics,
            )
            .await;

            assert!(
                cursors.read().await.contains_key(&42)
                    && detection_contexts.read().await.contains_key(&42)
                    && pane_activity_tracker.read().await.contains_key(&42),
                "native lifecycle input must not tear down exact capture-owned state"
            );
            assert_eq!(
                backpressure.panes_with_drops(),
                0,
                "native lifecycle input preserves prompt attribution cleanup"
            );

            let cx = crate::cx::for_testing();
            let received = subscriber
                .recv_cx(&cx)
                .await
                .expect("bus subscriber should receive PaneDisappeared after PaneDestroyed");
            assert!(
                matches!(received, Event::PaneDisappeared { pane_id: 42 }),
                "expected Event::PaneDisappeared{{pane_id:42}}, got {received:?}"
            );
        });
    }

    /// [ft-pp7jk] Companion to the bus-present test: with `event_bus =
    /// None`, `handle_native_event` on PaneDestroyed must preserve
    /// capture-owned per-pane state and must not panic, emit, or otherwise
    /// require the bus.
    #[cfg(feature = "native-wezterm")]
    #[test]
    fn ft_pp7jk_pane_destroyed_without_bus_defers_capture_teardown() {
        run_async_test(async {
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();

            let (capture_tx, _capture_rx) = mpsc::channel::<CaptureEvent>(4);
            let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::from([(
                7_u64,
                PaneCursor::from_seq(7, 0),
            )])));
            let detection_contexts = Arc::new(RwLock::new(HashMap::<u64, DetectionContext>::from(
                [(7_u64, DetectionContext::new())],
            )));
            let pane_activity_tracker =
                Arc::new(RwLock::new(HashMap::<u64, PaneActivityState>::from([(
                    7_u64,
                    PaneActivityState {
                        last_seq: 0,
                        last_output_at_ms: 0,
                        generation: 1,
                        first_seen_at_ms: 0,
                    },
                )])));
            let pane_filter = PaneFilterConfig::default();
            let backpressure = Arc::new(BackpressureMetrics::default());
            backpressure.record_segment_dropped(7);
            assert_eq!(backpressure.panes_with_drops(), 1);
            let capture_authority = CaptureAuthority::new();
            let pane_identity = capture_authority
                .activate_pane(7)
                .expect("native test pane authority");
            let _native_lease = capture_authority
                .issue_source(pane_identity, CaptureSourceKind::NativePush)
                .expect("native test source authority");
            let metrics = RuntimeMetrics::default();
            let runtime_cx = runtime_loop_cx();

            handle_native_event(
                &runtime_cx,
                NativeEvent::PaneDestroyed {
                    pane_id: 7,
                    timestamp_ms: 500,
                },
                &capture_tx,
                &cursors,
                &storage,
                None,
                &pane_filter,
                &backpressure,
                &capture_authority,
                &metrics,
            )
            .await;

            assert!(cursors.read().await.contains_key(&7));
            assert!(detection_contexts.read().await.contains_key(&7));
            assert!(pane_activity_tracker.read().await.contains_key(&7));
            assert_eq!(backpressure.panes_with_drops(), 0);
        });
    }

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn native_event_precancel_has_no_cursor_queue_or_lifecycle_side_effects() {
        run_async_test(async {
            let pane_id = 73;
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let bus = Arc::new(crate::events::EventBus::new(8));
            let mut subscriber = bus.subscribe();
            let (capture_tx, mut capture_rx) = mpsc::channel::<CaptureEvent>(4);
            let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::from([(
                pane_id,
                PaneCursor::from_seq(pane_id, 0),
            )])));
            let pane_filter = PaneFilterConfig::default();
            let backpressure = Arc::new(BackpressureMetrics::default());
            backpressure.record_segment_dropped(pane_id);
            let capture_authority = CaptureAuthority::new();
            let pane_identity = capture_authority
                .activate_pane(pane_id)
                .expect("native test pane authority");
            let _native_lease = capture_authority
                .issue_source(pane_identity, CaptureSourceKind::NativePush)
                .expect("native test source authority");
            let metrics = RuntimeMetrics::default();
            let runtime_cx = crate::cx::for_testing();
            runtime_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("SECRET native event cancellation"),
            );

            handle_native_event(
                &runtime_cx,
                NativeEvent::PaneOutput {
                    pane_id,
                    data: b"must not be captured".to_vec(),
                    timestamp_ms: 1_000,
                    dropped_bytes: 0,
                },
                &capture_tx,
                &cursors,
                &storage,
                Some(&bus),
                &pane_filter,
                &backpressure,
                &capture_authority,
                &metrics,
            )
            .await;
            handle_native_event(
                &runtime_cx,
                NativeEvent::PaneDestroyed {
                    pane_id,
                    timestamp_ms: 1_001,
                },
                &capture_tx,
                &cursors,
                &storage,
                Some(&bus),
                &pane_filter,
                &backpressure,
                &capture_authority,
                &metrics,
            )
            .await;

            assert_eq!(
                cursors.read().await.get(&pane_id).map(PaneCursor::last_seq),
                Some(-1),
                "pre-cancelled native output must not advance the cursor"
            );
            assert!(
                capture_rx.try_recv().is_err(),
                "pre-cancelled native output must not enqueue capture work"
            );
            assert_eq!(
                backpressure.panes_with_drops(),
                1,
                "pre-cancelled pane destruction must not mutate lifecycle metrics"
            );
            assert!(
                subscriber.try_recv().is_none(),
                "pre-cancelled pane destruction must not publish a lifecycle event"
            );
        });
    }

    #[cfg(feature = "native-wezterm")]
    #[test]
    fn native_pane_created_persists_identity_without_initializing_capture_state() {
        run_async_test(async {
            let pane_id = 23;
            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.unwrap();
            let (capture_tx, _capture_rx) = mpsc::channel::<CaptureEvent>(4);
            let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
            let pane_filter = PaneFilterConfig::default();
            let backpressure = Arc::new(BackpressureMetrics::default());
            let capture_authority = CaptureAuthority::new();
            let metrics = RuntimeMetrics::default();
            let runtime_cx = runtime_loop_cx();

            handle_native_event(
                &runtime_cx,
                NativeEvent::PaneCreated {
                    pane_id,
                    domain: "local".to_string(),
                    cwd: Some("/tmp/native-created".to_string()),
                    timestamp_ms: 1_234,
                },
                &capture_tx,
                &cursors,
                &storage,
                None,
                &pane_filter,
                &backpressure,
                &capture_authority,
                &metrics,
            )
            .await;

            let record = storage
                .get_pane_with_cx(&runtime_cx, pane_id)
                .await
                .expect("read native pane record")
                .expect("native pane identity is durable");
            assert_eq!(record.domain, "local");
            assert_eq!(record.cwd.as_deref(), Some("/tmp/native-created"));
            assert_eq!(record.first_seen_at, 1_234);
            assert_eq!(record.last_seen_at, 1_234);
            assert!(record.observed);
            assert_eq!(
                metrics.capture_authority_rejections(),
                0,
                "lifecycle-only native events must not require capture producer authority"
            );
            assert!(
                !cursors.read().await.contains_key(&pane_id),
                "only post-drain capture reconciliation may initialize a cursor"
            );
        });
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
    fn mpsc_sender_capacity_is_not_queue_depth() {
        // asupersync Sender::capacity reports fixed channel capacity, not
        // remaining capacity. Runtime queue depth must come from receiver-side
        // observation published through RuntimeMetrics.
        let (tx, _rx) = mpsc::channel::<u8>(16);
        assert_eq!(mpsc_max_capacity(&tx), 16);
        assert_eq!(tx.capacity(), 16);
    }

    #[test]
    fn runtime_metrics_records_capture_queue_depth() {
        let metrics = RuntimeMetrics::default();

        assert_eq!(metrics.capture_queue_depth(), 0);
        metrics.record_capture_queue_depth(3);
        assert_eq!(metrics.capture_queue_depth(), 3);
        metrics.record_capture_queue_depth(1);
        assert_eq!(metrics.capture_queue_depth(), 1);
    }

    #[test]
    fn capture_pipeline_depth_includes_ingress_ring_and_relay_slot() {
        let metrics = RuntimeMetrics::default();
        let (ingress_tx, ingress_rx) = mpsc::channel(4);
        let (ring_tx, _ring_rx) = spsc_channel(4);

        ingress_tx.try_send(test_capture_event(1)).unwrap();
        ring_tx.try_send(test_capture_event(2)).unwrap();

        record_capture_pipeline_depth(&metrics, &ingress_rx, &ring_tx, 1);

        assert_eq!(metrics.capture_queue_depth(), 3);
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
        fetch.note_error(7);
        fetch.note_error(9);

        assert!(fetch.telemetry_blind(2));
        assert!(!fetch.telemetry_partial(2));
        assert_eq!(
            fetch.error_samples(),
            [
                "pane 7: summary_unavailable".to_string(),
                "pane 9: summary_unavailable".to_string(),
            ]
        );
    }

    #[test]
    fn pane_tiered_scrollback_fetch_marks_partial_when_some_samples_succeed() {
        let mut fetch = PaneTieredScrollbackFetch::default();
        fetch
            .summaries
            .insert(11, PaneTieredScrollbackSummary::default());
        fetch.note_error(12);

        assert!(!fetch.telemetry_blind(2));
        assert!(fetch.telemetry_partial(2));
    }

    #[test]
    fn pane_tiered_scrollback_fetch_never_reflects_backend_error_text() {
        let secret = "backend-secret-that-must-not-enter-maintenance-metadata";
        let mut fetch = PaneTieredScrollbackFetch::default();
        record_pane_tiered_scrollback_summary_result(
            &mut fetch,
            7,
            Err(crate::Error::Wezterm(
                crate::error::WeztermError::CommandFailed(secret.to_string()),
            )),
        );

        assert!(fetch.telemetry_blind(1));
        assert!(!fetch.telemetry_partial(1));
        let error_samples = fetch.error_samples();
        assert_eq!(
            error_samples,
            vec!["pane 7: summary_unavailable".to_string()]
        );
        assert!(!error_samples[0].contains(secret));
    }

    #[test]
    fn pane_tiered_scrollback_error_samples_are_bounded_and_order_stable() {
        let mut fetch = PaneTieredScrollbackFetch::default();
        for pane_id in [90, 7, 42, 3, 11, 1, 7] {
            fetch.note_error(pane_id);
        }

        assert_eq!(fetch.errors, 7);
        assert_eq!(
            fetch.error_samples(),
            [
                "pane 1: summary_unavailable".to_string(),
                "pane 3: summary_unavailable".to_string(),
                "pane 7: summary_unavailable".to_string(),
                "pane 11: summary_unavailable".to_string(),
            ]
        );
    }

    #[test]
    fn backend_watchdog_warnings_are_count_bounded_sanitized_and_redacted() {
        let mut source = vec![format!(
            "pass\u{1b}[31mword={}\nsecond line",
            "secret-value".repeat(8 * 1024)
        )];
        source.extend((1..40).map(|index| format!("warning {index}")));

        let mut warnings = Vec::new();
        append_bounded_watchdog_warnings(&mut warnings, source);

        assert_eq!(warnings.len(), MAX_RUNTIME_WATCHDOG_WARNINGS);
        assert_eq!(
            warnings.last().map(String::as_str),
            Some("9 additional mux watchdog warnings omitted")
        );
        assert!(warnings[0].len() <= MAX_RUNTIME_WATCHDOG_WARNING_BYTES);
        assert!(!warnings[0].contains("secret-value"));
        assert!(!warnings[0].contains('\u{1b}'));
        assert!(!warnings[0].contains('\n'));
    }

    #[test]
    fn watchdog_warning_input_prefix_preserves_utf8_boundaries() {
        let value = "abcé";
        assert_eq!(utf8_prefix_at_most(value, 4), "abc");
        assert_eq!(utf8_prefix_at_most(value, value.len()), value);
    }

    #[test]
    fn fleet_coordinator_maintenance_skips_stable_noop_ticks() {
        let normal = FleetCoordinatorMaintenanceState {
            pressure: crate::fleet_memory_controller::FleetPressureTier::Normal,
            telemetry_blind: false,
            telemetry_partial: false,
            recommended_actions: 0,
        };

        assert!(fleet_coordinator_maintenance_is_noteworthy(
            None, normal, 0, 0, 0,
        ));
        assert!(!fleet_coordinator_maintenance_is_noteworthy(
            Some(normal),
            normal,
            0,
            0,
            0,
        ));
    }

    #[test]
    fn fleet_coordinator_maintenance_keeps_transitions_and_activity() {
        let normal = FleetCoordinatorMaintenanceState {
            pressure: crate::fleet_memory_controller::FleetPressureTier::Normal,
            telemetry_blind: false,
            telemetry_partial: false,
            recommended_actions: 0,
        };
        let blind = FleetCoordinatorMaintenanceState {
            telemetry_blind: true,
            ..normal
        };

        assert!(fleet_coordinator_maintenance_is_noteworthy(
            Some(normal),
            blind,
            0,
            0,
            0,
        ));
        assert!(fleet_coordinator_maintenance_is_noteworthy(
            Some(normal),
            normal,
            1,
            0,
            0,
        ));
        assert!(fleet_coordinator_maintenance_is_noteworthy(
            Some(normal),
            normal,
            0,
            1,
            0,
        ));
        assert!(fleet_coordinator_maintenance_is_noteworthy(
            Some(normal),
            normal,
            0,
            0,
            1,
        ));
        let actions_changed = FleetCoordinatorMaintenanceState {
            recommended_actions: 1,
            ..normal
        };
        assert!(fleet_coordinator_maintenance_is_noteworthy(
            Some(normal),
            actions_changed,
            0,
            0,
            0,
        ));
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
            swarm_capacity: None,
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
            ..SchedulerSnapshot::default()
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
            swarm_capacity: None,
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
                ..SchedulerSnapshot::default()
            }),
            backpressure_tier: None,
            last_activity_by_pane: vec![],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
            fleet_pressure_tier: None,
            swarm_capacity: None,
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
        assert_eq!(event.matched_text.as_deref(), Some("out of memory"));
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
    fn runtime_capture_concurrency_preserves_u32_upper_boundary_exactly() {
        let config = RuntimeConfig {
            max_concurrent_captures: u32::MAX,
            ..RuntimeConfig::default()
        };
        let hot = initial_hot_reloadable_config(&config);
        assert_eq!(hot.max_concurrent_captures, u32::MAX);
        assert_eq!(
            capture_concurrency_usize(config.max_concurrent_captures),
            usize::try_from(u32::MAX).expect("supported target usize width")
        );
    }

    #[test]
    fn runtime_config_default_retention_days() {
        let config = RuntimeConfig::default();
        assert_eq!(config.retention_days, 30);
    }

    #[test]
    fn runtime_startup_hot_config_preserves_custom_compiled_retention_policy() {
        let custom_tiers = vec![crate::config::RetentionTier {
            name: "startup-custom".to_string(),
            retention_days: 123,
            severities: vec!["critical".to_string()],
            event_types: vec!["agent.".to_string()],
            handled: Some(false),
        }];
        let storage_config = StorageConfig {
            retention_tiers: custom_tiers.clone(),
            ..StorageConfig::default()
        };
        let config = RuntimeConfig {
            retention_policy: storage_config
                .compile_retention_policy()
                .expect("compile custom startup policy"),
            ..RuntimeConfig::default()
        };

        let hot = initial_hot_reloadable_config(&config);
        assert_eq!(hot.retention_policy.tiers(), custom_tiers);
    }

    #[test]
    fn hot_reload_retention_policy_rejects_invalid_tiers_before_publish() {
        let mut config = crate::config::Config::default();
        config.storage.retention_tiers = vec![crate::config::RetentionTier {
            name: " ".to_string(),
            retention_days: 7,
            severities: Vec::new(),
            event_types: Vec::new(),
            handled: None,
        }];
        let error = HotReloadableConfig::from_config(&config)
            .expect_err("blank tier names must fail closed before channel publication");
        assert!(error.to_string().contains("name must not be empty"));
    }

    #[test]
    fn runtime_config_debug_never_reflects_retention_policy_content() {
        let secret = "runtime-debug-secret-filter-marker";
        let storage_config = StorageConfig {
            retention_tiers: vec![crate::config::RetentionTier {
                name: "runtime-debug-secret-name".to_string(),
                retention_days: 9,
                severities: vec![secret.to_string()],
                event_types: Vec::new(),
                handled: None,
            }],
            ..StorageConfig::default()
        };
        let config = RuntimeConfig {
            retention_policy: storage_config
                .compile_retention_policy()
                .expect("compile debug redaction policy"),
            ..RuntimeConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains(secret));
        assert!(!debug.contains("runtime-debug-secret-name"));
        assert!(debug.contains("canonical_filters"));
    }

    #[test]
    fn retention_maintenance_is_immediate_but_failed_attempts_do_not_busy_loop() {
        let start = Instant::now();
        let mut schedule = RetentionMaintenanceSchedule::new(start);
        assert!(schedule.should_attempt(start), "startup cleanup is due");

        schedule.finish_attempt(start, false);
        assert!(schedule.due, "a failed startup cleanup remains due");
        assert!(
            !schedule.should_attempt(start),
            "failure must not retry in the same maintenance turn"
        );
        let before_retry = start
            .checked_add(RETENTION_MAINTENANCE_RETRY_DELAY - Duration::from_millis(1))
            .expect("test instant range");
        assert!(!schedule.should_attempt(before_retry));
        let retry_at = start
            .checked_add(RETENTION_MAINTENANCE_RETRY_DELAY)
            .expect("test instant range");
        assert!(schedule.should_attempt(retry_at));

        schedule.finish_attempt(retry_at, true);
        assert!(!schedule.due);
        let before_cadence = retry_at
            .checked_add(RETENTION_MAINTENANCE_CADENCE - Duration::from_millis(1))
            .expect("test instant range");
        assert!(!schedule.should_attempt(before_cadence));
        let cadence_at = retry_at
            .checked_add(RETENTION_MAINTENANCE_CADENCE)
            .expect("test instant range");
        assert!(schedule.should_attempt(cadence_at));

        schedule.finish_attempt(cadence_at, true);
        schedule.mark_due();
        assert!(
            schedule.should_attempt(cadence_at),
            "a retention policy or size-cap update bypasses the hourly cadence"
        );

        let long_start = cadence_at;
        let long_completion = long_start
            .checked_add(Duration::from_secs(20 * 60))
            .expect("test long-attempt instant range");
        schedule.finish_attempt(long_completion, true);
        let old_start_based_hour = long_start
            .checked_add(RETENTION_MAINTENANCE_CADENCE)
            .expect("test old start-based cadence instant");
        assert!(
            !schedule.should_attempt(old_start_based_hour),
            "a long cleanup must receive the full cadence after completion"
        );
        let completion_based_hour = long_completion
            .checked_add(RETENTION_MAINTENANCE_CADENCE)
            .expect("test completion-based cadence instant");
        assert!(schedule.should_attempt(completion_based_hour));

        let failed_completion = completion_based_hour
            .checked_add(Duration::from_secs(20 * 60))
            .expect("test failed-attempt completion instant");
        schedule.finish_attempt(failed_completion, false);
        assert!(!schedule.should_attempt(failed_completion));
        assert!(
            schedule.should_attempt(
                failed_completion
                    .checked_add(RETENTION_MAINTENANCE_RETRY_DELAY)
                    .expect("test completion-based retry instant")
            ),
            "failed long work retries only after the delay measured from completion"
        );
    }

    #[test]
    fn periodic_maintenance_cadence_starts_at_completion_and_honors_interval_changes() {
        let start = Instant::now();
        let ten_minutes = Duration::from_secs(10 * 60);
        let mut schedule = CompletionTimedSchedule::new(start);
        assert!(!schedule.should_run(start, ten_minutes));

        let operation_start = start
            .checked_add(ten_minutes)
            .expect("test due instant");
        assert!(schedule.should_run(operation_start, ten_minutes));
        let long_completion = operation_start
            .checked_add(Duration::from_secs(7 * 60))
            .expect("test completion instant");
        schedule.finish(long_completion);
        assert!(
            !schedule.should_run(
                operation_start
                    .checked_add(ten_minutes)
                    .expect("old start-based instant"),
                ten_minutes,
            ),
            "long maintenance must retain the full interval after completion"
        );
        assert!(schedule.should_run(
            long_completion
                .checked_add(ten_minutes)
                .expect("completion-based due instant"),
            ten_minutes,
        ));

        let shorter = Duration::from_secs(60);
        assert!(
            schedule.should_run(
                long_completion
                    .checked_add(shorter)
                    .expect("shortened interval due instant"),
                shorter,
            ),
            "a shortened hot-reload interval is measured from the last completion"
        );
        assert!(!schedule.should_run(
            long_completion
                .checked_add(Duration::from_secs(24 * 60 * 60))
                .expect("disabled interval probe"),
            Duration::ZERO,
        ));
    }

    #[test]
    fn runtime_config_default_retention_max_mb_unlimited() {
        let config = RuntimeConfig::default();
        assert_eq!(config.retention_max_mb, 0);
    }

    #[test]
    fn size_retention_receipts_are_exact_bounded_and_status_specific() {
        let outcome = SizeEvictionOutcome {
            deleted_segments: 17,
            used_bytes_before: 9_000_000,
            used_bytes_after: 4_000_000,
            over_limit_after: false,
        };
        for (status, expected_status) in [
            (SizeRetentionReceiptStatus::Completed, "completed"),
            (
                SizeRetentionReceiptStatus::InterruptedPartial,
                "interrupted_partial",
            ),
        ] {
            let pending = PendingSizeRetentionReceipt {
                outcome,
                status,
                retention_max_mb: 4,
                attempt_timestamp: 123_456,
            };
            let record = size_retention_receipt_record(pending);
            assert_eq!(record.event_type, "size_retention");
            assert_eq!(record.timestamp, 123_456);
            assert_eq!(
                size_retention_receipt_record(pending).timestamp,
                123_456,
                "receipt retries must preserve the original attempt timestamp"
            );
            let message = record.message.expect("size receipt message");
            assert!(message.contains(expected_status));
            assert!(message.contains("17 durable segment deletions"));
            assert!(message.len() < 160, "receipt message must remain bounded");

            let metadata: serde_json::Value = serde_json::from_str(
                record.metadata.as_deref().expect("size receipt metadata"),
            )
            .expect("parse fixed-shape size receipt");
            assert_eq!(metadata["schema"], "size_retention_receipt.v1");
            assert_eq!(metadata["attempt_status"], expected_status);
            assert_eq!(metadata["retention_max_mb"], 4);
            assert_eq!(metadata["deleted_segments"], 17);
            assert_eq!(metadata["used_bytes_before"], 9_000_000);
            assert_eq!(metadata["used_bytes_after"], 4_000_000);
            assert_eq!(metadata["over_limit_after"], false);
            assert_eq!(
                metadata.as_object().expect("receipt object").len(),
                7,
                "the receipt schema must remain fixed and content-free"
            );
        }
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
    fn vendored_streaming_identity_for_single_backend_uses_raw_pane_id() {
        let socket_paths = vec![PathBuf::from("/tmp/wa.sock")];
        let lease = test_capture_lease(42, CaptureSourceKind::VendoredStreaming);
        let identity =
            vendored_streaming_identity_for_pane(&socket_paths, 42, 3, lease.stamp())
                .expect("single socket");
        assert_eq!(identity.global_pane_id, 42);
        assert_eq!(identity.local_pane_id, 42);
        assert_eq!(identity.socket_shard, ShardId(0));
        assert_eq!(identity.socket_path, PathBuf::from("/tmp/wa.sock"));
        assert_eq!(identity.generation, 3);
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn vendored_streaming_identity_for_sharded_backend_decodes_pane_bits() {
        let socket_paths = vec![
            PathBuf::from("/tmp/wa-0.sock"),
            PathBuf::from("/tmp/wa-1.sock"),
        ];
        let global_pane_id =
            crate::sharding::encode_sharded_pane_id(crate::sharding::ShardId(1), 7);
        let lease = test_capture_lease(global_pane_id, CaptureSourceKind::VendoredStreaming);
        let identity = vendored_streaming_identity_for_pane(
            &socket_paths,
            global_pane_id,
            9,
            lease.stamp(),
        )
        .expect("sharded socket");
        assert_eq!(identity.global_pane_id, global_pane_id);
        assert_eq!(identity.local_pane_id, 7);
        assert_eq!(identity.socket_shard, ShardId(1));
        assert_eq!(identity.socket_path, PathBuf::from("/tmp/wa-1.sock"));
        assert_eq!(identity.generation, 9);
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn vendored_streaming_identity_for_unknown_shard_returns_none() {
        let socket_paths = vec![
            PathBuf::from("/tmp/wa-0.sock"),
            PathBuf::from("/tmp/wa-1.sock"),
        ];
        let global_pane_id =
            crate::sharding::encode_sharded_pane_id(crate::sharding::ShardId(3), 9);
        let lease = test_capture_lease(global_pane_id, CaptureSourceKind::VendoredStreaming);
        assert!(
            vendored_streaming_identity_for_pane(
                &socket_paths,
                global_pane_id,
                0,
                lease.stamp(),
            )
            .is_none()
        );
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn vendored_streaming_route_rejects_non_persistable_or_wrong_shard_id() {
        let one_socket = vec![PathBuf::from("/tmp/wa.sock")];
        let two_sockets = vec![
            PathBuf::from("/tmp/wa-0.sock"),
            PathBuf::from("/tmp/wa-1.sock"),
        ];
        let shard_one =
            crate::sharding::encode_sharded_pane_id(crate::sharding::ShardId(1), 7);

        assert!(vendored_streaming_route_for_pane(&one_socket, u64::MAX).is_none());
        assert!(vendored_streaming_route_for_pane(&one_socket, shard_one).is_none());
        assert!(vendored_streaming_route_for_pane(&two_sockets, u64::MAX).is_none());
        assert_eq!(
            vendored_streaming_route_for_pane(&one_socket, 7),
            Some((ShardId(0), 7, one_socket[0].clone()))
        );
        assert_eq!(
            vendored_streaming_route_for_pane(&two_sockets, shard_one),
            Some((ShardId(1), 7, two_sockets[1].clone()))
        );
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn vendored_streaming_remaps_equal_local_ids_on_two_shards_for_every_delta_variant() {
        let socket_paths = vec![
            PathBuf::from("/tmp/wa-0.sock"),
            PathBuf::from("/tmp/wa-1.sock"),
        ];
        let local_pane_id = 7;
        let global_zero = crate::sharding::encode_sharded_pane_id(
            crate::sharding::ShardId(0),
            local_pane_id,
        );
        let global_one = crate::sharding::encode_sharded_pane_id(
            crate::sharding::ShardId(1),
            local_pane_id,
        );
        let lease_zero = test_capture_lease(global_zero, CaptureSourceKind::VendoredStreaming);
        let lease_one = test_capture_lease(global_one, CaptureSourceKind::VendoredStreaming);
        let identity_zero = vendored_streaming_identity_for_pane(
            &socket_paths,
            global_zero,
            2,
            lease_zero.stamp(),
        )
        .expect("shard zero identity");
        let identity_one = vendored_streaming_identity_for_pane(
            &socket_paths,
            global_one,
            4,
            lease_one.stamp(),
        )
        .expect("shard one identity");

        assert_eq!(identity_zero.local_pane_id, identity_one.local_pane_id);
        assert_ne!(identity_zero.global_pane_id, identity_one.global_pane_id);
        assert_ne!(identity_zero.socket_shard, identity_one.socket_shard);

        for identity in [&identity_zero, &identity_one] {
            let output = remap_vendored_streaming_delta(
                identity,
                PaneDelta::Output {
                    pane_id: local_pane_id,
                    seqno: 11,
                    delta_text: "delta".to_string(),
                    title: "shell".to_string(),
                    dirty_range_count: 2,
                    dirty_row_count: 3,
                },
            )
            .expect("matching output identity");
            assert!(matches!(
                output,
                PaneDelta::Output {
                    pane_id,
                    seqno: 11,
                    ..
                } if pane_id == identity.global_pane_id
            ));

            let gap = remap_vendored_streaming_delta(
                identity,
                PaneDelta::Gap {
                    pane_id: local_pane_id,
                    reason: "gap".to_string(),
                },
            )
            .expect("matching gap identity");
            assert!(matches!(
                gap,
                PaneDelta::Gap { pane_id, reason }
                    if pane_id == identity.global_pane_id && reason == "gap"
            ));

            let ended = remap_vendored_streaming_delta(
                identity,
                PaneDelta::Ended {
                    pane_id: local_pane_id,
                    reason: "ended".to_string(),
                },
            )
            .expect("matching ended identity");
            assert!(matches!(
                ended,
                PaneDelta::Ended { pane_id, reason }
                    if pane_id == identity.global_pane_id && reason == "ended"
            ));
        }
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn equal_local_ids_on_two_shards_persist_under_distinct_global_ids() {
        run_async_test_isolated(|| async {
            let socket_paths = vec![
                PathBuf::from("/tmp/wa-0.sock"),
                PathBuf::from("/tmp/wa-1.sock"),
            ];
            let local_pane_id = 7;
            let global_zero = crate::sharding::encode_sharded_pane_id(
                crate::sharding::ShardId(0),
                local_pane_id,
            );
            let global_one = crate::sharding::encode_sharded_pane_id(
                crate::sharding::ShardId(1),
                local_pane_id,
            );
            assert_ne!(global_zero, global_one);

            let (_dir, db_path) = temp_db_path();
            let storage = StorageHandle::new(&db_path).await.expect("test storage");
            for pane_id in [global_zero, global_one] {
                storage
                    .upsert_pane(test_pane_record(pane_id))
                    .await
                    .expect("persist sharded test pane");
            }
            let wezterm: WeztermHandle = Arc::new(crate::wezterm::MockWezterm::new());
            let runtime = ObservationRuntime::new(
                RuntimeConfig::default(),
                storage.clone(),
                Arc::new(RwLock::new(PatternEngine::new())),
            )
            .with_wezterm_handle(wezterm);

            let pane_zero = runtime
                .capture_authority
                .activate_pane(global_zero)
                .expect("activate shard zero pane");
            let pane_one = runtime
                .capture_authority
                .activate_pane(global_one)
                .expect("activate shard one pane");
            let lease_zero = runtime
                .capture_authority
                .issue_source(pane_zero, CaptureSourceKind::VendoredStreaming)
                .expect("issue shard zero source");
            let lease_one = runtime
                .capture_authority
                .issue_source(pane_one, CaptureSourceKind::VendoredStreaming)
                .expect("issue shard one source");
            let identity_zero = vendored_streaming_identity_for_pane(
                &socket_paths,
                global_zero,
                2,
                lease_zero.stamp(),
            )
            .expect("shard zero identity");
            let identity_one = vendored_streaming_identity_for_pane(
                &socket_paths,
                global_one,
                4,
                lease_one.stamp(),
            )
            .expect("shard one identity");
            assert_eq!(identity_zero.local_pane_id, identity_one.local_pane_id);

            let revision_zero = DiscoveryRevision(20);
            let revision_one = DiscoveryRevision(21);
            runtime.capture_metadata.write().await.extend([
                (
                    pane_zero.pane_incarnation(),
                    CapturePaneMetadata {
                        pane_uuid: "shard-zero-uuid".to_string(),
                        discovery_generation: 2,
                        discovery_revision: revision_zero,
                    },
                ),
                (
                    pane_one.pane_incarnation(),
                    CapturePaneMetadata {
                        pane_uuid: "shard-one-uuid".to_string(),
                        discovery_generation: 4,
                        discovery_revision: revision_one,
                    },
                ),
            ]);
            let (_publication_tx, publication_rx) = watch::channel(
                DiscoveryCapturePublication {
                    epoch: 1,
                    observed_panes: Arc::new(HashMap::from([
                        (
                            global_zero,
                            ObservedCapturePane {
                                info: make_pane(global_zero, "shard-zero"),
                                generation: 2,
                                pane_uuid: "shard-zero-uuid".to_string(),
                                revision: revision_zero,
                                requires_storage_resync: false,
                            },
                        ),
                        (
                            global_one,
                            ObservedCapturePane {
                                info: make_pane(global_one, "shard-one"),
                                generation: 4,
                                pane_uuid: "shard-one-uuid".to_string(),
                                revision: revision_one,
                                requires_storage_resync: false,
                            },
                        ),
                    ])),
                    transitioning_pane_ids: Arc::new(HashSet::new()),
                    transitions: Arc::new(HashMap::new()),
                },
            );

            let (capture_tx, capture_rx) = mpsc::channel(4);
            let loop_cx = runtime_loop_cx();
            let mut bridge_zero = StreamingBridge::new();
            let mut bridge_one = StreamingBridge::new();
            assert!(
                forward_vendored_streaming_delta(
                    &loop_cx,
                    &mut bridge_zero,
                    &capture_tx,
                    &identity_zero,
                    &lease_zero,
                    PaneDelta::Output {
                        pane_id: local_pane_id,
                        seqno: 1,
                        delta_text: "output from shard zero".to_string(),
                        title: "zero".to_string(),
                        dirty_range_count: 1,
                        dirty_row_count: 1,
                    },
                )
                .await
                .is_none()
            );
            assert!(
                forward_vendored_streaming_delta(
                    &loop_cx,
                    &mut bridge_one,
                    &capture_tx,
                    &identity_one,
                    &lease_one,
                    PaneDelta::Output {
                        pane_id: local_pane_id,
                        seqno: 1,
                        delta_text: "output from shard one".to_string(),
                        title: "one".to_string(),
                        dirty_range_count: 1,
                        dirty_row_count: 1,
                    },
                )
                .await
                .is_none()
            );
            drop(capture_tx);

            assert_eq!(
                bridge_zero
                    .render_metadata(global_zero)
                    .map(|metadata| metadata.title.as_str()),
                Some("zero")
            );
            assert!(bridge_zero.render_metadata(global_one).is_none());
            assert_eq!(
                bridge_one
                    .render_metadata(global_one)
                    .map(|metadata| metadata.title.as_str()),
                Some("one")
            );
            assert!(bridge_one.render_metadata(global_zero).is_none());

            let (ring_tx, ring_rx) = spsc_channel(4);
            let relay = runtime.spawn_capture_relay_task(capture_rx, ring_tx);
            let checkpoints: CaptureCheckpointCache =
                Arc::new(StdMutex::new(LruCache::new(4)));
            let persistence = runtime.spawn_persistence_task(
                ring_rx,
                Arc::clone(&runtime.cursors),
                publication_rx,
                checkpoints,
            );
            relay.await.expect("capture relay task");
            persistence.await.expect("capture persistence task");

            let shard_zero_segments = storage
                .get_segments(global_zero, 10)
                .await
                .expect("read shard zero segments");
            let shard_one_segments = storage
                .get_segments(global_one, 10)
                .await
                .expect("read shard one segments");
            assert_eq!(shard_zero_segments.len(), 1);
            assert_eq!(shard_one_segments.len(), 1);
            assert_eq!(shard_zero_segments[0].pane_id, global_zero);
            assert_eq!(shard_one_segments[0].pane_id, global_one);
            assert_eq!(shard_zero_segments[0].content, "output from shard zero");
            assert_eq!(shard_one_segments[0].content, "output from shard one");
            assert_eq!(runtime.metrics.segments_persisted(), 2);
            assert_eq!(runtime.metrics.capture_authority_rejections(), 0);

            storage.shutdown().await.expect("shutdown test storage");
        });
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn vendored_streaming_rejects_mismatched_local_id_before_bridge() {
        run_async_test(async {
            let global_pane_id = crate::sharding::encode_sharded_pane_id(ShardId(1), 7);
            let lease =
                test_capture_lease(global_pane_id, CaptureSourceKind::VendoredStreaming);
            let identity = test_streaming_identity(global_pane_id, 7, 1, 5, &lease);
            for mismatched_delta in [
                PaneDelta::Gap {
                    pane_id: 8,
                    reason: "wrong gap pane".to_string(),
                },
                PaneDelta::Ended {
                    pane_id: 8,
                    reason: "wrong ended pane".to_string(),
                },
            ] {
                assert!(
                    remap_vendored_streaming_delta(&identity, mismatched_delta).is_err(),
                    "every non-output delta variant must fail closed on a local-id mismatch"
                );
            }

            let (capture_tx, mut capture_rx) = mpsc::channel(1);
            let loop_cx = runtime_loop_cx();
            let mut bridge = StreamingBridge::new();

            let exit_reason = forward_vendored_streaming_delta(
                &loop_cx,
                &mut bridge,
                &capture_tx,
                &identity,
                &lease,
                PaneDelta::Output {
                    pane_id: 8,
                    seqno: 1,
                    delta_text: "wrong pane".to_string(),
                    title: "shell".to_string(),
                    dirty_range_count: 1,
                    dirty_row_count: 1,
                },
            )
            .await
            .expect("mismatch must end subscription");

            assert!(exit_reason.contains("expected local pane 7"));
            assert!(exit_reason.contains("received local pane 8"));
            assert_eq!(bridge.events_processed(), 0);
            assert!(capture_rx.try_recv().is_err());
        });
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn vendored_streaming_generation_change_replaces_task_and_ignores_stale_exit() {
        let global_pane_id = crate::sharding::encode_sharded_pane_id(ShardId(1), 7);
        let lease = test_capture_lease(global_pane_id, CaptureSourceKind::VendoredStreaming);
        let old_identity = test_streaming_identity(global_pane_id, 7, 1, 4, &lease);
        let replacement_identity = test_streaming_identity(global_pane_id, 7, 1, 5, &lease);
        let old_exit = StreamingTaskExit {
            identity: old_identity.clone(),
            token: StreamingTaskToken(41),
            reason: "old generation ended".to_string(),
        };
        let replacement_exit = StreamingTaskExit {
            identity: replacement_identity.clone(),
            token: StreamingTaskToken(42),
            reason: "replacement ended".to_string(),
        };
        let stale_same_generation_exit = StreamingTaskExit {
            identity: replacement_identity.clone(),
            token: StreamingTaskToken(41),
            reason: "prior task token ended".to_string(),
        };

        assert_eq!(
            streaming_task_reconcile_action(
                &old_identity,
                Some((
                    replacement_identity.global_pane_id,
                    replacement_identity.generation,
                    replacement_identity.capture_stamp,
                )),
            ),
            StreamingTaskReconcileAction::Replace,
        );
        assert_eq!(
            streaming_task_reconcile_action(
                &replacement_identity,
                Some((
                    replacement_identity.global_pane_id,
                    replacement_identity.generation,
                    replacement_identity.capture_stamp,
                )),
            ),
            StreamingTaskReconcileAction::Keep,
        );
        assert_eq!(
            streaming_task_reconcile_action(&replacement_identity, None),
            StreamingTaskReconcileAction::Remove,
        );
        assert!(!streaming_exit_matches_active(
            &replacement_identity,
            StreamingTaskToken(42),
            &old_exit,
        ));
        assert!(!streaming_exit_matches_active(
            &replacement_identity,
            StreamingTaskToken(42),
            &stale_same_generation_exit,
        ));
        assert!(streaming_exit_matches_active(
            &replacement_identity,
            StreamingTaskToken(42),
            &replacement_exit,
        ));
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

    /// Round-trip plus poisoned-lock recovery for the global lock/memory
    /// telemetry snapshot. Pre-fix, `update_global` used `if let Ok` (silently
    /// dropping the update on a poisoned lock) and `get_global` used `.ok()`
    /// (returning None), so a poisoned lock took telemetry permanently dark.
    /// Post-fix both recover via `unwrap_or_else(|e| e.into_inner())`, matching
    /// the global-state lock idiom used throughout runtime_telemetry.rs.
    ///
    /// One test (not split) so the process-global static is touched on a single
    /// thread — no inter-test race on the shared value.
    #[test]
    fn lock_memory_snapshot_global_round_trip_and_poison_recovery() {
        let snap = RuntimeLockMemoryTelemetrySnapshot {
            timestamp_ms: 777_777,
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

        // Basic round-trip on a healthy lock.
        RuntimeLockMemoryTelemetrySnapshot::update_global(snap.clone());
        assert_eq!(
            RuntimeLockMemoryTelemetrySnapshot::get_global().map(|s| s.timestamp_ms),
            Some(777_777),
            "update_global value must be retrievable via get_global"
        );

        // Poison the global lock by panicking while holding the write guard.
        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let lock = GLOBAL_RUNTIME_LOCK_MEMORY_TELEMETRY.get_or_init(|| StdRwLock::new(None));
            let _guard = lock.write().unwrap_or_else(|e| e.into_inner());
            std::panic::panic_any("poison global lock/memory telemetry lock");
        }));
        assert!(poison.is_err());

        // ft-lo0ip: capture the poison counter just before exercising the
        // recovered paths so the assertion is a delta (robust against other
        // tests that may touch the now-poisoned process-global lock).
        let poison_count_before = runtime_lock_memory_telemetry_poisoned_count();

        // After poison: update_global must RECOVER and apply the new snapshot
        // (not silently drop it), and get_global must recover and return it.
        let mut snap2 = snap;
        snap2.timestamp_ms = 888_888;
        RuntimeLockMemoryTelemetrySnapshot::update_global(snap2);
        assert_eq!(
            RuntimeLockMemoryTelemetrySnapshot::get_global().map(|s| s.timestamp_ms),
            Some(888_888),
            "update/get must recover from a poisoned lock and not lose the update"
        );

        // ft-lo0ip: recovery must also be OBSERVABLE — the recovered
        // update_global + get_global each bump the poison counter (>= +2),
        // instead of recovering silently.
        assert!(
            runtime_lock_memory_telemetry_poisoned_count() >= poison_count_before + 2,
            "poisoned-lock recovery must bump the observability counter \
             (before={poison_count_before}, after={})",
            runtime_lock_memory_telemetry_poisoned_count()
        );
    }

    // =========================================================================
    // Constant validation tests
    // =========================================================================

    #[test]
    fn telemetry_percentile_window_capacity_is_positive() {
        const {
            assert!(TELEMETRY_PERCENTILE_WINDOW_CAPACITY > 0);
        }
    }

    #[test]
    fn resize_watchdog_thresholds_are_ordered() {
        const {
            assert!(RESIZE_WATCHDOG_WARNING_THRESHOLD_MS < RESIZE_WATCHDOG_CRITICAL_THRESHOLD_MS);
        }
    }

    #[test]
    fn resize_watchdog_critical_stalled_limit_is_positive() {
        const {
            assert!(RESIZE_WATCHDOG_CRITICAL_STALLED_LIMIT > 0);
        }
    }

    #[test]
    fn resize_watchdog_sample_limit_is_positive() {
        const {
            assert!(RESIZE_WATCHDOG_SAMPLE_LIMIT > 0);
        }
    }

    #[test]
    fn storage_lock_warn_thresholds_exist() {
        // These constants are used in health snapshot warnings
        const {
            assert!(STORAGE_LOCK_WAIT_WARN_MS > 0.0);
            assert!(STORAGE_LOCK_HOLD_WARN_MS > 0.0);
            assert!(CURSOR_SNAPSHOT_MEMORY_WARN_BYTES > 0);
        }
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

    // ft-6n7hs: per-pane logical budget level derivation.
    const TEST_PANE_BUDGET_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
    const TEST_PANE_HIGH_RATIO: f64 = 0.8;

    #[test]
    fn classify_pane_budget_level_thresholds() {
        let budget = TEST_PANE_BUDGET_BYTES;
        let high = (budget as f64 * TEST_PANE_HIGH_RATIO) as u64;

        // Well under the soft limit.
        assert_eq!(
            classify_pane_budget_level(high - 1, budget, TEST_PANE_HIGH_RATIO),
            BudgetLevel::Normal
        );
        // Exactly at the soft limit throttles.
        assert_eq!(
            classify_pane_budget_level(high, budget, TEST_PANE_HIGH_RATIO),
            BudgetLevel::Throttled
        );
        // Between soft limit and budget throttles.
        assert_eq!(
            classify_pane_budget_level(budget - 1, budget, TEST_PANE_HIGH_RATIO),
            BudgetLevel::Throttled
        );
        // At or above the hard budget is over budget.
        assert_eq!(
            classify_pane_budget_level(budget, budget, TEST_PANE_HIGH_RATIO),
            BudgetLevel::OverBudget
        );
        assert_eq!(
            classify_pane_budget_level(budget * 4, budget, TEST_PANE_HIGH_RATIO),
            BudgetLevel::OverBudget
        );
    }

    #[test]
    fn classify_pane_budget_level_zero_budget_is_normal() {
        // A zero budget disables the dimension rather than reporting everything
        // as over budget (mirrors PaneBudget::usage_ratio guarding div-by-zero).
        assert_eq!(
            classify_pane_budget_level(u64::MAX, 0, TEST_PANE_HIGH_RATIO),
            BudgetLevel::Normal
        );
    }

    #[test]
    fn classify_pane_budget_level_nan_ratio_falls_back_to_default() {
        let budget = TEST_PANE_BUDGET_BYTES;
        let default_high = (budget as f64 * MemoryBudgetConfig::default().high_ratio) as u64;
        // NaN ratio must not classify a small pane as throttled.
        assert_eq!(
            classify_pane_budget_level(default_high - 1, budget, f64::NAN),
            BudgetLevel::Normal
        );
        assert_eq!(
            classify_pane_budget_level(default_high, budget, f64::NAN),
            BudgetLevel::Throttled
        );
    }

    #[test]
    fn worst_pane_budget_level_empty_is_normal() {
        assert_eq!(
            worst_pane_budget_level(
                std::iter::empty::<u64>(),
                TEST_PANE_BUDGET_BYTES,
                TEST_PANE_HIGH_RATIO
            ),
            BudgetLevel::Normal
        );
    }

    #[test]
    fn worst_pane_budget_level_takes_worst_across_panes() {
        let budget = TEST_PANE_BUDGET_BYTES;
        let high = (budget as f64 * TEST_PANE_HIGH_RATIO) as u64;

        // All comfortably under budget -> Normal.
        assert_eq!(
            worst_pane_budget_level([0u64, 1, high / 2], budget, TEST_PANE_HIGH_RATIO),
            BudgetLevel::Normal
        );
        // One throttled pane raises the fleet dimension.
        assert_eq!(
            worst_pane_budget_level([0u64, high, high / 2], budget, TEST_PANE_HIGH_RATIO),
            BudgetLevel::Throttled
        );
        // A single over-budget pane wins over throttled/normal peers — this is
        // the case the old hardcoded BudgetLevel::Normal could never surface.
        assert_eq!(
            worst_pane_budget_level([0u64, high, budget + 1], budget, TEST_PANE_HIGH_RATIO),
            BudgetLevel::OverBudget
        );
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
        use crate::test_fixtures::lab_runtime::{LabConfig, lab_runtime_test_with_config};
        use std::sync::atomic::{AtomicU64, AtomicUsize};

        /// Helper: build a LabRuntime, create a root region and a task, run to
        /// quiescence via auto-advance. Asserts termination is clean.
        ///
        /// br-ft-c8x87 migration: thin shim over the lab_runtime
        /// fixture preserving this module's worker_count(2) +
        /// 50_000 step budget. The fixture itself defaults to
        /// worker_count(1); these tests intentionally exercise
        /// multi-worker scheduling, so we go through with_config.
        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let config = LabConfig::new(seed)
                .with_auto_advance()
                .worker_count(2)
                .max_steps(50_000);
            lab_runtime_test_with_config(config, move |_cx| async move {
                f().await;
            });
        }

        /// 1. Event loop channel dispatch: send events through mpsc,
        ///    verify the receiver processes them in order.
        #[test]
        fn channel_dispatch_under_labruntime() {
            run_lab(101, || async move {
                let (tx, mut rx) = crate::runtime_async::mpsc::channel::<u64>(16);
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
                let (tx_a, mut rx_a) = crate::runtime_async::mpsc::channel::<&str>(8);
                let (tx_b, mut rx_b) = crate::runtime_async::mpsc::channel::<u32>(8);
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
                            crate::runtime_async::sleep_with_cx(&cx, Duration::from_millis(100))
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
                        let _ = crate::runtime_async::sleep_with_cx(&cx, Duration::from_millis(10))
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
                        crate::runtime_async::sleep_with_cx(&cx, Duration::from_millis(50)).await;
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

            let lock = Arc::new(crate::runtime_async::RwLock::new(0u64));

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
                                crate::runtime_async::sleep_with_cx(&cx, Duration::from_millis(5))
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
                            let _ =
                                crate::runtime_async::sleep_with_cx(&cx, Duration::from_millis(10))
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
                let (tx, mut rx) = crate::runtime_async::mpsc::channel::<u32>(4);

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
                let (tx, rx) = crate::runtime_async::watch::channel(0u64);

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
                let (mpsc_tx, mut mpsc_rx) = crate::runtime_async::mpsc::channel::<String>(16);
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
                        match crate::runtime_async::timeout_with_cx(
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
                        crate::runtime_async::sleep_with_cx(&cx, Duration::from_millis(50)).await;
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
