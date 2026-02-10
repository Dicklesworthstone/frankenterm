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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{RwLock, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, error, info, instrument, warn};

use crate::config::{
    CaptureBudgetConfig, HotReloadableConfig, PaneFilterConfig, PanePriorityConfig, PatternsConfig,
};
use crate::crash::{HealthSnapshot, ShutdownSummary};
use crate::error::Result;
#[cfg(feature = "native-wezterm")]
use crate::events::{Event, UserVarPayload};
use crate::events::{EventBus, event_identity_key};
use crate::ingest::{PaneCursor, PaneRegistry, persist_captured_segment};
#[cfg(feature = "native-wezterm")]
use crate::native_events::{NativeEvent, NativeEventListener};
use crate::patterns::{Detection, DetectionContext, PatternEngine};
use crate::recording::RecordingManager;
#[cfg(feature = "native-wezterm")]
use crate::storage::PaneRecord;
use crate::storage::{StorageHandle, StoredEvent};
use crate::tailer::{CaptureEvent, TailerConfig, TailerSupervisor};
use crate::watchdog::HeartbeatRegistry;
use crate::wezterm::{
    PaneInfo, WeztermHandle, WeztermHandleSource, WeztermInterface, wezterm_handle_with_timeout,
};

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
#[derive(Debug)]
pub struct RuntimeMetrics {
    /// Count of segments persisted
    segments_persisted: AtomicU64,
    /// Count of events recorded
    events_recorded: AtomicU64,
    /// Timestamp when runtime started (epoch ms)
    started_at: AtomicU64,
    /// Last DB write timestamp (epoch ms)
    last_db_write_at: AtomicU64,
    /// Sum of ingest lag samples (for averaging)
    ingest_lag_sum_ms: AtomicU64,
    /// Count of ingest lag samples
    ingest_lag_count: AtomicU64,
    /// Maximum ingest lag observed
    ingest_lag_max_ms: AtomicU64,
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self {
            segments_persisted: AtomicU64::new(0),
            events_recorded: AtomicU64::new(0),
            started_at: AtomicU64::new(0),
            last_db_write_at: AtomicU64::new(0),
            ingest_lag_sum_ms: AtomicU64::new(0),
            ingest_lag_count: AtomicU64::new(0),
            ingest_lag_max_ms: AtomicU64::new(0),
        }
    }
}

impl RuntimeMetrics {
    /// Record an ingest lag sample.
    pub fn record_ingest_lag(&self, lag_ms: u64) {
        self.ingest_lag_sum_ms.fetch_add(lag_ms, Ordering::SeqCst);
        self.ingest_lag_count.fetch_add(1, Ordering::SeqCst);

        // Update max using compare-and-swap loop
        let mut current_max = self.ingest_lag_max_ms.load(Ordering::SeqCst);
        while lag_ms > current_max {
            match self.ingest_lag_max_ms.compare_exchange_weak(
                current_max,
                lag_ms,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(v) => current_max = v,
            }
        }
    }

    /// Record a successful DB write.
    pub fn record_db_write(&self) {
        self.last_db_write_at
            .store(epoch_ms_u64(), Ordering::SeqCst);
    }

    /// Get average ingest lag in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn avg_ingest_lag_ms(&self) -> f64 {
        let sum = self.ingest_lag_sum_ms.load(Ordering::SeqCst);
        let count = self.ingest_lag_count.load(Ordering::SeqCst);
        if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        }
    }

    /// Get total ingest lag sample count.
    pub fn ingest_lag_count(&self) -> u64 {
        self.ingest_lag_count.load(Ordering::SeqCst)
    }

    /// Get total ingest lag sum in milliseconds.
    pub fn ingest_lag_sum_ms(&self) -> u64 {
        self.ingest_lag_sum_ms.load(Ordering::SeqCst)
    }

    /// Get maximum ingest lag in milliseconds.
    pub fn max_ingest_lag_ms(&self) -> u64 {
        self.ingest_lag_max_ms.load(Ordering::SeqCst)
    }

    /// Get last DB write timestamp (epoch ms), or None if never written.
    pub fn last_db_write(&self) -> Option<u64> {
        let ts = self.last_db_write_at.load(Ordering::SeqCst);
        if ts == 0 { None } else { Some(ts) }
    }

    /// Get total segments persisted.
    pub fn segments_persisted(&self) -> u64 {
        self.segments_persisted.load(Ordering::SeqCst)
    }

    /// Get total events recorded.
    pub fn events_recorded(&self) -> u64 {
        self.events_recorded.load(Ordering::SeqCst)
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
    config_tx: watch::Sender<HotReloadableConfig>,
    /// Hot-reloadable config receiver (for tasks to receive updates)
    config_rx: watch::Receiver<HotReloadableConfig>,
    /// Optional event bus for publishing detection events to workflow runners
    event_bus: Option<Arc<EventBus>>,
    /// Optional recording manager for capturing session recordings
    recording: Option<Arc<RecordingManager>>,
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
        metrics.started_at.store(epoch_ms_u64(), Ordering::SeqCst);

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
            config_tx,
            config_rx,
            event_bus: None,
            recording: None,
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

    /// Start the observation runtime.
    ///
    /// Returns handles for the spawned tasks. Call `shutdown()` to stop.
    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<RuntimeHandle> {
        info!("Starting observation runtime");

        let (capture_tx, capture_rx) = mpsc::channel::<CaptureEvent>(self.config.channel_buffer);

        // Clone capture_tx for queue depth instrumentation before moving it
        let capture_tx_probe = capture_tx.clone();

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
            self.spawn_capture_task(capture_tx.clone())
        };

        // Spawn native event listener if configured and supported.
        #[cfg(feature = "native-wezterm")]
        let native_handle =
            native_socket.map(|socket| self.spawn_native_event_task(socket, capture_tx.clone()));
        #[cfg(not(feature = "native-wezterm"))]
        let native_handle = {
            if native_socket.is_some() {
                warn!(
                    "Native event socket configured but wa-core built without native-wezterm feature"
                );
            }
            None
        };

        // Spawn persistence and detection task
        let persistence_handle = self.spawn_persistence_task(
            capture_rx,
            Arc::clone(&self.cursors),
            Arc::clone(&self.registry),
        );

        // Spawn maintenance task
        let maintenance_handle = self.spawn_maintenance_task(capture_tx_probe.clone());

        info!("Observation runtime started");

        Ok(RuntimeHandle {
            discovery: discovery_handle,
            capture: capture_handle,
            persistence: persistence_handle,
            maintenance: Some(maintenance_handle),
            shutdown_flag: Arc::clone(&self.shutdown_flag),
            storage: Arc::clone(&self.storage),
            metrics: Arc::clone(&self.metrics),
            registry: Arc::clone(&self.registry),
            cursors: Arc::clone(&self.cursors),
            start_time: Instant::now(),
            config_tx: self.config_tx.clone(),
            event_bus: self.event_bus.clone(),
            heartbeats: Arc::clone(&self.heartbeats),
            capture_tx: capture_tx_probe,
            native_events: native_handle,
            scheduler_snapshot: Arc::clone(&self.scheduler_snapshot),
        })
    }

    /// Spawn the maintenance task.
    fn spawn_maintenance_task(&self, capture_tx: mpsc::Sender<CaptureEvent>) -> JoinHandle<()> {
        let storage = Arc::clone(&self.storage);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let metrics = Arc::clone(&self.metrics);
        let scheduler_snapshot = Arc::clone(&self.scheduler_snapshot);

        let initial_retention_days = self.config.retention_days;
        let initial_checkpoint_secs = self.config.checkpoint_interval_secs;

        tokio::spawn(async move {
            let mut retention_days = initial_retention_days;
            let mut checkpoint_secs = initial_checkpoint_secs;
            let mut last_health_snapshot = Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(Instant::now);
            let health_interval = Duration::from_secs(30);

            // Run maintenance every minute, but only do expensive ops when needed
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            let mut last_retention_check = Instant::now();
            let mut last_checkpoint = Instant::now();

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        heartbeats.record_maintenance();

                        if shutdown_flag.load(Ordering::SeqCst) {
                            break;
                        }

                        // Check for config updates
                        if config_rx.has_changed().unwrap_or(false) {
                            let new_config = config_rx.borrow_and_update().clone();
                            if new_config.retention_days != retention_days {
                                info!(old = retention_days, new = new_config.retention_days, "Retention policy updated");
                                retention_days = new_config.retention_days;
                            }
                            if new_config.checkpoint_interval_secs != checkpoint_secs {
                                info!(old = checkpoint_secs, new = new_config.checkpoint_interval_secs, "Checkpoint interval updated");
                                checkpoint_secs = new_config.checkpoint_interval_secs;
                            }
                        }

                        let now = Instant::now();

                        // Run retention cleanup every hour (or if just started/updated)
                        if now.duration_since(last_retention_check) >= Duration::from_secs(3600) {
                            if retention_days > 0 {
                                let cutoff_days = u64::from(retention_days);
                                let cutoff_window_ms = cutoff_days.saturating_mul(24 * 60 * 60 * 1000);
                                let cutoff_ms = epoch_ms().saturating_sub(
                                    i64::try_from(cutoff_window_ms).unwrap_or(i64::MAX),
                                );
                                let storage_guard = storage.lock().await;
                                if let Err(e) = storage_guard.retention_cleanup(cutoff_ms).await {
                                    error!(error = %e, "Retention cleanup failed");
                                } else {
                                    debug!("Retention cleanup completed");
                                }
                                // Also purge old audit actions
                                if let Err(e) = storage_guard.purge_audit_actions_before(cutoff_ms).await {
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
                            let storage_guard = storage.lock().await;
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
                            last_checkpoint = now;
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

                            let last_seq_by_pane: Vec<(u64, i64)> = {
                                let cursors = cursors.read().await;
                                cursors
                                    .iter()
                                    .map(|(pane_id, cursor)| (*pane_id, cursor.last_seq()))
                                    .collect()
                            };

                            let capture_cap = capture_tx.max_capacity();
                            let capture_depth = capture_cap.saturating_sub(capture_tx.capacity());

                            let (write_depth, write_cap, db_writable) = {
                                let storage_guard = storage.lock().await;
                                let wd = storage_guard.write_queue_depth();
                                let wc = storage_guard.write_queue_capacity();
                                let writable = storage_guard.is_writable().await;
                                drop(storage_guard);
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
                                            expires_at: ov
                                                .expires_at
                                                .and_then(|e| u64::try_from(e).ok()),
                                        })
                                        .collect()
                                },
                                scheduler: {
                                    let snap = scheduler_snapshot.read().await;
                                    if snap.budget_active { Some(snap.clone()) } else { None }
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
                            last_health_snapshot = now;
                        }
                    }
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
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let initial_interval = self.config.discovery_interval;
        let mut config_rx = self.config_rx.clone();
        let heartbeats = Arc::clone(&self.heartbeats);
        let wezterm = Arc::clone(&self.wezterm_handle);

        tokio::spawn(async move {
            let mut current_interval = initial_interval;

            loop {
                // Wait for interval, checking shutdown periodically to ensure responsiveness
                let deadline = tokio::time::Instant::now() + current_interval;
                loop {
                    if shutdown_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    // Sleep in short bursts to remain responsive to shutdown signals
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }

                // Check shutdown flag
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Discovery task: shutdown signal received");
                    break;
                }

                // Check for config updates (non-blocking)
                if config_rx.has_changed().unwrap_or(false) {
                    let new_config = config_rx.borrow_and_update().clone();
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
                        let mut reg = registry.write().await;
                        let diff = reg.discovery_tick(panes);

                        // Handle new panes
                        for pane_id in &diff.new_panes {
                            if let Some(entry) = reg.get_entry(*pane_id) {
                                // Upsert pane in storage
                                let record = entry.to_pane_record();
                                let storage_guard = storage.lock().await;
                                if let Err(e) = storage_guard.upsert_pane(record).await {
                                    error!(pane_id = pane_id, error = %e, "Failed to upsert pane");
                                }
                                drop(storage_guard);

                                // Create cursor if observed
                                if entry.should_observe() {
                                    // Initialize cursor from storage to resume capture
                                    let storage_guard = storage.lock().await;
                                    let max_seq =
                                        storage_guard.get_max_seq(*pane_id).await.unwrap_or(None);
                                    drop(storage_guard);

                                    let next_seq = max_seq.map_or(0, |s| s + 1);

                                    {
                                        let mut cursors = cursors.write().await;
                                        cursors.insert(
                                            *pane_id,
                                            PaneCursor::from_seq(*pane_id, next_seq),
                                        );
                                    }

                                    {
                                        let mut contexts = detection_contexts.write().await;
                                        let mut ctx = DetectionContext::new();
                                        ctx.pane_id = Some(*pane_id);
                                        contexts.insert(*pane_id, ctx);
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

            // Sync tailers periodically with discovery interval
            let mut sync_tick = tokio::time::interval(discovery_interval);
            let mut join_set = JoinSet::new();

            loop {
                // Determine poll interval dynamically from supervisor config
                // (Using min_interval for responsiveness)
                // Actually supervisor manages per-tailer intervals. We just need to wake up often enough to spawn ready tasks.
                // A fixed tick is fine, supervisor filters ready tasks.
                let tick_duration = Duration::from_millis(10);

                tokio::select! {
                    _ = sync_tick.tick() => {
                        heartbeats.record_capture();

                        if shutdown_flag.load(Ordering::SeqCst) {
                            debug!("Capture task: shutdown signal received");
                            break;
                        }

                        // Check for config updates
                        if config_rx.has_changed().unwrap_or(false) {
                            let new_config = config_rx.borrow_and_update().clone();
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
                    () = tokio::time::sleep(tick_duration) => {
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
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                if shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }
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

            while let Some(event) = event_rx.recv().await {
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

            let _ = accept_handle.await;
        })
    }

    /// Spawn the persistence and detection task.
    fn spawn_persistence_task(
        &self,
        mut capture_rx: mpsc::Receiver<CaptureEvent>,
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
            // Process events until channel closes or shutdown
            while let Some(event) = capture_rx.recv().await {
                heartbeats.record_persistence();
                // Check shutdown flag - if set, drain remaining events quickly
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Persistence task: shutdown signal received, draining remaining events");
                    // Continue to drain but don't block forever
                }

                if config_rx.has_changed().unwrap_or(false) {
                    let new_config = config_rx.borrow_and_update().clone();
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
                let storage_guard = storage.lock().await;
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
                        metrics.segments_persisted.fetch_add(1, Ordering::SeqCst);

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

                        // Run pattern detection on the content
                        let detections = {
                            let mut contexts = detection_contexts.write().await;
                            let ctx = contexts.entry(pane_id).or_insert_with(|| {
                                let mut c = DetectionContext::new();
                                c.pane_id = Some(pane_id);
                                c
                            });

                            // If this was a gap/discontinuity, clear the tail buffer because
                            // previous context is no longer valid or contiguous.
                            if persisted.gap.is_some() {
                                ctx.tail_buffer.clear();
                            }

                            let detections = {
                                let engine = pattern_engine.read().await;
                                engine.detect_with_context(&content, ctx)
                            };
                            drop(contexts);
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
                                        metrics.events_recorded.fetch_add(1, Ordering::SeqCst);

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

/// Handle to the running observation runtime.
pub struct RuntimeHandle {
    /// Discovery task handle
    pub discovery: JoinHandle<()>,
    /// Capture task handle
    pub capture: JoinHandle<()>,
    /// Native events listener task handle (optional)
    pub native_events: Option<JoinHandle<()>>,
    /// Persistence task handle
    pub persistence: JoinHandle<()>,
    /// Maintenance task handle (retention, checkpointing)
    pub maintenance: Option<JoinHandle<()>>,
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
    config_tx: watch::Sender<HotReloadableConfig>,
    /// Optional event bus for workflow integration
    pub event_bus: Option<Arc<EventBus>>,
    /// Heartbeat registry for watchdog monitoring
    pub heartbeats: Arc<HeartbeatRegistry>,
    /// Capture channel sender (cloned for queue depth instrumentation)
    capture_tx: mpsc::Sender<CaptureEvent>,
    /// Shared scheduler snapshot for health reporting (written by capture task).
    scheduler_snapshot: Arc<RwLock<crate::tailer::SchedulerSnapshot>>,
}

/// Backpressure warning threshold as a fraction of channel capacity.
///
/// When queue depth exceeds this fraction of max capacity, a warning is
/// included in the health snapshot.  0.75 = warn at 75% full.
const BACKPRESSURE_WARN_RATIO: f64 = 0.75;

impl RuntimeHandle {
    /// Current capture channel queue depth (pending items waiting for persistence).
    #[must_use]
    pub fn capture_queue_depth(&self) -> usize {
        self.capture_tx.max_capacity() - self.capture_tx.capacity()
    }

    /// Maximum capture channel capacity.
    #[must_use]
    pub fn capture_queue_capacity(&self) -> usize {
        self.capture_tx.max_capacity()
    }

    /// Current write queue depth (pending commands for the storage writer thread).
    pub async fn write_queue_depth(&self) -> usize {
        let storage_guard = self.storage.lock().await;
        storage_guard.write_queue_depth()
    }

    /// Wait for all tasks to complete.
    pub async fn join(self) {
        let _ = self.discovery.await;
        let _ = self.capture.await;
        if let Some(native) = self.native_events {
            let _ = native.await;
        }
        let _ = self.persistence.await;
        if let Some(maintenance) = self.maintenance {
            let _ = maintenance.await;
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
        info!("Shutdown signal sent");

        // Wait for tasks with timeout
        let timeout = Duration::from_secs(5);
        let join_result = tokio::time::timeout(timeout, async {
            let _ = self.discovery.await;
            let _ = self.capture.await;
            if let Some(native) = self.native_events {
                let _ = native.await;
            }
            let _ = self.persistence.await;
        })
        .await;

        let clean = if join_result.is_err() {
            warnings.push("Tasks did not complete within timeout".to_string());
            false
        } else {
            true
        };

        // Get final metrics
        let segments_persisted = self.metrics.segments_persisted.load(Ordering::SeqCst);
        let events_recorded = self.metrics.events_recorded.load(Ordering::SeqCst);

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
            let storage_guard = self.storage.lock().await;
            if let Err(e) = storage_guard.shutdown().await {
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
        self.join().await;
    }

    /// Signal shutdown without waiting.
    pub fn signal_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
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

        let last_seq_by_pane: Vec<(u64, i64)> = {
            let cursors = self.cursors.read().await;
            cursors
                .iter()
                .map(|(pane_id, cursor)| (*pane_id, cursor.last_seq()))
                .collect()
        };

        // Measure queue depths for backpressure visibility
        let capture_depth = self.capture_queue_depth();
        let capture_cap = self.capture_queue_capacity();

        let (write_depth, write_cap, db_writable) = {
            let storage_guard = self.storage.lock().await;
            let wd = storage_guard.write_queue_depth();
            let wc = storage_guard.write_queue_capacity();
            let writable = storage_guard.is_writable().await;
            drop(storage_guard);
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

    #[test]
    fn backpressure_warn_ratio_is_valid() {
        assert!(BACKPRESSURE_WARN_RATIO > 0.0);
        assert!(BACKPRESSURE_WARN_RATIO < 1.0);
    }

    #[test]
    fn mpsc_queue_depth_computation_is_correct() {
        // Validates the max_capacity - capacity pattern used by RuntimeHandle
        let (tx, _rx) = mpsc::channel::<u8>(16);
        let max_cap = tx.max_capacity();
        assert_eq!(max_cap, 16);

        // Empty queue: depth should be 0
        let depth = max_cap - tx.capacity();
        assert_eq!(depth, 0);
    }

    #[tokio::test]
    async fn mpsc_queue_depth_increases_with_sends() {
        let (tx, mut rx) = mpsc::channel::<u8>(16);

        // Send some items
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        tx.send(3).await.unwrap();

        let depth = tx.max_capacity() - tx.capacity();
        assert_eq!(depth, 3);

        // Drain one item, depth should decrease
        let _ = rx.recv().await;
        let depth = tx.max_capacity() - tx.capacity();
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
